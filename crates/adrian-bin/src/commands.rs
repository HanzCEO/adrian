//! Subcommand implementations.

use crate::config::Config;
use adrian_core::chunker::WhitespaceTokenCounter;
use adrian_core::embedder::EmbedderClient;
use adrian_core::export::{self, ExportFormat};
use adrian_core::ingest::{ingest, IngestRequest};
use adrian_core::qdrant::QdrantClient;
use adrian_core::store::Store;
use adrian_core::types::*;
use adrian_core::worker::{self, WorkerConfig};
use std::io::Write;
use std::process::ExitCode;
use std::sync::Arc;
use tokio::sync::watch;

type Result<T> = anyhow::Result<T>;

/// Build the store, applying migrations. Used by every subcommand that touches data.
async fn open_store(config: &Config) -> Result<Store> {
    let store = Store::open(&config.db_path)?;
    store.migrate().await?;
    Ok(store)
}

/// Run the daemon.
pub async fn serve(config: Config) -> Result<ExitCode> {
    let store = open_store(&config).await?;
    store.ensure_default_workspace().await?;

    let embedder = EmbedderClient::new(config.embedder_url.clone())?;
    let qdrant = QdrantClient::new(config.qdrant_url.clone())?;

    // Refuse to run against an engine serving a different model version: comparing vectors
    // from two models produces plausible nonsense rather than an error.
    let info = embedder
        .verify_compatibility(&config.embed_model, config.embed_version)
        .await?;
    tracing::info!(
        model = %info.model,
        version = info.version,
        dim = info.dim,
        device = %info.device,
        "embedding engine ready"
    );

    qdrant.ensure_collection(&config.collection).await?;
    tracing::info!(collection = %config.collection, "vector index ready");

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let worker_handle = if config.worker {
        Some(worker::spawn(
            store.clone(),
            embedder.clone(),
            qdrant.clone(),
            WorkerConfig {
                collection: config.collection.clone(),
                embed_model: config.embed_model.clone(),
                embed_version: config.embed_version,
                chunk_version: adrian_core::CHUNK_VERSION,
            },
            shutdown_rx,
        ))
    } else {
        None
    };

    let settings = adrian_api::state::Settings {
        collection: config.collection.clone(),
        embed_model: config.embed_model.clone(),
        embed_version: config.embed_version,
        api_token: config.effective_api_token().map(|token| token.to_string()),
        mcp_token: config.effective_mcp_token().map(|token| token.to_string()),
        max_body_bytes: config.max_body_bytes,
    };

    let state = adrian_api::AppState::new(store, embedder, qdrant, settings);
    let router = adrian_api::router(state);

    let listener = tokio::net::TcpListener::bind(&config.bind).await?;
    tracing::info!(address = %config.bind, collection = %config.collection, "adrian listening");

    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            wait_for_shutdown().await;
            let _ = shutdown_tx.send(true);
        })
        .await?;

    if let Some(handle) = worker_handle {
        // Give the worker a bounded window to finish the batch it is in.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), handle).await;
    }

    tracing::info!("adrian stopped");
    Ok(ExitCode::SUCCESS)
}

async fn wait_for_shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received");
}

/// Apply migrations and exit.
pub async fn migrate(config: Config) -> Result<ExitCode> {
    let store = open_store(&config).await?;
    store.ensure_default_workspace().await?;
    let version = store.schema_version().await?;
    println!("{} is at schema version {version}", config.db_path);
    Ok(ExitCode::SUCCESS)
}

/// Compare SQLite against the vector index and report discrepancies.
///
/// Reports only; it does not repair. A reconcile that silently "fixes" things hides the bug
/// that caused the drift.
pub async fn check(config: Config) -> Result<ExitCode> {
    let store = open_store(&config).await?;
    let qdrant = QdrantClient::new(config.qdrant_url.clone())?;

    let mut problems = 0usize;

    let missing = store
        .chunks_without_vectors(config.embed_model.clone(), config.embed_version)
        .await?;
    if missing.is_empty() {
        println!("ok    every chunk has a vector row");
    } else {
        problems += missing.len();
        println!(
            "FAIL  {} chunk(s) have no vector for {} v{}",
            missing.len(),
            config.embed_model,
            config.embed_version
        );
        for chunk in missing.iter().take(10) {
            println!("        chunk {chunk}");
        }
    }

    let failed = store.failed_embed_jobs().await?;
    if failed.is_empty() {
        println!("ok    no embed jobs have given up");
    } else {
        problems += failed.len();
        println!("FAIL  {} embed job(s) are parked as failed", failed.len());
        for (id, chunk, error) in failed.iter().take(10) {
            println!("        job {id} chunk {chunk}: {error}");
        }
    }

    let (chunks, vectors) = store.counts().await?;
    println!("      {chunks} chunk(s), {vectors} vector row(s) in SQLite");

    match qdrant.count(&config.collection).await {
        Ok(points) => {
            println!(
                "      {points} point(s) in collection {}",
                config.collection
            );
            if points != vectors {
                problems += 1;
                println!(
                    "FAIL  SQLite has {vectors} vector row(s) but Qdrant has {points} point(s)"
                );
            }
        }
        Err(err) => {
            problems += 1;
            println!("FAIL  cannot read collection {}: {err}", config.collection);
        }
    }

    if problems == 0 {
        println!("all checks passed");
        Ok(ExitCode::SUCCESS)
    } else {
        println!("{problems} problem(s) found");
        Ok(ExitCode::FAILURE)
    }
}

/// Write a project's decisions out.
pub async fn export(
    config: Config,
    project_slug: &str,
    format: &str,
    out: Option<&str>,
) -> Result<ExitCode> {
    let store = open_store(&config).await?;
    let project = store
        .get_project_by_slug(project_slug.to_string())
        .await?
        .ok_or_else(|| anyhow::anyhow!("project `{project_slug}` does not exist"))?;

    let format = ExportFormat::parse(format)?;
    let rendered = export::export_project(&store, project.id, format).await?;

    match out {
        Some(path) => {
            std::fs::write(path, &rendered)?;
            println!("wrote {} byte(s) to {path}", rendered.len());
        }
        None => {
            let mut stdout = std::io::stdout();
            stdout.write_all(&rendered)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Proxy newline-delimited JSON-RPC from stdin to a running daemon.
///
/// stdout carries the protocol and nothing else; diagnostics go to stderr.
pub async fn mcp_stdio(url: &str, token: Option<&str>) -> Result<()> {
    use std::io::{BufRead, BufReader};

    let endpoint = format!("{}/mcp", url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?;

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    for line in BufReader::new(stdin.lock()).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let mut request = client
            .post(&endpoint)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(line.clone());
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }

        let response = match request.send().await {
            Ok(response) => response,
            Err(err) => {
                eprintln!("adrian mcp-stdio: request failed: {err}");
                let error = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": serde_json::Value::Null,
                    "error": { "code": -32603, "message": format!("daemon unreachable: {err}") }
                });
                writeln!(stdout, "{error}")?;
                stdout.flush()?;
                continue;
            }
        };

        let status = response.status();
        let body = response.text().await?;
        if body.trim().is_empty() {
            // A notification has no response body.
            continue;
        }
        if !status.is_success() {
            eprintln!("adrian mcp-stdio: daemon returned {status}: {body}");
        }
        // One response line per request line, in order.
        writeln!(stdout, "{}", body.trim())?;
        stdout.flush()?;
    }

    Ok(())
}

/// Ingest a directory of ADRs. Used by the smoke test and by anyone bootstrapping a corpus.
#[allow(dead_code)]
pub async fn ingest_paths(
    store: &Store,
    project: &Project,
    paths: &[std::path::PathBuf],
    actor: &str,
) -> Result<Vec<IngestOutcome>> {
    let counter = WhitespaceTokenCounter;
    let mut outcomes = Vec::new();
    for path in paths {
        let source = std::fs::read(path)?;
        let external_ref = path.to_string_lossy().to_string();
        let outcome = ingest(
            store,
            IngestRequest {
                project_id: project.id,
                external_ref,
                source,
                source_format: "markdown".to_string(),
                actor: actor.to_string(),
                declared_scopes: Vec::new(),
                status_override: None,
                embed_model: project.embed_model.clone(),
                embed_version: project.embed_version,
                number: None,
            },
            &counter,
        )
        .await?;
        outcomes.push(outcome);
    }
    Ok(outcomes)
}

/// Shared handle type for the worker, kept so `serve` can await it on shutdown.
#[allow(dead_code)]
type WorkerJoin = Arc<tokio::task::JoinHandle<()>>;
