//! Shared application state.
//!
//! Everything a handler needs is reachable from here, and every dependency is behind a
//! handle rather than a concrete socket, so handler tests can substitute stubs.

use adrian_core::embedder::EmbedderClient;
use adrian_core::qdrant::QdrantClient;
use adrian_core::store::Store;
use std::sync::Arc;

/// Runtime settings the handlers need to know about.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Qdrant collection that holds chunk vectors.
    pub collection: String,
    /// Embedding model name this daemon writes and reads.
    pub embed_model: String,
    /// Embedding model version.
    pub embed_version: i64,
    /// Static bearer token for `/v1`, if any.
    pub api_token: Option<String>,
    /// Static bearer token for `/mcp`, if any.
    pub mcp_token: Option<String>,
    /// Maximum accepted body size in bytes.
    pub max_body_bytes: usize,
}

/// The state every handler receives.
#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub embedder: EmbedderClient,
    pub qdrant: QdrantClient,
    pub settings: Arc<Settings>,
}

impl AppState {
    pub fn new(
        store: Store,
        embedder: EmbedderClient,
        qdrant: QdrantClient,
        settings: Settings,
    ) -> AppState {
        AppState {
            store,
            embedder,
            qdrant,
            settings: Arc::new(settings),
        }
    }
}
