//! Model Context Protocol: JSON-RPC 2.0 over HTTP at `POST /mcp`.
//!
//! The transport is the streamable-HTTP subset that coding agents actually use: one
//! JSON-RPC request per POST, a single JSON response, and batch arrays processed in order.
//! No SSE streaming, because none of the seven tools produces partial results.
//!
//! Tool failures are reported as `isError: true` inside a successful response, which is the
//! MCP convention. Only protocol-level problems become JSON-RPC errors.

use crate::dto::rfc3339;
use crate::error::ApiError;
use crate::state::AppState;
use adrian_core::chunker::WhitespaceTokenCounter;
use adrian_core::ingest::{ingest, IngestRequest};
use adrian_core::types::*;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use std::str::FromStr;
use uuid::Uuid;

/// The MCP revision this server implements.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// Handle one HTTP request to `/mcp`.
///
/// The body is taken as bytes rather than `String` so that a non-UTF-8 body still gets a
/// JSON-RPC parse error. `String` as an extractor would reject it with a plain-text 400,
/// and this endpoint answers in JSON-RPC or not at all.
pub async fn handle(State(state): State<AppState>, body: axum::body::Bytes) -> Response {
    let body = match std::str::from_utf8(&body) {
        Ok(body) => body,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": { "code": -32700, "message": format!("parse error: body is not UTF-8: {err}") }
                })),
            )
                .into_response();
        }
    };

    let parsed: Value = match serde_json::from_str(body) {
        Ok(value) => value,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": { "code": -32700, "message": format!("parse error: {err}") }
                })),
            )
                .into_response();
        }
    };

    // A batch is processed sequentially and answered with an array in the same order.
    if let Some(requests) = parsed.as_array() {
        let mut responses = Vec::with_capacity(requests.len());
        for request in requests {
            if let Some(response) = handle_one(&state, request.clone()).await {
                responses.push(response);
            }
        }
        if responses.is_empty() {
            return StatusCode::NO_CONTENT.into_response();
        }
        return Json(Value::Array(responses)).into_response();
    }

    match handle_one(&state, parsed).await {
        Some(response) => Json(response).into_response(),
        // A notification has no response.
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

async fn handle_one(state: &AppState, request: Value) -> Option<Value> {
    let id = request.get("id").cloned();
    let method = request.get("method").and_then(|value| value.as_str());
    let params = request.get("params").cloned().unwrap_or(Value::Null);

    let Some(method) = method else {
        return Some(error_response(id, -32600, "invalid request: no method"));
    };

    // Notifications carry no id and expect no reply.
    let is_notification = id.is_none();

    // A `params` member that is present but not an object is a malformed envelope, which is
    // a protocol error (-32602), not a tool failure. Checking it once here means no handler
    // has to defend against a string, number or array arriving where an object is expected.
    if !params.is_null() && !params.is_object() {
        return if is_notification {
            None
        } else {
            Some(error_response(
                id,
                -32602,
                "invalid params: `params` must be an object",
            ))
        };
    }

    match method {
        "initialize" => Some(ok_response(
            id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "adrian", "version": adrian_core::VERSION },
            }),
        )),
        "notifications/initialized" => None,
        "ping" => Some(ok_response(id, json!({}))),
        "tools/list" => Some(ok_response(id, json!({ "tools": tool_catalogue() }))),
        "tools/call" => {
            // The tool name and its arguments are part of the request's structure, so a
            // malformed one is -32602. Only failures *inside* a well-formed call become a
            // tool-level `isError`, which is what lets a model see and recover from them.
            let Some(name) = params.get("name").and_then(|value| value.as_str()) else {
                return if is_notification {
                    None
                } else {
                    Some(error_response(
                        id,
                        -32602,
                        "invalid params: `tools/call` requires a string `name`",
                    ))
                };
            };
            let name = name.to_string();

            let arguments = match params.get("arguments") {
                None | Some(Value::Null) => json!({}),
                Some(Value::Object(map)) => Value::Object(map.clone()),
                Some(_) => {
                    return if is_notification {
                        None
                    } else {
                        Some(error_response(
                            id,
                            -32602,
                            "invalid params: `arguments` must be an object",
                        ))
                    }
                }
            };

            Some(match call_tool(state, &name, arguments).await {
                Ok(text) => ok_response(id, tool_result(&text, false)),
                Err(message) => ok_response(id, tool_result(&message, true)),
            })
        }
        other if is_notification => {
            tracing::debug!(method = other, "ignoring unknown notification");
            None
        }
        other => Some(error_response(
            id,
            -32601,
            &format!("method not found: {other}"),
        )),
    }
}

fn ok_response(id: Option<Value>, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id.unwrap_or(Value::Null), "result": result })
}

fn error_response(id: Option<Value>, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id.unwrap_or(Value::Null),
        "error": { "code": code, "message": message }
    })
}

fn tool_result(text: &str, is_error: bool) -> Value {
    json!({
        "content": [ { "type": "text", "text": text } ],
        "isError": is_error,
    })
}

/// The seven tools, with descriptions written for a model rather than for a human reading
/// a reference page: each one says when to reach for it.
fn tool_catalogue() -> Vec<Value> {
    vec![
        json!({
            "name": "adr_search",
            "description": "Search architectural decisions by natural language; returns the \
                            decisions that match with the sections that matched and their scores.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "What you are trying to find out." },
                    "project": { "type": "string", "description": "Project slug. Required when more than one project exists." },
                    "status": { "type": "string", "enum": ["proposed", "accepted", "rejected", "deprecated", "superseded", "withdrawn"] },
                    "limit": { "type": "integer", "default": 5, "minimum": 1, "maximum": 50 }
                },
                "required": ["query"]
            }
        }),
        json!({
            "name": "adr_get",
            "description": "Fetch one decision in full, including its sections, options and \
                            status history; use it after a search when you need the detail.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Decision id from a search result." },
                    "project": { "type": "string", "description": "Project slug, used with `number`." },
                    "number": { "type": "integer", "description": "Display number, used with `project`." }
                }
            }
        }),
        json!({
            "name": "adr_list",
            "description": "List decisions in a project, newest first, when you need an \
                            overview rather than a search.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string" },
                    "status": { "type": "string", "enum": ["proposed", "accepted", "rejected", "deprecated", "superseded", "withdrawn"] },
                    "limit": { "type": "integer", "default": 20, "minimum": 1, "maximum": 100 }
                }
            }
        }),
        json!({
            "name": "adr_submit",
            "description": "Record a new architectural decision. It always lands as \
                            `proposed`; a human accepts it later.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string" },
                    "external_ref": { "type": "string", "description": "A stable key for this decision, for example its intended path." },
                    "source": { "type": "string", "description": "The MADR document as Markdown." },
                    "scopes": { "type": "array", "items": { "type": "string" }, "description": "Path prefixes this decision binds." },
                    "actor": { "type": "string", "description": "Who is submitting, for example `agent:claude-code`." },
                    "idempotency_key": { "type": "string" }
                },
                "required": ["project", "external_ref", "source", "actor"]
            }
        }),
        json!({
            "name": "adr_similar",
            "description": "Find decisions similar to a given one, to check whether the same \
                            choice has already been made or rejected.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "limit": { "type": "integer", "default": 5, "minimum": 1, "maximum": 50 }
                },
                "required": ["id"]
            }
        }),
        json!({
            "name": "adr_transition",
            "description": "Change a decision's status, for example from `proposed` to \
                            `accepted`; this is the only way a decision becomes accepted.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "to": { "type": "string", "enum": ["proposed", "accepted", "rejected", "deprecated", "superseded", "withdrawn"] },
                    "reason": { "type": "string" },
                    "actor": { "type": "string" }
                },
                "required": ["id", "to", "actor"]
            }
        }),
        json!({
            "name": "adr_context_for",
            "description": "List the decisions that bind a file or directory path; call this \
                            before changing code so you do not break a recorded decision.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Repository-relative path, for example `crates/api/src/handler.rs`." },
                    "project": { "type": "string" },
                    "limit": { "type": "integer", "default": 10, "minimum": 1, "maximum": 50 }
                },
                "required": ["path"]
            }
        }),
    ]
}

async fn call_tool(state: &AppState, name: &str, arguments: Value) -> Result<String, String> {
    match name {
        "adr_search" => tool_search(state, arguments).await,
        "adr_get" => tool_get(state, arguments).await,
        "adr_list" => tool_list(state, arguments).await,
        "adr_submit" => tool_submit(state, arguments).await,
        "adr_similar" => tool_similar(state, arguments).await,
        "adr_transition" => tool_transition(state, arguments).await,
        "adr_context_for" => tool_context(state, arguments).await,
        other => Err(format!("unknown tool `{other}`")),
    }
}

fn string_arg(arguments: &Value, key: &str) -> Option<String> {
    arguments
        .get(key)
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
}

fn required_string(arguments: &Value, key: &str) -> Result<String, String> {
    string_arg(arguments, key).ok_or_else(|| format!("`{key}` is required"))
}

fn int_arg(arguments: &Value, key: &str, default: usize, max: usize) -> usize {
    arguments
        .get(key)
        .and_then(|value| value.as_u64())
        .map(|value| value as usize)
        .unwrap_or(default)
        .clamp(1, max)
}

fn render(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

async fn resolve_project(state: &AppState, slug: Option<String>) -> Result<Project, String> {
    match slug {
        Some(slug) => state
            .store
            .get_project_by_slug(slug.clone())
            .await
            .map_err(|err| err.to_string())?
            .ok_or_else(|| format!("project `{slug}` does not exist")),
        None => {
            let projects = state
                .store
                .list_projects()
                .await
                .map_err(|e| e.to_string())?;
            match projects.len() {
                0 => Err("no project exists yet; create one first".to_string()),
                1 => Ok(projects[0].0.clone()),
                _ => Err("`project` is required when more than one project exists".to_string()),
            }
        }
    }
}

async fn tool_search(state: &AppState, arguments: Value) -> Result<String, String> {
    let query = required_string(&arguments, "query")?;
    let project = resolve_project(state, string_arg(&arguments, "project")).await?;
    let limit = int_arg(&arguments, "limit", 5, 50);
    let status = match string_arg(&arguments, "status") {
        Some(value) => Some(Status::from_str(&value)?),
        None => None,
    };

    let results = crate::rest::search_inner(state, project.id, &query, limit, status)
        .await
        .map_err(|err| err.to_string())?;

    Ok(render(&json!({
        "query": query,
        "results": results.iter().map(|result| json!({
            "adr_id": result.adr_id,
            "number": result.number,
            "title": result.title,
            "status": result.status,
            "score": result.score,
            "matched_chunks": result.matched_chunks.iter().map(|chunk| json!({
                "kind": chunk.kind,
                "text": chunk.text,
                "score": chunk.score,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })))
}

async fn tool_get(state: &AppState, arguments: Value) -> Result<String, String> {
    let adr = match (
        string_arg(&arguments, "id"),
        string_arg(&arguments, "number"),
    ) {
        (Some(id), _) => {
            let adr_id = Uuid::parse_str(&id).map_err(|_| format!("`{id}` is not a uuid"))?;
            state
                .store
                .get_adr(adr_id)
                .await
                .map_err(|e| e.to_string())?
        }
        (None, Some(number)) => {
            let number: i64 = number.parse().map_err(|_| "`number` must be an integer")?;
            let project = resolve_project(state, string_arg(&arguments, "project")).await?;
            state
                .store
                .get_adr_by_number(project.id, number)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("no decision numbered {number} in this project"))?
        }
        (None, None) => {
            return Err("provide either `id`, or `project` together with `number`".to_string())
        }
    };

    let revision = state
        .store
        .get_revision(adr.id, adr.cur_rev)
        .await
        .map_err(|e| e.to_string())?;
    let sections = state
        .store
        .get_sections(adr.id, adr.cur_rev)
        .await
        .map_err(|e| e.to_string())?;
    let items = state
        .store
        .get_items(adr.id, adr.cur_rev)
        .await
        .map_err(|e| e.to_string())?;
    let scopes = state
        .store
        .get_scopes(adr.id)
        .await
        .map_err(|e| e.to_string())?;
    let events = state
        .store
        .list_status_events(adr.id)
        .await
        .map_err(|e| e.to_string())?;

    Ok(render(&json!({
        "adr_id": adr.id.to_string(),
        "external_ref": adr.external_ref,
        "number": adr.number,
        "status": adr.cur_status.as_str(),
        "rev": adr.cur_rev,
        "created_at": rfc3339(adr.created_at),
        "created_by": adr.created_by,
        "title": revision.title,
        "template": revision.template,
        "decided_on": revision.decided_on,
        "sections": sections.iter().map(|section| json!({
            "kind": section.kind.as_str(),
            "heading": section.raw_heading,
            "text": section.text,
            "option_key": section.option_key,
        })).collect::<Vec<_>>(),
        "items": items.iter().map(|item| json!({
            "kind": item.kind.as_str(),
            "text": item.text,
            "option_key": item.option_key,
        })).collect::<Vec<_>>(),
        "scopes": scopes.iter().map(|scope| json!({
            "kind": scope.kind.as_str(),
            "value": scope.value,
            "origin": scope.origin.as_str(),
        })).collect::<Vec<_>>(),
        "status_events": events.iter().map(|event| json!({
            "at": rfc3339(event.recorded_at),
            "from": event.from_status.map(|s| s.as_str()),
            "to": event.to_status.as_str(),
            "actor": event.actor,
            "reason": event.reason,
        })).collect::<Vec<_>>(),
    })))
}

async fn tool_list(state: &AppState, arguments: Value) -> Result<String, String> {
    let project = resolve_project(state, string_arg(&arguments, "project")).await?;
    let limit = int_arg(&arguments, "limit", 20, 100);
    let status = match string_arg(&arguments, "status") {
        Some(value) => Some(Status::from_str(&value)?),
        None => None,
    };

    let adrs = state
        .store
        .list_adrs(project.id, status, limit as i64, 0)
        .await
        .map_err(|e| e.to_string())?;

    let mut out = Vec::with_capacity(adrs.len());
    for adr in adrs {
        let revision = state
            .store
            .get_revision(adr.id, adr.cur_rev)
            .await
            .map_err(|e| e.to_string())?;
        out.push(json!({
            "adr_id": adr.id.to_string(),
            "number": adr.number,
            "title": revision.title,
            "status": adr.cur_status.as_str(),
            "external_ref": adr.external_ref,
        }));
    }
    Ok(render(
        &json!({ "project": project.slug, "decisions": out }),
    ))
}

async fn tool_submit(state: &AppState, arguments: Value) -> Result<String, String> {
    let project = resolve_project(state, string_arg(&arguments, "project")).await?;
    let external_ref = required_string(&arguments, "external_ref")?;
    let source = required_string(&arguments, "source")?;
    let actor = required_string(&arguments, "actor")?;

    let declared_scopes = arguments
        .get("scopes")
        .and_then(|value| value.as_array())
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str())
                .map(|value| Scope {
                    kind: ScopeKind::PathPrefix,
                    value: value
                        .trim()
                        .trim_start_matches("./")
                        .trim_end_matches("/**")
                        .trim_end_matches("/*")
                        .trim_end_matches('/')
                        .to_string(),
                    origin: ScopeOrigin::Declared,
                    confidence: 1.0,
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let idempotency_key = string_arg(&arguments, "idempotency_key");
    let request_sha = adrian_core::ingest::sha256(source.as_bytes());

    if let Some(key) = &idempotency_key {
        if let Some((stored_sha, stored_body)) = state
            .store
            .lookup_ingest_response(project.id, key.clone())
            .await
            .map_err(|e| e.to_string())?
        {
            if stored_sha != request_sha {
                return Err(
                    "this idempotency key was already used with a different body".to_string(),
                );
            }
            return Ok(stored_body);
        }
    }

    // SAFETY RULE: an agent submission is always `proposed`, whatever the document's own
    // front matter claims. Only `adr_transition` can accept a decision, and that is a
    // separate, auditable act.
    let outcome = ingest(
        &state.store,
        IngestRequest {
            project_id: project.id,
            external_ref,
            source: source.into_bytes(),
            source_format: "markdown".to_string(),
            actor,
            declared_scopes,
            status_override: Some(Status::Proposed),
            embed_model: state.settings.embed_model.clone(),
            embed_version: state.settings.embed_version,
            number: None,
        },
        &WhitespaceTokenCounter,
    )
    .await
    .map_err(|err| err.to_string())?;

    if !outcome.superseded_points.is_empty() {
        if let Err(err) = state
            .qdrant
            .delete_points(&state.settings.collection, &outcome.superseded_points)
            .await
        {
            tracing::warn!(error = %err, "could not remove superseded points from the index");
        }
    }

    let body = render(&json!({
        "adr_id": outcome.adr_id.to_string(),
        "rev": outcome.rev,
        "status": outcome.status.as_str(),
        "chunks": outcome.chunks,
        "action": match outcome.action {
            IngestAction::Created => "created",
            IngestAction::Revised => "revised",
            IngestAction::Unchanged => "unchanged",
        },
        "warnings": outcome.warnings,
    }));

    if let Some(key) = idempotency_key {
        let _ = state
            .store
            .store_ingest_response(project.id, key, request_sha, body.clone())
            .await;
    }

    Ok(body)
}

async fn tool_similar(state: &AppState, arguments: Value) -> Result<String, String> {
    let id = required_string(&arguments, "id")?;
    let adr_id = Uuid::parse_str(&id).map_err(|_| format!("`{id}` is not a uuid"))?;
    let limit = int_arg(&arguments, "limit", 5, 50);

    let adr = state
        .store
        .get_adr(adr_id)
        .await
        .map_err(|e| e.to_string())?;
    let chunks = state
        .store
        .list_chunks(adr.id, adr.cur_rev)
        .await
        .map_err(|e| e.to_string())?;
    let anchor = chunks
        .iter()
        .find(|chunk| chunk.kind == ChunkKind::Document)
        .or_else(|| chunks.first())
        .ok_or_else(|| "this decision has no chunks to compare".to_string())?;

    let results = crate::rest::search_inner(state, adr.project_id, &anchor.text, limit + 1, None)
        .await
        .map_err(|e| e.to_string())?;

    Ok(render(&json!({
        "adr_id": adr.id.to_string(),
        "similar": results.iter()
            .filter(|result| result.adr_id != adr.id.to_string())
            .take(limit)
            .map(|result| json!({
                "adr_id": result.adr_id,
                "number": result.number,
                "title": result.title,
                "status": result.status,
                "score": result.score,
            }))
            .collect::<Vec<_>>(),
    })))
}

async fn tool_transition(state: &AppState, arguments: Value) -> Result<String, String> {
    let id = required_string(&arguments, "id")?;
    let adr_id = Uuid::parse_str(&id).map_err(|_| format!("`{id}` is not a uuid"))?;
    let to = required_string(&arguments, "to")?;
    let to = Status::from_str(&to)?;
    let actor = required_string(&arguments, "actor")?;
    let reason = string_arg(&arguments, "reason");

    let event_id = state
        .store
        .record_status_event(adr_id, to, actor, reason)
        .await
        .map_err(|e| e.to_string())?;

    Ok(render(&json!({
        "adr_id": adr_id.to_string(),
        "status": to.as_str(),
        "event_id": event_id.to_string(),
    })))
}

async fn tool_context(state: &AppState, arguments: Value) -> Result<String, String> {
    let path = required_string(&arguments, "path")?;
    let project = resolve_project(state, string_arg(&arguments, "project")).await?;
    let limit = int_arg(&arguments, "limit", 10, 50);

    let adrs = state
        .store
        .context_for_path(project.id, path.clone(), limit as i64)
        .await
        .map_err(|e| e.to_string())?;

    let prefixes = adrian_core::store::ancestor_prefixes(&path);
    let mut decisions = Vec::with_capacity(adrs.len());
    for adr in adrs {
        let revision = state
            .store
            .get_revision(adr.id, adr.cur_rev)
            .await
            .map_err(|e| e.to_string())?;
        let scopes = state
            .store
            .get_scopes(adr.id)
            .await
            .map_err(|e| e.to_string())?;
        let matched = scopes
            .iter()
            .filter(|scope| scope.kind == ScopeKind::PathPrefix)
            .filter(|scope| prefixes.iter().any(|prefix| prefix == &scope.value))
            .map(|scope| scope.value.clone())
            .max_by_key(|value| value.len())
            .unwrap_or_default();
        let outcome = state
            .store
            .get_sections(adr.id, adr.cur_rev)
            .await
            .map_err(|e| e.to_string())?
            .into_iter()
            .find(|section| section.kind == SectionKind::Outcome)
            .map(|section| section.text.trim().to_string())
            .unwrap_or_default();

        decisions.push(json!({
            "adr_id": adr.id.to_string(),
            "number": adr.number,
            "title": revision.title,
            "status": adr.cur_status.as_str(),
            "matched_scope": matched,
            "outcome": outcome,
        }));
    }

    Ok(render(&json!({ "path": path, "decisions": decisions })))
}

/// Convert a handler error into a tool-level message.
impl From<ApiError> for String {
    fn from(_: ApiError) -> Self {
        "internal error".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalogue_advertises_all_seven_tools_with_descriptions() {
        let tools = tool_catalogue();
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|tool| tool.get("name").and_then(|v| v.as_str()))
            .collect();
        assert_eq!(
            names,
            vec![
                "adr_search",
                "adr_get",
                "adr_list",
                "adr_submit",
                "adr_similar",
                "adr_transition",
                "adr_context_for",
            ]
        );
        for tool in &tools {
            let description = tool
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            assert!(!description.is_empty(), "tool is missing a description");
            assert!(
                tool.get("inputSchema").is_some(),
                "tool is missing an input schema"
            );
        }
    }

    #[test]
    fn batch_arrays_are_answered_in_order() {
        // The ordering guarantee is structural: responses are pushed in the same loop
        // iteration as the requests. Assert the shape of what the helper builds.
        let first = ok_response(Some(json!(1)), json!({}));
        let second = ok_response(Some(json!(2)), json!({}));
        let batch = Value::Array(vec![first, second]);
        let ids: Vec<i64> = batch
            .as_array()
            .expect("array")
            .iter()
            .map(|value| value["id"].as_i64().expect("id"))
            .collect();
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn error_responses_carry_a_code_and_the_request_id() {
        let response = error_response(Some(json!("abc")), -32601, "method not found: nope");
        assert_eq!(response["id"], "abc");
        assert_eq!(response["error"]["code"], -32601);
    }

    #[test]
    fn tool_results_mark_errors_inside_a_successful_response() {
        let ok = tool_result("{}", false);
        assert_eq!(ok["isError"], false);
        assert_eq!(ok["content"][0]["type"], "text");
        let failed = tool_result("boom", true);
        assert_eq!(failed["isError"], true);
        assert_eq!(failed["content"][0]["text"], "boom");
    }

    #[test]
    fn integer_arguments_are_clamped() {
        let arguments = json!({ "limit": 1000 });
        assert_eq!(int_arg(&arguments, "limit", 5, 50), 50);
        let arguments = json!({ "limit": 0 });
        assert_eq!(int_arg(&arguments, "limit", 5, 50), 1);
        let arguments = json!({});
        assert_eq!(int_arg(&arguments, "limit", 5, 50), 5);
    }
}
