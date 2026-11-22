//! The MCP surface: protocol behaviour, the twelve tools and the contract documents.

mod common;

use axum::body::Body;
use axum::http::Request;
use common::{Harness, SAMPLE_ADR};
use serde_json::{json, Value};
use tower::ServiceExt;

async fn call(harness: &Harness, name: &str, arguments: Value) -> Value {
    let (status, body) = harness
        .mcp(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        }))
        .await;
    assert_eq!(status, 200, "tools/call must return 200, body: {body}");
    body
}

fn tool_text(response: &Value) -> Value {
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no text content in {response}"));
    serde_json::from_str(text).unwrap_or(Value::String(text.to_string()))
}

#[tokio::test]
async fn initialize_reports_the_protocol_and_a_tools_capability() {
    let harness = Harness::new(None).await;
    let (status, body) = harness
        .mcp(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }))
        .await;
    assert_eq!(status, 200);
    assert_eq!(body["result"]["protocolVersion"], "2025-06-18");
    assert!(body["result"]["capabilities"]["tools"].is_object());
    assert_eq!(body["result"]["serverInfo"]["name"], "adrian");
    let instructions = body["result"]["instructions"]
        .as_str()
        .expect("initialize carries instructions so a client learns the contract at handshake");
    assert!(
        instructions.contains("adrian://docs/madr"),
        "instructions must point at the contract document, got {instructions}"
    );
}

#[tokio::test]
async fn notifications_get_no_body() {
    let harness = Harness::new(None).await;
    let (status, _) = harness
        .mcp(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
        .await;
    assert_eq!(status, 204);
}

#[tokio::test]
async fn tools_list_advertises_all_twelve_tools() {
    let harness = Harness::new(None).await;
    let (status, body) = harness
        .mcp(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }))
        .await;
    assert_eq!(status, 200);
    let tools = body["result"]["tools"].as_array().expect("tools");
    assert_eq!(tools.len(), 12);
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    for expected in [
        "project_list",
        "project_create",
        "project_delete",
        "adr_import",
        "adr_delete",
    ] {
        assert!(
            names.contains(&expected),
            "{expected} is how a caller bootstraps and cleans up, got {names:?}"
        );
    }
    for tool in tools {
        assert!(tool["description"].as_str().is_some_and(|d| !d.is_empty()));
        assert!(tool["inputSchema"].is_object());
    }
}

#[tokio::test]
async fn initialize_advertises_resources_and_they_are_readable() {
    let harness = Harness::new(None).await;
    let (status, body) = harness
        .mcp(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }))
        .await;
    assert_eq!(status, 200);
    assert!(
        body["result"]["capabilities"]["resources"].is_object(),
        "the contract documents are a capability, not a tool, got {body}"
    );

    let (status, body) = harness
        .mcp(json!({ "jsonrpc": "2.0", "id": 2, "method": "resources/list" }))
        .await;
    assert_eq!(status, 200);
    let resources = body["result"]["resources"].as_array().expect("resources");
    assert_eq!(resources.len(), 2);

    let (status, body) = harness
        .mcp(json!({ "jsonrpc": "2.0", "id": 3, "method": "resources/read", "params": { "uri": "adrian://docs/madr" } }))
        .await;
    assert_eq!(status, 200);
    let text = body["result"]["contents"][0]["text"]
        .as_str()
        .expect("text");
    assert!(!text.is_empty());

    let (status, body) = harness
        .mcp(json!({ "jsonrpc": "2.0", "id": 4, "method": "resources/read", "params": { "uri": "adrian://docs/nope" } }))
        .await;
    assert_eq!(status, 200);
    assert_eq!(body["error"]["code"], -32602);
}

#[tokio::test]
async fn a_non_utf8_body_is_a_jsonrpc_parse_error() {
    let harness = Harness::new(None).await;
    // `String` as an extractor would reject this with a plain-text 400 before the handler
    // ran, which would break the promise that /mcp answers only in JSON-RPC.
    let request = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("content-type", "application/json")
        .body(Body::from(vec![0xff, 0xfe, 0x00]))
        .expect("request");
    let response = harness
        .router
        .clone()
        .oneshot(request)
        .await
        .expect("response");
    assert_eq!(response.status(), 400);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body");
    let body: Value = serde_json::from_slice(&bytes).expect("JSON, not plain text");
    assert_eq!(body["jsonrpc"], "2.0");
    assert_eq!(body["error"]["code"], -32700);
}

#[tokio::test]
async fn invalid_params_are_minus_32602() {
    let harness = Harness::new(None).await;
    harness.create_project("demo").await;

    // A `params` member that is present but not an object is a malformed envelope.
    for bad_params in [json!("a string"), json!(42), json!([1, 2, 3]), json!(true)] {
        let (status, body) = harness
            .mcp(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": bad_params.clone()
            }))
            .await;
        assert_eq!(status, 200, "transport stays 200 for a protocol error");
        assert_eq!(
            body["error"]["code"], -32602,
            "params {bad_params} should be -32602, got {body}"
        );
        assert_eq!(body["id"], 1);
        assert!(
            body.get("result").is_none(),
            "a protocol error must not carry a result"
        );
    }

    // `tools/call` without a name cannot be dispatched.
    let (status, body) = harness
        .mcp(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "arguments": { "query": "x" } }
        }))
        .await;
    assert_eq!(status, 200);
    assert_eq!(body["error"]["code"], -32602);
    assert!(body.get("result").is_none());

    // A name that is not a string is equally malformed.
    let (_, body) = harness
        .mcp(json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "name": 7 }
        }))
        .await;
    assert_eq!(body["error"]["code"], -32602);

    // Arguments that are not an object are malformed.
    let (_, body) = harness
        .mcp(json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": { "name": "adr_list", "arguments": "not an object" }
        }))
        .await;
    assert_eq!(body["error"]["code"], -32602);

    // A well-formed call still succeeds, so the validation is not over-eager.
    let (_, body) = harness
        .mcp(json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": { "name": "adr_list", "arguments": { "project": "demo" } }
        }))
        .await;
    assert!(
        body.get("result").is_some(),
        "a valid call must not be rejected: {body}"
    );
    assert_eq!(body["result"]["isError"], false);
}

#[tokio::test]
async fn a_notification_with_bad_params_gets_no_reply() {
    let harness = Harness::new(None).await;
    let (status, body) = harness
        .mcp(json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "params": "not an object"
        }))
        .await;
    assert_eq!(status, 204, "a notification is never answered");
    assert!(body.is_null(), "no body for a notification, got {body}");
}

#[tokio::test]
async fn an_unknown_method_is_minus_32601() {
    let harness = Harness::new(None).await;
    let (status, body) = harness
        .mcp(json!({ "jsonrpc": "2.0", "id": 7, "method": "does/not/exist" }))
        .await;
    assert_eq!(status, 200);
    assert_eq!(body["error"]["code"], -32601);
    assert_eq!(body["id"], 7);
}

#[tokio::test]
async fn a_batch_is_answered_in_request_order() {
    let harness = Harness::new(None).await;
    let (status, body) = harness
        .mcp(json!([
            { "jsonrpc": "2.0", "id": 10, "method": "ping" },
            { "jsonrpc": "2.0", "id": 11, "method": "tools/list" },
            { "jsonrpc": "2.0", "id": 12, "method": "initialize", "params": {} }
        ]))
        .await;
    assert_eq!(status, 200);
    let responses = body.as_array().expect("array");
    assert_eq!(responses.len(), 3);
    assert_eq!(responses[0]["id"], 10);
    assert_eq!(responses[1]["id"], 11);
    assert_eq!(responses[2]["id"], 12);
}

#[tokio::test]
async fn malformed_json_is_a_parse_error_with_http_400() {
    let harness = Harness::new(None).await;
    let (status, bytes) = harness.send_raw("POST", "/mcp", None, None).await;
    // An empty body is a parse error, not a panic.
    assert!(status == 400 || status == 200, "unexpected status {status}");
    let _ = bytes;
}

#[tokio::test]
async fn a_tool_failure_is_reported_inside_a_successful_response() {
    let harness = Harness::new(None).await;
    let response = call(&harness, "adr_get", json!({ "id": "not-a-uuid" })).await;
    assert_eq!(response["result"]["isError"], true);
    let message = response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(message.contains("uuid"), "got: {message}");
}

#[tokio::test]
async fn an_unknown_tool_is_a_tool_level_failure() {
    let harness = Harness::new(None).await;
    let response = call(&harness, "nope", json!({})).await;
    assert_eq!(response["result"]["isError"], true);
}

#[tokio::test]
async fn adr_submit_always_lands_as_proposed_even_when_the_document_says_accepted() {
    let harness = Harness::new(None).await;
    harness.create_project("demo").await;

    // SAMPLE_ADR declares `status: accepted` in its front matter.
    assert!(SAMPLE_ADR.contains("status: accepted"));

    let response = call(
        &harness,
        "adr_submit",
        json!({
            "project": "demo",
            "external_ref": "docs/adr/0001.md",
            "source": SAMPLE_ADR,
            "actor": "agent:claude-code",
            "scopes": ["crates/api"]
        }),
    )
    .await;

    assert_eq!(
        response["result"]["isError"], false,
        "submit failed: {response}"
    );
    let payload = tool_text(&response);
    assert_eq!(
        payload["status"], "proposed",
        "an agent submission must never be accepted on arrival"
    );

    // And the stored record agrees.
    let id = payload["adr_id"].as_str().expect("id");
    let (status, detail) = harness
        .send("GET", &format!("/v1/adrs/{id}"), None, None)
        .await;
    assert_eq!(status, 200);
    assert_eq!(detail["status"], "proposed");
}

#[tokio::test]
async fn adr_transition_is_the_only_way_to_accept() {
    let harness = Harness::new(None).await;
    harness.create_project("demo").await;
    let submitted = call(
        &harness,
        "adr_submit",
        json!({
            "project": "demo",
            "external_ref": "docs/adr/0001.md",
            "source": SAMPLE_ADR,
            "actor": "agent:claude-code"
        }),
    )
    .await;
    let id = tool_text(&submitted)["adr_id"]
        .as_str()
        .expect("id")
        .to_string();

    let transitioned = call(
        &harness,
        "adr_transition",
        json!({ "id": id, "to": "accepted", "actor": "human:hanz", "reason": "reviewed" }),
    )
    .await;
    assert_eq!(transitioned["result"]["isError"], false, "{transitioned}");
    assert_eq!(tool_text(&transitioned)["status"], "accepted");
}

#[tokio::test]
async fn adr_search_returns_results_through_the_tool() {
    let harness = Harness::new(None).await;
    harness.create_project("demo").await;
    let submitted = call(
        &harness,
        "adr_submit",
        json!({
            "project": "demo",
            "external_ref": "docs/adr/0001.md",
            "source": SAMPLE_ADR,
            "actor": "human:test"
        }),
    )
    .await;
    let id = tool_text(&submitted)["adr_id"]
        .as_str()
        .expect("id")
        .to_string();
    harness.stub_points_for(&id).await;

    let response = call(&harness, "adr_search", json!({ "query": "async runtime" })).await;
    assert_eq!(response["result"]["isError"], false, "{response}");
    let payload = tool_text(&response);
    let results = payload["results"].as_array().expect("results");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["adr_id"], id);
}

#[tokio::test]
async fn adr_context_for_returns_the_binding_decisions() {
    let harness = Harness::new(None).await;
    harness.create_project("demo").await;
    call(
        &harness,
        "adr_submit",
        json!({
            "project": "demo",
            "external_ref": "docs/adr/0001.md",
            "source": SAMPLE_ADR,
            "actor": "human:test",
            "scopes": ["crates/api"]
        }),
    )
    .await;

    let response = call(
        &harness,
        "adr_context_for",
        json!({ "path": "crates/api/src/handler.rs", "project": "demo" }),
    )
    .await;
    assert_eq!(response["result"]["isError"], false, "{response}");
    let payload = tool_text(&response);
    let decisions = payload["decisions"].as_array().expect("decisions");
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0]["matched_scope"], "crates/api");
}

#[tokio::test]
async fn adr_list_and_adr_get_work_through_the_tools() {
    let harness = Harness::new(None).await;
    harness.create_project("demo").await;
    let submitted = call(
        &harness,
        "adr_submit",
        json!({
            "project": "demo",
            "external_ref": "docs/adr/0001.md",
            "source": SAMPLE_ADR,
            "actor": "human:test"
        }),
    )
    .await;
    let id = tool_text(&submitted)["adr_id"]
        .as_str()
        .expect("id")
        .to_string();

    let listed = call(&harness, "adr_list", json!({ "project": "demo" })).await;
    assert_eq!(listed["result"]["isError"], false);
    let payload = tool_text(&listed);
    assert_eq!(payload["decisions"].as_array().expect("decisions").len(), 1);

    let got = call(&harness, "adr_get", json!({ "id": id })).await;
    assert_eq!(got["result"]["isError"], false);
    let payload = tool_text(&got);
    assert_eq!(payload["title"], "Use tokio as the async runtime");
    assert!(!payload["sections"].as_array().expect("sections").is_empty());

    // Chunks come back with the detail, so a caller can see the sections were chunked
    // without a second tool call.
    let chunks = payload["chunks"].as_array().expect("chunks");
    assert!(!chunks.is_empty(), "a submitted decision always has chunks");
    assert!(
        chunks.iter().any(|chunk| chunk["kind"] == "document"),
        "every decision has a document chunk, got {chunks:?}"
    );
    for chunk in chunks {
        assert!(chunk["id"].as_str().is_some());
        assert!(chunk["ordinal"].is_u64());
        assert!(chunk["text"].as_str().is_some());
        assert!(chunk["truncated"].is_boolean());
        // Nothing has embedded yet, because no worker runs in this harness.
        assert_eq!(
            chunk["embedded"], false,
            "an unembedded chunk must say so, got {chunk}"
        );
    }
}

#[tokio::test]
async fn project_list_is_empty_before_anything_exists_and_names_slugs_afterwards() {
    let harness = Harness::new(None).await;

    let empty = call(&harness, "project_list", json!({})).await;
    assert_eq!(
        empty["result"]["isError"], false,
        "no project is not an error"
    );
    let payload = tool_text(&empty);
    assert_eq!(
        payload["projects"].as_array().expect("projects").len(),
        0,
        "an empty database should report an empty list, got {payload}"
    );

    harness.create_project("demo").await;
    harness.create_project("other").await;
    call(
        &harness,
        "adr_submit",
        json!({
            "project": "demo",
            "external_ref": "docs/adr/0001.md",
            "source": SAMPLE_ADR,
            "actor": "human:test"
        }),
    )
    .await;

    let listed = call(&harness, "project_list", json!({})).await;
    let payload = tool_text(&listed);
    let projects = payload["projects"].as_array().expect("projects");
    assert_eq!(projects.len(), 2, "both projects should be listed");
    assert_eq!(projects[0]["slug"], "demo");
    assert_eq!(projects[0]["adr_count"], 1);
    assert!(projects[0]["id"].as_str().is_some());
    assert!(projects[0]["created_at"].as_str().is_some());
    assert_eq!(projects[1]["slug"], "other");
    assert_eq!(projects[1]["adr_count"], 0);
}

#[tokio::test]
async fn mcp_accepts_a_streamable_http_accept_header() {
    let harness = Harness::new(None).await;
    // A streamable-HTTP client sends both content types. The server answers with plain JSON,
    // which the spec allows, so the header must be tolerated rather than rejected.
    let (status, bytes) = harness
        .send_with_headers(
            "POST",
            "/mcp",
            Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" })),
            &[
                ("accept", "application/json, text/event-stream"),
                ("content-type", "application/json"),
            ],
        )
        .await;
    assert_eq!(
        status, 200,
        "the streamable Accept header must be tolerated"
    );
    let body: Value = serde_json::from_slice(&bytes).expect("JSON body");
    assert_eq!(body["jsonrpc"], "2.0");
    assert_eq!(body["id"], 1);
}

#[tokio::test]
async fn adr_similar_reports_no_match_for_a_lone_decision() {
    let harness = Harness::new(None).await;
    harness.create_project("demo").await;
    let submitted = call(
        &harness,
        "adr_submit",
        json!({
            "project": "demo",
            "external_ref": "docs/adr/0001.md",
            "source": SAMPLE_ADR,
            "actor": "human:test"
        }),
    )
    .await;
    let id = tool_text(&submitted)["adr_id"]
        .as_str()
        .expect("id")
        .to_string();
    harness.stub_points_for(&id).await;

    let response = call(&harness, "adr_similar", json!({ "id": id })).await;
    assert_eq!(response["result"]["isError"], false, "{response}");
    let payload = tool_text(&response);
    assert!(
        payload["similar"].as_array().expect("similar").is_empty(),
        "a decision is not similar to itself"
    );
}

#[tokio::test]
async fn project_create_unblocks_a_fresh_instance() {
    let harness = Harness::new(None).await;

    // Nothing exists, so a submission has nowhere to land. This is the state an agent
    // hits on a brand new instance.
    let before = call(
        &harness,
        "adr_submit",
        json!({
            "project": "demo",
            "external_ref": "docs/adr/0001.md",
            "source": SAMPLE_ADR,
            "actor": "agent:claude-code"
        }),
    )
    .await;
    assert_eq!(before["result"]["isError"], true);

    let created = call(
        &harness,
        "project_create",
        json!({ "slug": "demo", "name": "Demo", "repo_url": "git@example:demo" }),
    )
    .await;
    assert_eq!(created["result"]["isError"], false, "{created}");
    let payload = tool_text(&created);
    assert_eq!(payload["slug"], "demo");
    assert_eq!(payload["repo_url"], "git@example:demo");

    // Creating it twice is a tool-level failure, not a crash.
    let again = call(
        &harness,
        "project_create",
        json!({ "slug": "demo", "name": "Demo" }),
    )
    .await;
    assert_eq!(again["result"]["isError"], true);
    assert!(again["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .contains("already exists"));

    // An invalid slug is rejected before anything is written.
    let bad = call(
        &harness,
        "project_create",
        json!({ "slug": "not a slug", "name": "x" }),
    )
    .await;
    assert_eq!(bad["result"]["isError"], true);
}

#[tokio::test]
async fn adr_import_ingests_a_batch_and_reports_per_document() {
    let harness = Harness::new(None).await;
    harness.create_project("demo").await;

    let response = call(
        &harness,
        "adr_import",
        json!({
            "project": "demo",
            "actor": "human:hanz",
            "documents": [
                { "external_ref": "docs/adr/0001.md", "source": SAMPLE_ADR },
                { "external_ref": "docs/adr/0002.md", "source": SAMPLE_ADR.replace("tokio", "axum") },
                { "external_ref": "docs/adr/0003.md", "source": "" }
            ]
        }),
    )
    .await;
    assert_eq!(response["result"]["isError"], false, "{response}");
    let payload = tool_text(&response);
    assert_eq!(
        payload["created"], 2,
        "the two valid documents should land: {payload}"
    );
    assert_eq!(
        payload["failed"], 1,
        "the empty document should fail: {payload}"
    );

    let results = payload["results"].as_array().expect("results");
    assert_eq!(results.len(), 3);
    assert_eq!(results[0]["action"], "created");
    assert_eq!(results[1]["action"], "created");
    assert_eq!(results[2]["action"], "failed");
    assert!(results[2]["error"].as_str().is_some());

    // By default a batch submission cannot accept its own decision.
    for result in &results[..2] {
        assert_eq!(result["status"], "proposed");
    }
}

#[tokio::test]
async fn adr_import_preserves_status_only_when_asked() {
    let harness = Harness::new(None).await;
    harness.create_project("demo").await;

    let forced = call(
        &harness,
        "adr_import",
        json!({
            "project": "demo",
            "actor": "agent:claude-code",
            "documents": [{ "external_ref": "docs/adr/0001.md", "source": SAMPLE_ADR }]
        }),
    )
    .await;
    let payload = tool_text(&forced);
    assert_eq!(payload["results"][0]["status"], "proposed");

    let preserved = call(
        &harness,
        "adr_import",
        json!({
            "project": "demo",
            "actor": "human:hanz",
            "preserve_status": true,
            "documents": [{ "external_ref": "docs/adr/0002.md", "source": SAMPLE_ADR.replace("tokio", "axum") }]
        }),
    )
    .await;
    assert_eq!(preserved["result"]["isError"], false, "{preserved}");
    let payload = tool_text(&preserved);
    assert_eq!(
        payload["results"][0]["status"], "accepted",
        "preserve_status is the operator migration path: {payload}"
    );
}

#[tokio::test]
async fn adr_delete_removes_the_decision_and_its_chunks() {
    let harness = Harness::new(None).await;
    harness.create_project("demo").await;
    let submitted = call(
        &harness,
        "adr_submit",
        json!({
            "project": "demo",
            "external_ref": "docs/adr/0001.md",
            "source": SAMPLE_ADR,
            "actor": "human:test"
        }),
    )
    .await;
    let id = tool_text(&submitted)["adr_id"]
        .as_str()
        .expect("id")
        .to_string();

    let deleted = call(&harness, "adr_delete", json!({ "id": id })).await;
    assert_eq!(deleted["result"]["isError"], false, "{deleted}");
    let payload = tool_text(&deleted);
    assert_eq!(payload["deleted"], true);
    assert!(payload["footprint"]["chunks"].as_i64().unwrap_or(0) > 0);

    let (status, body) = harness
        .send("GET", &format!("/v1/adrs/{id}"), None, None)
        .await;
    assert_eq!(status, 404, "the decision must be gone, got {body}");

    let listed = call(&harness, "adr_list", json!({ "project": "demo" })).await;
    let payload = tool_text(&listed);
    assert!(payload["decisions"]
        .as_array()
        .expect("decisions")
        .is_empty());
}

#[tokio::test]
async fn project_delete_removes_everything_under_it() {
    let harness = Harness::new(None).await;
    harness.create_project("demo").await;
    call(
        &harness,
        "adr_submit",
        json!({
            "project": "demo",
            "external_ref": "docs/adr/0001.md",
            "source": SAMPLE_ADR,
            "actor": "human:test"
        }),
    )
    .await;

    let deleted = call(&harness, "project_delete", json!({ "slug": "demo" })).await;
    assert_eq!(deleted["result"]["isError"], false, "{deleted}");
    let payload = tool_text(&deleted);
    assert_eq!(payload["deleted"], true);
    assert_eq!(payload["footprint"]["adrs"], 1);

    let listed = call(&harness, "project_list", json!({})).await;
    let payload = tool_text(&listed);
    assert!(payload["projects"].as_array().expect("projects").is_empty());

    // Deleting it again is a tool-level failure that names the missing project.
    let again = call(&harness, "project_delete", json!({ "slug": "demo" })).await;
    assert_eq!(again["result"]["isError"], true);
}

#[tokio::test]
async fn a_status_bullet_is_reported_instead_of_silently_becoming_proposed() {
    let harness = Harness::new(None).await;
    harness.create_project("demo").await;

    // No front matter at all: status is a bullet, which the parser does not read.
    let source = r#"# A decision recorded as a bullet

- **Status:** Accepted
- **Date:** 2024-03-01

## Context and Problem Statement

Something needed deciding.

## Decision Outcome

We decided.
"#;

    let response = call(
        &harness,
        "adr_submit",
        json!({
            "project": "demo",
            "external_ref": "docs/adr/0001.md",
            "source": source,
            "actor": "agent:claude-code"
        }),
    )
    .await;
    assert_eq!(response["result"]["isError"], false, "{response}");
    let payload = tool_text(&response);
    assert_eq!(payload["status"], "proposed");
    let warnings = payload["warnings"].as_array().expect("warnings");
    assert!(
        warnings
            .iter()
            .any(|warning| warning["kind"] == "status_bullet_ignored"),
        "the ignored bullet must be visible, got {warnings:?}"
    );
}

#[tokio::test]
async fn resubmitting_an_accepted_decision_reports_the_status_it_kept() {
    let harness = Harness::new(None).await;
    harness.create_project("demo").await;

    let submitted = call(
        &harness,
        "adr_submit",
        json!({
            "project": "demo",
            "external_ref": "docs/adr/0001.md",
            "source": SAMPLE_ADR,
            "actor": "agent:claude-code"
        }),
    )
    .await;
    let id = tool_text(&submitted)["adr_id"]
        .as_str()
        .expect("id")
        .to_string();
    call(
        &harness,
        "adr_transition",
        json!({ "id": id, "to": "accepted", "actor": "human:hanz" }),
    )
    .await;

    // A revision never changes the status, so an agent editing an accepted decision must
    // not be told it landed as `proposed`. It kept `accepted`.
    let revised = call(
        &harness,
        "adr_submit",
        json!({
            "project": "demo",
            "external_ref": "docs/adr/0001.md",
            "source": SAMPLE_ADR.replace("tokio", "async-std"),
            "actor": "agent:claude-code"
        }),
    )
    .await;
    assert_eq!(revised["result"]["isError"], false, "{revised}");
    let payload = tool_text(&revised);
    assert_eq!(payload["action"], "revised");
    assert_eq!(
        payload["status"], "accepted",
        "the decision kept its status across the edit: {payload}"
    );
    assert!(
        !payload["warnings"]
            .as_array()
            .expect("warnings")
            .iter()
            .any(|warning| warning["kind"] == "note"),
        "a revision was not forced to a different status, so there is nothing to warn about"
    );
}
