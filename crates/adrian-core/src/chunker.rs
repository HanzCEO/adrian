//! Semantic chunking. Implemented in a later task.

use thiserror::Error;

/// A failure while chunking.
#[derive(Debug, Error)]
pub enum ChunkerError {
    #[error("{0}")]
    Invalid(String),
}
