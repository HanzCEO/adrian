//! REST handlers.
//!
//! Every failure goes through [`ApiError`], so a client can branch on a stable `code`
//! without parsing prose.

use crate::dto::*;
use crate::error::{ApiError, ApiResult};
use crate::json::{ApiJson, ApiPath, ApiQuery};
use crate::state::AppState;
use adrian_core::chunker::WhitespaceTokenCounter;
use adrian_core::export::{self, ExportFormat};
use adrian_core::ingest::{ingest, IngestRequest};
use adrian_core::types::*;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde_json::json;
use std::collections::BTreeMap;
use std::str::FromStr;
use uuid::Uuid;

/// Reciprocal rank fusion constant.
///
/// The two retrieval arms produce scores that are not comparable: MaxSim is a bounded
/// cosine and BM25 is unbounded. Fusing ranks rather than scores is the only defensible
/// way to combine them.
const RRF_K: f32 = 60.0;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/models", get(models))
        .route("/v1/projects", get(list_projects).post(create_project))
        .route("/v1/projects/{*slug}", delete(delete_project))
        .route("/v1/adrs", get(list_adrs).post(create_adr))
        .route("/v1/adrs/batch", post(batch_import))
        .route("/v1/adrs/{id}", get(get_adr).delete(delete_adr))
        .route("/v1/adrs/{id}/chunks", get(get_adr_chunks))
        .route("/v1/adrs/{id}/transition", post(transition))
        .route("/v1/adrs/{id}/similar", get(similar))
        .route("/v1/search", post(search))
        .route("/v1/context", get(context))
        .route("/v1/export", post(export_decisions))
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

#[tracing::instrument(name = "healthz", skip(state))]
pub async fn healthz(State(state): State<AppState>) -> Json<HealthResponse> {
    let embedder = match state.embedder.models().await {
        Ok(info) => DependencyHealth {
            reachable: true,
            model: Some(info.model),
            version: Some(info.version),
            collection: None,
            error: None,
        },
        Err(err) => DependencyHealth {
            reachable: false,
            model: None,
            version: None,
            collection: None,
            error: Some(err.to_string()),
        },
    };

    let qdrant = if state.qdrant.healthy().await {
        DependencyHealth {
            reachable: true,
            model: None,
            version: None,
            collection: Some(state.settings.collection.clone()),
            error: None,
        }
    } else {
        DependencyHealth {
            reachable: false,
            model: None,
            version: None,
            collection: Some(state.settings.collection.clone()),
            error: Some("qdrant did not answer its readiness probe".to_string()),
        }
    };

    Json(HealthResponse {
        status: "ok",
        version: adrian_core::VERSION.to_string(),
        embedder,
        qdrant,
    })
}

#[tracing::instrument(name = "models", skip(state))]
pub async fn models(State(state): State<AppState>) -> ApiResult<Json<ModelsResponse>> {
    let model = state.embedder.models().await?;
    Ok(Json(ModelsResponse {
        model,
        collection: state.settings.collection.clone(),
    }))
}

// ---------------------------------------------------------------------------
// Projects
// ---------------------------------------------------------------------------

#[tracing::instrument(name = "create_project", skip(state, request))]
pub async fn create_project(
    State(state): State<AppState>,
    ApiJson(request): ApiJson<CreateProjectRequest>,
) -> ApiResult<(StatusCode, Json<ProjectResponse>)> {
    let slug = request.slug.trim();
    adrian_core::types::validate_project_slug(slug).map_err(ApiError::invalid)?;
    if state
        .store
        .get_project_by_slug(slug.to_string())
        .await?
        .is_some()
    {
        return Err(ApiError::conflict(format!(
            "project `{slug}` already exists"
        )));
    }

    let workspace = state.store.ensure_default_workspace().await?;
    let project = Project {
        id: Uuid::now_v7(),
        workspace_id: workspace,
        slug: slug.to_string(),
        name: request.name.trim().to_string(),
        repo_url: request.repo_url,
        default_template: "madr".to_string(),
        embed_model: state.settings.embed_model.clone(),
        embed_version: state.settings.embed_version,
        created_at: chrono::Utc::now().timestamp(),
    };
    state.store.create_project(project.clone()).await?;
    Ok((StatusCode::CREATED, Json(project_view(&project))))
}

#[tracing::instrument(name = "list_projects", skip(state))]
pub async fn list_projects(State(state): State<AppState>) -> ApiResult<Json<Vec<ProjectListItem>>> {
    let projects = state.store.list_projects().await?;
    Ok(Json(
        projects
            .into_iter()
            .map(|(project, count)| ProjectListItem {
                id: project.id.to_string(),
                slug: project.slug,
                name: project.name,
                adr_count: count,
            })
            .collect(),
    ))
}

#[tracing::instrument(name = "delete_project", skip(state))]
pub async fn delete_project(
    State(state): State<AppState>,
    ApiPath(slug): ApiPath<String>,
) -> ApiResult<Json<DeleteProjectResponse>> {
    let project = state
        .store
        .get_project_by_slug(slug.clone())
        .await?
        .ok_or_else(|| ApiError::not_found(format!("project `{slug}` does not exist")))?;

    // Clear the index first. A point that outlives its row makes search return a decision
    // that no longer exists, which is worse than a delete that did not happen.
    state
        .qdrant
        .delete_points_for_project(&state.settings.collection, project.id)
        .await?;
    let footprint = state.store.delete_project(project.id).await?;

    Ok(Json(DeleteProjectResponse {
        deleted: true,
        slug: project.slug,
        footprint: FootprintView {
            adrs: Some(footprint.adrs),
            revisions: footprint.revisions,
            chunks: footprint.chunks,
            vectors: footprint.vectors,
            embed_jobs: footprint.embed_jobs,
        },
    }))
}

// ---------------------------------------------------------------------------
// ADRs
// ---------------------------------------------------------------------------

#[tracing::instrument(name = "create_adr", skip(state, request))]
pub async fn create_adr(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    ApiJson(request): ApiJson<CreateAdrRequest>,
) -> ApiResult<(StatusCode, Json<IngestResponse>)> {
    let project = state
        .store
        .get_project_by_slug(request.project.clone())
        .await?
        .ok_or_else(|| {
            ApiError::not_found(format!("project `{}` does not exist", request.project))
        })?;

    let idempotency_key = request.idempotency_key.clone().or_else(|| {
        headers
            .get("idempotency-key")
            .and_then(|value| value.to_str().ok())
            .map(|value| value.to_string())
    });

    let actor = request
        .actor
        .clone()
        .unwrap_or_else(|| "human:anonymous".to_string());

    // The REST endpoint does not force a status: it is the operator surface, and an import
    // that loses its statuses is not an import. `adr_submit` over MCP is the one that
    // forces `proposed`.
    let response = ingest_document(
        &state,
        &project,
        DocumentIngest {
            external_ref: request.external_ref,
            source: request.source,
            source_format: request.source_format,
            scopes: request.scopes,
            actor,
            idempotency_key,
            status_override: None,
        },
    )
    .await?;

    Ok((StatusCode::CREATED, Json(response)))
}

/// One document to ingest, independent of which surface asked for it.
///
/// The fields are bundled rather than passed positionally because the batch, single and
/// MCP paths all build the same thing and a nine-argument call invites a swap.
pub(crate) struct DocumentIngest {
    pub external_ref: String,
    pub source: String,
    pub source_format: Option<String>,
    pub scopes: Vec<String>,
    pub actor: String,
    pub idempotency_key: Option<String>,
    pub status_override: Option<Status>,
}

/// Ingest one document and render the response.
///
/// Shared by `POST /v1/adrs`, `POST /v1/adrs/batch` and the MCP `adr_import` tool so that
/// idempotency replay, scope normalisation and index cleanup cannot drift between them.
pub(crate) async fn ingest_document(
    state: &AppState,
    project: &Project,
    document: DocumentIngest,
) -> ApiResult<IngestResponse> {
    let DocumentIngest {
        external_ref,
        source,
        source_format,
        scopes,
        actor,
        idempotency_key,
        status_override,
    } = document;

    if source.trim().is_empty() {
        return Err(ApiError::invalid("`source` must not be empty"));
    }
    if external_ref.trim().is_empty() {
        return Err(ApiError::invalid("`external_ref` must not be empty"));
    }

    let request_sha = adrian_core::ingest::sha256(source.as_bytes());

    // Replay a stored response when the same key arrives with the same body, and refuse
    // when it arrives with a different one. Agents retry; this is what makes that safe.
    if let Some(key) = &idempotency_key {
        if let Some((stored_sha, stored_body)) = state
            .store
            .lookup_ingest_response(project.id, key.clone())
            .await?
        {
            if stored_sha != request_sha {
                return Err(ApiError::conflict(
                    "this idempotency key was already used with a different body",
                ));
            }
            let response: IngestResponse =
                serde_json::from_str(stored_body.as_str()).map_err(|err| {
                    ApiError::internal(format!("stored response is unreadable: {err}"))
                })?;
            return Ok(response);
        }
    }

    let declared_scopes = scopes
        .iter()
        .filter_map(|value| normalize_declared_scope(value))
        .collect::<Vec<Scope>>();

    let outcome = ingest(
        &state.store,
        IngestRequest {
            project_id: project.id,
            external_ref,
            source: source.into_bytes(),
            source_format: source_format.unwrap_or_else(|| "markdown".to_string()),
            actor,
            declared_scopes,
            status_override,
            embed_model: state.settings.embed_model.clone(),
            embed_version: state.settings.embed_version,
            number: None,
        },
        &WhitespaceTokenCounter,
    )
    .await?;

    // A revision replaced older ones, so their points are no longer in SQLite. Remove them
    // from the index too; a failure here is worth reporting rather than swallowing.
    if !outcome.superseded_points.is_empty() {
        state
            .qdrant
            .delete_points(&state.settings.collection, &outcome.superseded_points)
            .await?;
    }

    let response = IngestResponse {
        id: outcome.adr_id.to_string(),
        rev: outcome.rev,
        status: outcome.status.as_str().to_string(),
        chunks: outcome.chunks,
        action: match outcome.action {
            IngestAction::Created => "created",
            IngestAction::Revised => "revised",
            IngestAction::Unchanged => "unchanged",
        }
        .to_string(),
        warnings: outcome.warnings,
    };

    if let Some(key) = idempotency_key {
        let body =
            serde_json::to_string(&response).map_err(|err| ApiError::internal(err.to_string()))?;
        state
            .store
            .store_ingest_response(project.id, key, request_sha, body)
            .await?;
    }

    Ok(response)
}

#[tracing::instrument(name = "batch_import", skip(state, request))]
pub async fn batch_import(
    State(state): State<AppState>,
    ApiJson(request): ApiJson<BatchImportRequest>,
) -> ApiResult<(StatusCode, Json<BatchImportResponse>)> {
    if request.documents.is_empty() {
        return Err(ApiError::invalid("`documents` must not be empty"));
    }

    let project = state
        .store
        .get_project_by_slug(request.project.clone())
        .await?
        .ok_or_else(|| {
            ApiError::not_found(format!("project `{}` does not exist", request.project))
        })?;

    let actor = request
        .actor
        .clone()
        .unwrap_or_else(|| "human:anonymous".to_string());

    // Off by default: a submission must not accept its own decision. `preserve_status` is
    // the operator migration path, where the decisions were accepted before Adrian saw
    // them and the document's own front matter is the authority.
    let status_override = if request.preserve_status {
        None
    } else {
        Some(Status::Proposed)
    };

    let mut created = 0usize;
    let mut revised = 0usize;
    let mut unchanged = 0usize;
    let mut failed = 0usize;
    let mut results = Vec::with_capacity(request.documents.len());

    for document in request.documents {
        let external_ref = document.external_ref.clone();
        match ingest_document(
            &state,
            &project,
            DocumentIngest {
                external_ref: document.external_ref,
                source: document.source,
                source_format: document.source_format,
                scopes: document.scopes,
                actor: actor.clone(),
                idempotency_key: document.idempotency_key,
                status_override,
            },
        )
        .await
        {
            Ok(response) => {
                match response.action.as_str() {
                    "created" => created += 1,
                    "revised" => revised += 1,
                    _ => unchanged += 1,
                }
                results.push(BatchItemResult {
                    external_ref,
                    action: response.action,
                    id: Some(response.id),
                    rev: Some(response.rev),
                    status: Some(response.status),
                    warnings: response.warnings,
                    error: None,
                });
            }
            Err(err) => {
                failed += 1;
                results.push(BatchItemResult {
                    external_ref,
                    action: "failed".to_string(),
                    id: None,
                    rev: None,
                    status: None,
                    warnings: Vec::new(),
                    error: Some(err.to_string()),
                });
            }
        }
    }

    Ok((
        StatusCode::CREATED,
        Json(BatchImportResponse {
            project: project.slug,
            created,
            revised,
            unchanged,
            failed,
            results,
        }),
    ))
}

#[tracing::instrument(name = "delete_adr", skip(state))]
pub async fn delete_adr(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
) -> ApiResult<Json<DeleteAdrResponse>> {
    let adr_id = parse_uuid(&id)?;
    let adr = state.store.get_adr(adr_id).await?;

    state
        .qdrant
        .delete_adr_points(&state.settings.collection, adr_id)
        .await?;
    let footprint = state.store.delete_adr(adr_id).await?;

    Ok(Json(DeleteAdrResponse {
        deleted: true,
        id: adr_id.to_string(),
        external_ref: adr.external_ref,
        footprint: FootprintView {
            adrs: None,
            revisions: footprint.revisions,
            chunks: footprint.chunks,
            vectors: footprint.vectors,
            embed_jobs: footprint.embed_jobs,
        },
    }))
}

fn normalize_declared_scope(value: &str) -> Option<Scope> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(Scope {
        kind: ScopeKind::PathPrefix,
        value: trimmed
            .trim_start_matches("./")
            .trim_start_matches('/')
            .trim_end_matches("/**")
            .trim_end_matches("/*")
            .trim_end_matches('/')
            .to_string(),
        origin: ScopeOrigin::Declared,
        confidence: 1.0,
    })
}

#[tracing::instrument(name = "list_adrs", skip(state, query))]
pub async fn list_adrs(
    State(state): State<AppState>,
    ApiQuery(query): ApiQuery<ListQuery>,
) -> ApiResult<Json<Vec<AdrSummary>>> {
    let project = match &query.project {
        Some(slug) => state
            .store
            .get_project_by_slug(slug.clone())
            .await?
            .ok_or_else(|| ApiError::not_found(format!("project `{slug}` does not exist")))?,
        None => {
            let projects = state.store.list_projects().await?;
            match projects.len() {
                0 => return Ok(Json(Vec::new())),
                1 => projects[0].0.clone(),
                _ => {
                    return Err(ApiError::invalid(
                        "`project` is required when more than one project exists",
                    ))
                }
            }
        }
    };

    let status = match &query.status {
        Some(value) => Some(Status::from_str(value).map_err(ApiError::invalid)?),
        None => None,
    };
    let limit = query.limit.unwrap_or(50).min(500) as i64;
    let offset = query.offset.unwrap_or(0) as i64;

    let adrs = state
        .store
        .list_adrs(project.id, status, limit, offset)
        .await?;
    let mut out = Vec::with_capacity(adrs.len());
    for adr in adrs {
        let revision = state.store.get_revision(adr.id, adr.cur_rev).await?;
        out.push(AdrSummary {
            id: adr.id.to_string(),
            number: adr.number,
            title: revision.title,
            status: adr.cur_status.as_str().to_string(),
            external_ref: adr.external_ref,
            rev: adr.cur_rev,
            created_at: rfc3339(adr.created_at),
        });
    }
    Ok(Json(out))
}

#[tracing::instrument(name = "get_adr", skip(state))]
pub async fn get_adr(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
    ApiQuery(query): ApiQuery<DetailQuery>,
) -> ApiResult<Json<AdrDetail>> {
    let adr_id = parse_uuid(&id)?;
    let adr = state.store.get_adr(adr_id).await?;
    let rev = query.rev.unwrap_or(adr.cur_rev);
    let revision = state.store.get_revision(adr_id, rev).await?;
    let sections = state.store.get_sections(adr_id, rev).await?;
    let items = state.store.get_items(adr_id, rev).await?;
    let scopes = state.store.get_scopes(adr_id).await?;
    let technologies = state.store.get_technologies(adr_id).await?;
    let chunks = state.store.list_chunks(adr_id, rev).await?;
    let events = state.store.list_status_events(adr_id).await?;

    let mut chunk_views = Vec::with_capacity(chunks.len());
    for chunk in &chunks {
        let embedded = state
            .store
            .chunk_embedded(
                chunk.id,
                state.settings.embed_model.clone(),
                state.settings.embed_version,
            )
            .await?;
        chunk_views.push(ChunkView {
            id: chunk.id.to_string(),
            kind: chunk.kind.as_str().to_string(),
            ordinal: chunk.ordinal,
            text: chunk.text.clone(),
            token_count: chunk.token_count,
            truncated: chunk.truncated,
            embedded,
        });
    }

    Ok(Json(AdrDetail {
        id: adr.id.to_string(),
        project_id: adr.project_id.to_string(),
        external_ref: adr.external_ref,
        number: adr.number,
        slug: adr.slug,
        status: adr.cur_status.as_str().to_string(),
        rev,
        created_at: rfc3339(adr.created_at),
        created_by: adr.created_by,
        title: revision.title,
        template: revision.template,
        source_format: revision.source_format,
        decided_on: revision.decided_on,
        sections: sections
            .iter()
            .map(|section| SectionView {
                kind: section.kind.as_str().to_string(),
                raw_heading: section.raw_heading.clone(),
                ordinal: section.ordinal,
                parent: section.parent,
                option_key: section.option_key.clone(),
                text: section.text.clone(),
                byte_start: section.byte_start,
                byte_end: section.byte_end,
            })
            .collect(),
        items: items
            .iter()
            .map(|item| ItemView {
                kind: item.kind.as_str().to_string(),
                section: item.section,
                ordinal: item.ordinal,
                text: item.text.clone(),
                option_key: item.option_key.clone(),
            })
            .collect(),
        scopes: scopes.iter().map(scope_view).collect(),
        technologies: technologies.iter().map(technology_view).collect(),
        chunks: chunk_views,
        status_events: events
            .iter()
            .map(|event| StatusEventView {
                at: rfc3339(event.recorded_at),
                from: event.from_status.map(|s| s.as_str().to_string()),
                to: event.to_status.as_str().to_string(),
                actor: event.actor.clone(),
                reason: event.reason.clone(),
            })
            .collect(),
    }))
}

#[tracing::instrument(name = "get_adr_chunks", skip(state))]
pub async fn get_adr_chunks(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
) -> ApiResult<Json<Vec<ChunkView>>> {
    let adr_id = parse_uuid(&id)?;
    let adr = state.store.get_adr(adr_id).await?;
    let chunks = state.store.list_chunks(adr_id, adr.cur_rev).await?;
    let mut out = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        let embedded = state
            .store
            .chunk_embedded(
                chunk.id,
                state.settings.embed_model.clone(),
                state.settings.embed_version,
            )
            .await?;
        out.push(ChunkView {
            id: chunk.id.to_string(),
            kind: chunk.kind.as_str().to_string(),
            ordinal: chunk.ordinal,
            text: chunk.text,
            token_count: chunk.token_count,
            truncated: chunk.truncated,
            embedded,
        });
    }
    Ok(Json(out))
}

#[tracing::instrument(name = "transition", skip(state, request))]
pub async fn transition(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
    ApiJson(request): ApiJson<TransitionRequest>,
) -> ApiResult<Json<TransitionResponse>> {
    let adr_id = parse_uuid(&id)?;
    // Parsing the target status against the closed vocabulary is what makes an invalid
    // transition a 400 rather than a row nobody can interpret later.
    let to = Status::from_str(&request.to).map_err(ApiError::invalid)?;
    let actor = request
        .actor
        .unwrap_or_else(|| "human:anonymous".to_string());
    let event_id = state
        .store
        .record_status_event(adr_id, to, actor, request.reason)
        .await?;
    Ok(Json(TransitionResponse {
        status: to.as_str().to_string(),
        event_id: event_id.to_string(),
    }))
}

// ---------------------------------------------------------------------------
// Retrieval
// ---------------------------------------------------------------------------

#[tracing::instrument(name = "search", skip(state, request))]
pub async fn search(
    State(state): State<AppState>,
    ApiJson(request): ApiJson<SearchRequest>,
) -> ApiResult<Json<SearchResponse>> {
    if request.query.trim().is_empty() {
        return Err(ApiError::invalid("`query` must not be empty"));
    }
    let project = resolve_project(&state, request.project.as_deref()).await?;
    let limit = request.limit.unwrap_or(5).clamp(1, 50);
    let status = match &request.status {
        Some(value) => Some(Status::from_str(value).map_err(ApiError::invalid)?),
        None => None,
    };

    let results = search_inner(&state, project.id, &request.query, limit, status).await?;
    Ok(Json(SearchResponse { results }))
}

pub async fn search_inner(
    state: &AppState,
    project_id: Uuid,
    query: &str,
    limit: usize,
    status: Option<Status>,
) -> ApiResult<Vec<SearchResult>> {
    // Arm 1: semantic. The query is embedded in the query role, never the document role:
    // the model caps query token vectors at 64 and the two sides are not interchangeable.
    let semantic = match state
        .embedder
        .embed(EmbedRole::Query, &[query.to_string()], true)
        .await
    {
        Ok(response) => match response.items.into_iter().next() {
            Some(item) => {
                let dense = item
                    .dense
                    .unwrap_or_else(|| vec![0.0; adrian_core::DENSE_DIM]);
                state
                    .qdrant
                    .search(
                        &state.settings.collection,
                        project_id,
                        item.vectors,
                        dense,
                        limit.saturating_mul(4).max(20),
                    )
                    .await?
            }
            None => Vec::new(),
        },
        Err(err) => {
            // A retrieval that silently returns nothing because the embedder is down is
            // worse than one that says so.
            return Err(ApiError::upstream(format!("cannot embed the query: {err}")));
        }
    };

    // Arm 2: lexical.
    let lexical = state
        .store
        .search_lexical(project_id, query.to_string(), (limit * 4).max(20) as i64)
        .await?;

    let fused = reciprocal_rank_fusion(vec![semantic, lexical]);

    // Group by decision, keeping the best chunk per decision.
    let mut per_adr: BTreeMap<Uuid, Vec<(Uuid, f32)>> = BTreeMap::new();
    for (chunk_id, adr_id, score) in fused {
        per_adr.entry(adr_id).or_default().push((chunk_id, score));
    }

    // `(decision, best chunk score, every matched chunk with its score)`
    type RankedDecision = (Uuid, f32, Vec<(Uuid, f32)>);
    let mut ranked: Vec<RankedDecision> = per_adr
        .into_iter()
        .map(|(adr_id, mut chunks)| {
            chunks.sort_by(|a, b| b.1.total_cmp(&a.1));
            let best = chunks.first().map(|(_, score)| *score).unwrap_or(0.0);
            (adr_id, best, chunks)
        })
        .collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));

    let mut out = Vec::new();
    for (adr_id, score, chunks) in ranked {
        if out.len() >= limit {
            break;
        }
        let Ok(adr) = state.store.get_adr(adr_id).await else {
            continue;
        };
        if let Some(wanted) = status {
            if adr.cur_status != wanted {
                continue;
            }
        }
        let Ok(revision) = state.store.get_revision(adr.id, adr.cur_rev).await else {
            continue;
        };

        let mut matched = Vec::new();
        for (chunk_id, chunk_score) in chunks.iter().take(3) {
            if let Ok(Some(chunk)) = state.store.get_chunk(*chunk_id).await {
                matched.push(MatchedChunkView {
                    chunk_id: chunk.id.to_string(),
                    kind: chunk.kind.as_str().to_string(),
                    text: chunk.text,
                    score: *chunk_score,
                });
            }
        }

        out.push(SearchResult {
            adr_id: adr.id.to_string(),
            number: adr.number,
            title: revision.title,
            status: adr.cur_status.as_str().to_string(),
            score,
            matched_chunks: matched,
        });
    }

    Ok(out)
}

/// Fuse ranked lists with reciprocal rank fusion.
///
/// Returns `(chunk_id, adr_id, fused_score)` ordered by fused score, best first.
pub fn reciprocal_rank_fusion(lists: Vec<Vec<ScoredChunk>>) -> Vec<(Uuid, Uuid, f32)> {
    let mut scores: BTreeMap<Uuid, (Uuid, f32)> = BTreeMap::new();
    for list in lists {
        for (rank, hit) in list.into_iter().enumerate() {
            let contribution = 1.0 / (RRF_K + (rank as f32) + 1.0);
            scores
                .entry(hit.chunk_id)
                .and_modify(|entry| entry.1 += contribution)
                .or_insert((hit.adr_id, contribution));
        }
    }
    let mut out: Vec<(Uuid, Uuid, f32)> = scores
        .into_iter()
        .map(|(chunk_id, (adr_id, score))| (chunk_id, adr_id, score))
        .collect();
    out.sort_by(|a, b| b.2.total_cmp(&a.2));
    out
}

#[tracing::instrument(name = "similar", skip(state, query))]
pub async fn similar(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
    ApiQuery(query): ApiQuery<SimilarQuery>,
) -> ApiResult<Json<SearchResponse>> {
    let adr_id = parse_uuid(&id)?;
    let adr = state.store.get_adr(adr_id).await?;
    let limit = query.limit.unwrap_or(5).clamp(1, 50);

    // Use the decision's own document chunk as the query text: it is the one chunk that
    // summarises the whole record.
    let chunks = state.store.list_chunks(adr_id, adr.cur_rev).await?;
    let anchor = chunks
        .iter()
        .find(|chunk| chunk.kind == ChunkKind::Document)
        .or_else(|| chunks.first())
        .ok_or_else(|| ApiError::not_found("this decision has no chunks to compare"))?;

    let results = search_inner(&state, adr.project_id, &anchor.text, limit + 1, None).await?;

    Ok(Json(SearchResponse {
        results: results
            .into_iter()
            .filter(|result| result.adr_id != adr_id.to_string())
            .take(limit)
            .collect(),
    }))
}

#[tracing::instrument(name = "context", skip(state, query))]
pub async fn context(
    State(state): State<AppState>,
    ApiQuery(query): ApiQuery<ContextQuery>,
) -> ApiResult<Json<ContextResponse>> {
    if query.path.trim().is_empty() {
        return Err(ApiError::invalid("`path` must not be empty"));
    }
    let project = resolve_project(&state, query.project.as_deref()).await?;
    let limit = query.limit.unwrap_or(10).clamp(1, 50);

    let adrs = state
        .store
        .context_for_path(project.id, query.path.clone(), limit as i64)
        .await?;

    let prefixes = adrian_core::store::ancestor_prefixes(&query.path);
    let mut decisions = Vec::with_capacity(adrs.len());
    for adr in adrs {
        let Ok(revision) = state.store.get_revision(adr.id, adr.cur_rev).await else {
            continue;
        };
        let scopes = state.store.get_scopes(adr.id).await?;
        let matched_scope = scopes
            .iter()
            .filter(|scope| scope.kind == ScopeKind::PathPrefix)
            .filter(|scope| prefixes.iter().any(|prefix| prefix == &scope.value))
            .max_by_key(|scope| scope.value.len())
            .map(|scope| scope.value.clone())
            .unwrap_or_default();
        let summary = state
            .store
            .get_sections(adr.id, adr.cur_rev)
            .await?
            .into_iter()
            .find(|section| section.kind == SectionKind::Context)
            .map(|section| section.text.trim().to_string())
            .unwrap_or_default();

        decisions.push(ContextDecision {
            adr_id: adr.id.to_string(),
            number: adr.number,
            title: revision.title,
            status: adr.cur_status.as_str().to_string(),
            matched_scope,
            scopes: scopes.iter().map(scope_view).collect(),
            summary,
        });
    }

    Ok(Json(ContextResponse {
        path: query.path,
        decisions,
    }))
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

#[tracing::instrument(name = "export_decisions", skip(state, request))]
pub async fn export_decisions(
    State(state): State<AppState>,
    ApiJson(request): ApiJson<ExportRequest>,
) -> ApiResult<axum::response::Response> {
    let project = state
        .store
        .get_project_by_slug(request.project.clone())
        .await?
        .ok_or_else(|| {
            ApiError::not_found(format!("project `{}` does not exist", request.project))
        })?;

    let format =
        ExportFormat::parse(&request.format).map_err(|err| ApiError::invalid(err.to_string()))?;

    // A single-record export renders just that record, so a caller does not have to slice
    // a concatenated file.
    if let Some(ids) = &request.adr_ids {
        if ids.len() == 1 {
            let adr_id = parse_uuid(&ids[0])?;
            let adr = state.store.get_adr(adr_id).await?;
            let body = match format {
                ExportFormat::Json => {
                    let record = export::export_adr_json(&state.store, &adr).await?;
                    serde_json::to_vec_pretty(&record)
                        .map_err(|err| ApiError::internal(err.to_string()))?
                }
                ExportFormat::Madr => export::export_adr_markdown(&state.store, &adr)
                    .await?
                    .into_bytes(),
            };
            return Ok(render_export(body, format));
        }
    }

    let body = export::export_project(&state.store, project.id, format)
        .await
        .map_err(|err| ApiError::internal(err.to_string()))?;
    Ok(render_export(body, format))
}

fn render_export(body: Vec<u8>, format: ExportFormat) -> axum::response::Response {
    use axum::response::IntoResponse;
    let content_type = match format {
        ExportFormat::Json => "application/json",
        ExportFormat::Madr => "text/markdown; charset=utf-8",
    };
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, content_type)],
        body,
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_uuid(value: &str) -> ApiResult<Uuid> {
    Uuid::parse_str(value).map_err(|_| ApiError::invalid(format!("`{value}` is not a uuid")))
}

async fn resolve_project(state: &AppState, slug: Option<&str>) -> ApiResult<Project> {
    match slug {
        Some(slug) => state
            .store
            .get_project_by_slug(slug.to_string())
            .await?
            .ok_or_else(|| ApiError::not_found(format!("project `{slug}` does not exist"))),
        None => {
            let projects = state.store.list_projects().await?;
            match projects.len() {
                0 => Err(ApiError::not_found("no project exists yet")),
                1 => Ok(projects[0].0.clone()),
                _ => Err(ApiError::invalid(
                    "`project` is required when more than one project exists",
                )),
            }
        }
    }
}

/// A JSON error body, used by the MCP layer for tool-level failures.
pub fn error_body(code: &str, message: &str) -> serde_json::Value {
    json!({ "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(chunk: Uuid, adr: Uuid) -> ScoredChunk {
        ScoredChunk {
            chunk_id: chunk,
            adr_id: adr,
            score: 1.0,
        }
    }

    #[test]
    fn rrf_rewards_items_that_appear_in_both_lists() {
        let shared = Uuid::now_v7();
        let only_semantic = Uuid::now_v7();
        let only_lexical = Uuid::now_v7();
        let adr = Uuid::now_v7();

        let fused = reciprocal_rank_fusion(vec![
            vec![hit(only_semantic, adr), hit(shared, adr)],
            vec![hit(only_lexical, adr), hit(shared, adr)],
        ]);

        assert_eq!(fused[0].0, shared, "the item in both lists must rank first");
        assert_eq!(fused.len(), 3);
        // Every fused score is positive and the list is sorted descending.
        assert!(fused.iter().all(|(_, _, score)| *score > 0.0));
        assert!(fused.windows(2).all(|pair| pair[0].2 >= pair[1].2));
    }

    #[test]
    fn rrf_of_one_list_preserves_its_order() {
        let adr = Uuid::now_v7();
        let first = Uuid::now_v7();
        let second = Uuid::now_v7();
        let fused = reciprocal_rank_fusion(vec![vec![hit(first, adr), hit(second, adr)]]);
        assert_eq!(fused[0].0, first);
        assert_eq!(fused[1].0, second);
    }

    #[test]
    fn rrf_of_nothing_is_empty() {
        assert!(reciprocal_rank_fusion(vec![Vec::new(), Vec::new()]).is_empty());
    }

    #[test]
    fn declared_scopes_are_normalised() {
        let scope = normalize_declared_scope("  ./crates/api/**  ").expect("scope");
        assert_eq!(scope.value, "crates/api");
        assert_eq!(scope.origin, ScopeOrigin::Declared);
        assert!(normalize_declared_scope("   ").is_none());
    }
}
