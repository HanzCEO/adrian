//! REST and MCP surfaces for Adrian.
//!
//! Both live on the same axum router and the same port, so a single exposed port is enough
//! for a self-hosted daemon.

pub mod auth;
pub mod dto;
pub mod error;
pub mod json;
pub mod mcp;
pub mod rest;
pub mod state;

pub use error::{ApiError, ApiResult};
pub use state::AppState;

use axum::extract::DefaultBodyLimit;
use axum::middleware;
use axum::routing::post;
use axum::Router;

/// Build the whole surface.
pub fn router(state: AppState) -> Router {
    let body_limit = state.settings.max_body_bytes;
    let auth_state = state.clone();

    Router::new()
        .merge(rest::router())
        .route("/mcp", post(mcp::handle))
        .layer(middleware::from_fn_with_state(auth_state, auth::require))
        .layer(DefaultBodyLimit::max(body_limit))
        .with_state(state)
}
