//! Runtime configuration.
//!
//! Everything comes from the environment. There is no config file in v0.1.0: the daemon
//! runs in a container where the environment is the natural place for this, and a config
//! file would need its own parser, validation and precedence rules for no benefit.

use std::fmt;

/// Everything the daemon needs to start.
#[derive(Debug, Clone)]
pub struct Config {
    /// Address the HTTP server binds to.
    pub bind: String,
    /// Path to the SQLite database file.
    pub db_path: String,
    /// Base URL of the Qdrant instance.
    pub qdrant_url: String,
    /// Base URL of the embedding engine.
    pub embedder_url: String,
    /// Name of the Qdrant collection that holds chunk vectors.
    pub collection: String,
    /// Static bearer token for `/v1`. Empty disables authentication.
    pub api_token: String,
    /// Static bearer token for `/mcp`. Empty means `/mcp` follows `api_token`.
    pub mcp_token: String,
    /// Whether the background embed worker runs in this process.
    pub worker: bool,
    /// `tracing` filter directive.
    pub log: String,
    /// Maximum accepted request body size in bytes.
    pub max_body_bytes: usize,
    /// The embedding model this daemon expects the engine to serve.
    pub embed_model: String,
    /// The embedding model version this daemon expects.
    pub embed_version: i64,
}

/// Why the configuration could not be built.
#[derive(Debug, Clone)]
pub struct ConfigError {
    pub variable: String,
    pub message: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid {}: {}", self.variable, self.message)
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    /// Build a configuration from the process environment.
    pub fn from_env() -> Result<Config, ConfigError> {
        let bind = env_or("ADRIAN_BIND", "0.0.0.0:38922");
        let db_path = env_or("ADRIAN_DB_PATH", "/data/adrian.db");
        let qdrant_url = trim_trailing_slash(env_or("ADRIAN_QDRANT_URL", "http://127.0.0.1:6333"));
        let embedder_url =
            trim_trailing_slash(env_or("ADRIAN_EMBEDDER_URL", "http://127.0.0.1:8081"));
        let collection = env_or("ADRIAN_COLLECTION", "adr_chunks__uibk_v1");
        let api_token = env_or("ADRIAN_API_TOKEN", "");
        let mcp_token = env_or("ADRIAN_MCP_TOKEN", "");
        let log = env_or("ADRIAN_LOG", "info");

        let worker = match env_or("ADRIAN_WORKER", "1").as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            other => {
                return Err(ConfigError {
                    variable: "ADRIAN_WORKER".to_string(),
                    message: format!("expected a boolean, got `{other}`"),
                })
            }
        };

        let max_body_bytes = match env_or("ADRIAN_MAX_BODY_BYTES", "4194304").parse::<usize>() {
            Ok(value) if value > 0 => value,
            Ok(_) => {
                return Err(ConfigError {
                    variable: "ADRIAN_MAX_BODY_BYTES".to_string(),
                    message: "must be greater than zero".to_string(),
                })
            }
            Err(err) => {
                return Err(ConfigError {
                    variable: "ADRIAN_MAX_BODY_BYTES".to_string(),
                    message: err.to_string(),
                })
            }
        };

        let embed_model = env_or("ADRIAN_EMBED_MODEL", adrian_core::EMBED_MODEL);
        let embed_version = match env_or("ADRIAN_EMBED_VERSION", "1").parse::<i64>() {
            Ok(value) => value,
            Err(err) => {
                return Err(ConfigError {
                    variable: "ADRIAN_EMBED_VERSION".to_string(),
                    message: err.to_string(),
                })
            }
        };

        Ok(Config {
            bind,
            db_path,
            qdrant_url,
            embedder_url,
            collection,
            api_token,
            mcp_token,
            worker,
            log,
            max_body_bytes,
            embed_model,
            embed_version,
        })
    }

    /// The token `/mcp` should require, falling back to the REST token.
    pub fn effective_mcp_token(&self) -> Option<&str> {
        let token = if self.mcp_token.is_empty() {
            self.api_token.as_str()
        } else {
            self.mcp_token.as_str()
        };
        if token.is_empty() {
            None
        } else {
            Some(token)
        }
    }

    /// The token `/v1` should require.
    pub fn effective_api_token(&self) -> Option<&str> {
        if self.api_token.is_empty() {
            None
        } else {
            Some(self.api_token.as_str())
        }
    }
}

fn env_or(key: &str, default: &str) -> String {
    match std::env::var(key) {
        Ok(value) => value,
        Err(_) => default.to_string(),
    }
}

fn trim_trailing_slash(mut value: String) -> String {
    while value.ends_with('/') {
        value.pop();
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_usable_without_any_environment() {
        // The test process may already carry some of these variables; read only the
        // defaults by constructing the struct the way `from_env` would with nothing set.
        assert_eq!(
            env_or("ADRIAN_DEFINITELY_UNSET_VAR_XYZ", "fallback"),
            "fallback"
        );
        assert_eq!(
            trim_trailing_slash("http://example:6333/".to_string()),
            "http://example:6333"
        );
        assert_eq!(
            trim_trailing_slash("http://example:6333".to_string()),
            "http://example:6333"
        );
    }

    #[test]
    fn mcp_token_falls_back_to_the_api_token() {
        let mut config = Config {
            bind: "0.0.0.0:38922".into(),
            db_path: ":memory:".into(),
            qdrant_url: "http://127.0.0.1:6333".into(),
            embedder_url: "http://127.0.0.1:8081".into(),
            collection: "c".into(),
            api_token: "secret".into(),
            mcp_token: String::new(),
            worker: false,
            log: "info".into(),
            max_body_bytes: 1024,
            embed_model: "uibk-embedding-v1".into(),
            embed_version: 1,
        };
        assert_eq!(config.effective_mcp_token(), Some("secret"));
        config.api_token.clear();
        assert_eq!(config.effective_mcp_token(), None);
        assert_eq!(config.effective_api_token(), None);
    }
}
