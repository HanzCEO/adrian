//! A small Markdown-aware heading scanner.
//!
//! This is not a full CommonMark parser. It needs to do exactly one thing correctly:
//! find ATX headings at the start of a line while ignoring anything that is inside a
//! fenced code block, an indented code block, or an HTML comment. Everything else about
//! the document is preserved as opaque text.

use crate::types::{normalize_typography, SectionKind};

/// One heading found in the document.
#[derive(Debug, Clone, PartialEq)]
pub struct Heading {
    pub level: u8,
    /// Heading text with the leading `#` markers and any trailing `#`s removed.
    pub text: String,
    /// Byte offset of the first `#` of the heading line.
    pub byte_start: usize,
    /// Byte offset just past the newline that ends the heading line.
    pub byte_end: usize,
    /// Byte offset just past the heading line, i.e. where the section body begins.
    pub body_start: usize,
}

/// Scan a document for ATX headings.
pub struct HeadingScanner;

impl HeadingScanner {
    pub fn scan(text: &str) -> Vec<Heading> {
        let mut headings = Vec::new();
        let bytes = text.as_bytes();
        let mut pos = 0usize;

        // Fence tracking: `Some(marker)` while inside a fenced block.
        let mut fence: Option<Fence> = None;
        // HTML comment tracking.
        let mut in_comment = false;

        while pos < bytes.len() {
            let line_start = pos;
            let line_end = match text[line_start..].find('\n') {
                Some(rel) => line_start + rel + 1,
                None => bytes.len(),
            };
            let line = &text[line_start..line_end];
            let trimmed_eol = line.trim_end_matches(['\r', '\n']);

            // Track HTML comments. A comment can open and close on the same line.
            if in_comment {
                if let Some(close) = trimmed_eol.find("-->") {
                    let _ = close;
                    in_comment = false;
                }
                pos = line_end;
                continue;
            }

            if let Some(active) = &fence {
                if active.is_closing(trimmed_eol) {
                    fence = None;
                }
                pos = line_end;
                continue;
            }

            if let Some(opened) = Fence::opening(trimmed_eol) {
                fence = Some(opened);
                pos = line_end;
                continue;
            }

            if let Some(comment_start) = trimmed_eol.find("<!--") {
                let rest = &trimmed_eol[comment_start + 4..];
                if !rest.contains("-->") {
                    in_comment = true;
                    pos = line_end;
                    continue;
                }
                // A comment that opens and closes on this line cannot hide a heading,
                // because the heading would have to start at column 0 and the comment
                // would then be inside the heading text. Fall through.
            }

            // Indented code blocks: four spaces or a tab at the start of the line.
            if trimmed_eol.starts_with("    ") || trimmed_eol.starts_with('\t') {
                pos = line_end;
                continue;
            }

            if let Some(heading) = parse_atx(trimmed_eol, line_start, line_end) {
                headings.push(heading);
            }

            pos = line_end;
        }

        headings
    }
}

struct Fence {
    marker: u8,
    len: usize,
}

impl Fence {
    fn opening(line: &str) -> Option<Fence> {
        let trimmed = line.trim_start();
        let bytes = trimmed.as_bytes();
        let marker = *bytes.first()?;
        if marker != b'`' && marker != b'~' {
            return None;
        }
        let len = bytes.iter().take_while(|b| **b == marker).count();
        if len < 3 {
            return None;
        }
        // An opening backtick fence may not contain backticks in its info string.
        if marker == b'`' && trimmed[len..].contains('`') {
            return None;
        }
        Some(Fence { marker, len })
    }

    fn is_closing(&self, line: &str) -> bool {
        let trimmed = line.trim_start();
        let bytes = trimmed.as_bytes();
        if bytes.first() != Some(&self.marker) {
            return false;
        }
        let len = bytes.iter().take_while(|b| **b == self.marker).count();
        if len < self.len {
            return false;
        }
        // A closing fence may only be followed by whitespace.
        trimmed[len..].trim().is_empty()
    }
}

fn parse_atx(line: &str, line_start: usize, line_end: usize) -> Option<Heading> {
    let bytes = line.as_bytes();
    let mut level = 0usize;
    while level < bytes.len() && bytes[level] == b'#' {
        level += 1;
    }
    if level == 0 || level > 6 {
        return None;
    }
    // A heading needs a space after the markers, unless the line is only markers.
    let after = &line[level..];
    if !after.is_empty() && !after.starts_with(' ') && !after.starts_with('\t') {
        return None;
    }

    let text = after.trim();
    // Strip an optional closing sequence of `#`s.
    let text = text.trim_end_matches('#').trim_end();

    Some(Heading {
        level: level as u8,
        text: text.to_string(),
        byte_start: line_start,
        byte_end: line_end,
        body_start: line_end,
    })
}

/// Map a heading's text onto the closed set of semantic section kinds.
///
/// `parent` is the kind of the enclosing H2, which is what disambiguates
/// "Consequences" (a section under Decision Outcome) from any other use of the word.
pub fn classify(text: &str, level: u8, parent: Option<SectionKind>) -> SectionKind {
    // Some tools emit explicit anchors in the heading, for example
    // `<a name="question"></a> Question`. They are presentation, not wording.
    let without_anchors = strip_html_anchors(text);
    let normalized = normalize_typography(&without_anchors)
        .to_ascii_lowercase()
        .replace([':', '.', ',', ';', '!', '?', '(', ')'], " ");
    let collapsed = normalized.split_whitespace().collect::<Vec<_>>().join(" ");

    // Sub-headings under Decision Outcome.
    if level >= 3 && parent == Some(SectionKind::Outcome) {
        match collapsed.as_str() {
            "consequences" => return SectionKind::Consequences,
            "confirmation" => return SectionKind::Confirmation,
            _ => {}
        }
    }

    // Inside "Pros and Cons of the Options": an H3 is an option title, and anything deeper
    // is a sub-label within that option (`#### Examples`, `#### Pros/cons` in MADR's own
    // ADR-0010). Sub-labels must stay `Other` so they inherit the option's key instead of
    // becoming a heading of their own; `pros/cons` in particular would otherwise match the
    // container alias and capture the option's arguments.
    if level >= 3 && parent == Some(SectionKind::ProsCons) {
        return if level == 3 {
            SectionKind::ProsCons
        } else {
            SectionKind::Other
        };
    }

    match collapsed.as_str() {
        "context"
        | "context and problem statement"
        | "problem statement"
        | "context and problem"
        | "motivation"
        | "issue"
        | "question" => SectionKind::Context,
        "decision drivers"
        | "drivers"
        | "forces"
        | "decision drivers and constraints"
        | "criteria" => SectionKind::Drivers,
        "considered options" | "options" | "alternatives" | "considered alternatives" => {
            SectionKind::Options
        }
        "decision outcome" | "outcome" | "decision" | "chosen option" | "resolution" => {
            SectionKind::Outcome
        }
        "consequences" => SectionKind::Consequences,
        "confirmation" | "validation" => SectionKind::Confirmation,
        "pros and cons of the options" | "pros and cons" | "pros/cons" => SectionKind::ProsCons,
        "more information" | "links" | "references" | "related decisions" | "notes"
        | "comments" => SectionKind::MoreInfo,
        _ => SectionKind::Other,
    }
}

/// Remove leading HTML tags from a heading's text, so that an explicit anchor does not
/// stop the heading from being recognised.
fn strip_html_anchors(text: &str) -> String {
    let mut working = text.trim();
    loop {
        if !working.starts_with('<') {
            break;
        }
        let Some(close) = working.find('>') else {
            break;
        };
        working = working[close + 1..].trim();
    }
    working.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_atx_headings() {
        let text = "# Title\n\n## Context\n\nbody\n\n### Sub\n";
        let headings = HeadingScanner::scan(text);
        assert_eq!(headings.len(), 3);
        assert_eq!(headings[0].level, 1);
        assert_eq!(headings[0].text, "Title");
        assert_eq!(headings[1].level, 2);
        assert_eq!(headings[1].text, "Context");
        assert_eq!(headings[2].level, 3);
        assert_eq!(headings[2].text, "Sub");
    }

    #[test]
    fn ignores_headings_inside_fenced_code() {
        let text = "# Real\n\n```\n# Not a heading\n## Also not\n```\n\n## After\n";
        let headings = HeadingScanner::scan(text);
        let texts: Vec<_> = headings.iter().map(|h| h.text.as_str()).collect();
        assert_eq!(texts, vec!["Real", "After"]);
    }

    #[test]
    fn ignores_headings_inside_tilde_fence() {
        let text = "# Real\n\n~~~markdown\n# Not a heading\n~~~\n\n## After\n";
        let headings = HeadingScanner::scan(text);
        let texts: Vec<_> = headings.iter().map(|h| h.text.as_str()).collect();
        assert_eq!(texts, vec!["Real", "After"]);
    }

    #[test]
    fn ignores_headings_inside_html_comment() {
        let text = "# Real\n\n<!--\n# Hidden\n-->\n\n## After\n";
        let headings = HeadingScanner::scan(text);
        let texts: Vec<_> = headings.iter().map(|h| h.text.as_str()).collect();
        assert_eq!(texts, vec!["Real", "After"]);
    }

    #[test]
    fn ignores_indented_code_blocks() {
        let text = "# Real\n\n    # indented code\n\n## After\n";
        let headings = HeadingScanner::scan(text);
        let texts: Vec<_> = headings.iter().map(|h| h.text.as_str()).collect();
        assert_eq!(texts, vec!["Real", "After"]);
    }

    #[test]
    fn rejects_hashes_without_space() {
        let text = "#NoSpace\n# Real\n";
        let headings = HeadingScanner::scan(text);
        assert_eq!(headings.len(), 1);
        assert_eq!(headings[0].text, "Real");
    }

    #[test]
    fn strips_closing_hashes() {
        let text = "## Context ##\n";
        let headings = HeadingScanner::scan(text);
        assert_eq!(headings[0].text, "Context");
    }

    #[test]
    fn classifies_known_headings() {
        assert_eq!(
            classify("Context and Problem Statement", 2, None),
            SectionKind::Context
        );
        assert_eq!(classify("Decision Drivers", 2, None), SectionKind::Drivers);
        assert_eq!(
            classify("Considered Options", 2, None),
            SectionKind::Options
        );
        assert_eq!(classify("Decision Outcome", 2, None), SectionKind::Outcome);
        assert_eq!(
            classify("Pros and Cons of the Options", 2, None),
            SectionKind::ProsCons
        );
        assert_eq!(classify("More Information", 2, None), SectionKind::MoreInfo);
        assert_eq!(classify("Random Heading", 2, None), SectionKind::Other);
    }

    #[test]
    fn strips_html_anchors_before_classifying() {
        assert_eq!(
            classify("<a name=\"question\"></a> Question", 2, None),
            SectionKind::Context
        );
        assert_eq!(
            classify("<a name=\"options\"></a> Options", 2, None),
            SectionKind::Options
        );
    }

    #[test]
    fn deeper_headings_under_pros_and_cons_are_not_new_options() {
        assert_eq!(
            classify("Examples", 4, Some(SectionKind::ProsCons)),
            SectionKind::Other
        );
        // `#### Pros/cons` is a sub-label inside an option, not the container and not a new
        // option. Reading it as a container is what swallowed a real option's arguments.
        assert_eq!(
            classify("Pros/cons", 4, Some(SectionKind::ProsCons)),
            SectionKind::Other
        );
        assert_eq!(
            classify(
                "Pros and Cons of the Options",
                4,
                Some(SectionKind::ProsCons)
            ),
            SectionKind::Other
        );
    }

    #[test]
    fn classifies_option_headings_under_pros_and_cons() {
        assert_eq!(
            classify("tokio", 3, Some(SectionKind::ProsCons)),
            SectionKind::ProsCons
        );
        assert_eq!(
            classify("Something Weird", 3, Some(SectionKind::ProsCons)),
            SectionKind::ProsCons
        );
    }

    #[test]
    fn classifies_consequences_by_parent() {
        assert_eq!(
            classify("Consequences", 3, Some(SectionKind::Outcome)),
            SectionKind::Consequences
        );
        assert_eq!(
            classify("Confirmation", 3, Some(SectionKind::Outcome)),
            SectionKind::Confirmation
        );
    }
}
