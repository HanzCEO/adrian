//! `adrian check` is the reconcile command: it reports drift between SQLite and the vector
//! index and exits non-zero so that it can be used in CI.

use serde_json::json;
use std::process::{Command, Stdio};
use std::sync::Arc;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_adrian")
}

#[derive(Clone)]
struct QdrantStub {
    points: Arc<std::sync::atomic::AtomicI64>,
}

async fn stub_embed(body: String) -> axum::response::Response {
    use axum::response::IntoResponse;
    let request: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
    let texts = request
        .get("texts")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let token = serde_json::Value::Array(vec![json!(0.1); 128]);
    let dense = serde_json::Value::Array(vec![json!(0.01); 768]);
    let items: Vec<serde_json::Value> = texts
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
        "items": items,
    }))
    .into_response()
}

async fn stub_models() -> axum::Json<serde_json::Value> {
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

async fn stub_readyz() -> &'static str {
    "ready"
}

async fn stub_collection(
    axum::extract::State(stub): axum::extract::State<QdrantStub>,
) -> axum::Json<serde_json::Value> {
    axum::Json(json!({
        "result": {
            "status": "green",
            "points_count": stub.points.load(std::sync::atomic::Ordering::SeqCst),
        }
    }))
}

async fn stub_ok() -> axum::Json<serde_json::Value> {
    axum::Json(json!({ "result": { "status": "completed" } }))
}

async fn spawn_stubs(points: i64) -> (String, String) {
    let embed_app = axum::Router::new()
        .route("/v1/embed", axum::routing::post(stub_embed))
        .route("/v1/models", axum::routing::get(stub_models));
    let embed_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let embed_url = format!("http://{}", embed_listener.local_addr().expect("addr"));
    tokio::spawn(async move {
        let _ = axum::serve(embed_listener, embed_app).await;
    });

    let stub = QdrantStub {
        points: Arc::new(std::sync::atomic::AtomicI64::new(points)),
    };
    let qdrant_app = axum::Router::new()
        .route("/readyz", axum::routing::get(stub_readyz))
        .route(
            "/collections/{name}",
            axum::routing::get(stub_collection).put(stub_ok),
        )
        .route("/collections/{name}/points", axum::routing::put(stub_ok))
        .route(
            "/collections/{name}/points/query",
            axum::routing::post(stub_ok),
        )
        .with_state(stub);
    let qdrant_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let qdrant_url = format!("http://{}", qdrant_listener.local_addr().expect("addr"));
    tokio::spawn(async move {
        let _ = axum::serve(qdrant_listener, qdrant_app).await;
    });

    (embed_url, qdrant_url)
}

/// A temp directory that cleans up after itself.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new() -> TempDir {
        let path = std::env::temp_dir().join(format!("adrian-check-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&path).expect("temp dir");
        TempDir(path)
    }

    fn db(&self) -> std::path::PathBuf {
        self.0.join("adrian.db")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run_check(db: &std::path::Path, embed_url: &str, qdrant_url: &str) -> (i32, String) {
    let output = Command::new(binary())
        .arg("check")
        .env("ADRIAN_DB_PATH", db)
        .env("ADRIAN_EMBEDDER_URL", embed_url)
        .env("ADRIAN_QDRANT_URL", qdrant_url)
        .env("ADRIAN_LOG", "warn")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run check");
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status.code().unwrap_or(-1), text)
}

#[tokio::test(flavor = "multi_thread")]
async fn check_reports_success_on_an_empty_database() {
    let (embed_url, qdrant_url) = spawn_stubs(0).await;
    let dir = TempDir::new();
    let (code, text) = run_check(&dir.db(), &embed_url, &qdrant_url);
    assert_eq!(code, 0, "an empty database is consistent:\n{text}");
    assert!(text.contains("all checks passed"), "got:\n{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn check_exits_non_zero_when_a_chunk_has_no_vector() {
    let (embed_url, qdrant_url) = spawn_stubs(0).await;
    let dir = TempDir::new();

    // Migrate and ingest through the library, so the database has chunks but no vectors.
    {
        use adrian_core::chunker::WhitespaceTokenCounter;
        use adrian_core::ingest::{ingest, IngestRequest};
        use adrian_core::store::Store;
        use adrian_core::types::Project;

        let store = Store::open(dir.db()).expect("store");
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
                external_ref: "docs/adr/0001.md".to_string(),
                source: b"# Use tokio\n\n## Context and Problem Statement\n\nx\n".to_vec(),
                source_format: "markdown".to_string(),
                actor: "test".to_string(),
                declared_scopes: Vec::new(),
                status_override: None,
                embed_model: adrian_core::EMBED_MODEL.to_string(),
                embed_version: adrian_core::EMBED_VERSION,
                number: None,
            },
            &WhitespaceTokenCounter,
        )
        .await
        .expect("ingest");
    }

    let (code, text) = run_check(&dir.db(), &embed_url, &qdrant_url);
    assert_ne!(code, 0, "unembedded chunks must fail the check:\n{text}");
    assert!(
        text.contains("have no vector"),
        "the report should name the problem, got:\n{text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn check_exits_non_zero_when_qdrant_is_unreachable() {
    let dir = TempDir::new();
    // A port nothing is listening on.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let dead_url = format!("http://{}", listener.local_addr().expect("addr"));
    drop(listener);

    let (code, text) = run_check(&dir.db(), "http://127.0.0.1:1", &dead_url);
    assert_ne!(code, 0, "an unreachable index must fail the check:\n{text}");
    assert!(
        text.contains("cannot read collection") || text.contains("FAIL"),
        "the report should name the failure, got:\n{text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn migrate_reports_the_schema_version_and_is_idempotent() {
    let dir = TempDir::new();
    for _ in 0..2 {
        let output = Command::new(binary())
            .arg("migrate")
            .env("ADRIAN_DB_PATH", dir.db())
            .env("ADRIAN_LOG", "warn")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("run migrate");
        assert!(output.status.success(), "migrate failed");
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(
            text.contains("schema version 1"),
            "expected the schema version in the output, got: {text}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn export_writes_markdown_for_a_project() {
    let dir = TempDir::new();
    {
        use adrian_core::chunker::WhitespaceTokenCounter;
        use adrian_core::ingest::{ingest, IngestRequest};
        use adrian_core::store::Store;
        use adrian_core::types::Project;

        let store = Store::open(dir.db()).expect("store");
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
                external_ref: "docs/adr/0001.md".to_string(),
                source: b"# Use tokio\n\n## Context and Problem Statement\n\nx\n".to_vec(),
                source_format: "markdown".to_string(),
                actor: "test".to_string(),
                declared_scopes: Vec::new(),
                status_override: None,
                embed_model: adrian_core::EMBED_MODEL.to_string(),
                embed_version: adrian_core::EMBED_VERSION,
                number: None,
            },
            &WhitespaceTokenCounter,
        )
        .await
        .expect("ingest");
    }

    let out = dir.0.join("export.md");
    let mut child = Command::new(binary())
        .args(["export", "--project", "demo", "--format", "madr"])
        .arg("--out")
        .arg(&out)
        .env("ADRIAN_DB_PATH", dir.db())
        .env("ADRIAN_LOG", "warn")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn export");
    let _ = child.stdin.take();
    let status = child.wait().expect("wait");
    assert!(status.success(), "export failed");
    let text = std::fs::read_to_string(&out).expect("read export");
    assert!(text.contains("# Use tokio"), "got: {text}");
}
