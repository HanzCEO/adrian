//! `adrian project add` and `adrian project rm`.
//!
//! Project lifecycle is an operator act, so it is tested through the binary rather than
//! through the library. The interesting cases are the refusals: a duplicate slug, a delete
//! without `--force`, and a delete whose index cleanup failed.

use serde_json::json;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_adrian")
}

// ---------------------------------------------------------------------------
// Stubs
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct QdrantStub {
    deletes: Arc<AtomicUsize>,
}

async fn stub_delete(
    axum::extract::State(stub): axum::extract::State<QdrantStub>,
) -> axum::Json<serde_json::Value> {
    stub.deletes.fetch_add(1, Ordering::SeqCst);
    axum::Json(json!({ "result": { "status": "completed" } }))
}

/// A Qdrant that answers the filtered point delete and counts the calls.
async fn spawn_qdrant() -> (String, QdrantStub) {
    let stub = QdrantStub {
        deletes: Arc::new(AtomicUsize::new(0)),
    };
    let app = axum::Router::new()
        .route(
            "/collections/{name}/points/delete",
            axum::routing::post(stub_delete),
        )
        .with_state(stub.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (url, stub)
}

/// A URL nothing is listening on.
fn dead_url() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    drop(listener);
    url
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A temp directory that cleans up after itself.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new() -> TempDir {
        let path = std::env::temp_dir().join(format!("adrian-project-{}", uuid::Uuid::now_v7()));
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

fn run(db: &std::path::Path, qdrant_url: &str, args: &[&str]) -> (i32, String) {
    let output = Command::new(binary())
        .args(args)
        .env("ADRIAN_DB_PATH", db)
        .env("ADRIAN_QDRANT_URL", qdrant_url)
        .env("ADRIAN_LOG", "warn")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run adrian");
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status.code().unwrap_or(-1), text)
}

/// A small but real MADR document, so the seeded project has sections and chunks.
const SOURCE: &str = "---\nstatus: accepted\n---\n\n# Use tokio\n\n## Context and Problem Statement\n\nWe need a runtime.\n";

/// Ingest one ADR so a project has something to count and something to delete.
async fn seed_adr(db: &std::path::Path, slug: &str) {
    use adrian_core::chunker::WhitespaceTokenCounter;
    use adrian_core::ingest::{ingest, IngestRequest};
    use adrian_core::store::Store;

    let store = Store::open(db).expect("store");
    store.migrate().await.expect("migrate");
    let project = store
        .get_project_by_slug(slug.to_string())
        .await
        .expect("lookup")
        .expect("project exists");
    ingest(
        &store,
        IngestRequest {
            project_id: project.id,
            external_ref: "docs/adr/0001-use-tokio.md".to_string(),
            source: SOURCE.as_bytes().to_vec(),
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

async fn count_rows(db: &std::path::Path, table: &'static str) -> i64 {
    use adrian_core::store::Store;
    let store = Store::open(db).expect("store");
    store
        .read(move |conn| {
            let count: i64 =
                conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })?;
            Ok(count)
        })
        .await
        .expect("count")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn project_add_creates_a_project_and_refuses_a_duplicate_slug() {
    let dir = TempDir::new();
    let (qdrant_url, _) = spawn_qdrant().await;

    let (code, text) = run(
        &dir.db(),
        &qdrant_url,
        &["project", "add", "--slug", "demo", "--name", "Demo"],
    );
    assert_eq!(code, 0, "add should succeed:\n{text}");
    assert!(text.contains("created project `demo`"), "got:\n{text}");
    assert_eq!(count_rows(&dir.db(), "project").await, 1);

    let (code, text) = run(
        &dir.db(),
        &qdrant_url,
        &["project", "add", "--slug", "demo", "--name", "Again"],
    );
    assert_ne!(code, 0, "a duplicate slug must fail:\n{text}");
    assert!(
        text.contains("project `demo` already exists"),
        "the message should name the collision, got:\n{text}"
    );
    assert_eq!(count_rows(&dir.db(), "project").await, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn project_add_rejects_a_slug_with_spaces() {
    let dir = TempDir::new();
    let (qdrant_url, _) = spawn_qdrant().await;

    let (code, text) = run(
        &dir.db(),
        &qdrant_url,
        &["project", "add", "--slug", "demo project", "--name", "Demo"],
    );
    assert_ne!(code, 0, "an invalid slug must fail:\n{text}");
    assert!(
        text.contains("may contain letters, digits"),
        "the message should come from the shared validator, got:\n{text}"
    );
    assert_eq!(count_rows(&dir.db(), "project").await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn project_rm_without_force_reports_the_footprint_and_deletes_nothing() {
    let dir = TempDir::new();
    let (qdrant_url, stub) = spawn_qdrant().await;

    let (code, text) = run(
        &dir.db(),
        &qdrant_url,
        &["project", "add", "--slug", "demo", "--name", "Demo"],
    );
    assert_eq!(code, 0, "add should succeed:\n{text}");
    seed_adr(&dir.db(), "demo").await;

    let (code, text) = run(&dir.db(), &qdrant_url, &["project", "rm", "--slug", "demo"]);
    assert_eq!(
        code, 1,
        "a delete without --force must exit non-zero:\n{text}"
    );
    assert!(
        text.contains("1 decision(s)") && text.contains("chunk(s)"),
        "the footprint should be reported, got:\n{text}"
    );
    assert!(
        text.contains("refusing to delete project `demo` without --force"),
        "got:\n{text}"
    );
    assert_eq!(count_rows(&dir.db(), "project").await, 1);
    assert_eq!(count_rows(&dir.db(), "adr").await, 1);
    assert_eq!(
        stub.deletes.load(Ordering::SeqCst),
        0,
        "a refused delete must not touch the index"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn project_rm_force_removes_the_project_and_every_row() {
    let dir = TempDir::new();
    let (qdrant_url, stub) = spawn_qdrant().await;

    let (code, text) = run(
        &dir.db(),
        &qdrant_url,
        &["project", "add", "--slug", "demo", "--name", "Demo"],
    );
    assert_eq!(code, 0, "add should succeed:\n{text}");
    seed_adr(&dir.db(), "demo").await;

    let (code, text) = run(
        &dir.db(),
        &qdrant_url,
        &["project", "rm", "--slug", "demo", "--force"],
    );
    assert_eq!(code, 0, "a forced delete should succeed:\n{text}");
    assert!(text.contains("deleted project `demo`"), "got:\n{text}");
    assert_eq!(stub.deletes.load(Ordering::SeqCst), 1);

    for table in [
        "project",
        "adr",
        "adr_rev",
        "section",
        "chunk",
        "chunk_fts",
        "chunk_vector",
        "status_event",
        "scope",
        "technology",
        "embed_job",
    ] {
        assert_eq!(
            count_rows(&dir.db(), table).await,
            0,
            "{table} should be empty after the delete"
        );
    }

    // A second delete has nothing to find.
    let (code, text) = run(
        &dir.db(),
        &qdrant_url,
        &["project", "rm", "--slug", "demo", "--force"],
    );
    assert_ne!(code, 0, "deleting a missing project must fail:\n{text}");
    assert!(
        text.contains("project `demo` does not exist"),
        "got:\n{text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn project_rm_force_leaves_the_project_when_the_index_cannot_be_cleared() {
    let dir = TempDir::new();
    let (qdrant_url, _) = spawn_qdrant().await;

    let (code, text) = run(
        &dir.db(),
        &qdrant_url,
        &["project", "add", "--slug", "demo", "--name", "Demo"],
    );
    assert_eq!(code, 0, "add should succeed:\n{text}");
    seed_adr(&dir.db(), "demo").await;

    let (code, text) = run(
        &dir.db(),
        &dead_url(),
        &["project", "rm", "--slug", "demo", "--force"],
    );
    assert_ne!(
        code, 0,
        "an unreachable index must fail the delete:\n{text}"
    );
    assert_eq!(
        count_rows(&dir.db(), "project").await,
        1,
        "the project must survive a failed index cleanup"
    );
    assert_eq!(count_rows(&dir.db(), "adr").await, 1);
}
