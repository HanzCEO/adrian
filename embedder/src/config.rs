//! Runtime configuration for the embedding engine.

use anyhow::{Context, Result};
use std::path::PathBuf;

/// Everything the engine needs to start.
#[derive(Debug, Clone)]
pub struct Config {
    /// Address the HTTP server binds to.
    pub bind: String,
    /// Directory holding the model files and the SHA256SUMS manifest.
    pub weights_dir: PathBuf,
    /// Whether to verify every weight file at startup.
    pub verify_weights: bool,
    /// Run on CUDA when the `cuda` feature is compiled in.
    pub cuda: bool,
    /// `tracing` filter directive.
    pub log: String,
}

impl Config {
    pub fn from_env() -> Result<Config> {
        Ok(Config {
            bind: env_or("EMBEDDER_BIND", "0.0.0.0:8081"),
            weights_dir: PathBuf::from(env_or("ADRIAN_WEIGHTS_DIR", "./weights")),
            verify_weights: !flag("EMBEDDER_SKIP_VERIFY", false),
            cuda: flag("EMBEDDER_CUDA", false),
            log: env_or("RUST_LOG", "info"),
        })
    }

    /// Resolve the model files, optionally verifying them first.
    pub fn model_files(&self) -> Result<crate::weights::ModelFiles> {
        let files = crate::weights::ModelFiles::discover(&self.weights_dir)?;
        if self.verify_weights {
            files
                .verify()
                .context("refusing to serve unverified model weights")?;
        }
        Ok(files)
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn flag(key: &str, default: bool) -> bool {
    match std::env::var(key) {
        Ok(value) => matches!(value.as_str(), "1" | "true" | "yes" | "on"),
        Err(_) => default,
    }
}
