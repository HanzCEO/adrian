//! REST and MCP surfaces for Adrian.
//!
//! Both live on the same axum router and the same port, so a single exposed port is
//! enough for a self-hosted daemon.

pub mod dto;
pub mod error;
pub mod mcp;
pub mod rest;
pub mod state;

pub use error::ApiError;
pub use state::AppState;
