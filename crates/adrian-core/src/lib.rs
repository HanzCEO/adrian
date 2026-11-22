//! Adrian: an opinionated architectural decision record database.
//!
//! The crate is organised around a strict separation of concerns:
//!
//! * [`types`] holds the domain vocabulary. Nothing in there knows about SQL or HTTP.
//! * [`parser`] turns MADR Markdown into a typed projection ([`types::ParsedAdr`]).
//! * [`chunker`] turns that projection into the units that get embedded.
//! * [`store`] owns SQLite, which is the system of record.
//! * [`qdrant`] owns the derived vector index.
//! * [`embedder`] is the client for the embedding engine.
//! * [`ingest`] and [`worker`] are the write path.
//! * [`export`] is the one-way projection back out to Markdown and JSON.

pub mod chunker;
pub mod embedder;
pub mod export;
pub mod ingest;
pub mod parser;
pub mod qdrant;
pub mod store;
pub mod types;
pub mod worker;

pub use types::*;

/// The version of this crate, reported by `/healthz` and MCP `initialize`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The embedding model this build is pinned to.
pub const EMBED_MODEL: &str = "uibk-embedding-v1";

/// The embedding model version, bumped when the model weights change.
pub const EMBED_VERSION: i64 = 1;

/// Token width of one ColBERT token vector.
pub const TOKEN_DIM: usize = 128;

/// Width of the pooled dense sentence vector.
pub const DENSE_DIM: usize = 768;

/// Chunking rules version. Bump when [`chunker`] changes what it emits so that
/// stored vectors can be told apart from freshly computed ones.
pub const CHUNK_VERSION: i64 = 1;
