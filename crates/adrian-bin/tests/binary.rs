//! The `adrian` binary itself: the stdio proxy and graceful shutdown.

use serde_json::{json, Value};
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_adrian")
}

// ---------------------------------------------------------------------------
// Stub services
// ---------------------------------------------------------------------------

struct Stubs;

async fn stub_embed(body: String) -> axum::response::Response {
    use axum::response::IntoResponse;
    let request: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
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

async fn stub_readyz() -> &'static str {
    "ready"
}

async fn stub_collection() -> axum::Json<Value> {
    axum::Json(json!({ "result": { "status": "green", "points_count": 0 } }))
}

async fn stub_ok() -> axum::Json<Value> {
    axum::Json(json!({ "result": { "status": "completed" } }))
}

/// Echo the request id, so the proxy's ordering guarantee is observable.
async fn stub_mcp(body: String) -> axum::Json<Value> {
    let request: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    axum::Json(json!({
        "jsonrpc": "2.0",
        "id": request.get("id").cloned().unwrap_or(Value::Null),
        "result": { "protocolVersion": "2025-06-18" }
    }))
}

/// Start the two dependencies plus an MCP echo, returning their base URLs.
async fn spawn_stubs() -> (String, String, String) {
    let _ = Arc::new(Stubs);

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
        .route(
            "/collections/{name}/points/payload",
            axum::routing::post(stub_ok),
        )
        .route(
            "/collections/{name}/points/delete",
            axum::routing::post(stub_ok),
        );
    let qdrant_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let qdrant_url = format!("http://{}", qdrant_listener.local_addr().expect("addr"));
    tokio::spawn(async move {
        let _ = axum::serve(qdrant_listener, qdrant_app).await;
    });

    let mcp_app = axum::Router::new().route("/mcp", axum::routing::post(stub_mcp));
    let mcp_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let mcp_url = format!("http://{}", mcp_listener.local_addr().expect("addr"));
    tokio::spawn(async move {
        let _ = axum::serve(mcp_listener, mcp_app).await;
    });

    (embed_url, qdrant_url, mcp_url)
}

// ---------------------------------------------------------------------------
// mcp-stdio
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_stdio_proxy_writes_only_jsonrpc_to_stdout() {
    let (_, _, mcp_url) = spawn_stubs().await;

    let mut child = Command::new(binary())
        .args(["mcp-stdio", "--url", &mcp_url])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mcp-stdio");

    {
        let stdin = child.stdin.as_mut().expect("stdin");
        writeln!(
            stdin,
            "{}",
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" })
        )
        .expect("write");
        // Two requests must produce two responses, in order.
        writeln!(
            stdin,
            "{}",
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" })
        )
        .expect("write");
    }

    let output = child.wait_with_output().expect("wait");
    assert!(
        output.status.success(),
        "the proxy exited with {:?}; stderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("utf8");
    let lines: Vec<&str> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    assert_eq!(lines.len(), 2, "one response line per request line");

    // Every line must be a complete JSON-RPC response, and nothing else may appear on
    // stdout: it is the protocol channel.
    for (index, line) in lines.iter().enumerate() {
        let value: Value = serde_json::from_str(line)
            .unwrap_or_else(|err| panic!("stdout line {index} is not JSON ({err}): {line:?}"));
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["id"], (index + 1) as i64);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_stdio_proxy_exits_zero_on_stdin_eof() {
    let (_, _, mcp_url) = spawn_stubs().await;
    let mut child = Command::new(binary())
        .args(["mcp-stdio", "--url", &mcp_url])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    // Close stdin immediately.
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("wait");
    assert!(
        output.status.success(),
        "an empty stdin must be a clean exit"
    );
    assert!(output.stdout.is_empty(), "nothing should be written");
}

// ---------------------------------------------------------------------------
// serve
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn serve_shuts_down_within_ten_seconds_of_sigterm() {
    let (embed_url, qdrant_url, _) = spawn_stubs().await;

    let dir = std::env::temp_dir().join(format!("adrian-serve-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let db_path = dir.join("adrian.db");

    // Pick a free port by binding and releasing it.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);

    let mut child = Command::new(binary())
        .arg("serve")
        .env("ADRIAN_BIND", format!("127.0.0.1:{port}"))
        .env("ADRIAN_DB_PATH", &db_path)
        .env("ADRIAN_QDRANT_URL", &qdrant_url)
        .env("ADRIAN_EMBEDDER_URL", &embed_url)
        .env("ADRIAN_WORKER", "0")
        .env("ADRIAN_LOG", "warn")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn serve");

    // Wait for the port to answer.
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}/healthz");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if let Ok(response) = client.get(&url).send().await {
            if response.status().is_success() {
                break;
            }
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let output = child.wait_with_output().expect("wait");
            panic!(
                "serve never became healthy; stderr: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    // Send SIGTERM and require a prompt, clean exit.
    let pid = child.id();
    let signal = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .expect("kill");
    assert!(signal.success(), "could not signal the daemon");

    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => break status,
            None => {
                if started.elapsed() > std::time::Duration::from_secs(10) {
                    let _ = child.kill();
                    panic!("serve did not exit within 10s of SIGTERM");
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    };
    assert!(
        status.success(),
        "serve exited with {:?} after SIGTERM",
        status.code()
    );

    let _ = std::fs::remove_dir_all(&dir);
}
