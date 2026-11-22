//! Request and response shapes.
//!
//! These are the single source of truth for the JSON the API speaks: handlers serialise
//! them directly, and the MCP tools return the same values as text so that an agent sees
//! exactly what a REST client sees.

use adrian_core::types::*;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Health and models
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub version: String,
    pub embedder: DependencyHealth,
    pub qdrant: DependencyHealth,
}

#[derive(Debug, Serialize)]
pub struct DependencyHealth {
    pub reachable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collection: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ModelsResponse {
    #[serde(flatten)]
    pub model: adrian_core::embedder::ModelInfo,
    pub collection: String,
}

// ---------------------------------------------------------------------------
// Projects
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateProjectRequest {
    pub slug: String,
    pub name: String,
    #[serde(default)]
    pub repo_url: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ProjectResponse {
    pub id: String,
    pub slug: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_url: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Serialize)]
pub struct ProjectListItem {
    pub id: String,
    pub slug: String,
    pub name: String,
    pub adr_count: i64,
}

#[derive(Debug, Serialize)]
pub struct DeleteProjectResponse {
    pub deleted: bool,
    pub slug: String,
    pub footprint: FootprintView,
}

/// What a delete reported removing. A project and a single decision share the shape so a
/// client can render both without a special case.
#[derive(Debug, Serialize)]
pub struct FootprintView {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adrs: Option<i64>,
    pub revisions: i64,
    pub chunks: i64,
    pub vectors: i64,
    pub embed_jobs: i64,
}

// ---------------------------------------------------------------------------
// ADRs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateAdrRequest {
    /// Project slug.
    pub project: String,
    /// The client's stable key for this decision.
    pub external_ref: String,
    /// The raw document.
    pub source: String,
    #[serde(default)]
    pub source_format: Option<String>,
    /// Scope the client asserts this decision binds.
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct IngestResponse {
    pub id: String,
    pub rev: i64,
    pub status: String,
    pub chunks: usize,
    /// `created`, `revised` or `unchanged`. A `String` rather than a `&'static str` so that
    /// a stored response can be deserialised for an idempotent replay.
    pub action: String,
    pub warnings: Vec<ParseWarning>,
}

#[derive(Debug, Serialize)]
pub struct AdrSummary {
    pub id: String,
    pub number: Option<i64>,
    pub title: String,
    pub status: String,
    pub external_ref: String,
    pub rev: i64,
    pub created_at: String,
}

#[derive(Debug, Serialize)]
pub struct AdrDetail {
    pub id: String,
    pub project_id: String,
    pub external_ref: String,
    pub number: Option<i64>,
    pub slug: String,
    pub status: String,
    pub rev: i64,
    pub created_at: String,
    pub created_by: String,
    pub title: String,
    pub template: String,
    pub source_format: String,
    pub decided_on: Option<String>,
    pub sections: Vec<SectionView>,
    pub items: Vec<ItemView>,
    pub scopes: Vec<ScopeView>,
    pub technologies: Vec<TechnologyView>,
    pub chunks: Vec<ChunkView>,
    pub status_events: Vec<StatusEventView>,
}

#[derive(Debug, Serialize)]
pub struct SectionView {
    pub kind: String,
    pub raw_heading: String,
    pub ordinal: usize,
    pub parent: Option<usize>,
    pub option_key: Option<String>,
    pub text: String,
    pub byte_start: usize,
    pub byte_end: usize,
}

#[derive(Debug, Serialize)]
pub struct ItemView {
    pub kind: String,
    pub section: usize,
    pub ordinal: usize,
    pub text: String,
    pub option_key: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ScopeView {
    pub kind: String,
    pub value: String,
    pub origin: String,
    pub confidence: f32,
}

#[derive(Debug, Serialize)]
pub struct TechnologyView {
    pub canonical: String,
    pub role: String,
}

#[derive(Debug, Serialize)]
pub struct ChunkView {
    pub id: String,
    pub kind: String,
    pub ordinal: usize,
    pub text: String,
    pub token_count: Option<usize>,
    pub truncated: bool,
    pub embedded: bool,
}

#[derive(Debug, Serialize)]
pub struct StatusEventView {
    pub at: String,
    pub from: Option<String>,
    pub to: String,
    pub actor: String,
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TransitionRequest {
    pub to: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub actor: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TransitionResponse {
    pub status: String,
    pub event_id: String,
}

#[derive(Debug, Serialize)]
pub struct DeleteAdrResponse {
    pub deleted: bool,
    pub id: String,
    pub external_ref: String,
    pub footprint: FootprintView,
}

// ---------------------------------------------------------------------------
// Batch import
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct BatchImportRequest {
    /// Project slug.
    pub project: String,
    #[serde(default)]
    pub actor: Option<String>,
    /// Keep the status each document's own front matter declares.
    ///
    /// Off by default, for the same reason `adr_submit` forces `proposed`: a submission
    /// must not accept its own decision. Set it only when migrating a corpus whose
    /// decisions were already accepted elsewhere.
    #[serde(default)]
    pub preserve_status: bool,
    pub documents: Vec<BatchDocument>,
}

#[derive(Debug, Deserialize)]
pub struct BatchDocument {
    pub external_ref: String,
    pub source: String,
    #[serde(default)]
    pub source_format: Option<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct BatchImportResponse {
    pub project: String,
    pub created: usize,
    pub revised: usize,
    pub unchanged: usize,
    pub failed: usize,
    pub results: Vec<BatchItemResult>,
}

/// One document's outcome inside a batch. A failure is recorded here rather than aborting
/// the batch, because a 163-file import should report the two bad files, not lose the 161
/// good ones.
#[derive(Debug, Serialize)]
pub struct BatchItemResult {
    pub external_ref: String,
    /// `created`, `revised`, `unchanged` or `failed`.
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rev: Option<i64>,
    /// The status the decision was recorded with, when the ingest succeeded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<ParseWarning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub results: Vec<SearchResult>,
}

#[derive(Debug, Serialize)]
pub struct SearchResult {
    pub adr_id: String,
    pub number: Option<i64>,
    pub title: String,
    pub status: String,
    pub score: f32,
    pub matched_chunks: Vec<MatchedChunkView>,
}

#[derive(Debug, Serialize)]
pub struct MatchedChunkView {
    pub chunk_id: String,
    pub kind: String,
    pub text: String,
    pub score: f32,
}

#[derive(Debug, Deserialize)]
pub struct SimilarQuery {
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct ContextQuery {
    pub path: String,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct ContextResponse {
    pub path: String,
    pub decisions: Vec<ContextDecision>,
}

#[derive(Debug, Serialize)]
pub struct ContextDecision {
    pub adr_id: String,
    pub number: Option<i64>,
    pub title: String,
    pub status: String,
    /// The scope value that matched, so the caller can see why this came back.
    pub matched_scope: String,
    pub scopes: Vec<ScopeView>,
    pub summary: String,
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ExportRequest {
    pub project: String,
    #[serde(default = "default_format")]
    pub format: String,
    #[serde(default)]
    pub adr_ids: Option<Vec<String>>,
}

fn default_format() -> String {
    "madr".to_string()
}

// ---------------------------------------------------------------------------
// List query
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct DetailQuery {
    #[serde(default)]
    pub rev: Option<i64>,
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

/// Format a Unix timestamp as RFC 3339.
///
/// An out-of-range timestamp is reported as the epoch rather than panicking: this is a
/// presentation helper on a request path, so a bad value must not take the server down.
pub fn rfc3339(seconds: i64) -> String {
    match chrono::DateTime::from_timestamp(seconds, 0) {
        Some(datetime) => datetime.to_rfc3339(),
        None => "1970-01-01T00:00:00+00:00".to_string(),
    }
}

pub fn project_view(project: &Project) -> ProjectResponse {
    ProjectResponse {
        id: project.id.to_string(),
        slug: project.slug.clone(),
        name: project.name.clone(),
        repo_url: project.repo_url.clone(),
        created_at: rfc3339(project.created_at),
    }
}

pub fn scope_view(scope: &Scope) -> ScopeView {
    ScopeView {
        kind: scope.kind.as_str().to_string(),
        value: scope.value.clone(),
        origin: scope.origin.as_str().to_string(),
        confidence: scope.confidence,
    }
}

pub fn technology_view(technology: &Technology) -> TechnologyView {
    TechnologyView {
        canonical: technology.canonical.clone(),
        role: technology.role.as_str().to_string(),
    }
}
