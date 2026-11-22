//! The `adrian` binary.

mod commands;
mod config;

use clap::{Parser, Subcommand};

/// Adrian: an opinionated architectural decision record database.
#[derive(Debug, Parser)]
#[command(name = "adrian", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the daemon: REST and MCP on one port, plus the background embed worker.
    Serve,
    /// Proxy newline-delimited JSON-RPC from stdin to a running daemon.
    McpStdio {
        /// Base URL of the daemon.
        #[arg(long, env = "ADRIAN_URL", default_value = "http://127.0.0.1:38922")]
        url: String,
        /// Bearer token to send, if the daemon requires one.
        #[arg(long, env = "ADRIAN_API_TOKEN")]
        token: Option<String>,
    },
    /// Write a project's decisions out as JSON or MADR Markdown.
    Export {
        /// Project slug to export.
        #[arg(long)]
        project: String,
        /// Output format.
        #[arg(long, default_value = "madr", value_parser = ["json", "madr"])]
        format: String,
        /// Where to write the output. Defaults to stdout.
        #[arg(long)]
        out: Option<String>,
    },
    /// Apply pending database migrations and exit.
    Migrate,
    /// Check SQLite against the vector index and report discrepancies.
    Check,
    /// Create or remove a project.
    ///
    /// Bootstrapping a project is an operator act, which is why it lives here rather than
    /// on the MCP surface that coding agents talk to.
    Project {
        #[command(subcommand)]
        action: ProjectCommand,
    },
    /// Import a directory of MADR files into a project.
    ///
    /// This is the bootstrap path for an existing corpus. Each document keeps the status
    /// its own front matter declares, which is why it is an operator command and not an
    /// MCP tool: an agent submitting through `adr_submit` cannot accept its own decision.
    Import {
        /// Project slug to import into. It must already exist.
        #[arg(long)]
        project: String,
        /// Directory to walk recursively, collecting `*.md`.
        ///
        /// Give it relative to the repository root, because the path as walked becomes the
        /// decision's `external_ref` and that is what later renames are recognised against.
        #[arg(long)]
        dir: String,
        /// Who the import is attributed to.
        #[arg(long, default_value = "human:adrian")]
        actor: String,
        /// Parse and report without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Debug, Subcommand)]
enum ProjectCommand {
    /// Create a project.
    Add {
        /// Slug every other command and the REST surface will use.
        #[arg(long)]
        slug: String,
        /// Human readable name.
        #[arg(long)]
        name: String,
        /// Repository this project's decisions come from.
        #[arg(long)]
        repo_url: Option<String>,
    },
    /// Delete a project and everything that belongs to it.
    ///
    /// Stop the daemon first. The embed worker shares the database and could race the
    /// deletion, and its in-flight batch would write points back into a project that is
    /// being removed.
    Rm {
        /// Slug of the project to delete.
        #[arg(long)]
        slug: String,
        /// Actually delete. Without this the command reports what would go and stops.
        #[arg(long)]
        force: bool,
    },
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();

    // `mcp-stdio` is a pure pipe and must keep stdout clean, so it sets up logging first
    // and to stderr only.
    match cli.command {
        Command::McpStdio { url, token } => {
            init_tracing("warn");
            match commands::mcp_stdio(&url, token.as_deref()).await {
                Ok(()) => std::process::ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("adrian mcp-stdio: {err:#}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
        other => {
            let config = match config::Config::from_env() {
                Ok(config) => config,
                Err(err) => {
                    eprintln!("adrian: {err}");
                    return std::process::ExitCode::FAILURE;
                }
            };
            init_tracing(&config.log);
            let result = match other {
                Command::Serve => commands::serve(config).await,
                Command::Export {
                    project,
                    format,
                    out,
                } => commands::export(config, &project, &format, out.as_deref()).await,
                Command::Migrate => commands::migrate(config).await,
                Command::Check => commands::check(config).await,
                Command::Project {
                    action:
                        ProjectCommand::Add {
                            slug,
                            name,
                            repo_url,
                        },
                } => commands::project_add(config, &slug, &name, repo_url).await,
                Command::Project {
                    action: ProjectCommand::Rm { slug, force },
                } => commands::project_rm(config, &slug, force).await,
                Command::Import {
                    project,
                    dir,
                    actor,
                    dry_run,
                } => commands::import(config, &project, &dir, &actor, dry_run).await,
                Command::McpStdio { .. } => unreachable!("handled above"),
            };
            match result {
                Ok(code) => code,
                Err(err) => {
                    tracing::error!("{err:#}");
                    eprintln!("adrian: {err:#}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
    }
}

fn init_tracing(filter: &str) {
    use tracing_subscriber::{fmt, EnvFilter};

    let env_filter = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt()
        .with_env_filter(env_filter)
        .with_writer(std::io::stderr)
        .try_init();
}
