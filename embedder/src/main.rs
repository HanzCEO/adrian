//! The `adrian-embedder` binary.

use adrian_embedder::config::Config;
use adrian_embedder::model::{Engine, SharedEngine};
use clap::{Parser, Subcommand};
use std::process::ExitCode;
use std::sync::Arc;

#[derive(Debug, Parser)]
#[command(
    name = "adrian-embedder",
    version,
    about = "uibk-embedding-v1 inference engine"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Serve the embedding API.
    Serve,
    /// Probe the local health endpoint and exit 0 or 1. Used by the container healthcheck.
    Healthcheck {
        /// URL to probe.
        #[arg(
            long,
            env = "EMBEDDER_HEALTH_URL",
            default_value = "http://127.0.0.1:8081/healthz"
        )]
        url: String,
    },
    /// Verify the model files against SHA256SUMS and exit.
    VerifyWeights,
    /// Print the model's description as JSON and exit.
    Info,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    let config = match Config::from_env() {
        Ok(config) => config,
        Err(err) => {
            eprintln!("adrian-embedder: {err:#}");
            return ExitCode::FAILURE;
        }
    };
    init_tracing(&config.log);

    let result = match cli.command {
        Command::Healthcheck { url } => healthcheck(&url).await,
        Command::VerifyWeights => match config.model_files() {
            Ok(files) => {
                println!("all model files verified in {}", files.root.display());
                Ok(())
            }
            Err(err) => Err(err),
        },
        Command::Info => info(&config).await,
        Command::Serve => serve(config).await,
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!("{err:#}");
            eprintln!("adrian-embedder: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn healthcheck(url: &str) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()?;
    let response = client.get(url).send().await?;
    if response.status().is_success() {
        Ok(())
    } else {
        anyhow::bail!("health endpoint returned {}", response.status())
    }
}

async fn info(config: &Config) -> anyhow::Result<()> {
    let files = config.model_files()?;
    let device = adrian_embedder::select_device(config.cuda)?;
    let engine = Engine::load(&files, device)?;
    println!("{}", serde_json::to_string_pretty(&engine.info())?);
    Ok(())
}

async fn serve(config: Config) -> anyhow::Result<()> {
    let files = config.model_files()?;
    let device = adrian_embedder::select_device(config.cuda)?;
    tracing::info!(
        weights = %files.root.display(),
        device = ?device,
        "loading model"
    );
    let started = std::time::Instant::now();
    let engine: SharedEngine = Arc::new(Engine::load(&files, device)?);
    tracing::info!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        "model loaded"
    );

    let listener = tokio::net::TcpListener::bind(&config.bind).await?;
    tracing::info!(address = %config.bind, "embedding API listening");

    axum::serve(listener, adrian_embedder::http::router(engine))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
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
    tracing::info!("shutting down");
}

fn init_tracing(filter: &str) {
    use tracing_subscriber::{fmt, EnvFilter};
    let env_filter = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt()
        .with_env_filter(env_filter)
        .with_writer(std::io::stderr)
        .try_init();
}
