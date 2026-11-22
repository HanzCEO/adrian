//! Subcommand implementations.

use crate::config::Config;
use adrian_core::chunker::WhitespaceTokenCounter;
use adrian_core::embedder::EmbedderClient;
use adrian_core::export::{self, ExportFormat};
use adrian_core::ingest::{ingest, IngestAction, IngestRequest};
use adrian_core::qdrant::QdrantClient;
use adrian_core::store::Store;
use adrian_core::types::*;
use adrian_core::worker::{self, WorkerConfig};
use std::collections::BTreeMap;
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

/// Create a project.
///
/// The REST surface can do this too, but a bootstrap step that needs a running daemon and
/// a curl invocation is not a bootstrap step. The slug rules are shared with REST through
/// [`adrian_core::types::validate_project_slug`], so the two cannot disagree.
pub async fn project_add(
    config: Config,
    slug: &str,
    name: &str,
    repo_url: Option<String>,
) -> Result<ExitCode> {
    let store = open_store(&config).await?;

    // Validate before anything is written, so a bad slug leaves no trace.
    let slug = slug.trim().to_string();
    adrian_core::types::validate_project_slug(&slug).map_err(anyhow::Error::msg)?;
    if store.get_project_by_slug(slug.clone()).await?.is_some() {
        anyhow::bail!("project `{slug}` already exists");
    }

    let workspace = store.ensure_default_workspace().await?;

    let project = Project {
        id: uuid::Uuid::now_v7(),
        workspace_id: workspace,
        slug: slug.clone(),
        name: name.trim().to_string(),
        repo_url,
        default_template: "madr".to_string(),
        embed_model: config.embed_model.clone(),
        embed_version: config.embed_version,
        created_at: unix_now(),
    };
    store.create_project(project).await?;

    println!("created project `{slug}`");
    Ok(ExitCode::SUCCESS)
}

/// Delete a project and everything that belongs to it.
///
/// The vector index is cleared first. A point that outlives its row makes search return a
/// decision that no longer exists, which is worse than a delete that did not happen.
pub async fn project_rm(config: Config, slug: &str, force: bool) -> Result<ExitCode> {
    let store = open_store(&config).await?;
    let project = store
        .get_project_by_slug(slug.to_string())
        .await?
        .ok_or_else(|| anyhow::anyhow!("project `{slug}` does not exist"))?;

    let footprint = store.project_footprint(project.id).await?;
    println!(
        "project `{}` holds {} decision(s), {} revision(s), {} chunk(s), {} vector(s) and {} embed job(s)",
        project.slug,
        footprint.adrs,
        footprint.revisions,
        footprint.chunks,
        footprint.vectors,
        footprint.embed_jobs
    );

    if !force {
        println!("refusing to delete project `{slug}` without --force");
        return Ok(ExitCode::FAILURE);
    }

    let qdrant = QdrantClient::new(config.qdrant_url.clone())?;
    qdrant
        .delete_points_for_project(&config.collection, project.id)
        .await?;

    let deleted = store.delete_project(project.id).await?;
    println!(
        "deleted project `{}` with {} decision(s), {} revision(s), {} chunk(s), {} vector(s) and {} embed job(s)",
        project.slug,
        deleted.adrs,
        deleted.revisions,
        deleted.chunks,
        deleted.vectors,
        deleted.embed_jobs
    );
    Ok(ExitCode::SUCCESS)
}

/// Import a directory of MADR files into an existing project.
///
/// The status comes from each document's own front matter, because an import is an
/// operator act. `adr_submit` forces `proposed` for the opposite reason: an agent must not
/// accept its own decision.
///
/// Re-running is safe. Identical bytes are reported as `unchanged` and enqueue nothing, so
/// an interrupted import can simply be run again.
pub async fn import(
    config: Config,
    project_slug: &str,
    dir: &str,
    actor: &str,
    dry_run: bool,
) -> Result<ExitCode> {
    let store = open_store(&config).await?;
    let project = store
        .get_project_by_slug(project_slug.to_string())
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "project `{project_slug}` does not exist. Create it first with \
                 `adrian project add --slug {project_slug} --name <name>`"
            )
        })?;

    let files = collect_markdown(dir)?;
    if files.is_empty() {
        anyhow::bail!("no Markdown files found under `{dir}`");
    }

    println!(
        "{} file(s) under `{dir}` for project `{}`",
        files.len(),
        project.slug
    );

    if dry_run {
        return preview_import(&store, &project, &files).await;
    }

    let counter = WhitespaceTokenCounter;
    let mut created = 0usize;
    let mut revised = 0usize;
    let mut unchanged = 0usize;
    let mut queued = 0usize;
    let mut failures = Vec::new();

    for path in &files {
        let external_ref = external_ref_for(path);
        let source = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) => {
                failures.push(format!("{external_ref} could not be read. {err}"));
                continue;
            }
        };

        let outcome = ingest(
            &store,
            IngestRequest {
                project_id: project.id,
                external_ref: external_ref.clone(),
                source,
                source_format: "markdown".to_string(),
                actor: actor.to_string(),
                declared_scopes: Vec::new(),
                // No override: the document's own front matter decides.
                status_override: None,
                embed_model: project.embed_model.clone(),
                embed_version: project.embed_version,
                number: None,
            },
            &counter,
        )
        .await;

        match outcome {
            Ok(outcome) => {
                let action = match outcome.action {
                    IngestAction::Created => {
                        created += 1;
                        queued += outcome.chunks;
                        "created"
                    }
                    IngestAction::Revised => {
                        revised += 1;
                        queued += outcome.chunks;
                        "revised"
                    }
                    IngestAction::Unchanged => {
                        unchanged += 1;
                        "unchanged"
                    }
                };
                println!(
                    "{action:<9}  {external_ref}  rev {}  {}  {} chunk(s)",
                    outcome.rev,
                    outcome.status.as_str(),
                    outcome.chunks
                );
                for warning in &outcome.warnings {
                    println!("           {warning:?}");
                }
            }
            Err(err) => failures.push(format!("{external_ref} failed. {err}")),
        }
    }

    println!();
    println!("{created} created, {revised} revised, {unchanged} unchanged");
    println!("{queued} chunk(s) queued for embedding");
    if queued > 0 {
        println!(
            "the daemon's embed worker will index them, and `adrian check` reports anything \
             it gives up on"
        );
    }

    if failures.is_empty() {
        Ok(ExitCode::SUCCESS)
    } else {
        println!();
        println!("{} file(s) failed", failures.len());
        for failure in &failures {
            println!("  {failure}");
        }
        Ok(ExitCode::FAILURE)
    }
}

/// Parse the corpus and report what an import would do, without writing anything.
async fn preview_import(
    store: &Store,
    project: &Project,
    files: &[std::path::PathBuf],
) -> Result<ExitCode> {
    let mut statuses: BTreeMap<String, usize> = BTreeMap::new();
    let mut numbers: BTreeMap<i64, Vec<String>> = BTreeMap::new();
    let mut new_count = 0usize;
    let mut update_count = 0usize;
    let mut unchanged_count = 0usize;
    let mut warnings_total = 0usize;
    let mut failures = Vec::new();

    for path in files {
        let external_ref = external_ref_for(path);
        let source = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) => {
                failures.push(format!("{external_ref} could not be read. {err}"));
                continue;
            }
        };

        let parsed = adrian_core::parser::parse(&source);
        let number = adrian_core::ingest::number_from_ref(&external_ref);
        let status = parsed
            .status
            .map(|status| status.as_str().to_string())
            .unwrap_or_else(|| "unspecified".to_string());
        *statuses.entry(status.clone()).or_default() += 1;
        if let Some(number) = number {
            numbers
                .entry(number)
                .or_default()
                .push(external_ref.clone());
        }

        let existing = store
            .get_adr_by_external_ref(project.id, external_ref.clone())
            .await?;
        let verdict = match existing {
            None => {
                new_count += 1;
                "new"
            }
            Some(adr) => {
                let revision = store.get_revision(adr.id, adr.cur_rev).await?;
                if revision.source_sha256 == adrian_core::ingest::sha256(&source) {
                    unchanged_count += 1;
                    "unchanged"
                } else {
                    update_count += 1;
                    "update"
                }
            }
        };

        warnings_total += parsed.warnings.len();
        println!(
            "{verdict:<9}  {external_ref}  {}  {status}  {}  {} section(s)  {} warning(s)",
            number
                .map(|number| format!("#{number}"))
                .unwrap_or_else(|| "-".to_string()),
            parsed.title,
            parsed.sections.len(),
            parsed.warnings.len()
        );
        for warning in &parsed.warnings {
            println!("           {warning:?}");
        }
    }

    println!();
    let mut duplicated = 0usize;
    for (number, refs) in &numbers {
        if refs.len() > 1 {
            duplicated += 1;
            println!(
                "warning  {} files share display number #{number}  {}",
                refs.len(),
                refs.join("  ")
            );
        }
    }
    if duplicated > 0 {
        println!(
            "warning  {duplicated} display number(s) collide. Numbers come from the \
             filename and are display-only, so identity is `external_ref` and the \
             collision is cosmetic"
        );
    }

    let mut summary = statuses
        .iter()
        .map(|(status, count)| format!("{count} {status}"))
        .collect::<Vec<_>>();
    summary.sort();
    println!("status   {}", summary.join(", "));
    println!("verdict  {new_count} new, {update_count} update, {unchanged_count} unchanged");
    println!(
        "parsed   {} file(s), {warnings_total} warning(s)",
        files.len()
    );
    println!("dry run wrote nothing");

    if failures.is_empty() {
        Ok(ExitCode::SUCCESS)
    } else {
        for failure in &failures {
            println!("  {failure}");
        }
        Ok(ExitCode::FAILURE)
    }
}

/// Every `*.md` under `dir`, sorted so that two runs report the same order.
fn collect_markdown(dir: &str) -> Result<Vec<std::path::PathBuf>> {
    let root = std::path::Path::new(dir);
    if !root.is_dir() {
        anyhow::bail!("`{dir}` is not a directory");
    }

    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = std::fs::read_dir(&current)
            .map_err(|err| anyhow::anyhow!("cannot read {}  {err}", current.display()))?;
        for entry in entries {
            let entry =
                entry.map_err(|err| anyhow::anyhow!("cannot read {}  {err}", current.display()))?;
            let path = entry.path();
            let file_type = entry
                .file_type()
                .map_err(|err| anyhow::anyhow!("cannot read {}  {err}", path.display()))?;
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file()
                && path
                    .extension()
                    .map(|ext| ext.to_string_lossy().eq_ignore_ascii_case("md"))
                    .unwrap_or(false)
            {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

/// Seconds since the unix epoch.
///
/// `adrian-bin` does not depend on `chrono` just for this, and the store speaks unix
/// seconds anyway.
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// The `external_ref` for a path, as walked.
///
/// The path inherits whatever prefix the operator typed, so `--dir docs/adr` produces
/// repository-relative refs and a later `--dir /abs/docs/adr` would not match them. That is
/// deliberate: the ref is the client's stable key, and rewriting it here would silently
/// turn every re-import into a rename.
fn external_ref_for(path: &std::path::Path) -> String {
    let raw = path.to_string_lossy().replace('\\', "/");
    raw.strip_prefix("./").unwrap_or(raw.as_str()).to_string()
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

/// Shared handle type for the worker, kept so `serve` can await it on shutdown.
#[allow(dead_code)]
type WorkerJoin = Arc<tokio::task::JoinHandle<()>>;
