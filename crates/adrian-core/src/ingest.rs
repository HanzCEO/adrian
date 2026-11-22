//! The write path.
//!
//! Ingest is the only way a record enters Adrian. It is idempotent on content and on an
//! explicit idempotency key, and it never mutates an existing revision: a changed document
//! becomes a new revision and the old one stays queryable forever.

use crate::chunker::{self, TokenCounter};
use crate::parser;
use crate::store::{ChunkRow, ItemRow, NewRevision, SectionRow, Store, StoreError};
use crate::types::*;
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

/// A failure while ingesting a document.
#[derive(Debug, Error)]
pub enum IngestError {
    #[error("{0}")]
    NotFound(String),

    #[error("{0}")]
    Conflict(String),

    #[error("{0}")]
    Invalid(String),

    #[error(transparent)]
    Store(#[from] StoreError),

    #[error(transparent)]
    Embedder(#[from] crate::embedder::EmbedderError),

    #[error(transparent)]
    Qdrant(#[from] crate::qdrant::QdrantError),
}

type Result<T> = std::result::Result<T, IngestError>;

/// Everything needed for one ingest call.
pub struct IngestRequest {
    pub project_id: Uuid,
    /// The client's stable key for this decision. Numbers are display-only, because Adrian
    /// cannot see the directory they are unique within.
    pub external_ref: String,
    pub source: Vec<u8>,
    /// `markdown`, `json`, `text`. Stored so that export can be lossless.
    pub source_format: String,
    /// Who is writing. Unverified in v0.1.0.
    pub actor: String,
    /// Scope the client asserts. Authoritative, unlike anything Adrian infers.
    pub declared_scopes: Vec<Scope>,
    /// Force the initial status, ignoring the document's own front matter.
    ///
    /// The MCP surface uses this to force `proposed` for agent submissions, so that an
    /// agent cannot self-accept a decision.
    pub status_override: Option<Status>,
    pub embed_model: String,
    pub embed_version: i64,
    /// Optional explicit number for display. Taken from the front matter or the filename
    /// by the caller when it has one.
    pub number: Option<i64>,
}

/// Ingest one document.
pub async fn ingest(
    store: &Store,
    request: IngestRequest,
    counter: &dyn TokenCounter,
) -> Result<IngestOutcome> {
    let source_sha = sha256(&request.source);
    let source_text = String::from_utf8_lossy(&request.source).into_owned();

    let existing = store
        .get_adr_by_external_ref(request.project_id, request.external_ref.clone())
        .await?;

    // Same bytes under the same key: nothing to do, and nothing to re-embed.
    if let Some(adr) = &existing {
        let revision = store.get_revision(adr.id, adr.cur_rev).await?;
        if revision.source_sha256 == source_sha {
            return Ok(IngestOutcome {
                action: IngestAction::Unchanged,
                adr_id: adr.id,
                rev: adr.cur_rev,
                status: adr.cur_status,
                chunks: store.list_chunks(adr.id, adr.cur_rev).await?.len(),
                warnings: Vec::new(),
                superseded_points: Vec::new(),
            });
        }
    }

    // Same bytes under a different key: this is a rename, not a new decision. Recognising
    // it keeps the decision's identity and history intact across a file move.
    if existing.is_none() {
        if let Some(adr) = store
            .find_adr_by_source_sha(request.project_id, source_sha)
            .await?
        {
            store
                .rename_external_ref(adr.id, request.external_ref.clone())
                .await?;
            tracing::info!(
                adr = %adr.id,
                new_ref = %request.external_ref,
                "recognised a rename: the bytes match an existing decision"
            );
            return Ok(IngestOutcome {
                action: IngestAction::Unchanged,
                adr_id: adr.id,
                rev: adr.cur_rev,
                status: adr.cur_status,
                chunks: store.list_chunks(adr.id, adr.cur_rev).await?.len(),
                warnings: Vec::new(),
                superseded_points: Vec::new(),
            });
        }
    }

    let parsed = parser::parse(&request.source);
    let adr_id = existing
        .as_ref()
        .map(|adr| adr.id)
        .unwrap_or_else(Uuid::now_v7);
    let rev = existing.as_ref().map(|adr| adr.cur_rev + 1).unwrap_or(1);

    let status = request
        .status_override
        .or(parsed.status)
        .unwrap_or(Status::Proposed);

    let chunked = chunker::chunk(
        &parsed,
        &source_text,
        adr_id,
        rev,
        counter,
        chunker::DEFAULT_MAX_TOKENS,
    )
    .map_err(|err| IngestError::Invalid(err.to_string()))?;

    let slug = slugify(&parsed.title, rev);
    let number = request
        .number
        .or_else(|| number_from_ref(&request.external_ref));

    // Section ids are deterministic so that a re-parse of the same revision is idempotent.
    let mut section_ids = Vec::with_capacity(parsed.sections.len());
    for section in &parsed.sections {
        section_ids.push(Uuid::new_v5(
            &Uuid::NAMESPACE_OID,
            format!("{adr_id}||{rev}||section||{}", section.ordinal).as_bytes(),
        ));
    }

    let sections: Vec<SectionRow> = parsed
        .sections
        .iter()
        .enumerate()
        .map(|(index, section)| SectionRow {
            id: section_ids[index],
            parent_id: section
                .parent
                .and_then(|parent| section_ids.get(parent).copied()),
            kind: section.kind,
            raw_heading: section.raw_heading.clone(),
            ordinal: section.ordinal,
            text: section.text.clone(),
            option_key: section.option_key.clone(),
            byte_start: section.byte_start,
            byte_end: section.byte_end,
            content_sha256: sha256(section.text.as_bytes()),
        })
        .collect();

    let items: Vec<ItemRow> = parsed
        .items
        .iter()
        .enumerate()
        .map(|(index, item)| ItemRow {
            id: Uuid::new_v5(
                &Uuid::NAMESPACE_OID,
                format!("{adr_id}||{rev}||item||{index}").as_bytes(),
            ),
            section_id: section_ids
                .get(item.section)
                .copied()
                .unwrap_or_else(Uuid::nil),
            kind: item.kind,
            ordinal: item.ordinal,
            text: item.text.clone(),
            option_key: item.option_key.clone(),
            byte_start: item.byte_start,
            byte_end: item.byte_end,
        })
        .collect();

    let chunks: Vec<ChunkRow> = chunked
        .chunks
        .iter()
        .map(|chunk| ChunkRow {
            id: chunk.id,
            kind: chunk.kind,
            ordinal: chunk.ordinal,
            section_id: None,
            text: chunk.text.clone(),
            text_sha256: chunk.text_sha256,
            token_count: chunk.token_count,
            truncated: chunk.truncated,
        })
        .collect();

    // Declared scope wins over anything inferred: the client has the repository and Adrian
    // does not.
    let scopes = merge_scopes(request.declared_scopes.clone(), chunked.scopes.clone());

    let content_sha = sha256(source_text.as_bytes());

    let revision = NewRevision {
        adr_id,
        project_id: request.project_id,
        rev,
        title: parsed.title.clone(),
        template: parsed.template.clone(),
        source_format: request.source_format.clone(),
        source_bytes: request.source.clone(),
        source_sha256: source_sha,
        content_sha256: content_sha,
        decided_on: parsed.decided_on.clone(),
        create_adr: existing.is_none(),
        external_ref: request.external_ref.clone(),
        number,
        slug,
        created_by: request.actor.clone(),
        status,
        actor: request.actor.clone(),
        sections,
        items,
        chunks,
        scopes,
        technologies: chunked.technologies.clone(),
        embed_model: request.embed_model.clone(),
        embed_version: request.embed_version,
    };

    let action = if existing.is_some() {
        // Close the previous revision's transaction interval before opening a new one.
        store.close_previous_revision(adr_id, rev - 1).await?;
        IngestAction::Revised
    } else {
        IngestAction::Created
    };

    let chunk_count = revision.chunks.len();
    let superseded = store.insert_adr_with_revision(revision).await?;

    if !superseded.is_empty() {
        tracing::info!(
            adr = %adr_id,
            points = superseded.len(),
            "revision supersedes older revisions; their index entries were dropped"
        );
    }

    Ok(IngestOutcome {
        action,
        adr_id,
        rev,
        status,
        chunks: chunk_count,
        warnings: parsed.warnings.clone(),
        superseded_points: superseded.into_iter().map(|(_, point)| point).collect(),
    })
}

/// Merge client-asserted scope with what the chunker guessed.
///
/// Declared entries win on a collision, because a client that has the repository knows
/// more than a regex over prose.
pub fn merge_scopes(declared: Vec<Scope>, extracted: Vec<Scope>) -> Vec<Scope> {
    let mut out: Vec<Scope> = Vec::new();
    let mut seen: std::collections::BTreeSet<(ScopeKind, String)> =
        std::collections::BTreeSet::new();

    for scope in declared {
        let key = (scope.kind, normalize_scope_value(&scope.value));
        if seen.insert(key) {
            out.push(Scope {
                origin: ScopeOrigin::Declared,
                confidence: 1.0,
                ..scope
            });
        }
    }
    for scope in extracted {
        let key = (scope.kind, normalize_scope_value(&scope.value));
        if seen.insert(key) {
            out.push(scope);
        }
    }
    out
}

/// Normalise a scope value so that two spellings of the same path do not both survive.
fn normalize_scope_value(value: &str) -> String {
    value
        .trim()
        .trim_start_matches("./")
        .trim_start_matches('/')
        .trim_end_matches('/')
        .to_string()
}

/// Turn a title into a filesystem-friendly slug.
fn slugify(title: &str, rev: i64) -> String {
    let mut slug = String::new();
    let mut last_dash = false;
    for c in title.chars() {
        if c.is_alphanumeric() {
            for lower in c.to_lowercase() {
                slug.push(lower);
            }
            last_dash = false;
        } else if !last_dash && !slug.is_empty() {
            slug.push('-');
            last_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        slug = format!("adr-{rev}");
    }
    slug.truncate(80);
    while slug.ends_with('-') {
        slug.pop();
    }
    slug
}

/// Pull a leading number out of a filename-like reference, e.g. `docs/adr/0042-use-x.md`.
fn number_from_ref(external_ref: &str) -> Option<i64> {
    let file = external_ref.rsplit('/').next()?;
    let digits: String = file.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse::<i64>().ok()
}

pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let mut out = [0u8; 32];
    out.copy_from_slice(&hasher.finalize());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunker::WhitespaceTokenCounter;

    fn scope(kind: ScopeKind, value: &str, origin: ScopeOrigin) -> Scope {
        Scope {
            kind,
            value: value.to_string(),
            origin,
            confidence: 0.5,
        }
    }

    #[test]
    fn declared_scope_wins_over_extracted() {
        let merged = merge_scopes(
            vec![scope(
                ScopeKind::PathPrefix,
                "crates/api",
                ScopeOrigin::Declared,
            )],
            vec![scope(
                ScopeKind::PathPrefix,
                "crates/api/",
                ScopeOrigin::Extracted,
            )],
        );
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].origin, ScopeOrigin::Declared);
        assert!((merged[0].confidence - 1.0).abs() < 1e-6);
    }

    #[test]
    fn slugs_are_stable_and_bounded() {
        assert_eq!(
            slugify("Use tokio as the runtime", 1),
            "use-tokio-as-the-runtime"
        );
        assert_eq!(slugify("", 3), "adr-3");
        assert_eq!(slugify("!!! ???", 2), "adr-2");
        let long = slugify(&"word ".repeat(40), 1);
        assert!(long.len() <= 80, "slug was {} characters", long.len());
        assert!(!long.ends_with('-'));
    }

    #[test]
    fn numbers_are_read_from_the_reference() {
        assert_eq!(number_from_ref("docs/adr/0042-use-x.md"), Some(42));
        assert_eq!(number_from_ref("0042-use-x.md"), Some(42));
        assert_eq!(number_from_ref("docs/adr/use-x.md"), None);
        assert_eq!(number_from_ref("adr-007.md"), None);
    }

    #[tokio::test]
    async fn ingest_creates_then_reports_unchanged_then_revises() {
        let store = Store::open_in_memory().expect("store");
        store.migrate().await.expect("migrate");
        let workspace = store.ensure_default_workspace().await.expect("workspace");
        let project = Project {
            id: Uuid::now_v7(),
            workspace_id: workspace,
            slug: "demo".to_string(),
            name: "Demo".to_string(),
            repo_url: None,
            default_template: "madr".to_string(),
            embed_model: crate::EMBED_MODEL.to_string(),
            embed_version: crate::EMBED_VERSION,
            created_at: 0,
        };
        store
            .create_project(project.clone())
            .await
            .expect("project");

        let source = b"---\nstatus: accepted\n---\n\n# Use tokio\n\n## Context and Problem Statement\n\nNeed a runtime.\n\n## Considered Options\n\n* tokio\n* async-std\n\n## Decision Outcome\n\nChosen option: \"tokio\", because it is standard.\n";
        let counter = WhitespaceTokenCounter;

        let make = |bytes: Vec<u8>| IngestRequest {
            project_id: project.id,
            external_ref: "docs/adr/0001-use-tokio.md".to_string(),
            source: bytes,
            source_format: "markdown".to_string(),
            actor: "test".to_string(),
            declared_scopes: vec![],
            status_override: None,
            embed_model: crate::EMBED_MODEL.to_string(),
            embed_version: crate::EMBED_VERSION,
            number: None,
        };

        let created = ingest(&store, make(source.to_vec()), &counter)
            .await
            .expect("create");
        assert_eq!(created.action, IngestAction::Created);
        assert_eq!(created.rev, 1);
        assert_eq!(created.status, Status::Accepted);
        assert_eq!(created.chunks, 5, "document, context, 2 options, outcome");

        let unchanged = ingest(&store, make(source.to_vec()), &counter)
            .await
            .expect("unchanged");
        assert_eq!(unchanged.action, IngestAction::Unchanged);
        assert_eq!(unchanged.rev, 1);

        let revised_source = String::from_utf8_lossy(source).replace("tokio", "async-std");
        let revised = ingest(&store, make(revised_source.into_bytes()), &counter)
            .await
            .expect("revise");
        assert_eq!(revised.action, IngestAction::Revised);
        assert_eq!(revised.rev, 2);

        // The previous revision is closed but still readable.
        let first = store.get_revision(project.id, 1).await;
        assert!(first.is_err(), "revision 1 belongs to a different adr id");

        let adr = store
            .get_adr_by_external_ref(project.id, "docs/adr/0001-use-tokio.md".to_string())
            .await
            .expect("lookup")
            .expect("found");
        assert_eq!(adr.cur_rev, 2);
        let old = store.get_revision(adr.id, 1).await.expect("old revision");
        assert!(
            old.tx_to.is_some(),
            "the superseded revision must be closed"
        );

        // The new revision's chunks get jobs; the old revision's chunks must not be
        // re-enqueued, or every edit would re-embed the whole history.
        let rev1_chunks = store.list_chunks(adr.id, 1).await.expect("rev 1 chunks");
        let rev2_chunks = store.list_chunks(adr.id, 2).await.expect("rev 2 chunks");
        assert!(!rev2_chunks.is_empty());

        let pending = store.claim_embed_jobs(1000).await.expect("claim");
        let claimed: std::collections::BTreeSet<uuid::Uuid> =
            pending.iter().map(|job| job.chunk_id).collect();
        for chunk in &rev2_chunks {
            assert!(
                claimed.contains(&chunk.id),
                "revision 2 chunk {} has no embed job",
                chunk.id
            );
        }
        for chunk in &rev1_chunks {
            assert!(
                !claimed.contains(&chunk.id),
                "revision 1 chunk {} must not be re-enqueued",
                chunk.id
            );
        }
        assert_eq!(
            claimed.len(),
            rev2_chunks.len(),
            "exactly the new revision's chunks should be queued"
        );
    }

    #[tokio::test]
    async fn an_unchanged_ingest_enqueues_nothing() {
        let store = Store::open_in_memory().expect("store");
        store.migrate().await.expect("migrate");
        let workspace = store.ensure_default_workspace().await.expect("workspace");
        let project = Project {
            id: Uuid::now_v7(),
            workspace_id: workspace,
            slug: "demo".to_string(),
            name: "Demo".to_string(),
            repo_url: None,
            default_template: "madr".to_string(),
            embed_model: crate::EMBED_MODEL.to_string(),
            embed_version: crate::EMBED_VERSION,
            created_at: 0,
        };
        store
            .create_project(project.clone())
            .await
            .expect("project");

        let source = b"# Use tokio\n\n## Context and Problem Statement\n\nx\n".to_vec();
        let make = || IngestRequest {
            project_id: project.id,
            external_ref: "a.md".to_string(),
            source: source.clone(),
            source_format: "markdown".to_string(),
            actor: "test".to_string(),
            declared_scopes: vec![],
            status_override: None,
            embed_model: crate::EMBED_MODEL.to_string(),
            embed_version: crate::EMBED_VERSION,
            number: None,
        };
        let counter = WhitespaceTokenCounter;

        let created = ingest(&store, make(), &counter).await.expect("create");
        // Drain the queue for revision 1.
        let first = store.claim_embed_jobs(1000).await.expect("claim");
        assert_eq!(first.len(), created.chunks);
        for job in &first {
            store.finish_embed_job(job.id).await.expect("finish");
        }

        let again = ingest(&store, make(), &counter).await.expect("again");
        assert_eq!(again.action, IngestAction::Unchanged);
        assert!(
            store
                .claim_embed_jobs(1000)
                .await
                .expect("claim")
                .is_empty(),
            "an unchanged ingest must not enqueue any work"
        );
    }

    #[tokio::test]
    async fn a_rename_is_recognised_rather_than_duplicated() {
        let store = Store::open_in_memory().expect("store");
        store.migrate().await.expect("migrate");
        let workspace = store.ensure_default_workspace().await.expect("workspace");
        let project = Project {
            id: Uuid::now_v7(),
            workspace_id: workspace,
            slug: "demo".to_string(),
            name: "Demo".to_string(),
            repo_url: None,
            default_template: "madr".to_string(),
            embed_model: crate::EMBED_MODEL.to_string(),
            embed_version: crate::EMBED_VERSION,
            created_at: 0,
        };
        store
            .create_project(project.clone())
            .await
            .expect("project");

        let source = b"# Use tokio\n\n## Context and Problem Statement\n\nx\n".to_vec();
        let counter = WhitespaceTokenCounter;
        let make = |external_ref: &str| IngestRequest {
            project_id: project.id,
            external_ref: external_ref.to_string(),
            source: source.clone(),
            source_format: "markdown".to_string(),
            actor: "test".to_string(),
            declared_scopes: vec![],
            status_override: None,
            embed_model: crate::EMBED_MODEL.to_string(),
            embed_version: crate::EMBED_VERSION,
            number: None,
        };

        let created = ingest(&store, make("docs/adr/0001-use-tokio.md"), &counter)
            .await
            .expect("create");
        let renamed = ingest(&store, make("docs/adr/0042-use-tokio.md"), &counter)
            .await
            .expect("rename");

        assert_eq!(renamed.action, IngestAction::Unchanged);
        assert_eq!(renamed.adr_id, created.adr_id);
        let adr = store.get_adr(created.adr_id).await.expect("adr");
        assert_eq!(adr.external_ref, "docs/adr/0042-use-tokio.md");
    }

    #[tokio::test]
    async fn a_status_override_beats_the_front_matter() {
        let store = Store::open_in_memory().expect("store");
        store.migrate().await.expect("migrate");
        let workspace = store.ensure_default_workspace().await.expect("workspace");
        let project = Project {
            id: Uuid::now_v7(),
            workspace_id: workspace,
            slug: "demo".to_string(),
            name: "Demo".to_string(),
            repo_url: None,
            default_template: "madr".to_string(),
            embed_model: crate::EMBED_MODEL.to_string(),
            embed_version: crate::EMBED_VERSION,
            created_at: 0,
        };
        store
            .create_project(project.clone())
            .await
            .expect("project");

        let outcome = ingest(
            &store,
            IngestRequest {
                project_id: project.id,
                external_ref: "a.md".to_string(),
                source: b"---\nstatus: accepted\n---\n# T\n".to_vec(),
                source_format: "markdown".to_string(),
                actor: "agent:test".to_string(),
                declared_scopes: vec![],
                status_override: Some(Status::Proposed),
                embed_model: crate::EMBED_MODEL.to_string(),
                embed_version: crate::EMBED_VERSION,
                number: None,
            },
            &WhitespaceTokenCounter,
        )
        .await
        .expect("ingest");

        assert_eq!(outcome.status, Status::Proposed);
    }
}
