//! Subcommand implementations. Filled in by later tasks.

use crate::config::Config;

type Result<T> = anyhow::Result<T>;

pub async fn serve(_config: Config) -> Result<std::process::ExitCode> {
    anyhow::bail!("`serve` is not implemented yet")
}

pub async fn export(
    _config: Config,
    _project: &str,
    _format: &str,
    _out: Option<&str>,
) -> Result<std::process::ExitCode> {
    anyhow::bail!("`export` is not implemented yet")
}

pub async fn migrate(_config: Config) -> Result<std::process::ExitCode> {
    anyhow::bail!("`migrate` is not implemented yet")
}

pub async fn check(_config: Config) -> Result<std::process::ExitCode> {
    anyhow::bail!("`check` is not implemented yet")
}

pub async fn mcp_stdio(_url: &str, _token: Option<&str>) -> Result<()> {
    anyhow::bail!("`mcp-stdio` is not implemented yet")
}
