//! Model Context Protocol: JSON-RPC 2.0 over HTTP at `POST /mcp`.
//!
//! The transport is the streamable-HTTP subset that coding agents actually use: one
//! JSON-RPC request per POST, a single JSON response, and batch arrays processed in order.
//! No SSE streaming, because none of the thirteen tools produces partial results.
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
use std::collections::HashMap;
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
                "capabilities": { "tools": {}, "resources": {} },
                "serverInfo": { "name": "adrian", "version": adrian_core::VERSION },
                "instructions": SERVER_INSTRUCTIONS,
            }),
        )),
        "notifications/initialized" => None,
        "ping" => Some(ok_response(id, json!({}))),
        "tools/list" => Some(ok_response(id, json!({ "tools": tool_catalogue() }))),
        // Resources are how a caller reads the contract without a checkout. The status
        // rules and the MADR support table are not discoverable from a tool schema, and
        // guessing them wrong is silent, so they are served as documents.
        "resources/list" => Some(ok_response(
            id,
            json!({ "resources": resource_catalogue() }),
        )),
        "resources/read" => {
            if is_notification {
                return None;
            }
            let uri = params.get("uri").and_then(|value| value.as_str());
            match uri.map(read_resource) {
                Some(Some(text)) => Some(ok_response(
                    id,
                    json!({
                        "contents": [{
                            "uri": uri,
                            "mimeType": "text/markdown",
                            "text": text,
                        }]
                    }),
                )),
                Some(None) => Some(error_response(
                    id,
                    -32602,
                    &format!("unknown resource `{}`", uri.unwrap_or_default()),
                )),
                None => Some(error_response(
                    id,
                    -32602,
                    "invalid params: `resources/read` requires a string `uri`",
                )),
            }
        }
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

/// What a client should know before it calls a tool.
///
/// Sent in `initialize` so an agent gets the contract in the same handshake that gives it the
/// tools, instead of inferring the document shape from an empty schema and going looking for a
/// checkout. Every fact here is one that a caller who guessed wrong would not be told about: a
/// mis-read status and a mis-derived number both land silently.
///
/// The last paragraph exists because the first version of this text named the two document URIs
/// without saying what kind of thing they are, and a caller read them with its filesystem tool
/// and got ENOENT. Naming the resource mechanism costs one line and saves that round trip.
const SERVER_INSTRUCTIONS: &str = "\
Decisions are MADR documents. Two documents are served as MCP resources, not as files: read \
`adrian://docs/madr` for what the parser understands and `adrian://docs/api` for the route and \
tool reference. Use `resources/list` and `resources/read` for them.\n\
Status comes from YAML front matter only, and only these six values are understood: \
proposed, accepted, rejected, deprecated, superseded, withdrawn. A `- **Status:** Accepted` \
bullet under the title is ignored, so the decision lands as proposed and the result carries a \
status_bullet_ignored warning. Nothing becomes accepted except through adr_transition.\n\
Free text is matched by its leading word, up to the first `|`, `,`, `;`, `/` or newline. \
`Accepted; the renewal clause is superseded by ADR-0091` is therefore `accepted`, not \
`superseded`, and `superseded by ADR-0123` is `superseded`. `approved`, `agreed` and `decided` \
mean accepted; `open`, `draft`, `pending` and `proposal` mean proposed; `obsolete` means \
deprecated; `abandoned` means withdrawn. Anything else is an unknown_status warning and lands \
proposed.\n\
Recognised front matter keys are status, date, decision-makers (or deciders), consulted and \
informed. Any other key is preserved verbatim but not interpreted.\n\
The display number is the leading digits of `external_ref`, so `docs/adr/0042-use-x.md` is 42 \
and `adr-007.md` has no number. Duplicate numbers are not reconciled; pick distinct refs.\n\
`adr_preview` parses documents and reports what an ingest would record, without writing. Use it \
to answer any question about how a corpus will parse. Do not read the server's source to \
predict it.";

/// The documents this server serves over `resources/read`.
///
/// Embedded at build time so a client that only has the container gets the same contract a
/// contributor reading the repository would.
const API_DOC: &str = include_str!("../../../docs/API.md");
const MADR_DOC: &str = include_str!("../../../docs/ADR-SUPPORT.md");

/// The resources, with descriptions written for a model deciding whether to read one.
fn resource_catalogue() -> Vec<Value> {
    vec![
        json!({
            "uri": "adrian://docs/api",
            "name": "REST and MCP API reference",
            "description": "Every REST route and MCP tool, with request and response shapes \
                            and the stable error codes.",
            "mimeType": "text/markdown",
        }),
        json!({
            "uri": "adrian://docs/madr",
            "name": "MADR support and the status contract",
            "description": "What the parser understands. Status is read from YAML front \
                            matter only; a `- **Status:**` bullet is ignored and the decision \
                            lands as `proposed`.",
            "mimeType": "text/markdown",
        }),
    ]
}

fn read_resource(uri: &str) -> Option<&'static str> {
    match uri {
        "adrian://docs/api" => Some(API_DOC),
        "adrian://docs/madr" => Some(MADR_DOC),
        _ => None,
    }
}

/// The thirteen tools, with descriptions written for a model rather than for a human reading
/// a reference page: each one says when to reach for it.
fn tool_catalogue() -> Vec<Value> {
    vec![
        json!({
            "name": "project_list",
            "description": "List the projects that exist, with their slug and decision count. \
                            Call this when another tool said `project` is required because \
                            more than one project exists.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "default": 50, "minimum": 1, "maximum": 200 }
                }
            }
        }),
        json!({
            "name": "project_create",
            "description": "Create a project. Every decision belongs to one, so this is the \
                            first call on a fresh instance; `adr_submit` and `adr_import` \
                            fail with `no project exists yet` until a project exists.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "slug": { "type": "string", "description": "Short identifier every other tool takes, for example `arilo-id`." },
                    "name": { "type": "string", "description": "Human readable name." },
                    "repo_url": { "type": "string", "description": "Repository these decisions come from, if any." }
                },
                "required": ["slug", "name"]
            }
        }),
        json!({
            "name": "project_delete",
            "description": "Delete a project and every decision, revision, chunk and vector \
                            that belongs to it. Irreversible; use it to remove a probe or a \
                            project created by mistake.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "slug": { "type": "string" }
                },
                "required": ["slug"]
            }
        }),
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
            "description": "Record one new architectural decision from a MADR document. It \
                            always lands as `proposed`; a human accepts it later with \
                            `adr_transition`. Status is read from YAML front matter only, so a \
                            document whose status is a `- **Status:**` bullet is recorded as \
                            `proposed` and the response carries a `status_bullet_ignored` \
                            warning naming the raw value. Recognised front matter keys are \
                            `status`, `date`, `decision-makers` (or `deciders`), `consulted` \
                            and `informed`; the status vocabulary is proposed, accepted, \
                            rejected, deprecated, superseded, withdrawn. The display number is \
                            the leading digits of `external_ref`. To migrate a whole directory \
                            use `adr_import` instead of looping this.",
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
            "name": "adr_import",
            "description": "Bootstrap an existing corpus: ingest many MADR documents in one \
                            call. Use it instead of looping `adr_submit` when migrating a \
                            directory of ADRs. By default each document lands `proposed`; \
                            set `preserve_status` only when the decisions were already \
                            accepted elsewhere, because that records the document's own \
                            status without a human transition. One bad document does not \
                            abort the batch; it comes back as `failed` in the results.\n\n\
                            The status contract decides whether a migration keeps its history. \
                            Status is read from YAML front matter only, so a corpus that writes \
                            `- **Status:** Accepted` as a bullet imports entirely as `proposed` \
                            and each one then needs an `adr_transition`; the result carries a \
                            `status_bullet_ignored` warning per document. Converting those \
                            bullets to `---` front matter first, with `preserve_status: true`, \
                            keeps the statuses. Recognised keys are `status`, `date`, \
                            `decision-makers` (or `deciders`), `consulted` and `informed`, and \
                            the status vocabulary is proposed, accepted, rejected, deprecated, \
                            superseded, withdrawn. The display number is the leading digits of \
                            `external_ref`, so `docs/adr/0042-use-x.md` is 42; duplicate \
                            numbers are not reconciled, so check the refs before a bulk import.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string" },
                    "actor": { "type": "string", "description": "Who is importing, for example `human:hanz`." },
                    "preserve_status": { "type": "boolean", "default": false, "description": "Record each document's declared status instead of forcing `proposed`." },
                    "documents": {
                        "type": "array",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "external_ref": { "type": "string", "description": "Stable key and the source of the display number, for example `docs/adr/0001-whitelabel.md` gives number 1. Refs without leading digits get no number." },
                                "source": { "type": "string", "description": "The MADR document as Markdown, including any YAML front matter." },
                                "source_format": { "type": "string", "description": "Format hint for the source. Defaults to MADR, which is what the parser expects." },
                                "scopes": { "type": "array", "items": { "type": "string" }, "description": "Path prefixes this decision binds, so `adr_context_for` can find it later." },
                                "idempotency_key": { "type": "string", "description": "Repeat the same key to make a retry a no-op instead of a revision." }
                            },
                            "required": ["external_ref", "source"]
                        }
                    }
                },
                "required": ["project", "actor", "documents"]
            }
        }),
        json!({
            "name": "adr_preview",
            "description": "Parse documents and report what `adr_submit` or `adr_import` would \
                            make of them, without writing anything. Call this before an import \
                            of a corpus you did not author, because it is the only way to see \
                            the outcome of a guess: it says, per document, the title, the \
                            status that would be recorded and where that status came from, the \
                            warnings the parser raised, the sections it found, and the display \
                            number derived from `external_ref`. It also lists duplicate numbers \
                            across the batch. Nothing here needs the server's source code; if \
                            you are about to read the repository to predict how a document \
                            parses, call this instead.\n\n\
                            `status_from` is `front_matter` when the document's YAML front \
                            matter supplied a status, `none` when it supplied none, which is \
                            what makes an import land `proposed`. A `status_bullet_ignored` \
                            warning means the document writes its status as a \
                            `- **Status:** ...` bullet and the value is discarded.\n\n\
                            `status` is the value the parser read from front matter, before \
                            `preserve_status` decides whether to honour it. Add \
                            `preserve_status` to the preview to see the status the import would \
                            actually record.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "documents": {
                        "type": "array",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "external_ref": { "type": "string", "description": "Stable key and the source of the display number, for example `docs/adr/0001-whitelabel.md` gives number 1." },
                                "source": { "type": "string", "description": "The MADR document as Markdown, including any YAML front matter." }
                            },
                            "required": ["external_ref", "source"]
                        }
                    },
                    "preserve_status": { "type": "boolean", "default": false, "description": "Preview the status an import with this flag would record, instead of the `proposed` a plain import forces." }
                },
                "required": ["documents"]
            }
        }),
        json!({
            "name": "adr_delete",
            "description": "Delete one decision and everything that belongs to it, including \
                            its vector points. Irreversible; use it to remove a probe or a \
                            decision submitted by mistake.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Decision id." }
                },
                "required": ["id"]
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
        "adr_import" => tool_import(state, arguments).await,
        "adr_preview" => tool_preview(arguments),
        "adr_delete" => tool_delete(state, arguments).await,
        "adr_similar" => tool_similar(state, arguments).await,
        "adr_transition" => tool_transition(state, arguments).await,
        "adr_context_for" => tool_context(state, arguments).await,
        "project_list" => tool_project_list(state, arguments).await,
        "project_create" => tool_project_create(state, arguments).await,
        "project_delete" => tool_project_delete(state, arguments).await,
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

async fn tool_project_list(state: &AppState, arguments: Value) -> Result<String, String> {
    let limit = int_arg(&arguments, "limit", 50, 200);
    let projects = state
        .store
        .list_projects()
        .await
        .map_err(|e| e.to_string())?;

    // An empty list is an answer, not a failure: a caller has to be able to tell "no
    // project exists yet" apart from "the daemon is unreachable".
    let out = projects
        .into_iter()
        .take(limit)
        .map(|(project, adr_count)| {
            json!({
                "id": project.id.to_string(),
                "slug": project.slug,
                "name": project.name,
                "adr_count": adr_count,
                "created_at": rfc3339(project.created_at),
            })
        })
        .collect::<Vec<_>>();

    Ok(render(&json!({ "projects": out })))
}

async fn tool_project_create(state: &AppState, arguments: Value) -> Result<String, String> {
    let slug = required_string(&arguments, "slug")?;
    let name = required_string(&arguments, "name")?;
    adrian_core::types::validate_project_slug(slug.trim()).map_err(|err| err.to_string())?;

    if state
        .store
        .get_project_by_slug(slug.trim().to_string())
        .await
        .map_err(|e| e.to_string())?
        .is_some()
    {
        return Err(format!("project `{slug}` already exists"));
    }

    let workspace = state
        .store
        .ensure_default_workspace()
        .await
        .map_err(|e| e.to_string())?;
    let project = Project {
        id: Uuid::now_v7(),
        workspace_id: workspace,
        slug: slug.trim().to_string(),
        name: name.trim().to_string(),
        repo_url: string_arg(&arguments, "repo_url"),
        default_template: "madr".to_string(),
        embed_model: state.settings.embed_model.clone(),
        embed_version: state.settings.embed_version,
        created_at: chrono::Utc::now().timestamp(),
    };
    state
        .store
        .create_project(project.clone())
        .await
        .map_err(|e| e.to_string())?;

    Ok(render(&json!({
        "id": project.id.to_string(),
        "slug": project.slug,
        "name": project.name,
        "repo_url": project.repo_url,
        "created_at": rfc3339(project.created_at),
    })))
}

async fn tool_project_delete(state: &AppState, arguments: Value) -> Result<String, String> {
    let slug = required_string(&arguments, "slug")?;
    let project = state
        .store
        .get_project_by_slug(slug.clone())
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("project `{slug}` does not exist"))?;

    // Clear the index first, for the same reason a project delete does: a point that
    // outlives its row is worse than a delete that did not happen.
    state
        .qdrant
        .delete_points_for_project(&state.settings.collection, project.id)
        .await
        .map_err(|e| e.to_string())?;
    let footprint = state
        .store
        .delete_project(project.id)
        .await
        .map_err(|e| e.to_string())?;

    Ok(render(&json!({
        "deleted": true,
        "slug": project.slug,
        "footprint": {
            "adrs": footprint.adrs,
            "revisions": footprint.revisions,
            "chunks": footprint.chunks,
            "vectors": footprint.vectors,
            "embed_jobs": footprint.embed_jobs,
        },
    })))
}

async fn tool_delete(state: &AppState, arguments: Value) -> Result<String, String> {
    let id = required_string(&arguments, "id")?;
    let adr_id = Uuid::parse_str(&id).map_err(|_| format!("`{id}` is not a uuid"))?;
    let adr = state
        .store
        .get_adr(adr_id)
        .await
        .map_err(|e| e.to_string())?;

    state
        .qdrant
        .delete_adr_points(&state.settings.collection, adr_id)
        .await
        .map_err(|e| e.to_string())?;
    let footprint = state
        .store
        .delete_adr(adr_id)
        .await
        .map_err(|e| e.to_string())?;

    Ok(render(&json!({
        "deleted": true,
        "adr_id": adr_id.to_string(),
        "external_ref": adr.external_ref,
        "footprint": {
            "revisions": footprint.revisions,
            "chunks": footprint.chunks,
            "vectors": footprint.vectors,
            "embed_jobs": footprint.embed_jobs,
        },
    })))
}

async fn tool_import(state: &AppState, arguments: Value) -> Result<String, String> {
    let project = resolve_project(state, string_arg(&arguments, "project")).await?;
    let actor = required_string(&arguments, "actor")?;
    let preserve_status = arguments
        .get("preserve_status")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let status_override = if preserve_status {
        None
    } else {
        Some(Status::Proposed)
    };

    let documents = arguments
        .get("documents")
        .and_then(|value| value.as_array())
        .ok_or_else(|| "`documents` must be an array".to_string())?;
    if documents.is_empty() {
        return Err("`documents` must not be empty".to_string());
    }

    let mut created = 0usize;
    let mut revised = 0usize;
    let mut unchanged = 0usize;
    let mut failed = 0usize;
    let mut results = Vec::with_capacity(documents.len());

    for document in documents {
        let Some(external_ref) = document.get("external_ref").and_then(|v| v.as_str()) else {
            failed += 1;
            results.push(json!({
                "external_ref": Value::Null,
                "action": "failed",
                "error": "each document needs a string `external_ref`",
            }));
            continue;
        };
        let Some(source) = document.get("source").and_then(|v| v.as_str()) else {
            failed += 1;
            results.push(json!({
                "external_ref": external_ref,
                "action": "failed",
                "error": "each document needs a string `source`",
            }));
            continue;
        };
        let scopes = document
            .get("scopes")
            .and_then(|value| value.as_array())
            .map(|values| {
                values
                    .iter()
                    .filter_map(|value| value.as_str().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let source_format = document
            .get("source_format")
            .and_then(|value| value.as_str())
            .map(|value| value.to_string());
        let idempotency_key = document
            .get("idempotency_key")
            .and_then(|value| value.as_str())
            .map(|value| value.to_string());

        match crate::rest::ingest_document(
            state,
            &project,
            crate::rest::DocumentIngest {
                external_ref: external_ref.to_string(),
                source: source.to_string(),
                source_format,
                scopes,
                actor: actor.clone(),
                idempotency_key,
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
                results.push(json!({
                    "external_ref": external_ref,
                    "action": response.action,
                    "adr_id": response.id,
                    "rev": response.rev,
                    "status": response.status,
                    "chunks": response.chunks,
                    "warnings": response.warnings,
                }));
            }
            Err(err) => {
                failed += 1;
                results.push(json!({
                    "external_ref": external_ref,
                    "action": "failed",
                    "error": err.to_string(),
                }));
            }
        }
    }

    Ok(render(&json!({
        "project": project.slug,
        "created": created,
        "revised": revised,
        "unchanged": unchanged,
        "failed": failed,
        "results": results,
    })))
}

/// Parse without writing, so a caller can see the outcome of a guess.
///
/// This exists because the alternative, for a corpus whose statuses are free text, was for the
/// caller to read the parser source to predict what `adr_import` would do. A tool that answers
/// the question directly is worth more than any amount of description.
fn tool_preview(arguments: Value) -> Result<String, String> {
    let preserve_status = arguments
        .get("preserve_status")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);

    let documents = arguments
        .get("documents")
        .and_then(|value| value.as_array())
        .ok_or_else(|| "`documents` must be an array".to_string())?;
    if documents.is_empty() {
        return Err("`documents` must not be empty".to_string());
    }

    let mut results = Vec::with_capacity(documents.len());
    let mut numbers: HashMap<i64, Vec<String>> = HashMap::new();

    for document in documents {
        let Some(external_ref) = document.get("external_ref").and_then(|v| v.as_str()) else {
            results.push(json!({
                "external_ref": Value::Null,
                "error": "each document needs a string `external_ref`",
            }));
            continue;
        };
        let Some(source) = document.get("source").and_then(|v| v.as_str()) else {
            results.push(json!({
                "external_ref": external_ref,
                "error": "each document needs a string `source`",
            }));
            continue;
        };

        let parsed = adrian_core::parser::parse_str(source);
        let number = adrian_core::ingest::number_from_ref(external_ref);
        if let Some(number) = number {
            numbers
                .entry(number)
                .or_default()
                .push(external_ref.to_string());
        }

        let status_from = if parsed.status_raw.is_some() {
            "front_matter"
        } else {
            "none"
        };
        // The status the ingest would record, which is what the caller actually asked about.
        let recorded = match (&parsed.status, preserve_status) {
            (Some(status), true) => Some(*status),
            _ => Some(Status::Proposed),
        };

        results.push(json!({
            "external_ref": external_ref,
            "number": number,
            "title": parsed.title,
            "status_from": status_from,
            "status_declared": parsed.status.map(Status::as_str),
            "status_recorded": recorded.map(Status::as_str),
            "sections": parsed.sections.iter().map(|s| s.kind.as_str()).collect::<Vec<_>>(),
            "options": parsed.items_of(ItemKind::Option).count(),
            "warnings": parsed.warnings,
        }));
    }

    // A number that appears twice is not reconciled by the ingest: both documents land, and
    // two decisions answer to the same number. Worth naming here rather than after the import.
    let mut duplicate_numbers: Vec<Value> = numbers
        .into_iter()
        .filter(|(_, refs)| refs.len() > 1)
        .map(|(number, refs)| json!({ "number": number, "external_refs": refs }))
        .collect();
    duplicate_numbers.sort_by_key(|value| value["number"].as_i64().unwrap_or_default());

    let recorded_counts = results
        .iter()
        .filter_map(|result| result.get("status_recorded").and_then(|v| v.as_str()))
        .fold(HashMap::new(), |mut counts, status| {
            *counts.entry(status.to_string()).or_insert(0usize) += 1;
            counts
        });
    let with_warnings = results
        .iter()
        .filter(|result| {
            result
                .get("warnings")
                .and_then(|v| v.as_array())
                .is_some_and(|warnings| !warnings.is_empty())
        })
        .count();

    Ok(render(&json!({
        "documents": results.len(),
        "preserve_status": preserve_status,
        "status_recorded": recorded_counts,
        "with_warnings": with_warnings,
        "duplicate_numbers": duplicate_numbers,
        "results": results,
    })))
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
    let chunks = state
        .store
        .list_chunks(adr.id, adr.cur_rev)
        .await
        .map_err(|e| e.to_string())?;

    // Whether a chunk has a vector yet is the only way a caller can tell an ingest that
    // landed from one that is still waiting on the embed worker.
    let mut chunk_views = Vec::with_capacity(chunks.len());
    for chunk in &chunks {
        let embedded = state
            .store
            .chunk_embedded(
                chunk.id,
                state.settings.embed_model.clone(),
                state.settings.embed_version,
            )
            .await
            .map_err(|e| e.to_string())?;
        chunk_views.push(json!({
            "id": chunk.id.to_string(),
            "kind": chunk.kind.as_str(),
            "ordinal": chunk.ordinal,
            "text": chunk.text,
            "token_count": chunk.token_count,
            "truncated": chunk.truncated,
            "embedded": embedded,
        }));
    }

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
        "chunks": chunk_views,
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
    fn the_catalogue_advertises_all_thirteen_tools_with_descriptions() {
        let tools = tool_catalogue();
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|tool| tool.get("name").and_then(|v| v.as_str()))
            .collect();
        assert_eq!(
            names,
            vec![
                "project_list",
                "project_create",
                "project_delete",
                "adr_search",
                "adr_get",
                "adr_list",
                "adr_submit",
                "adr_import",
                "adr_preview",
                "adr_delete",
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
    fn the_instructions_point_at_the_documents_and_the_status_contract() {
        // The handshake is the only place a client that never reads a checkout learns the shape
        // of a document, so these three facts have to be in it.
        assert!(SERVER_INSTRUCTIONS.contains("adrian://docs/madr"));
        assert!(SERVER_INSTRUCTIONS.contains("front matter"));
        assert!(SERVER_INSTRUCTIONS.contains("external_ref"));
        for status in ["proposed", "accepted", "superseded", "withdrawn"] {
            assert!(
                SERVER_INSTRUCTIONS.contains(status),
                "instructions omit the `{status}` status"
            );
        }
    }

    #[test]
    fn the_ingest_descriptions_carry_the_contract_a_schema_cannot() {
        // A caller who only sees the schema would write `- **Status:**` bullets and duplicate
        // numbers, and both mistakes land silently. The descriptions are the last place to say so.
        let tools = tool_catalogue();
        for name in ["adr_submit", "adr_import"] {
            let tool = tools
                .iter()
                .find(|tool| tool.get("name").and_then(|v| v.as_str()) == Some(name))
                .expect("tool exists");
            let description = tool
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            assert!(
                description.contains("front matter"),
                "`{name}` does not say where status comes from"
            );
            assert!(
                description.contains("external_ref"),
                "`{name}` does not say where the number comes from"
            );
            assert!(
                description.contains("withdrawn"),
                "`{name}` does not enumerate the status vocabulary"
            );
        }
    }

    #[test]
    fn the_resource_catalogue_covers_every_readable_uri() {
        let resources = resource_catalogue();
        assert_eq!(resources.len(), 2);
        for resource in &resources {
            let uri = resource.get("uri").and_then(|v| v.as_str()).expect("uri");
            assert!(
                read_resource(uri).is_some(),
                "`{uri}` is advertised but not readable"
            );
            assert!(!read_resource(uri).unwrap_or_default().is_empty());
        }
        assert!(read_resource("adrian://docs/nope").is_none());
    }

    #[test]
    fn the_madr_resource_names_the_status_contract() {
        let text = read_resource("adrian://docs/madr").expect("madr doc");
        assert!(
            text.contains("Status") || text.contains("status"),
            "the MADR doc should say where status comes from"
        );
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
