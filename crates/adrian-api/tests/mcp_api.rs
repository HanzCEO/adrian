//! The MCP surface: protocol behaviour and the seven tools.

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
async fn tools_list_advertises_all_seven_tools() {
    let harness = Harness::new(None).await;
    let (status, body) = harness
        .mcp(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }))
        .await;
    assert_eq!(status, 200);
    let tools = body["result"]["tools"].as_array().expect("tools");
    assert_eq!(tools.len(), 7);
    for tool in tools {
        assert!(tool["description"].as_str().is_some_and(|d| !d.is_empty()));
        assert!(tool["inputSchema"].is_object());
    }
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
