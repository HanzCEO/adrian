//! The embed worker against stub services.
//!
//! A real Qdrant and a real model are not needed to test the worker's contract: what
//! matters is that a job is claimed once, marked done on success, retried on failure, and
//! parked after five attempts.

use adrian_core::embedder::EmbedderClient;
use adrian_core::ingest::{ingest, IngestRequest};
use adrian_core::qdrant::QdrantClient;
use adrian_core::store::{Store, MAX_EMBED_ATTEMPTS};
use adrian_core::types::*;
use adrian_core::worker::{self, WorkerConfig};
use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::watch;

struct StubState {
    embed_calls: AtomicUsize,
    qdrant_calls: AtomicUsize,
    /// Fail the embed call while this is true.
    fail_embed: bool,
    last_qdrant_body: std::sync::Mutex<Option<Value>>,
}

async fn stub_embed(State(state): State<Arc<StubState>>, body: String) -> axum::response::Response {
    use axum::response::IntoResponse;
    state.embed_calls.fetch_add(1, Ordering::SeqCst);
    if state.fail_embed {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "stub failure" })),
        )
            .into_response();
    }
    let request: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let texts = request
        .get("texts")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    // `json!` has no repeat syntax, so the constant vectors are built as values first.
    let token_vector = Value::Array(vec![json!(0.1); 128]);
    let dense_vector = Value::Array(vec![json!(0.01); 768]);
    let items: Vec<Value> = texts
        .iter()
        .map(|_| {
            json!({
                "tokens": 4,
                "truncated": false,
                "vectors": [
                    token_vector.clone(),
                    token_vector.clone(),
                    token_vector.clone(),
                    token_vector.clone()
                ],
                "dense": dense_vector.clone(),
            })
        })
        .collect();
    Json(json!({
        "model": "uibk-embedding-v1",
        "version": 1,
        "dim": 128,
        "dense_dim": 768,
        "items": items,
    }))
    .into_response()
}

async fn stub_models() -> Json<Value> {
    Json(json!({
        "model": "uibk-embedding-v1",
        "version": 1,
        "dim": 128,
        "dense_dim": 768,
        "max_doc_tokens": 2048,
        "max_query_tokens": 256,
        "query_local_cap": 64,
        "device": "cpu",
        "revision": "stub",
    }))
}

async fn stub_upsert(
    State(state): State<Arc<StubState>>,
    body: String,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    state.qdrant_calls.fetch_add(1, Ordering::SeqCst);
    let value: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    *state.last_qdrant_body.lock().expect("lock") = Some(value);
    Json(json!({ "result": { "status": "completed" } })).into_response()
}

async fn spawn_stubs(fail_embed: bool) -> (Arc<StubState>, String, String) {
    let state = Arc::new(StubState {
        embed_calls: AtomicUsize::new(0),
        qdrant_calls: AtomicUsize::new(0),
        fail_embed,
        last_qdrant_body: std::sync::Mutex::new(None),
    });

    let embed_app = Router::new()
        .route("/v1/embed", post(stub_embed))
        .route("/v1/models", axum::routing::get(stub_models))
        .with_state(state.clone());
    let embed_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let embed_addr = embed_listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(embed_listener, embed_app).await;
    });

    let qdrant_app = Router::new()
        .route(
            "/collections/{name}/points",
            axum::routing::put(stub_upsert),
        )
        .with_state(state.clone());
    let qdrant_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let qdrant_addr = qdrant_listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(qdrant_listener, qdrant_app).await;
    });

    (
        state,
        format!("http://{embed_addr}"),
        format!("http://{qdrant_addr}"),
    )
}

async fn seeded_store() -> (Store, uuid::Uuid) {
    let store = Store::open_in_memory().expect("store");
    store.migrate().await.expect("migrate");
    let workspace = store.ensure_default_workspace().await.expect("workspace");
    let project = Project {
        id: uuid::Uuid::now_v7(),
        workspace_id: workspace,
        slug: "demo".to_string(),
        name: "Demo".to_string(),
        repo_url: None,
        default_template: "madr".to_string(),
        embed_model: adrian_core::EMBED_MODEL.to_string(),
        embed_version: adrian_core::EMBED_VERSION,
        created_at: 0,
    };
    store
        .create_project(project.clone())
        .await
        .expect("project");

    ingest(
        &store,
        IngestRequest {
            project_id: project.id,
            external_ref: "docs/adr/0001-use-tokio.md".to_string(),
            source: b"# Use tokio\n\n## Context and Problem Statement\n\nNeed a runtime.\n\n## Considered Options\n\n* tokio\n* async-std\n\n## Decision Outcome\n\nChosen option: \"tokio\", because standard.\n".to_vec(),
            source_format: "markdown".to_string(),
            actor: "test".to_string(),
            declared_scopes: vec![Scope {
                kind: ScopeKind::PathPrefix,
                value: "crates/api".to_string(),
                origin: ScopeOrigin::Declared,
                confidence: 1.0,
            }],
            status_override: None,
            embed_model: adrian_core::EMBED_MODEL.to_string(),
            embed_version: adrian_core::EMBED_VERSION,
            number: None,
        },
        &adrian_core::chunker::WhitespaceTokenCounter,
    )
    .await
    .expect("ingest");

    (store, project.id)
}

fn worker_config() -> WorkerConfig {
    WorkerConfig {
        collection: "test_collection".to_string(),
        embed_model: adrian_core::EMBED_MODEL.to_string(),
        embed_version: adrian_core::EMBED_VERSION,
        chunk_version: adrian_core::CHUNK_VERSION,
    }
}

#[tokio::test]
async fn the_worker_embeds_every_chunk_and_marks_jobs_done() {
    let (state, embed_url, qdrant_url) = spawn_stubs(false).await;
    let (store, _) = seeded_store().await;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = worker::spawn(
        store.clone(),
        EmbedderClient::new(embed_url).expect("embedder"),
        QdrantClient::new(qdrant_url).expect("qdrant"),
        worker_config(),
        shutdown_rx,
    );

    // Wait until every chunk has a vector row. Polling the outcome rather than the queue
    // avoids competing with the worker for claims.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let missing = store
            .chunks_without_vectors(
                adrian_core::EMBED_MODEL.to_string(),
                adrian_core::EMBED_VERSION,
            )
            .await
            .expect("chunks without vectors");
        if missing.is_empty() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the worker did not drain the queue in time; {} chunks still unembedded",
            missing.len()
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let _ = shutdown_tx.send(true);
    let _ = handle.await;

    assert!(
        state.embed_calls.load(Ordering::SeqCst) > 0,
        "the embedder was never called"
    );
    assert_eq!(
        state.qdrant_calls.load(Ordering::SeqCst),
        5,
        "one point per chunk"
    );

    let body = state
        .last_qdrant_body
        .lock()
        .expect("lock")
        .clone()
        .expect("a body was sent");
    let point = &body["points"][0];
    assert!(point["vector"]["colbert"].is_array());
    assert!(point["vector"]["dense"].is_array());
    assert!(
        point["payload"].get("text").is_none(),
        "the payload must not carry chunk text"
    );
    assert_eq!(point["payload"]["scope_prefixes"][0], "crates/api");
    assert_eq!(
        point["payload"]["chunk_version"],
        adrian_core::CHUNK_VERSION
    );
}

#[tokio::test]
async fn a_failing_embedder_is_retried_then_parked() {
    let (state, embed_url, qdrant_url) = spawn_stubs(true).await;
    let (store, _) = seeded_store().await;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = worker::spawn(
        store.clone(),
        EmbedderClient::new(embed_url).expect("embedder"),
        QdrantClient::new(qdrant_url).expect("qdrant"),
        worker_config(),
        shutdown_rx,
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let failed = store.failed_embed_jobs().await.expect("failed jobs");
        if failed.len() == 5 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "jobs never reached the failed state; {} parked so far",
            failed.len()
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let _ = shutdown_tx.send(true);
    let _ = handle.await;

    let failed = store.failed_embed_jobs().await.expect("failed jobs");
    assert_eq!(failed.len(), 5);
    assert!(
        failed[0].2.contains("500"),
        "the recorded error should mention the failure, got {:?}",
        failed[0].2
    );
    assert!(
        state.embed_calls.load(Ordering::SeqCst) >= MAX_EMBED_ATTEMPTS as usize,
        "the worker should have retried"
    );
    assert_eq!(
        state.qdrant_calls.load(Ordering::SeqCst),
        0,
        "nothing should be indexed when embedding fails"
    );
}
