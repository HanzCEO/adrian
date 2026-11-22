//! A test harness: a real store, a real router, and stub dependencies.
//!
//! The handler tests need a store and the HTTP surface but not a 179M parameter model or a
//! vector database, so both dependencies are replaced by small axum servers that speak the
//! same contract.

#![allow(dead_code)]

use adrian_api::state::Settings;
use adrian_api::AppState;
use adrian_core::embedder::EmbedderClient;
use adrian_core::qdrant::QdrantClient;
use adrian_core::store::Store;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tower::ServiceExt;

/// Counters and knobs for the stub services.
pub struct StubState {
    pub embed_calls: AtomicUsize,
    pub qdrant_queries: AtomicUsize,
    /// Roles the embedder was called with, so tests can assert the query role was used.
    pub roles: std::sync::Mutex<Vec<String>>,
    /// Points returned by the stub Qdrant, as `(chunk_id, adr_id)`.
    pub points: std::sync::Mutex<Vec<(String, String)>>,
}

async fn stub_embed(
    axum::extract::State(state): axum::extract::State<Arc<StubState>>,
    body: String,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    state.embed_calls.fetch_add(1, Ordering::SeqCst);
    let request: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let role = request
        .get("role")
        .and_then(|v| v.as_str())
        .unwrap_or("document")
        .to_string();
    state.roles.lock().expect("lock").push(role.clone());

    let texts = request
        .get("texts")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let token = Value::Array(vec![json!(0.1); 128]);
    let dense = Value::Array(vec![json!(0.01); 768]);
    let items: Vec<Value> = texts
        .iter()
        .map(|_| {
            json!({
                "tokens": 4,
                "truncated": false,
                "vectors": [token.clone(), token.clone(), token.clone(), token.clone()],
                "dense": dense.clone(),
            })
        })
        .collect();
    axum::Json(json!({
        "model": "uibk-embedding-v1",
        "version": 1,
        "dim": 128,
        "dense_dim": 768,
        "role": role,
        "items": items,
    }))
    .into_response()
}

async fn stub_models() -> axum::Json<Value> {
    axum::Json(json!({
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

async fn stub_query(
    axum::extract::State(state): axum::extract::State<Arc<StubState>>,
) -> axum::Json<Value> {
    state.qdrant_queries.fetch_add(1, Ordering::SeqCst);
    let points = state.points.lock().expect("lock").clone();
    let rendered: Vec<Value> = points
        .iter()
        .enumerate()
        .map(|(index, (chunk, adr))| {
            // Qdrant point ids must be real uuids: the client parses them when it builds
            // the candidate filter for the second retrieval stage.
            json!({
                "id": uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, format!("point-{index}").as_bytes()).to_string(),
                "score": 1.0 - (index as f32 * 0.1),
                "payload": { "chunk_id": chunk, "adr_id": adr },
            })
        })
        .collect();
    axum::Json(json!({ "result": { "points": rendered } }))
}

async fn stub_readyz() -> &'static str {
    "ready"
}

async fn stub_upsert() -> axum::Json<Value> {
    axum::Json(json!({ "result": { "status": "completed" } }))
}

async fn stub_delete() -> axum::Json<Value> {
    axum::Json(json!({ "result": { "status": "completed" } }))
}

pub struct Harness {
    pub state: AppState,
    pub router: Router,
    pub stubs: Arc<StubState>,
}

impl Harness {
    pub async fn new(api_token: Option<&str>) -> Harness {
        Self::with_body_limit(api_token, 4096).await
    }

    pub async fn with_body_limit(api_token: Option<&str>, max_body_bytes: usize) -> Harness {
        let stubs = Arc::new(StubState {
            embed_calls: AtomicUsize::new(0),
            qdrant_queries: AtomicUsize::new(0),
            roles: std::sync::Mutex::new(Vec::new()),
            points: std::sync::Mutex::new(Vec::new()),
        });

        let embed_app = Router::new()
            .route("/v1/embed", axum::routing::post(stub_embed))
            .route("/v1/models", axum::routing::get(stub_models))
            .with_state(stubs.clone());
        let embed_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let embed_url = format!("http://{}", embed_listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            let _ = axum::serve(embed_listener, embed_app).await;
        });

        let qdrant_app = Router::new()
            .route(
                "/collections/{name}/points/query",
                axum::routing::post(stub_query),
            )
            .route(
                "/collections/{name}/points",
                axum::routing::put(stub_upsert),
            )
            .route(
                "/collections/{name}/points/delete",
                axum::routing::post(stub_delete),
            )
            .route("/readyz", axum::routing::get(stub_readyz))
            .with_state(stubs.clone());
        let qdrant_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let qdrant_url = format!("http://{}", qdrant_listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            let _ = axum::serve(qdrant_listener, qdrant_app).await;
        });

        let store = Store::open_in_memory().expect("store");
        store.migrate().await.expect("migrate");
        store.ensure_default_workspace().await.expect("workspace");

        let settings = Settings {
            collection: "test".to_string(),
            embed_model: adrian_core::EMBED_MODEL.to_string(),
            embed_version: adrian_core::EMBED_VERSION,
            api_token: api_token.map(|token| token.to_string()),
            mcp_token: api_token.map(|token| token.to_string()),
            max_body_bytes,
        };

        let state = AppState::new(
            store,
            EmbedderClient::new(embed_url).expect("embedder"),
            QdrantClient::new(qdrant_url).expect("qdrant"),
            settings,
        );
        let router = adrian_api::router(state.clone());

        Harness {
            state,
            router,
            stubs,
        }
    }

    /// Make the stub Qdrant return this decision's chunks.
    pub async fn stub_points_for(&self, adr_id: &str) {
        let id = uuid::Uuid::parse_str(adr_id).expect("uuid");
        let adr = self.state.store.get_adr(id).await.expect("adr");
        let chunks = self
            .state
            .store
            .list_chunks(adr.id, adr.cur_rev)
            .await
            .expect("chunks");
        let mut points = self.stubs.points.lock().expect("lock");
        points.clear();
        for chunk in chunks {
            points.push((chunk.id.to_string(), adr_id.to_string()));
        }
    }

    pub async fn send(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
        bearer: Option<&str>,
    ) -> (StatusCode, Value) {
        let (status, bytes) = self.send_raw(method, uri, body, bearer).await;
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).to_string()))
        };
        (status, value)
    }

    pub async fn send_raw(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
        bearer: Option<&str>,
    ) -> (StatusCode, Vec<u8>) {
        let mut builder = Request::builder().method(method).uri(uri);
        let body = match body {
            Some(value) => {
                builder = builder.header("content-type", "application/json");
                Body::from(value.to_string())
            }
            None => Body::empty(),
        };
        if let Some(token) = bearer {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        let request = builder.body(body).expect("request");
        let response = self
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("response");
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes()
            .to_vec();
        (status, bytes)
    }

    /// Send a raw body with an explicit content type, for testing extractor rejections.
    pub async fn send_typed(
        &self,
        method: &str,
        uri: &str,
        body: String,
        content_type: &str,
    ) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", content_type)
            .body(Body::from(body))
            .expect("request");
        let response = self
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("response");
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes()
            .to_vec();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    /// Send a request with extra headers, for tests that care about them.
    pub async fn send_with_headers(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> (StatusCode, Vec<u8>) {
        let mut builder = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let body = match body {
            Some(value) => Body::from(value.to_string()),
            None => Body::empty(),
        };
        let request = builder.body(body).expect("request");
        let response = self
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("response");
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes()
            .to_vec();
        (status, bytes)
    }

    /// Send an MCP JSON-RPC request.
    pub async fn mcp(&self, body: Value) -> (StatusCode, Value) {
        self.send("POST", "/mcp", Some(body), None).await
    }

    pub async fn create_project(&self, slug: &str) -> (StatusCode, Value) {
        self.send(
            "POST",
            "/v1/projects",
            Some(json!({ "slug": slug, "name": slug })),
            None,
        )
        .await
    }

    pub async fn create_adr(&self, project: &str, source: &str) -> (StatusCode, Value) {
        self.send(
            "POST",
            "/v1/adrs",
            Some(json!({
                "project": project,
                "external_ref": "docs/adr/0001-test.md",
                "source": source,
                "actor": "human:test",
                "scopes": ["crates/api"]
            })),
            None,
        )
        .await
    }
}

pub const SAMPLE_ADR: &str = r#"---
status: accepted
date: 2024-03-01
---

# Use tokio as the async runtime

## Context and Problem Statement

The API server needs an async runtime with a mature ecosystem.

## Decision Drivers

* Performance under load

## Considered Options

* tokio
* async-std

## Decision Outcome

Chosen option: "tokio", because it is the de-facto standard.

### Consequences

* Good, because ecosystem support is excellent
"#;
