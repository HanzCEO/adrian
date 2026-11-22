//! Client for the embedding engine.
//!
//! The engine is a separate container so that a model reload does not restart the
//! database daemon and so that GPU memory is isolated from everything else. The cost is
//! one local HTTP hop per batch, which is negligible next to a 179M parameter forward pass.
//!
//! The one rule that matters: **role is never inferred.** The model recognises a query by
//! a `[Q]` marker at position 1 and keeps only the first 64 token vectors for queries, so
//! the same text embeds differently on the two sides of a retrieval. Every call states its
//! role explicitly.

use crate::types::EmbedRole;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use thiserror::Error;

/// A failure talking to the embedding engine.
#[derive(Debug, Error)]
pub enum EmbedderError {
    #[error("embedder transport error: {0}")]
    Transport(String),

    #[error("embedder returned {status}: {body}")]
    Status { status: u16, body: String },

    #[error("embedder returned a malformed response: {0}")]
    Decode(String),

    #[error("embedder is serving {served} but this daemon expects {expected}")]
    ModelMismatch { served: String, expected: String },

    #[error("embedder is serving version {served} but this daemon expects {expected}")]
    VersionMismatch { served: i64, expected: i64 },
}

/// What the engine says about itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub model: String,
    pub version: i64,
    pub dim: usize,
    pub dense_dim: usize,
    pub max_doc_tokens: usize,
    pub max_query_tokens: usize,
    pub query_local_cap: usize,
    pub device: String,
    pub revision: String,
}

/// One embedded text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbedItem {
    pub tokens: usize,
    pub truncated: bool,
    /// One 128-dimensional unit vector per surviving token position. The first three are
    /// slot vectors carrying the pooled dense embedding.
    pub vectors: Vec<Vec<f32>>,
    /// The pooled 768-dimensional sentence vector, when requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dense: Option<Vec<f32>>,
}

/// The engine's reply to an embed request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbedResponse {
    pub model: String,
    pub version: i64,
    pub dim: usize,
    pub dense_dim: usize,
    pub items: Vec<EmbedItem>,
}

#[derive(Debug, Serialize)]
struct EmbedRequest<'a> {
    role: &'a str,
    texts: &'a [String],
    return_dense: bool,
}

/// A handle to the embedding engine.
#[derive(Clone)]
pub struct EmbedderClient {
    base_url: String,
    http: reqwest::Client,
}

impl EmbedderClient {
    pub fn new(base_url: impl Into<String>) -> Result<EmbedderClient, EmbedderError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|err| EmbedderError::Transport(err.to_string()))?;
        Ok(EmbedderClient {
            base_url: base_url.into(),
            http,
        })
    }

    /// Ask the engine what it is serving.
    pub async fn models(&self) -> Result<ModelInfo, EmbedderError> {
        let url = format!("{}/v1/models", self.base_url);
        let response = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|err| EmbedderError::Transport(err.to_string()))?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(EmbedderError::Status { status, body });
        }
        response
            .json::<ModelInfo>()
            .await
            .map_err(|err| EmbedderError::Decode(err.to_string()))
    }

    /// Embed a batch of texts in one role.
    pub async fn embed(
        &self,
        role: EmbedRole,
        texts: &[String],
        return_dense: bool,
    ) -> Result<EmbedResponse, EmbedderError> {
        if texts.is_empty() {
            return Ok(EmbedResponse {
                model: crate::EMBED_MODEL.to_string(),
                version: crate::EMBED_VERSION,
                dim: crate::TOKEN_DIM,
                dense_dim: crate::DENSE_DIM,
                items: Vec::new(),
            });
        }
        let url = format!("{}/v1/embed", self.base_url);
        let body = EmbedRequest {
            role: role.as_str(),
            texts,
            return_dense,
        };
        let response = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|err| EmbedderError::Transport(err.to_string()))?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(EmbedderError::Status { status, body });
        }
        response
            .json::<EmbedResponse>()
            .await
            .map_err(|err| EmbedderError::Decode(err.to_string()))
    }

    /// Refuse to operate against an engine that is serving a different model version.
    ///
    /// Comparing vectors produced by two different models yields plausible nonsense, so
    /// this is checked once at startup rather than discovered later in a ranking.
    pub async fn verify_compatibility(
        &self,
        expected_model: &str,
        expected_version: i64,
    ) -> Result<ModelInfo, EmbedderError> {
        let info = self.models().await?;
        if info.model != expected_model {
            return Err(EmbedderError::ModelMismatch {
                served: info.model,
                expected: expected_model.to_string(),
            });
        }
        if info.version != expected_version {
            return Err(EmbedderError::VersionMismatch {
                served: info.version,
                expected: expected_version,
            });
        }
        Ok(info)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_info_deserialises_the_documented_shape() {
        let json = r#"{
            "model": "uibk-embedding-v1",
            "version": 1,
            "dim": 128,
            "dense_dim": 768,
            "max_doc_tokens": 2048,
            "max_query_tokens": 256,
            "query_local_cap": 64,
            "device": "cpu",
            "revision": "b18df82e"
        }"#;
        let info: ModelInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.dim, 128);
        assert_eq!(info.dense_dim, 768);
        assert_eq!(info.query_local_cap, 64);
    }

    #[test]
    fn embed_response_tolerates_a_missing_dense_field() {
        let json = r#"{
            "model": "uibk-embedding-v1",
            "version": 1,
            "dim": 128,
            "dense_dim": 768,
            "items": [{"tokens": 4, "truncated": false, "vectors": [[0.1, 0.2]]}]
        }"#;
        let response: EmbedResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.items.len(), 1);
        assert!(response.items[0].dense.is_none());
    }
}
