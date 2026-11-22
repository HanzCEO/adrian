//! Qdrant client: the derived vector index. Implemented in a later task.

use thiserror::Error;

/// A failure talking to Qdrant.
#[derive(Debug, Error)]
pub enum QdrantError {
    #[error("qdrant transport error: {0}")]
    Transport(String),

    #[error("qdrant returned {status}: {body}")]
    Status { status: u16, body: String },

    #[error("qdrant returned a malformed response: {0}")]
    Decode(String),
}

/// A handle to the Qdrant instance. Filled in by the indexing task.
#[derive(Clone)]
pub struct QdrantClient {
    base_url: String,
    http: reqwest::Client,
}

impl QdrantClient {
    pub fn new(base_url: impl Into<String>) -> Result<QdrantClient, QdrantError> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|err| QdrantError::Transport(err.to_string()))?;
        Ok(QdrantClient {
            base_url: base_url.into(),
            http,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }
}
