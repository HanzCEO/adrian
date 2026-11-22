//! One-way export to canonical JSON and MADR Markdown. Implemented in a later task.

use thiserror::Error;

/// A failure while exporting.
#[derive(Debug, Error)]
pub enum ExportError {
    #[error("{0}")]
    NotFound(String),

    #[error("{0}")]
    Invalid(String),

    #[error(transparent)]
    Store(#[from] crate::store::StoreError),
}
