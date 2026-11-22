//! The engine's HTTP contract, tested through the router rather than through the handlers
//! so that the extractors and status codes are exercised too.

use adrian_embedder::model::{Engine, SharedEngine};
use adrian_embedder::weights::ModelFiles;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use std::path::Path;
use std::sync::Arc;
use tower::ServiceExt;

fn engine() -> Option<SharedEngine> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("weights");
    match ModelFiles::discover(&dir) {
        Ok(files) => {
            let device = adrian_embedder::select_device(false).expect("device");
            Some(Arc::new(Engine::load(&files, device).expect("load model")))
        }
        Err(err) => {
            if std::env::var("ADRIAN_SKIP_PARITY").is_ok() {
                eprintln!(
                    "SKIPPING HTTP TESTS: model unavailable at {} ({err:#})",
                    dir.display()
                );
                None
            } else {
                panic!("cannot load the model from {}: {err:#}", dir.display());
            }
        }
    }
}

async fn post(engine: SharedEngine, body: &str) -> (StatusCode, serde_json::Value) {
    let app = adrian_embedder::http::router(engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/embed")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

async fn get(engine: SharedEngine, uri: &str) -> (StatusCode, serde_json::Value) {
    let app = adrian_embedder::http::router(engine);
    let response = app
        .oneshot(
            Request::builder()
                .uri(uri)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

#[tokio::test]
async fn models_endpoint_reports_the_pinned_contract() {
    let Some(engine) = engine() else { return };
    let (status, body) = get(engine, "/v1/models").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["model"], "uibk-embedding-v1");
    assert_eq!(body["version"], 1);
    assert_eq!(body["dim"], 128);
    assert_eq!(body["dense_dim"], 768);
    assert_eq!(body["query_local_cap"], 64);
    assert_eq!(body["max_doc_tokens"], 2048);
    assert_eq!(body["max_query_tokens"], 256);
    assert_eq!(body["revision"], "b18df82e77306006a1dd0162199f6dfeacb5492e");
    assert_eq!(body["device"], "cpu");
}

#[tokio::test]
async fn healthz_is_available() {
    let Some(engine) = engine() else { return };
    let (status, body) = get(engine, "/healthz").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
}

#[tokio::test]
async fn a_missing_role_is_rejected() {
    let Some(engine) = engine() else { return };
    let (status, body) = post(engine, r#"{"texts":["hello"]}"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_request");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("role"),
        "the message must name the missing field, got: {message}"
    );
}

#[tokio::test]
async fn an_unknown_role_is_rejected() {
    let Some(engine) = engine() else { return };
    let (status, body) = post(engine, r#"{"role":"both","texts":["hello"]}"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_request");
}

#[tokio::test]
async fn an_empty_text_list_is_unprocessable() {
    let Some(engine) = engine() else { return };
    let (status, _) = post(engine, r#"{"role":"document","texts":[]}"#).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn a_missing_text_list_is_unprocessable() {
    let Some(engine) = engine() else { return };
    let (status, _) = post(engine, r#"{"role":"document"}"#).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn malformed_json_is_a_bad_request() {
    let Some(engine) = engine() else { return };
    let (status, body) = post(engine, "{not json").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_request");
}

#[tokio::test]
async fn a_document_embedding_has_the_documented_shape() {
    let Some(engine) = engine() else { return };
    let (status, body) = post(
        engine,
        r#"{"role":"document","texts":["hello world"],"return_dense":true}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["dim"], 128);
    assert_eq!(body["dense_dim"], 768);

    let items = body["items"].as_array().expect("items");
    assert_eq!(items.len(), 1);
    let vectors = items[0]["vectors"].as_array().expect("vectors");
    assert_eq!(vectors.len(), 5, "one vector per token position");
    assert_eq!(vectors[0].as_array().expect("row").len(), 128);
    assert_eq!(items[0]["dense"].as_array().expect("dense").len(), 768);
    assert_eq!(items[0]["truncated"], false);
    assert!(items[0]["tokens"].as_u64().expect("tokens") >= 4);
}

#[tokio::test]
async fn dense_vectors_are_omitted_unless_requested() {
    let Some(engine) = engine() else { return };
    let (status, body) = post(engine, r#"{"role":"query","texts":["hello"]}"#).await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().expect("items");
    assert!(
        items[0].get("dense").is_none(),
        "dense must be omitted when return_dense is false"
    );
}

#[tokio::test]
async fn a_batch_is_processed_in_order() {
    let Some(engine) = engine() else { return };
    let (status, body) = post(
        engine,
        r#"{"role":"document","texts":["one two", "three four five"]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().expect("items");
    assert_eq!(items.len(), 2);
    let first = items[0]["vectors"].as_array().expect("vectors").len();
    let second = items[1]["vectors"].as_array().expect("vectors").len();
    assert!(
        second > first,
        "the longer text must produce more vectors: {first} vs {second}"
    );
}
