//! One-way export.
//!
//! Adrian owns the record, so export is a projection, not a round trip. Two properties
//! matter and both are tested:
//!
//! * A record imported from Markdown and never modified exports as its original bytes.
//!   Nobody forgives a tool that reformats their files.
//! * Canonical JSON is byte-identical across runs, so it can be diffed and committed.

use crate::store::{Store, StoreError};
use crate::types::*;
use serde_json::json;
use std::fmt;
use thiserror::Error;

/// A failure while exporting.
#[derive(Debug, Error)]
pub enum ExportError {
    #[error("{0}")]
    NotFound(String),

    #[error("{0}")]
    Invalid(String),

    #[error(transparent)]
    Store(#[from] StoreError),
}

/// The output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    /// Canonical JSON, one object per decision, stable key order.
    Json,
    /// MADR Markdown rendered from the projection.
    Madr,
}

impl ExportFormat {
    pub fn parse(value: &str) -> Result<ExportFormat, ExportError> {
        match value {
            "json" => Ok(ExportFormat::Json),
            "madr" | "markdown" | "md" => Ok(ExportFormat::Madr),
            other => Err(ExportError::Invalid(format!(
                "unknown export format `{other}`; expected `json` or `madr`"
            ))),
        }
    }
}

impl fmt::Display for ExportFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExportFormat::Json => f.write_str("json"),
            ExportFormat::Madr => f.write_str("madr"),
        }
    }
}

/// The schema version written into canonical JSON.
pub const SCHEMA_VERSION: &str = "1.0";

/// Export every decision in a project.
pub async fn export_project(
    store: &Store,
    project_id: uuid::Uuid,
    format: ExportFormat,
) -> Result<Vec<u8>, ExportError> {
    let adrs = store.list_adrs(project_id, None, 10_000, 0).await?;
    // Oldest first, so the output reads like a decision log rather than a reverse feed.
    let mut adrs = adrs;
    adrs.sort_by_key(|adr| (adr.number.unwrap_or(i64::MAX), adr.created_at));

    let mut out = String::new();
    match format {
        ExportFormat::Json => {
            out.push_str("[\n");
            for (index, adr) in adrs.iter().enumerate() {
                let record = export_adr_json(store, adr).await?;
                let rendered = serde_json::to_string_pretty(&record)
                    .map_err(|err| ExportError::Invalid(err.to_string()))?;
                if index > 0 {
                    out.push_str(",\n");
                }
                out.push_str(&indent(&rendered, 2));
            }
            out.push_str("\n]\n");
        }
        ExportFormat::Madr => {
            for adr in &adrs {
                let body = export_adr_markdown(store, adr).await?;
                out.push_str(&body);
                if !body.ends_with('\n') {
                    out.push('\n');
                }
                out.push('\n');
            }
        }
    }
    Ok(out.into_bytes())
}

fn indent(text: &str, spaces: usize) -> String {
    let pad = " ".repeat(spaces);
    text.lines()
        .map(|line| format!("{pad}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// One decision as canonical JSON.
pub async fn export_adr_json(store: &Store, adr: &Adr) -> Result<serde_json::Value, ExportError> {
    let revision = store.get_revision(adr.id, adr.cur_rev).await?;
    let sections = store.get_sections(adr.id, adr.cur_rev).await?;
    let items = store.get_items(adr.id, adr.cur_rev).await?;
    let scopes = store.get_scopes(adr.id).await?;
    let technologies = store.get_technologies(adr.id).await?;
    let events = store.list_status_events(adr.id).await?;

    Ok(json!({
        "schema_version": SCHEMA_VERSION,
        "id": adr.id.to_string(),
        "project_id": adr.project_id.to_string(),
        "external_ref": adr.external_ref,
        "number": adr.number,
        "slug": adr.slug,
        "status": adr.cur_status.as_str(),
        "rev": adr.cur_rev,
        "created_at": rfc3339(adr.created_at),
        "created_by": adr.created_by,
        "revision": {
            "title": revision.title,
            "template": revision.template,
            "source_format": revision.source_format,
            "decided_on": revision.decided_on,
            "source_sha256": hex(&revision.source_sha256),
            "content_sha256": hex(&revision.content_sha256),
            "tx_from": rfc3339(revision.tx_from),
            "tx_to": revision.tx_to.map(rfc3339),
        },
        "sections": sections.iter().map(|section| json!({
            "kind": section.kind.as_str(),
            "raw_heading": section.raw_heading,
            "ordinal": section.ordinal,
            "parent": section.parent,
            "option_key": section.option_key,
            "text": section.text,
            "byte_start": section.byte_start,
            "byte_end": section.byte_end,
        })).collect::<Vec<_>>(),
        "items": items.iter().map(|item| json!({
            "kind": item.kind.as_str(),
            "section": item.section,
            "ordinal": item.ordinal,
            "text": item.text,
            "option_key": item.option_key,
            "byte_start": item.byte_start,
            "byte_end": item.byte_end,
        })).collect::<Vec<_>>(),
        "scopes": scopes.iter().map(|scope| json!({
            "kind": scope.kind.as_str(),
            "value": scope.value,
            "origin": scope.origin.as_str(),
            "confidence": scope.confidence,
        })).collect::<Vec<_>>(),
        "technologies": technologies.iter().map(|technology| json!({
            "canonical": technology.canonical,
            "role": technology.role.as_str(),
        })).collect::<Vec<_>>(),
        "status_events": events.iter().map(|event| json!({
            "at": rfc3339(event.recorded_at),
            "from": event.from_status.map(|status| status.as_str()),
            "to": event.to_status.as_str(),
            "actor": event.actor,
            "reason": event.reason,
        })).collect::<Vec<_>>(),
    }))
}

/// One decision as MADR Markdown.
///
/// An imported record that was never modified is returned verbatim, so that exporting a
/// corpus that Adrian merely indexed does not rewrite it.
pub async fn export_adr_markdown(store: &Store, adr: &Adr) -> Result<String, ExportError> {
    let revision = store.get_revision(adr.id, adr.cur_rev).await?;
    if revision.source_format == "markdown" && adr.cur_rev == 1 {
        if let Ok(text) = String::from_utf8(revision.source_bytes.clone()) {
            return Ok(text);
        }
    }
    render_madr(store, adr).await
}

/// Render a decision from its projection rather than from its original bytes.
pub async fn render_madr(store: &Store, adr: &Adr) -> Result<String, ExportError> {
    let revision = store.get_revision(adr.id, adr.cur_rev).await?;
    let sections = store.get_sections(adr.id, adr.cur_rev).await?;
    let events = store.list_status_events(adr.id).await?;

    let mut out = String::new();

    // Front matter, omitting keys with no value so that the output is not padded with
    // empty fields a reader would have to skip.
    let mut front: Vec<String> = Vec::new();
    front.push(format!("status: {}", adr.cur_status.as_str()));
    if let Some(date) = &revision.decided_on {
        front.push(format!("date: {date}"));
    }
    let deciders: Vec<String> = events
        .iter()
        .filter(|event| event.to_status == Status::Accepted)
        .map(|event| event.actor.clone())
        .collect();
    if !deciders.is_empty() {
        front.push(format!("decision-makers: {}", join_list(&deciders)));
    }
    out.push_str("---\n");
    for line in front {
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str("---\n\n");

    out.push_str(&format!("# {}\n\n", revision.title));

    // Every section is rendered, including an empty one. Dropping an empty heading would
    // break the parent chain on re-parse (an option heading with no H2 above it stops being
    // recognised as an option), and it would also quietly discard a section the author
    // deliberately left blank, as the MADR bare template does.
    for (index, section) in sections.iter().enumerate() {
        // Reproduce the author's heading depth rather than flattening to two levels: a
        // heading nested under an option (`#### Examples`) must not come back as an option
        // of its own.
        let level = match depth_of(&sections, index) {
            0 => "##",
            1 => "###",
            2 => "####",
            3 => "#####",
            _ => "######",
        };

        out.push_str(&format!("{level} {}\n\n", section.raw_heading));

        // The body is written exactly as parsed. Re-emitting the derived items instead was
        // what made this lossy: an option's prose was dropped because only its bullets were
        // written, and a sub-label's bullets were written twice because both the sub-label
        // and the option above it emitted them. The section text already contains every
        // bullet, every paragraph and every fenced block, so writing it verbatim reproduces
        // the document and the items are simply re-derived on the next parse.
        let body = section.text.trim();
        if !body.is_empty() {
            out.push_str(body);
            out.push('\n');
            out.push('\n');
        }
    }

    Ok(out.trim_end().to_string() + "\n")
}

/// How deep a section sits below the top level.
fn depth_of(sections: &[ParsedSection], index: usize) -> usize {
    let mut depth = 0usize;
    let mut current = sections.get(index).and_then(|section| section.parent);
    // The chain is finite because a parent always has a lower ordinal.
    while let Some(parent) = current {
        depth += 1;
        if depth > 8 {
            break;
        }
        current = sections.get(parent).and_then(|section| section.parent);
    }
    depth
}

fn join_list(values: &[String]) -> String {
    let mut seen = std::collections::BTreeSet::new();
    let unique: Vec<&str> = values
        .iter()
        .filter(|value| seen.insert(value.as_str()))
        .map(|value| value.as_str())
        .collect();
    if unique.len() == 1 {
        unique[0].to_string()
    } else {
        unique.join(", ")
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn rfc3339(seconds: i64) -> String {
    chrono::DateTime::from_timestamp(seconds, 0)
        .unwrap_or_else(|| chrono::DateTime::from_timestamp(0, 0).expect("epoch is valid"))
        .to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_parse_and_reject_nonsense() {
        assert_eq!(ExportFormat::parse("json").unwrap(), ExportFormat::Json);
        assert_eq!(ExportFormat::parse("madr").unwrap(), ExportFormat::Madr);
        assert_eq!(ExportFormat::parse("md").unwrap(), ExportFormat::Madr);
        assert!(ExportFormat::parse("pdf").is_err());
    }

    #[test]
    fn join_list_deduplicates_and_preserves_order() {
        let values = vec![
            "human:a".to_string(),
            "human:b".to_string(),
            "human:a".to_string(),
        ];
        assert_eq!(join_list(&values), "human:a, human:b");
        assert_eq!(join_list(&["human:a".to_string()]), "human:a");
    }

    #[test]
    fn rfc3339_handles_the_epoch() {
        assert!(rfc3339(0).starts_with("1970-01-01"));
    }
}
