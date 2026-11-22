//! Domain vocabulary for Adrian.
//!
//! These types are deliberately storage-agnostic. The SQLite layer converts to and
//! from them; the HTTP layer serialises them. Anything that is a closed set of values
//! is an enum here so that an invalid value cannot be represented.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// The lifecycle state of a decision.
///
/// MADR leaves this vocabulary open on purpose and its own ADR-0013 records that as a
/// weakness ("pretends to be more accurate than it can be"). Adrian closes it, because a
/// database has to be able to compare two statuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Proposed,
    Accepted,
    Rejected,
    Deprecated,
    Superseded,
    Withdrawn,
}

impl Status {
    pub const ALL: [Status; 6] = [
        Status::Proposed,
        Status::Accepted,
        Status::Rejected,
        Status::Deprecated,
        Status::Superseded,
        Status::Withdrawn,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Status::Proposed => "proposed",
            Status::Accepted => "accepted",
            Status::Rejected => "rejected",
            Status::Deprecated => "deprecated",
            Status::Superseded => "superseded",
            Status::Withdrawn => "withdrawn",
        }
    }

    /// Map a free-form status found in an imported document onto the closed set.
    ///
    /// Returns `None` when the value is not recognisable, so the caller can record a
    /// warning and fall back to [`Status::Proposed`] rather than silently guessing.
    pub fn parse_loose(raw: &str) -> Option<Status> {
        let needle = raw
            .trim()
            .trim_matches(|c: char| c == '"' || c == '\'' || c == '{' || c == '}')
            .to_ascii_lowercase();
        // Authors write things like "superseded by ADR-0123" or "proposed | accepted".
        // Take the leading word or phrase and match on that.
        let needle = needle
            .split(['|', ',', ';', '/', '\n'])
            .next()
            .unwrap_or("")
            .trim();

        const PREFIXES: [(&str, Status); 15] = [
            ("superseded by", Status::Superseded),
            ("superseded", Status::Superseded),
            ("deprecated", Status::Deprecated),
            ("withdrawn", Status::Withdrawn),
            ("proposed", Status::Proposed),
            ("accepted", Status::Accepted),
            ("rejected", Status::Rejected),
            ("approved", Status::Accepted),
            ("abandoned", Status::Withdrawn),
            ("obsolete", Status::Deprecated),
            ("draft", Status::Proposed),
            ("pending", Status::Proposed),
            ("open", Status::Proposed),
            ("decided", Status::Accepted),
            ("agreed", Status::Accepted),
        ];
        PREFIXES
            .into_iter()
            .find(|(prefix, _)| needle.starts_with(prefix))
            .map(|(_, status)| status)
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Status {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let normalized = s.trim().to_ascii_lowercase();
        Status::ALL
            .into_iter()
            .find(|candidate| candidate.as_str() == normalized)
            .ok_or_else(|| {
                format!(
                    "unknown status `{s}`; expected one of {}",
                    Status::ALL
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
    }
}

/// The semantic role of a section, independent of the template that produced it.
///
/// This is the normalisation that lets MADR, Nygard and Y-statement records live in one
/// table. The original heading is kept alongside as `raw_heading`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SectionKind {
    Context,
    Drivers,
    Options,
    Outcome,
    Consequences,
    Confirmation,
    ProsCons,
    MoreInfo,
    Other,
}

impl SectionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SectionKind::Context => "context",
            SectionKind::Drivers => "drivers",
            SectionKind::Options => "options",
            SectionKind::Outcome => "outcome",
            SectionKind::Consequences => "consequences",
            SectionKind::Confirmation => "confirmation",
            SectionKind::ProsCons => "pros_cons",
            SectionKind::MoreInfo => "more_info",
            SectionKind::Other => "other",
        }
    }

    pub fn from_str_lossy(s: &str) -> Option<SectionKind> {
        Some(match s {
            "context" => SectionKind::Context,
            "drivers" => SectionKind::Drivers,
            "options" => SectionKind::Options,
            "outcome" => SectionKind::Outcome,
            "consequences" => SectionKind::Consequences,
            "confirmation" => SectionKind::Confirmation,
            "pros_cons" => SectionKind::ProsCons,
            "more_info" => SectionKind::MoreInfo,
            "other" => SectionKind::Other,
            _ => return None,
        })
    }
}

impl fmt::Display for SectionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A single bullet extracted from a section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Driver,
    Option,
    Pro,
    Con,
    Neutral,
    ConsequenceGood,
    ConsequenceBad,
    ConsequenceOther,
}

impl ItemKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ItemKind::Driver => "driver",
            ItemKind::Option => "option",
            ItemKind::Pro => "pro",
            ItemKind::Con => "con",
            ItemKind::Neutral => "neutral",
            ItemKind::ConsequenceGood => "consequence_good",
            ItemKind::ConsequenceBad => "consequence_bad",
            ItemKind::ConsequenceOther => "consequence_other",
        }
    }

    pub fn from_str_lossy(s: &str) -> Option<ItemKind> {
        Some(match s {
            "driver" => ItemKind::Driver,
            "option" => ItemKind::Option,
            "pro" => ItemKind::Pro,
            "con" => ItemKind::Con,
            "neutral" => ItemKind::Neutral,
            "consequence_good" => ItemKind::ConsequenceGood,
            "consequence_bad" => ItemKind::ConsequenceBad,
            "consequence_other" => ItemKind::ConsequenceOther,
            _ => return None,
        })
    }
}

impl fmt::Display for ItemKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The unit that actually gets embedded.
///
/// The ADR template is the chunking scheme: one chunk per decision-relevant argument,
/// not one chunk per N characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkKind {
    Document,
    Context,
    Driver,
    Option,
    Outcome,
    Consequence,
    Confirmation,
    MoreInfo,
}

impl ChunkKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ChunkKind::Document => "document",
            ChunkKind::Context => "context",
            ChunkKind::Driver => "driver",
            ChunkKind::Option => "option",
            ChunkKind::Outcome => "outcome",
            ChunkKind::Consequence => "consequence",
            ChunkKind::Confirmation => "confirmation",
            ChunkKind::MoreInfo => "more_info",
        }
    }

    pub fn from_str_lossy(s: &str) -> Option<ChunkKind> {
        Some(match s {
            "document" => ChunkKind::Document,
            "context" => ChunkKind::Context,
            "driver" => ChunkKind::Driver,
            "option" => ChunkKind::Option,
            "outcome" => ChunkKind::Outcome,
            "consequence" => ChunkKind::Consequence,
            "confirmation" => ChunkKind::Confirmation,
            "more_info" => ChunkKind::MoreInfo,
            _ => return None,
        })
    }
}

impl fmt::Display for ChunkKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a piece of extracted knowledge came from.
///
/// Adrian cannot see a repository, so scope is an assertion by the client. Making the
/// origin explicit is the only honest way to expose that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScopeOrigin {
    /// Pushed by the client alongside the ADR. Authoritative.
    Declared,
    /// Pulled out of the ADR text by the chunker. A guess.
    Extracted,
    /// Derived from a technology binding. A weaker guess.
    Inferred,
}

impl ScopeOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            ScopeOrigin::Declared => "declared",
            ScopeOrigin::Extracted => "extracted",
            ScopeOrigin::Inferred => "inferred",
        }
    }

    pub fn from_str_lossy(s: &str) -> Option<ScopeOrigin> {
        Some(match s {
            "declared" => ScopeOrigin::Declared,
            "extracted" => ScopeOrigin::Extracted,
            "inferred" => ScopeOrigin::Inferred,
            _ => return None,
        })
    }
}

impl fmt::Display for ScopeOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The kind of code region a decision binds to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeKind {
    PathPrefix,
    Crate,
    Module,
    Symbol,
    Service,
    ConfigKey,
}

impl ScopeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ScopeKind::PathPrefix => "path_prefix",
            ScopeKind::Crate => "crate",
            ScopeKind::Module => "module",
            ScopeKind::Symbol => "symbol",
            ScopeKind::Service => "service",
            ScopeKind::ConfigKey => "config_key",
        }
    }

    pub fn from_str_lossy(s: &str) -> Option<ScopeKind> {
        Some(match s {
            "path_prefix" | "path" => ScopeKind::PathPrefix,
            "crate" => ScopeKind::Crate,
            "module" => ScopeKind::Module,
            "symbol" => ScopeKind::Symbol,
            "service" => ScopeKind::Service,
            "config_key" => ScopeKind::ConfigKey,
            _ => return None,
        })
    }
}

impl fmt::Display for ScopeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether a technology was picked or passed over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TechnologyRole {
    Chosen,
    Rejected,
    Constrained,
}

impl TechnologyRole {
    pub fn as_str(self) -> &'static str {
        match self {
            TechnologyRole::Chosen => "chosen",
            TechnologyRole::Rejected => "rejected",
            TechnologyRole::Constrained => "constrained",
        }
    }

    pub fn from_str_lossy(s: &str) -> Option<TechnologyRole> {
        Some(match s {
            "chosen" => TechnologyRole::Chosen,
            "rejected" => TechnologyRole::Rejected,
            "constrained" => TechnologyRole::Constrained,
            _ => return None,
        })
    }
}

impl fmt::Display for TechnologyRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A code region a decision is asserted to bind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scope {
    pub kind: ScopeKind,
    pub value: String,
    pub origin: ScopeOrigin,
    pub confidence: f32,
}

/// A technology named by a decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Technology {
    pub canonical: String,
    pub role: TechnologyRole,
}

/// A parsed section, with byte offsets back into the original document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParsedSection {
    pub kind: SectionKind,
    pub raw_heading: String,
    pub ordinal: usize,
    /// Index into [`ParsedAdr::sections`] of the parent, for H3 under an H2.
    pub parent: Option<usize>,
    pub text: String,
    pub option_key: Option<String>,
    pub byte_start: usize,
    pub byte_end: usize,
}

/// A parsed list item, with byte offsets back into the original document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParsedItem {
    pub kind: ItemKind,
    /// Index into [`ParsedAdr::sections`] of the owning section.
    pub section: usize,
    pub ordinal: usize,
    pub text: String,
    pub option_key: Option<String>,
    pub byte_start: usize,
    pub byte_end: usize,
}

/// A non-fatal observation made while parsing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ParseWarning {
    /// The document has no H1 title.
    MissingTitle,
    /// The front matter is present but not valid YAML.
    InvalidFrontMatter { message: String },
    /// The front matter status is not in Adrian's closed vocabulary.
    UnknownStatus { raw: String },
    /// An option appears in "Considered Options" with no matching H3 under pros and cons.
    UnmatchedOption { option: String },
    /// An H3 under pros and cons matches no entry in "Considered Options".
    OrphanProsCons { heading: String },
    /// A heading that MADR knows about was not found.
    MissingSection { section: String },
    /// Something was skipped and the reason is worth reporting.
    Note { message: String },
}

/// The full projection of one ADR document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParsedAdr {
    pub title: String,
    pub template: String,
    pub decided_on: Option<String>,
    pub status_raw: Option<String>,
    pub status: Option<Status>,
    pub decision_makers: Vec<String>,
    pub consulted: Vec<String>,
    pub informed: Vec<String>,
    /// Front matter keys Adrian does not interpret, preserved verbatim.
    pub extra: serde_json::Map<String, serde_json::Value>,
    pub sections: Vec<ParsedSection>,
    pub items: Vec<ParsedItem>,
    pub warnings: Vec<ParseWarning>,
}

impl ParsedAdr {
    /// The section with the given kind, if present.
    pub fn section(&self, kind: SectionKind) -> Option<&ParsedSection> {
        self.sections.iter().find(|s| s.kind == kind)
    }

    /// All items with the given kind, in source order.
    pub fn items_of(&self, kind: ItemKind) -> impl Iterator<Item = &ParsedItem> {
        self.items.iter().filter(move |i| i.kind == kind)
    }
}

/// One unit of text handed to the embedder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Chunk {
    pub id: uuid::Uuid,
    pub adr_id: uuid::Uuid,
    pub rev: i64,
    pub kind: ChunkKind,
    pub ordinal: usize,
    pub section_id: Option<uuid::Uuid>,
    pub text: String,
    pub text_sha256: [u8; 32],
    pub token_count: Option<usize>,
    pub truncated: bool,
}

/// A stored ADR header row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Adr {
    pub id: uuid::Uuid,
    pub project_id: uuid::Uuid,
    pub external_ref: String,
    pub number: Option<i64>,
    pub slug: String,
    pub created_at: i64,
    pub created_by: String,
    pub cur_rev: i64,
    pub cur_status: Status,
}

/// One immutable revision of an ADR.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdrRevision {
    pub adr_id: uuid::Uuid,
    pub rev: i64,
    pub title: String,
    pub template: String,
    pub source_format: String,
    pub source_bytes: Vec<u8>,
    pub source_sha256: [u8; 32],
    pub content_sha256: [u8; 32],
    pub decided_on: Option<String>,
    pub tx_from: i64,
    pub tx_to: Option<i64>,
}

/// A recorded status change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusEvent {
    pub id: uuid::Uuid,
    pub adr_id: uuid::Uuid,
    pub from_status: Option<Status>,
    pub to_status: Status,
    pub effective_from: Option<String>,
    pub recorded_at: i64,
    pub actor: String,
    pub reason: Option<String>,
    pub superseded_by: Option<uuid::Uuid>,
}

/// A project: one bounded context whose decisions are logged together.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub id: uuid::Uuid,
    pub workspace_id: uuid::Uuid,
    pub slug: String,
    pub name: String,
    pub repo_url: Option<String>,
    pub default_template: String,
    pub embed_model: String,
    pub embed_version: i64,
    pub created_at: i64,
}

/// One search hit at chunk granularity, as returned by the vector index.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoredChunk {
    pub chunk_id: uuid::Uuid,
    pub adr_id: uuid::Uuid,
    pub score: f32,
}

/// A hydrated search result at ADR granularity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    pub adr_id: uuid::Uuid,
    pub title: String,
    pub status: Status,
    pub score: f32,
    pub matched_chunks: Vec<MatchedChunk>,
}

/// One chunk that contributed to a [`SearchHit`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MatchedChunk {
    pub chunk_id: uuid::Uuid,
    pub kind: ChunkKind,
    pub text: String,
    pub score: f32,
}

/// What an ingest call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IngestAction {
    Created,
    Revised,
    Unchanged,
}

/// The outcome of an ingest call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IngestOutcome {
    pub action: IngestAction,
    pub adr_id: uuid::Uuid,
    pub rev: i64,
    pub status: Status,
    pub chunks: usize,
    pub warnings: Vec<ParseWarning>,
}

/// A role in the embedder contract.
///
/// This is not an inference. The model recognises a query by a `[Q]` marker at position 1
/// and truncates query token vectors at position 64, so the same text embeds differently
/// depending on which side of the retrieval it is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EmbedRole {
    Document,
    Query,
}

impl EmbedRole {
    pub fn as_str(self) -> &'static str {
        match self {
            EmbedRole::Document => "document",
            EmbedRole::Query => "query",
        }
    }
}

/// Compute the deterministic option key used to join "Considered Options" to the
/// matching H3 under "Pros and Cons of the Options".
///
/// MADR deliberately has no option identifiers: its own ADR-0006 rejected them because
/// "there is no markdown standard for identifiers". The two mentions of an option are
/// therefore related only by title string equality, so Adrian synthesises a stable key.
pub fn option_key(title: &str) -> String {
    let normalized = normalize_typography(title);
    let mut working = normalized.trim().to_string();

    // Markdown emphasis markers around or inside the title.
    working = working.replace(['*', '_', '`'], "");

    // Surrounding quotes, possibly nested, possibly repeated.
    loop {
        let trimmed = working.trim();
        let stripped = trimmed
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .or_else(|| {
                trimmed
                    .strip_prefix('\'')
                    .and_then(|s| s.strip_suffix('\''))
            })
            .or_else(|| trimmed.strip_prefix('`').and_then(|s| s.strip_suffix('`')));
        match stripped {
            Some(inner) => working = inner.trim().to_string(),
            None => break,
        }
    }

    // Trailing punctuation that authors sprinkle inconsistently.
    let working = working.trim_end_matches(|c: char| {
        matches!(
            c,
            '.' | ',' | ':' | ';' | '!' | '?' | '-' | '\u{2013}' | '\u{2014}'
        )
    });

    // Collapse internal whitespace, then lowercase.
    let collapsed = working.split_whitespace().collect::<Vec<_>>().join(" ");
    let lowered = collapsed.to_lowercase();

    format!("opt:{lowered}")
}

/// Replace typographic quotes and dashes with their ASCII equivalents.
///
/// Authors mix these freely and MADR's own corpus contains both forms for the same option,
/// so normalising before comparison is what makes the join work in practice.
pub fn normalize_typography(input: &str) -> String {
    input
        .chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' | '\u{2032}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' | '\u{2033}' => '"',
            '\u{2013}' | '\u{2014}' | '\u{2015}' | '\u{2212}' => '-',
            '\u{00A0}' | '\u{2007}' | '\u{202F}' => ' ',
            other => other,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_key_strips_quotes_punctuation_and_emphasis() {
        assert_eq!(
            option_key("Use YAML front matter"),
            "opt:use yaml front matter"
        );
        assert_eq!(
            option_key("\"Use YAML front matter\""),
            "opt:use yaml front matter"
        );
        assert_eq!(
            option_key("Use YAML front matter."),
            "opt:use yaml front matter"
        );
        assert_eq!(
            option_key("  Use   YAML\tfront matter  "),
            "opt:use yaml front matter"
        );
        assert_eq!(
            option_key("**Use YAML front matter**"),
            "opt:use yaml front matter"
        );
    }

    #[test]
    fn option_key_normalizes_smart_quotes_and_dashes() {
        assert_eq!(
            option_key("Include in section \u{201C}More Information\u{201D}"),
            option_key("Include in section \"More Information\"")
        );
        assert_eq!(
            option_key("Plain \u{2014} JUnit5"),
            option_key("Plain - JUnit5")
        );
    }

    #[test]
    fn option_key_handles_empty_and_odd_input() {
        assert_eq!(option_key(""), "opt:");
        assert_eq!(option_key("   "), "opt:");
        assert_eq!(option_key("???"), "opt:");
    }

    #[test]
    fn status_parses_loosely_but_rejects_nonsense() {
        assert_eq!(Status::parse_loose("Accepted"), Some(Status::Accepted));
        assert_eq!(
            Status::parse_loose("{proposed | rejected | accepted}"),
            Some(Status::Proposed)
        );
        assert_eq!(
            Status::parse_loose("superseded by ADR-0123"),
            Some(Status::Superseded)
        );
        assert_eq!(Status::parse_loose("decided"), Some(Status::Accepted));
        assert_eq!(Status::parse_loose("we will see"), None);
    }

    #[test]
    fn status_round_trips_through_str() {
        for status in Status::ALL {
            assert_eq!(status.as_str().parse::<Status>().unwrap(), status);
        }
        assert!("nonsense".parse::<Status>().is_err());
    }
}
