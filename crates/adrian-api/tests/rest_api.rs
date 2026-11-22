//! The REST surface, exercised through the router.

mod common;

use common::{Harness, SAMPLE_ADR};
use serde_json::json;

#[tokio::test]
async fn creating_a_project_returns_201_and_is_idempotent_to_repeat() {
    let harness = Harness::new(None).await;
    let (status, body) = harness.create_project("demo").await;
    assert_eq!(status, 201, "body: {body}");
    assert_eq!(body["slug"], "demo");
    assert!(body["id"].is_string());

    let (status, body) = harness.create_project("demo").await;
    assert_eq!(status, 409, "a duplicate project must conflict");
    assert_eq!(body["error"]["code"], "conflict");
}

#[tokio::test]
async fn an_unknown_adr_is_a_404_in_the_documented_shape() {
    let harness = Harness::new(None).await;
    let id = uuid::Uuid::now_v7();
    let (status, body) = harness
        .send("GET", &format!("/v1/adrs/{id}"), None, None)
        .await;
    assert_eq!(status, 404);
    assert_eq!(body["error"]["code"], "not_found");
    assert!(body["error"]["message"].is_string());
}

#[tokio::test]
async fn a_malformed_id_is_a_400() {
    let harness = Harness::new(None).await;
    let (status, body) = harness.send("GET", "/v1/adrs/not-a-uuid", None, None).await;
    assert_eq!(status, 400);
    assert_eq!(body["error"]["code"], "invalid_request");
}

#[tokio::test]
async fn an_unknown_transition_target_is_a_400() {
    let harness = Harness::new(None).await;
    harness.create_project("demo").await;
    let (status, body) = harness.create_adr("demo", SAMPLE_ADR).await;
    assert_eq!(status, 201, "body: {body}");
    let id = body["id"].as_str().expect("id");

    let (status, body) = harness
        .send(
            "POST",
            &format!("/v1/adrs/{id}/transition"),
            Some(json!({ "to": "maybe", "actor": "human:test" })),
            None,
        )
        .await;
    assert_eq!(status, 400);
    assert_eq!(body["error"]["code"], "invalid_request");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("proposed"),
        "the message should list the valid statuses, got: {message}"
    );
}

#[tokio::test]
async fn an_oversized_body_is_a_413_in_the_documented_shape() {
    let harness = Harness::with_body_limit(None, 512).await;
    let big = "x".repeat(4096);
    let (status, body) = harness
        .send(
            "POST",
            "/v1/projects",
            Some(json!({ "slug": "demo", "name": big })),
            None,
        )
        .await;
    assert_eq!(status, 413);
    assert_eq!(body["error"]["code"], "payload_too_large");
    assert!(body["error"]["message"].is_string());
}

/// Every failure the HTTP layer can produce must use the documented body, including the
/// ones axum's own extractors raise before a handler ever runs. A client that only knows
/// `{"error": {"code", "message"}}` must never be handed a plain-text rejection.
#[tokio::test]
async fn extractor_failures_use_the_documented_error_shape() {
    let harness = Harness::new(None).await;

    // Malformed JSON: the body cannot be parsed at all.
    let (status, body) = harness
        .send_typed(
            "POST",
            "/v1/projects",
            "{ this is not json".to_string(),
            "application/json",
        )
        .await;
    assert_eq!(status, 400);
    assert_eq!(body["error"]["code"], "invalid_request");
    assert!(body["error"]["message"].is_string());

    // Valid JSON, wrong shape: a required field is missing.
    let (status, body) = harness
        .send_typed(
            "POST",
            "/v1/projects",
            json!({ "name": "no slug here" }).to_string(),
            "application/json",
        )
        .await;
    assert_eq!(
        status, 400,
        "a missing field must be 400, not an undocumented 422"
    );
    assert_eq!(body["error"]["code"], "invalid_request");

    // Wrong content type.
    let (status, body) = harness
        .send_typed(
            "POST",
            "/v1/projects",
            json!({ "slug": "demo", "name": "Demo" }).to_string(),
            "text/plain",
        )
        .await;
    assert_eq!(status, 415);
    assert_eq!(body["error"]["code"], "unsupported_media_type");

    // A wrong type in an otherwise well-formed body.
    let (status, body) = harness
        .send_typed(
            "POST",
            "/v1/projects",
            json!({ "slug": 7, "name": "Demo" }).to_string(),
            "application/json",
        )
        .await;
    assert_eq!(status, 400);
    assert_eq!(body["error"]["code"], "invalid_request");

    // And every one of them carried JSON, not text/plain.
    let (status, body) = harness
        .send_typed(
            "POST",
            "/v1/projects",
            "nonsense".to_string(),
            "application/json",
        )
        .await;
    assert_eq!(status, 400);
    assert!(
        body.get("error").is_some(),
        "an extractor failure must still be a JSON error object, got {body}"
    );
}

#[tokio::test]
async fn bearer_auth_is_enforced_when_configured() {
    let harness = Harness::new(Some("secret")).await;

    let (status, body) = harness
        .send(
            "POST",
            "/v1/projects",
            Some(json!({ "slug": "demo", "name": "demo" })),
            None,
        )
        .await;
    assert_eq!(status, 401, "no token must be rejected");
    assert_eq!(body["error"]["code"], "unauthorized");

    let (status, _) = harness
        .send(
            "POST",
            "/v1/projects",
            Some(json!({ "slug": "demo", "name": "demo" })),
            Some("wrong"),
        )
        .await;
    assert_eq!(status, 401, "a wrong token must be rejected");

    let (status, _) = harness
        .send(
            "POST",
            "/v1/projects",
            Some(json!({ "slug": "demo", "name": "demo" })),
            Some("secret"),
        )
        .await;
    assert_eq!(status, 201, "the right token must be accepted");
}

#[tokio::test]
async fn auth_is_bypassed_when_no_token_is_configured() {
    let harness = Harness::new(None).await;
    let (status, _) = harness
        .send(
            "POST",
            "/v1/projects",
            Some(json!({ "slug": "demo", "name": "demo" })),
            None,
        )
        .await;
    assert_eq!(status, 201);
}

#[tokio::test]
async fn healthz_is_reachable_without_a_token() {
    let harness = Harness::new(Some("secret")).await;
    let (status, body) = harness.send("GET", "/healthz", None, None).await;
    assert_eq!(status, 200, "healthz must stay open for container probes");
    assert_eq!(body["status"], "ok");
    assert_eq!(body["embedder"]["reachable"], true);
    assert_eq!(body["qdrant"]["reachable"], true);
}

#[tokio::test]
async fn an_idempotency_key_replays_the_same_response_and_conflicts_on_a_different_body() {
    let harness = Harness::new(None).await;
    harness.create_project("demo").await;

    let first = harness
        .send(
            "POST",
            "/v1/adrs",
            Some(json!({
                "project": "demo",
                "external_ref": "docs/adr/0001.md",
                "source": SAMPLE_ADR,
                "actor": "human:test",
                "idempotency_key": "key-1"
            })),
            None,
        )
        .await;
    assert_eq!(first.0, 201, "body: {}", first.1);

    let replay = harness
        .send(
            "POST",
            "/v1/adrs",
            Some(json!({
                "project": "demo",
                "external_ref": "docs/adr/0001.md",
                "source": SAMPLE_ADR,
                "actor": "human:test",
                "idempotency_key": "key-1"
            })),
            None,
        )
        .await;
    assert_eq!(replay.0, 201);
    assert_eq!(replay.1, first.1, "the replay must be byte-identical");

    let conflict = harness
        .send(
            "POST",
            "/v1/adrs",
            Some(json!({
                "project": "demo",
                "external_ref": "docs/adr/0001.md",
                "source": "# Different\n",
                "actor": "human:test",
                "idempotency_key": "key-1"
            })),
            None,
        )
        .await;
    assert_eq!(conflict.0, 409);
    assert_eq!(conflict.1["error"]["code"], "conflict");
}

#[tokio::test]
async fn search_uses_the_query_role_and_hydrates_text_from_sqlite() {
    let harness = Harness::new(None).await;
    harness.create_project("demo").await;
    let (status, body) = harness.create_adr("demo", SAMPLE_ADR).await;
    assert_eq!(status, 201);
    let id = body["id"].as_str().expect("id").to_string();
    harness.stub_points_for(&id).await;

    let (status, body) = harness
        .send(
            "POST",
            "/v1/search",
            Some(json!({ "query": "which async runtime", "project": "demo" })),
            None,
        )
        .await;
    assert_eq!(status, 200, "body: {body}");

    let roles = harness.stubs.roles.lock().expect("lock").clone();
    assert!(
        roles.contains(&"query".to_string()),
        "the query must be embedded in the query role, got {roles:?}"
    );

    let results = body["results"].as_array().expect("results");
    assert!(!results.is_empty(), "search returned nothing");
    assert_eq!(results[0]["adr_id"], id);
    let matched = results[0]["matched_chunks"].as_array().expect("matched");
    assert!(!matched.is_empty());
    let hydrated: Vec<&str> = matched
        .iter()
        .filter_map(|chunk| chunk["text"].as_str())
        .collect();
    assert!(
        hydrated.iter().any(|text| text.contains("tokio")),
        "chunk text must be hydrated from SQLite, got {hydrated:?}"
    );
    assert!(
        matched
            .iter()
            .all(|chunk| chunk["text"].as_str().is_some_and(|text| !text.is_empty())),
        "every matched chunk must carry text"
    );
}

#[tokio::test]
async fn search_rejects_an_empty_query() {
    let harness = Harness::new(None).await;
    harness.create_project("demo").await;
    let (status, body) = harness
        .send("POST", "/v1/search", Some(json!({ "query": "   " })), None)
        .await;
    assert_eq!(status, 400);
    assert_eq!(body["error"]["code"], "invalid_request");
}

#[tokio::test]
async fn context_returns_only_decisions_whose_scope_is_an_ancestor_prefix() {
    let harness = Harness::new(None).await;
    harness.create_project("demo").await;
    let (status, body) = harness.create_adr("demo", SAMPLE_ADR).await;
    assert_eq!(status, 201);
    let id = body["id"].as_str().expect("id").to_string();

    let (status, body) = harness
        .send(
            "GET",
            "/v1/context?path=crates/api/src/handler.rs&project=demo",
            None,
            None,
        )
        .await;
    assert_eq!(status, 200, "body: {body}");
    let decisions = body["decisions"].as_array().expect("decisions");
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0]["adr_id"], id);
    assert_eq!(decisions[0]["matched_scope"], "crates/api");

    let (status, body) = harness
        .send(
            "GET",
            "/v1/context?path=crates/web/src/lib.rs&project=demo",
            None,
            None,
        )
        .await;
    assert_eq!(status, 200);
    assert!(
        body["decisions"].as_array().expect("decisions").is_empty(),
        "a path outside the scope must not match"
    );
}

#[tokio::test]
async fn exporting_an_unmodified_import_returns_the_original_bytes() {
    let harness = Harness::new(None).await;
    harness.create_project("demo").await;
    let (status, body) = harness.create_adr("demo", SAMPLE_ADR).await;
    assert_eq!(status, 201);
    let id = body["id"].as_str().expect("id").to_string();

    let (status, bytes) = harness
        .send_raw(
            "POST",
            "/v1/export",
            Some(json!({ "project": "demo", "format": "madr", "adr_ids": [id] })),
            None,
        )
        .await;
    assert_eq!(status, 200);
    let text = String::from_utf8(bytes).expect("utf8");
    assert_eq!(
        text, SAMPLE_ADR,
        "an imported record that was never modified must export verbatim"
    );
}

#[tokio::test]
async fn listing_adrs_returns_summaries_with_titles() {
    let harness = Harness::new(None).await;
    harness.create_project("demo").await;
    harness.create_adr("demo", SAMPLE_ADR).await;

    let (status, body) = harness
        .send("GET", "/v1/adrs?project=demo", None, None)
        .await;
    assert_eq!(status, 200, "body: {body}");
    let adrs = body.as_array().expect("array");
    assert_eq!(adrs.len(), 1);
    assert_eq!(adrs[0]["title"], "Use tokio as the async runtime");
    assert_eq!(adrs[0]["status"], "accepted");
}

#[tokio::test]
async fn getting_an_adr_returns_sections_items_scopes_and_events() {
    let harness = Harness::new(None).await;
    harness.create_project("demo").await;
    let (status, body) = harness.create_adr("demo", SAMPLE_ADR).await;
    assert_eq!(status, 201);
    let id = body["id"].as_str().expect("id");

    let (status, body) = harness
        .send("GET", &format!("/v1/adrs/{id}"), None, None)
        .await;
    assert_eq!(status, 200, "body: {body}");
    assert_eq!(body["title"], "Use tokio as the async runtime");
    assert_eq!(body["template"], "madr");
    assert!(!body["sections"].as_array().expect("sections").is_empty());
    assert!(!body["items"].as_array().expect("items").is_empty());
    assert_eq!(body["scopes"][0]["origin"], "declared");
    assert!(!body["status_events"].as_array().expect("events").is_empty());
    assert!(body["chunks"].as_array().expect("chunks").len() >= 5);
}

#[tokio::test]
async fn a_transition_moves_the_status_and_is_recorded() {
    let harness = Harness::new(None).await;
    harness.create_project("demo").await;
    let (status, body) = harness.create_adr("demo", SAMPLE_ADR).await;
    assert_eq!(status, 201);
    let id = body["id"].as_str().expect("id");

    let (status, body) = harness
        .send(
            "POST",
            &format!("/v1/adrs/{id}/transition"),
            Some(json!({ "to": "deprecated", "reason": "replaced", "actor": "human:test" })),
            None,
        )
        .await;
    assert_eq!(status, 200, "body: {body}");
    assert_eq!(body["status"], "deprecated");

    let (_, detail) = harness
        .send("GET", &format!("/v1/adrs/{id}"), None, None)
        .await;
    assert_eq!(detail["status"], "deprecated");
    let events = detail["status_events"].as_array().expect("events");
    assert_eq!(events.last().expect("last")["to"], "deprecated");
}

#[tokio::test]
async fn similar_excludes_the_decision_itself() {
    let harness = Harness::new(None).await;
    harness.create_project("demo").await;
    let (status, body) = harness.create_adr("demo", SAMPLE_ADR).await;
    assert_eq!(status, 201);
    let id = body["id"].as_str().expect("id").to_string();
    harness.stub_points_for(&id).await;

    let (status, body) = harness
        .send("GET", &format!("/v1/adrs/{id}/similar"), None, None)
        .await;
    assert_eq!(status, 200, "body: {body}");
    let results = body["results"].as_array().expect("results");
    assert!(
        results.iter().all(|result| result["adr_id"] != id),
        "the anchor decision must not appear in its own similar list"
    );
}
