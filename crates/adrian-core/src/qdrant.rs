//! Qdrant client: the derived vector index.
//!
//! Qdrant holds vectors and a small payload of identifiers and filters. It never holds
//! chunk text. Text lives in SQLite and only there, because two copies of the same string
//! drift and then a citation points at text that no longer exists.
//!
//! Two named vectors per point:
//!
//! * `colbert`, a 128-wide multivector compared with MaxSim. This is the model's own
//!   output, and a plain MaxSim over it already equals `0.9 * (token MaxSim + 0.75 * dense
//!   cosine)`, so the hybrid score needs no fusion at query time.
//! * `dense`, a 768-wide single vector, used to generate candidates cheaply before the
//!   expensive MaxSim rerank.

use crate::types::ScoredChunk;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::Duration;
use thiserror::Error;
use uuid::Uuid;

/// A failure talking to Qdrant.
#[derive(Debug, Error)]
pub enum QdrantError {
    #[error("qdrant transport error: {0}")]
    Transport(String),

    #[error("qdrant returned {status}: {body}")]
    Status { status: u16, body: String },

    #[error("qdrant returned a malformed response: {0}")]
    Decode(String),

    #[error("qdrant operation did not complete in time: {0}")]
    Timeout(String),
}

type Result<T> = std::result::Result<T, QdrantError>;

/// The payload stored alongside every point.
///
/// Filters live here so that they can be applied inside Qdrant rather than after
/// retrieval, where a selective filter would silently lose recall.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChunkPayload {
    pub adr_id: String,
    pub project_id: String,
    pub number: Option<i64>,
    pub rev: i64,
    pub chunk_id: String,
    pub chunk_kind: String,
    pub option_key: Option<String>,
    pub template: String,
    pub status: String,
    pub model: String,
    pub model_version: i64,
    pub chunk_version: i64,
    /// Path prefixes the decision is asserted to bind, for scope filtering.
    pub scope_prefixes: Vec<String>,
    /// Technologies named by the decision, for structured filtering.
    pub technologies: Vec<String>,
}

/// One point to upsert.
#[derive(Debug, Clone)]
pub struct ChunkPoint {
    pub id: Uuid,
    pub payload: ChunkPayload,
    /// The model's output matrix, one 128-dimensional vector per surviving position.
    pub token_vectors: Vec<Vec<f32>>,
    /// The pooled 768-dimensional sentence vector.
    pub dense: Vec<f32>,
}

/// A handle to the Qdrant instance.
#[derive(Clone)]
pub struct QdrantClient {
    base_url: String,
    http: reqwest::Client,
}

impl QdrantClient {
    pub fn new(base_url: impl Into<String>) -> Result<QdrantClient> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
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

    /// The point id for a chunk under a given model version.
    ///
    /// Deterministic, so re-embedding the same chunk under the same model is idempotent and
    /// two model versions can coexist without colliding.
    pub fn point_id(chunk_id: Uuid, model: &str, version: i64) -> Uuid {
        let name = format!("{chunk_id}||{model}||{version}");
        Uuid::new_v5(&Uuid::NAMESPACE_OID, name.as_bytes())
    }

    /// Create the collection if it does not exist, and wait until it is usable.
    pub async fn ensure_collection(&self, name: &str) -> Result<()> {
        let url = format!("{}/collections/{name}", self.base_url);
        let response = self.send(self.http.get(&url)).await?;

        if response.status().is_success() {
            self.wait_until_green(name).await?;
            return Ok(());
        }
        if response.status().as_u16() != 404 {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(QdrantError::Status { status, body });
        }

        let body = json!({
            "vectors": {
                "colbert": {
                    "size": crate::TOKEN_DIM,
                    "distance": "Cosine",
                    "multivector_config": { "comparator": "max_sim" }
                },
                "dense": {
                    "size": crate::DENSE_DIM,
                    "distance": "Cosine"
                }
            }
        });

        let put = self.send(self.http.put(&url).json(&body)).await?;
        if !put.status().is_success() {
            let status = put.status().as_u16();
            let body = put.text().await.unwrap_or_default();
            return Err(QdrantError::Status { status, body });
        }
        self.wait_until_green(name).await
    }

    /// Poll until the collection reports green.
    async fn wait_until_green(&self, name: &str) -> Result<()> {
        let url = format!("{}/collections/{name}", self.base_url);
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            let response = self.send(self.http.get(&url)).await?;
            if response.status().is_success() {
                let value: serde_json::Value = response
                    .json()
                    .await
                    .map_err(|err| QdrantError::Decode(err.to_string()))?;
                let status = value
                    .pointer("/result/status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                if status == "green" {
                    return Ok(());
                }
            }
            if std::time::Instant::now() > deadline {
                return Err(QdrantError::Timeout(format!(
                    "collection {name} did not become green"
                )));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Upsert one point.
    pub async fn upsert_chunk(&self, collection: &str, point: ChunkPoint) -> Result<()> {
        let url = format!("{}/collections/{collection}/points", self.base_url);
        let body = json!({
            "points": [{
                "id": point.id.to_string(),
                "vector": {
                    "colbert": point.token_vectors,
                    "dense": point.dense,
                },
                "payload": point.payload,
            }]
        });
        let response = self.send(self.http.put(&url).json(&body)).await?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(QdrantError::Status { status, body });
        }
        Ok(())
    }

    /// Upsert a batch of points.
    pub async fn upsert_chunks(&self, collection: &str, points: Vec<ChunkPoint>) -> Result<()> {
        if points.is_empty() {
            return Ok(());
        }
        let url = format!("{}/collections/{collection}/points", self.base_url);
        let payload: Vec<serde_json::Value> = points
            .into_iter()
            .map(|point| {
                json!({
                    "id": point.id.to_string(),
                    "vector": {
                        "colbert": point.token_vectors,
                        "dense": point.dense,
                    },
                    "payload": point.payload,
                })
            })
            .collect();
        let response = self
            .send(self.http.put(&url).json(&json!({ "points": payload })))
            .await?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(QdrantError::Status { status, body });
        }
        Ok(())
    }

    /// Remove every point belonging to one ADR.
    pub async fn delete_adr_points(&self, collection: &str, adr_id: Uuid) -> Result<()> {
        let url = format!("{}/collections/{collection}/points/delete", self.base_url);
        let body = json!({
            "filter": { "must": [ { "key": "adr_id", "match": { "value": adr_id.to_string() } } ] }
        });
        let response = self.send(self.http.post(&url).json(&body)).await?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(QdrantError::Status { status, body });
        }
        Ok(())
    }

    /// Remove specific points by id.
    ///
    /// Used when a revision supersedes older ones: SQLite has already forgotten those
    /// vectors, so leaving the points behind would let a search return text that no longer
    /// describes the current decision.
    pub async fn delete_points(&self, collection: &str, ids: &[Uuid]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let url = format!("{}/collections/{collection}/points/delete", self.base_url);
        let body = json!({ "points": ids.iter().map(|id| id.to_string()).collect::<Vec<_>>() });
        let response = self.send(self.http.post(&url).json(&body)).await?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(QdrantError::Status { status, body });
        }
        Ok(())
    }

    /// Patch the cached status on every point of one ADR.
    ///
    /// The status in the payload is a cache for filtering, not the truth. SQLite is the
    /// truth, and a payload that disagrees is repaired by `adrian check` plus a rebuild.
    pub async fn update_status_payload(
        &self,
        collection: &str,
        adr_id: Uuid,
        status: &str,
    ) -> Result<()> {
        let url = format!("{}/collections/{collection}/points/payload", self.base_url);
        let body = json!({
            "payload": { "status": status },
            "filter": { "must": [ { "key": "adr_id", "match": { "value": adr_id.to_string() } } ] }
        });
        let response = self.send(self.http.post(&url).json(&body)).await?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(QdrantError::Status { status, body });
        }
        Ok(())
    }

    /// Two-stage retrieval: cheap dense candidates, then an exact MaxSim rerank.
    ///
    /// The rerank uses the same model's own token matrix, so the score is the one the
    /// model defines rather than an approximation of it.
    pub async fn search(
        &self,
        collection: &str,
        project_id: Uuid,
        query_vectors: Vec<Vec<f32>>,
        dense: Vec<f32>,
        limit: usize,
    ) -> Result<Vec<ScoredChunk>> {
        let candidates = self
            .dense_candidates(collection, project_id, &dense, 200)
            .await?;

        let rerank_limit = (limit * 5).max(50);
        match self
            .maxsim_search(
                collection,
                project_id,
                &query_vectors,
                rerank_limit,
                Some(&candidates),
            )
            .await
        {
            Ok(hits) => Ok(hits),
            Err(err) => {
                // A Qdrant build without prefetch support cannot take a candidate filter
                // together with a multivector query. Falling back to a single-stage search
                // is slower but still correct, and saying so in the log is better than
                // silently returning nothing.
                tracing::warn!(
                    error = %err,
                    "two-stage retrieval unavailable, falling back to a single MaxSim pass"
                );
                self.maxsim_search(collection, project_id, &query_vectors, rerank_limit, None)
                    .await
            }
        }
    }

    async fn dense_candidates(
        &self,
        collection: &str,
        project_id: Uuid,
        dense: &[f32],
        limit: usize,
    ) -> Result<Vec<Uuid>> {
        let url = format!("{}/collections/{collection}/points/query", self.base_url);
        let body = json!({
            "query": dense,
            "using": "dense",
            "limit": limit,
            "with_payload": false,
            "filter": {
                "must": [ { "key": "project_id", "match": { "value": project_id.to_string() } } ]
            }
        });
        let response = self.send(self.http.post(&url).json(&body)).await?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(QdrantError::Status { status, body });
        }
        let value: serde_json::Value = response
            .json()
            .await
            .map_err(|err| QdrantError::Decode(err.to_string()))?;
        Ok(parse_ids(&value))
    }

    async fn maxsim_search(
        &self,
        collection: &str,
        project_id: Uuid,
        query_vectors: &[Vec<f32>],
        limit: usize,
        restrict_to: Option<&[Uuid]>,
    ) -> Result<Vec<ScoredChunk>> {
        let url = format!("{}/collections/{collection}/points/query", self.base_url);

        let mut must = vec![json!({
            "key": "project_id",
            "match": { "value": project_id.to_string() }
        })];
        if let Some(ids) = restrict_to {
            if ids.is_empty() {
                return Ok(Vec::new());
            }
            let values: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
            must.push(json!({ "has_id": values }));
        }

        let body = json!({
            "query": query_vectors,
            "using": "colbert",
            "limit": limit,
            "with_payload": true,
            "filter": { "must": must }
        });

        let response = self.send(self.http.post(&url).json(&body)).await?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(QdrantError::Status { status, body });
        }
        let value: serde_json::Value = response
            .json()
            .await
            .map_err(|err| QdrantError::Decode(err.to_string()))?;

        parse_scored_chunks(&value)
    }

    /// Number of points in a collection, for `adrian check`.
    pub async fn count(&self, collection: &str) -> Result<i64> {
        let url = format!("{}/collections/{collection}", self.base_url);
        let response = self.send(self.http.get(&url)).await?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(QdrantError::Status { status, body });
        }
        let value: serde_json::Value = response
            .json()
            .await
            .map_err(|err| QdrantError::Decode(err.to_string()))?;
        Ok(value
            .pointer("/result/points_count")
            .and_then(|v| v.as_i64())
            .unwrap_or(0))
    }

    /// Whether the instance answers at all.
    pub async fn healthy(&self) -> bool {
        let url = format!("{}/readyz", self.base_url);
        match self
            .http
            .get(&url)
            .timeout(Duration::from_secs(3))
            .send()
            .await
        {
            Ok(response) => response.status().is_success(),
            Err(_) => false,
        }
    }

    /// Send with retries on connection errors and 5xx responses.
    async fn send(&self, request: reqwest::RequestBuilder) -> Result<reqwest::Response> {
        let mut last_error = None;
        for attempt in 0..3u32 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(100 * 2u64.pow(attempt))).await;
            }
            // `RequestBuilder` is consumed by `send`, so it has to be rebuilt each time.
            let cloned = request
                .try_clone()
                .ok_or_else(|| QdrantError::Transport("request body is not cloneable".into()))?;
            match cloned.send().await {
                Ok(response) if response.status().is_server_error() => {
                    let status = response.status().as_u16();
                    let body = response.text().await.unwrap_or_default();
                    last_error = Some(QdrantError::Status { status, body });
                }
                Ok(response) => return Ok(response),
                Err(err) => {
                    last_error = Some(QdrantError::Transport(err.to_string()));
                }
            }
        }
        Err(last_error.unwrap_or_else(|| QdrantError::Transport("request failed".into())))
    }
}

fn parse_ids(value: &serde_json::Value) -> Vec<Uuid> {
    value
        .pointer("/result/points")
        .and_then(|v| v.as_array())
        .map(|points| {
            points
                .iter()
                .filter_map(|point| point.get("id").and_then(|id| id.as_str()))
                .filter_map(|id| Uuid::parse_str(id).ok())
                .collect()
        })
        .unwrap_or_default()
}

fn parse_scored_chunks(value: &serde_json::Value) -> Result<Vec<ScoredChunk>> {
    let Some(points) = value.pointer("/result/points").and_then(|v| v.as_array()) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for point in points {
        let score = point.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
        let payload = point
            .get("payload")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let chunk_id = payload
            .get("chunk_id")
            .and_then(|v| v.as_str())
            .and_then(|v| Uuid::parse_str(v).ok());
        let adr_id = payload
            .get("adr_id")
            .and_then(|v| v.as_str())
            .and_then(|v| Uuid::parse_str(v).ok());
        match (chunk_id, adr_id) {
            (Some(chunk_id), Some(adr_id)) => out.push(ScoredChunk {
                chunk_id,
                adr_id,
                score,
            }),
            _ => {
                return Err(QdrantError::Decode(
                    "a result point is missing chunk_id or adr_id in its payload".to_string(),
                ))
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn point_ids_are_deterministic_and_version_scoped() {
        let chunk = Uuid::now_v7();
        let first = QdrantClient::point_id(chunk, "uibk-embedding-v1", 1);
        let second = QdrantClient::point_id(chunk, "uibk-embedding-v1", 1);
        let other_version = QdrantClient::point_id(chunk, "uibk-embedding-v1", 2);
        assert_eq!(first, second);
        assert_ne!(first, other_version);
    }

    #[test]
    fn payload_serialises_without_any_text_field() {
        let payload = ChunkPayload {
            adr_id: Uuid::nil().to_string(),
            project_id: Uuid::nil().to_string(),
            number: Some(1),
            rev: 1,
            chunk_id: Uuid::nil().to_string(),
            chunk_kind: "option".to_string(),
            option_key: Some("opt:tokio".to_string()),
            template: "madr".to_string(),
            status: "accepted".to_string(),
            model: "uibk-embedding-v1".to_string(),
            model_version: 1,
            chunk_version: 1,
            scope_prefixes: vec!["crates/api".to_string()],
            technologies: vec!["tokio".to_string()],
        };
        let json = serde_json::to_string(&payload).expect("serialise");
        assert!(!json.contains("\"text\""));
        assert!(json.contains("opt:tokio"));
    }

    #[test]
    fn parsing_scored_chunks_rejects_a_payload_without_ids() {
        let value = json!({
            "result": { "points": [ { "id": "x", "score": 0.5, "payload": {} } ] }
        });
        let err = parse_scored_chunks(&value).unwrap_err();
        assert!(matches!(err, QdrantError::Decode(_)));
    }

    #[test]
    fn parsing_scored_chunks_reads_ids_and_scores() {
        let chunk = Uuid::now_v7();
        let adr = Uuid::now_v7();
        let value = json!({
            "result": { "points": [ {
                "id": "irrelevant",
                "score": 0.25,
                "payload": { "chunk_id": chunk.to_string(), "adr_id": adr.to_string() }
            } ] }
        });
        let hits = parse_scored_chunks(&value).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].chunk_id, chunk);
        assert_eq!(hits[0].adr_id, adr);
        assert!((hits[0].score - 0.25).abs() < 1e-6);
    }

    #[test]
    fn an_empty_result_is_not_an_error() {
        let value = json!({ "result": { "points": [] } });
        assert!(parse_scored_chunks(&value).unwrap().is_empty());
        assert!(parse_ids(&value).is_empty());
    }
}
