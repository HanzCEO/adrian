//! Background embed worker. Implemented in a later task.

use thiserror::Error;

/// A failure in the worker loop.
#[derive(Debug, Error)]
pub enum WorkerError {
    #[error("worker stopped: {0}")]
    Stopped(String),
}
