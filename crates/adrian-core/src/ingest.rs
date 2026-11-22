//! The write path: parse, chunk, persist, enqueue. Implemented in a later task.

use thiserror::Error;

/// A failure while ingesting a document.
#[derive(Debug, Error)]
pub enum IngestError {
    #[error("{0}")]
    NotFound(String),

    #[error("{0}")]
    Conflict(String),

    #[error("{0}")]
    Invalid(String),

    #[error(transparent)]
    Store(#[from] crate::store::StoreError),

    #[error(transparent)]
    Embedder(#[from] crate::embedder::EmbedderError),

    #[error(transparent)]
    Qdrant(#[from] crate::qdrant::QdrantError),
}
