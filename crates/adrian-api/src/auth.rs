//! Authentication.
//!
//! One optional static bearer token per surface. That is deliberately the whole story for
//! v0.1.0: it is a self-hosted single-user daemon, and `actor` on every write is an
//! unverified string. Pretending otherwise would be worse than saying so.

use crate::error::ApiError;
use crate::state::AppState;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;

/// Paths that never require a token, so a container healthcheck works.
const OPEN_PATHS: [&str; 1] = ["/healthz"];

pub async fn require(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let path = request.uri().path().to_string();
    if OPEN_PATHS.contains(&path.as_str()) {
        return Ok(next.run(request).await);
    }

    let expected = if path == "/mcp" {
        state.settings.mcp_token.as_deref()
    } else {
        state.settings.api_token.as_deref()
    };

    let Some(expected) = expected else {
        // No token configured: the daemon is trusting its network boundary. That is the
        // documented behaviour for a private install.
        return Ok(next.run(request).await);
    };

    let presented = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(|value| value.trim());

    match presented {
        Some(token) if constant_time_eq(token, expected) => Ok(next.run(request).await),
        Some(_) => Err(ApiError::unauthorized("the bearer token is not valid")),
        None => Err(ApiError::unauthorized(
            "this daemon requires an Authorization: Bearer token",
        )),
    }
}

/// Compare without an early exit, so a wrong token does not leak its prefix by timing.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut difference = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        difference |= x ^ y;
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comparison_matches_only_on_equality() {
        assert!(constant_time_eq("secret", "secret"));
        assert!(!constant_time_eq("secret", "secreT"));
        assert!(!constant_time_eq("secret", "secre"));
        assert!(!constant_time_eq("", "x"));
        assert!(constant_time_eq("", ""));
    }
}
