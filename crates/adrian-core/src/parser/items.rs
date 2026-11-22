//! Bullet extraction.
//!
//! MADR puts the arguments for an option in a different section from the option itself,
//! so items are extracted with byte spans and an optional `option_key` that the caller
//! uses to join the two halves back together.

use crate::types::{normalize_typography, ItemKind};

/// One raw bullet found inside a section body.
#[derive(Debug, Clone, PartialEq)]
pub struct RawBullet {
    /// Bullet text with the marker removed and trailing HTML comments stripped.
    pub text: String,
    /// Byte offset of the list marker itself, so spans point at real source.
    pub byte_start: usize,
    /// Byte offset just past the bullet's last line.
    pub byte_end: usize,
    /// Nesting depth: 0 for a top-level bullet.
    pub depth: usize,
}

/// Extract the top-level and nested bullets from a section body.
///
/// `body` must be the exact slice of the source document, and `body_offset` its byte
/// offset, so that the returned spans index the original document.
pub fn extract_bullets(body: &str, body_offset: usize) -> Vec<RawBullet> {
    let mut bullets = Vec::new();
    let bytes = body.as_bytes();
    let mut pos = 0usize;
    let mut fence: Option<u8> = None;

    while pos < bytes.len() {
        let line_start = pos;
        let line_end = match body[line_start..].find('\n') {
            Some(rel) => line_start + rel + 1,
            None => bytes.len(),
        };
        let line = &body[line_start..line_end];
        let trimmed_eol = line.trim_end_matches(['\r', '\n']);

        // Do not treat list markers inside fenced code as bullets.
        let leading = trimmed_eol.trim_start();
        if let Some(marker) = fence_marker(leading) {
            match fence {
                Some(active) if active == marker => fence = None,
                None => fence = Some(marker),
                _ => {}
            }
            pos = line_end;
            continue;
        }
        if fence.is_some() {
            pos = line_end;
            continue;
        }

        if let Some((marker_offset, depth)) = bullet_marker(trimmed_eol) {
            let text_start = marker_offset + marker_len(&trimmed_eol[marker_offset..]);
            let raw_text = trimmed_eol[text_start..].trim();
            let text = strip_trailing_html_comment(raw_text);
            if !text.is_empty() {
                bullets.push(RawBullet {
                    text,
                    byte_start: body_offset + line_start,
                    byte_end: body_offset + line_end,
                    depth,
                });
            }
        }

        pos = line_end;
    }

    bullets
}

fn fence_marker(trimmed: &str) -> Option<u8> {
    let bytes = trimmed.as_bytes();
    let marker = *bytes.first()?;
    if marker != b'`' && marker != b'~' {
        return None;
    }
    let len = bytes.iter().take_while(|b| **b == marker).count();
    if len >= 3 {
        Some(marker)
    } else {
        None
    }
}

/// Return the byte offset of the list marker within the line, and its nesting depth.
fn bullet_marker(line: &str) -> Option<(usize, usize)> {
    let mut depth = 0usize;
    let mut idx = 0usize;
    let bytes = line.as_bytes();

    loop {
        // Count one indentation step (up to four spaces or one tab).
        let mut spaces = 0usize;
        while idx < bytes.len() && bytes[idx] == b' ' && spaces < 2 {
            idx += 1;
            spaces += 1;
        }
        if spaces == 0 && idx < bytes.len() && bytes[idx] == b'\t' {
            idx += 1;
            spaces = 2;
        }
        if spaces == 0 {
            break;
        }
        match bytes.get(idx) {
            Some(b'*') | Some(b'-') | Some(b'+') => {
                depth += 1;
                continue;
            }
            Some(b'0'..=b'9') => {
                let digits = bytes[idx..]
                    .iter()
                    .take_while(|b| b.is_ascii_digit())
                    .count();
                if bytes.get(idx + digits) == Some(&b'.') || bytes.get(idx + digits) == Some(&b')')
                {
                    depth += 1;
                    idx += digits + 1;
                    continue;
                }
                break;
            }
            _ => break,
        }
    }

    let start = idx;
    match bytes.get(start) {
        Some(b'*') | Some(b'-') | Some(b'+') => Some((start, depth)),
        Some(b'0'..=b'9') => {
            let digits = bytes[start..]
                .iter()
                .take_while(|b| b.is_ascii_digit())
                .count();
            match bytes.get(start + digits) {
                Some(b'.') | Some(b')') => Some((start, depth)),
                _ => None,
            }
        }
        _ => None,
    }
}

fn marker_len(rest: &str) -> usize {
    let bytes = rest.as_bytes();
    let mut idx = 0usize;
    if let Some(b'0'..=b'9') = bytes.first() {
        idx += bytes.iter().take_while(|b| b.is_ascii_digit()).count();
    }
    if matches!(bytes.get(idx), Some(b'.') | Some(b')')) {
        idx += 1;
    } else if matches!(bytes.get(idx), Some(b'*') | Some(b'-') | Some(b'+')) {
        idx += 1;
    }
    // One optional space after the marker.
    if bytes.get(idx) == Some(&b' ') {
        idx += 1;
    }
    idx
}

/// Drop a trailing `<!-- ... -->` annotation such as MADR's
/// `<!-- numbers of options can vary -->`.
fn strip_trailing_html_comment(text: &str) -> String {
    let mut working = text.trim().to_string();
    loop {
        let trimmed = working.trim_end();
        if !trimmed.ends_with("-->") {
            break;
        }
        let Some(open) = trimmed.rfind("<!--") else {
            break;
        };
        working = trimmed[..open].trim_end().to_string();
    }
    working
}

/// Classify a bullet inside a section of the given kind.
pub fn classify_bullet(section: crate::types::SectionKind, text: &str) -> Option<ItemKind> {
    use crate::types::SectionKind;
    let lowered = normalize_typography(text).to_ascii_lowercase();
    let head = lowered.trim_start();

    match section {
        SectionKind::Drivers => Some(ItemKind::Driver),
        SectionKind::Options => Some(ItemKind::Option),
        SectionKind::Consequences => {
            if head.starts_with("good") {
                Some(ItemKind::ConsequenceGood)
            } else if head.starts_with("bad") {
                Some(ItemKind::ConsequenceBad)
            } else {
                Some(ItemKind::ConsequenceOther)
            }
        }
        SectionKind::ProsCons => {
            if head.starts_with("good") {
                Some(ItemKind::Pro)
            } else if head.starts_with("bad") {
                Some(ItemKind::Con)
            } else if head.starts_with("neutral") {
                Some(ItemKind::Neutral)
            } else {
                // Prose under an option heading, not a Good/Neutral/Bad bullet.
                None
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SectionKind;

    #[test]
    fn extracts_top_level_bullets() {
        let body = "* first\n* second\n* third\n";
        let bullets = extract_bullets(body, 0);
        assert_eq!(bullets.len(), 3);
        assert_eq!(bullets[0].text, "first");
        assert_eq!(bullets[2].text, "third");
    }

    #[test]
    fn strips_madr_placeholder_comments() {
        let body = "* option one\n* \u{2026} <!-- numbers of options can vary -->\n";
        let bullets = extract_bullets(body, 0);
        assert_eq!(bullets.len(), 2);
        assert_eq!(bullets[1].text, "\u{2026}");
    }

    #[test]
    fn skips_fenced_code() {
        let body = "```\n* not a bullet\n```\n* real\n";
        let bullets = extract_bullets(body, 0);
        assert_eq!(bullets.len(), 1);
        assert_eq!(bullets[0].text, "real");
    }

    #[test]
    fn tracks_depth() {
        let body = "* top\n  * nested\n";
        let bullets = extract_bullets(body, 0);
        assert_eq!(bullets.len(), 2);
        assert_eq!(bullets[0].depth, 0);
        assert_eq!(bullets[1].depth, 1);
    }

    #[test]
    fn supports_ordered_and_dash_markers() {
        let body = "1. one\n2. two\n- three\n+ four\n";
        let bullets = extract_bullets(body, 0);
        let texts: Vec<_> = bullets.iter().map(|b| b.text.as_str()).collect();
        assert_eq!(texts, vec!["one", "two", "three", "four"]);
    }

    #[test]
    fn classifies_consequence_and_pros_cons_bullets() {
        assert_eq!(
            classify_bullet(SectionKind::Consequences, "Good, because faster"),
            Some(ItemKind::ConsequenceGood)
        );
        assert_eq!(
            classify_bullet(SectionKind::Consequences, "Bad, because slower"),
            Some(ItemKind::ConsequenceBad)
        );
        assert_eq!(
            classify_bullet(SectionKind::ProsCons, "Neutral, because it depends"),
            Some(ItemKind::Neutral)
        );
        assert_eq!(classify_bullet(SectionKind::ProsCons, "Just prose"), None);
    }
}
