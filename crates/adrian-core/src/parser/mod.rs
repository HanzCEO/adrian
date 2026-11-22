//! MADR parsing: Markdown in, typed projection out.
//!
//! The parser is deliberately forgiving. Real ADR corpora contain bare templates,
//! annotated templates, missing front matter, headings inside fenced code blocks and
//! option titles that differ only in punctuation. None of that is an error: it produces a
//! [`ParseWarning`] and a best-effort projection.
//!
//! Every section and item carries `byte_start`/`byte_end` into the original source, so a
//! consumer can cite the exact text an assertion came from.

mod frontmatter;
mod headings;
mod items;

pub use frontmatter::FrontMatter;
pub use headings::{Heading, HeadingScanner};

use crate::types::*;

/// Parse one MADR document.
pub fn parse(source: &[u8]) -> ParsedAdr {
    let text = String::from_utf8_lossy(source);
    parse_str(text.as_ref())
}

/// Parse one MADR document from a `&str`.
pub fn parse_str(text: &str) -> ParsedAdr {
    let mut warnings = Vec::new();
    let (front, body_offset) = frontmatter::parse(text, &mut warnings);

    let body = &text[body_offset.min(text.len())..];
    let headings = HeadingScanner::scan(body);

    // Collect the H1 as the title, everything else becomes a section.
    let mut title = String::new();
    let mut section_headings: Vec<(usize, &Heading)> = Vec::new();
    for heading in &headings {
        if heading.level == 1 && title.is_empty() {
            title = heading.text.clone();
            continue;
        }
        section_headings.push((body_offset, heading));
    }

    // Some formats (ADG, and anything Jekyll-flavoured) keep the title in front matter
    // rather than as an H1. Accept it, but only as a fallback: the H1 wins when both exist.
    if title.is_empty() {
        if let Some(from_front) = front.extra.get("title").and_then(|v| v.as_str()) {
            let trimmed = from_front.trim();
            if !trimmed.is_empty() {
                title = trimmed.to_string();
            }
        }
    }
    if title.is_empty() {
        warnings.push(ParseWarning::MissingTitle);
    }

    // Build the section list with parent links.
    let mut sections: Vec<ParsedSection> = Vec::new();
    // Stack of (level, index) for ancestor resolution.
    let mut ancestors: Vec<(u8, usize)> = Vec::new();

    for (position, (base, heading)) in section_headings.iter().enumerate() {
        let abs_start = base + heading.byte_start;
        let abs_body_start = base + heading.body_start;
        let abs_end = match section_headings.get(position + 1) {
            Some((next_base, next)) => next_base + next.byte_start,
            None => text.len(),
        };

        while let Some((level, _)) = ancestors.last() {
            if *level >= heading.level {
                ancestors.pop();
            } else {
                break;
            }
        }
        let parent = ancestors.last().map(|(_, idx)| *idx);
        let parent_kind = parent.and_then(|idx| sections.get(idx)).map(|s| s.kind);

        let kind = headings::classify(&heading.text, heading.level, parent_kind);
        let body_text = &text[abs_body_start.min(text.len())..abs_end];
        let option_key = if kind == SectionKind::ProsCons && heading.level >= 3 {
            Some(option_key_of_heading(&heading.text))
        } else {
            None
        };

        let index = sections.len();
        sections.push(ParsedSection {
            kind,
            raw_heading: heading.text.clone(),
            ordinal: index,
            parent,
            text: body_text.to_string(),
            option_key,
            byte_start: abs_start,
            byte_end: abs_end,
        });
        ancestors.push((heading.level, index));
    }

    // An option's body can contain deeper headings (`#### Examples` under an option in
    // MADR's own ADR-0010). Those sections are not new options, but their bullets still
    // belong to the enclosing option, so the key is inherited down the ancestor chain.
    let mut inherited: Vec<Option<String>> = Vec::with_capacity(sections.len());
    for section in sections.iter() {
        let key = match (&section.option_key, section.parent) {
            (Some(key), _) => Some(key.clone()),
            (None, Some(parent)) => inherited.get(parent).cloned().flatten(),
            (None, None) => None,
        };
        inherited.push(key);
    }
    for (index, section) in sections.iter_mut().enumerate() {
        if section.option_key.is_none() {
            if let Some(key) = inherited.get(index).cloned().flatten() {
                section.option_key = Some(key);
            }
        }
    }

    // Extract items from the sections that carry them.
    let mut parsed_items: Vec<ParsedItem> = Vec::new();
    for (section_index, section) in sections.iter().enumerate() {
        let bullets = items::extract_bullets(&section.text, section.byte_start_of_body(text));
        let mut ordinal = 0usize;
        for bullet in bullets {
            let Some(kind) = items::classify_bullet(section.kind, &bullet.text) else {
                continue;
            };
            parsed_items.push(ParsedItem {
                kind,
                section: section_index,
                ordinal,
                text: bullet.text.clone(),
                option_key: section.option_key.clone(),
                byte_start: bullet.byte_start,
                byte_end: bullet.byte_end,
            });
            ordinal += 1;
        }
    }

    // Join "Considered Options" entries to their "Pros and Cons" subsections.
    let option_titles: Vec<String> = parsed_items
        .iter()
        .filter(|i| i.kind == ItemKind::Option)
        .map(|i| i.text.clone())
        .collect();
    let pros_cons_keys: Vec<String> = sections
        .iter()
        .filter(|s| s.kind == SectionKind::ProsCons && s.option_key.is_some())
        .filter_map(|s| s.option_key.clone())
        .collect();

    for title_text in &option_titles {
        let key = option_key(title_text);
        if !pros_cons_keys.contains(&key) {
            warnings.push(ParseWarning::UnmatchedOption {
                option: title_text.clone(),
            });
        }
    }
    for section in &sections {
        if section.kind != SectionKind::ProsCons {
            continue;
        }
        let Some(key) = &section.option_key else {
            continue;
        };
        let matched = option_titles.iter().any(|t| &option_key(t) == key);
        if !matched {
            warnings.push(ParseWarning::OrphanProsCons {
                heading: section.raw_heading.clone(),
            });
        }
    }

    if front.status_raw.is_some() && front.status.is_none() {
        // Already recorded by the front matter parser as UnknownStatus; nothing to add.
    }

    ParsedAdr {
        title,
        template: detect_template(text, &front),
        decided_on: front.date.clone(),
        status_raw: front.status_raw.clone(),
        status: front.status,
        decision_makers: front.decision_makers.clone(),
        consulted: front.consulted.clone(),
        informed: front.informed.clone(),
        extra: front.extra.clone(),
        sections,
        items: parsed_items,
        warnings,
    }
}

impl ParsedSection {
    /// Absolute byte offset where this section's body starts.
    ///
    /// The body begins just past the heading line, which is `byte_start` plus the
    /// heading line's own length. Recomputing it here keeps [`ParsedSection`] free of a
    /// redundant field.
    fn byte_start_of_body(&self, text: &str) -> usize {
        match text[self.byte_start..].find('\n') {
            Some(rel) => self.byte_start + rel + 1,
            None => self.byte_end,
        }
    }
}

fn option_key_of_heading(heading: &str) -> String {
    option_key(heading)
}

/// Best-effort template identification. Used for reporting, not for parsing.
fn detect_template(text: &str, front: &FrontMatter) -> String {
    let lowered = text.to_ascii_lowercase();
    let has_front_matter = !front.extra.is_empty()
        || front.status_raw.is_some()
        || front.date.is_some()
        || !front.decision_makers.is_empty();

    if has_front_matter {
        return "madr".to_string();
    }
    if lowered.contains("## status") && lowered.contains("## decision") {
        return "nygard".to_string();
    }
    if lowered.contains("in the context of") && lowered.contains("we decided for") {
        return "y-statement".to_string();
    }
    "madr".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE: &str = r#"---
status: accepted
date: 2024-03-01
decision-makers: Alice, Bob
---

# Use tokio as the async runtime

## Context and Problem Statement

We need an async runtime for the API server.

## Decision Drivers

* Performance
* Ecosystem maturity

## Considered Options

* tokio
* async-std

## Decision Outcome

Chosen option: "tokio", because it is the de-facto standard.

### Consequences

* Good, because ecosystem support is excellent
* Bad, because the dependency tree is large

## Pros and Cons of the Options

### tokio

The most widely used runtime.

* Good, because it has the largest ecosystem
* Bad, because it is opinionated

### async-std

A smaller alternative.

* Good, because the API mirrors std
* Bad, because adoption is low
"#;

    #[test]
    fn parses_a_full_madr() {
        let adr = parse_str(SIMPLE);
        assert_eq!(adr.title, "Use tokio as the async runtime");
        assert_eq!(adr.status, Some(Status::Accepted));
        assert_eq!(adr.decided_on.as_deref(), Some("2024-03-01"));
        assert_eq!(adr.decision_makers, vec!["Alice", "Bob"]);
        assert!(
            adr.warnings.is_empty(),
            "unexpected warnings: {:?}",
            adr.warnings
        );

        let kinds: Vec<SectionKind> = adr.sections.iter().map(|s| s.kind).collect();
        assert_eq!(
            kinds,
            vec![
                SectionKind::Context,
                SectionKind::Drivers,
                SectionKind::Options,
                SectionKind::Outcome,
                SectionKind::Consequences,
                // The H2 container, then one H3 per option.
                SectionKind::ProsCons,
                SectionKind::ProsCons,
                SectionKind::ProsCons,
            ]
        );
    }

    #[test]
    fn joins_options_to_their_pros_and_cons() {
        let adr = parse_str(SIMPLE);
        let option_items: Vec<&ParsedItem> = adr.items_of(ItemKind::Option).collect();
        assert_eq!(option_items.len(), 2);
        assert_eq!(option_items[0].text, "tokio");

        let pros_cons_keys: Vec<&str> = adr
            .sections
            .iter()
            .filter(|s| s.kind == SectionKind::ProsCons)
            .filter_map(|s| s.option_key.as_deref())
            .collect();
        assert_eq!(pros_cons_keys, vec!["opt:tokio", "opt:async-std"]);
    }

    #[test]
    fn byte_spans_slice_the_original_source() {
        let adr = parse_str(SIMPLE);
        for section in &adr.sections {
            let slice = &SIMPLE[section.byte_start..section.byte_end];
            assert!(
                slice.contains(&section.raw_heading),
                "section span must contain its heading, got {slice:?}"
            );
        }
        for item in &adr.items {
            let slice = &SIMPLE[item.byte_start..item.byte_end];
            assert!(
                slice.contains(item.text.split('\n').next().unwrap_or("")),
                "item span {slice:?} must contain its text {:?}",
                item.text
            );
        }
    }

    #[test]
    fn warns_when_an_option_has_no_pros_and_cons_section() {
        let source = "# T\n\n## Considered Options\n\n* a\n* b\n";
        let adr = parse_str(source);
        let unmatched = adr
            .warnings
            .iter()
            .filter(|w| matches!(w, ParseWarning::UnmatchedOption { .. }))
            .count();
        assert_eq!(unmatched, 2);
    }

    #[test]
    fn warns_when_a_pros_and_cons_heading_matches_nothing() {
        let source = "# T\n\n## Considered Options\n\n* a\n\n## Pros and Cons of the Options\n\n### zzz\n\n* Good, because yes\n";
        let adr = parse_str(source);
        assert!(adr
            .warnings
            .iter()
            .any(|w| matches!(w, ParseWarning::OrphanProsCons { .. })));
    }

    #[test]
    fn missing_title_is_a_warning_not_an_error() {
        let adr = parse_str("## Context and Problem Statement\n\nsomething\n");
        assert!(adr.warnings.contains(&ParseWarning::MissingTitle));
        assert_eq!(adr.sections.len(), 1);
    }

    #[test]
    fn unknown_status_falls_back_to_none() {
        let adr = parse_str("---\nstatus: maybe later\n---\n# T\n");
        assert!(adr.status.is_none());
        assert_eq!(adr.status_raw.as_deref(), Some("maybe later"));
        assert!(adr
            .warnings
            .iter()
            .any(|w| matches!(w, ParseWarning::UnknownStatus { .. })));
    }

    #[test]
    fn empty_status_is_not_a_warning() {
        let adr = parse_str("---\nstatus:\ndate:\n---\n# T\n");
        assert!(adr.status.is_none());
        assert!(adr.status_raw.is_none());
        assert!(!adr
            .warnings
            .iter()
            .any(|w| matches!(w, ParseWarning::UnknownStatus { .. })));
    }

    #[test]
    fn never_panics_on_truncated_input() {
        for end in 0..SIMPLE.len() {
            let _ = parse(&SIMPLE.as_bytes()[..end]);
        }
    }
}
