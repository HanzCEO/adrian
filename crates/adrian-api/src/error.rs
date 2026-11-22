//! The one error type the HTTP surface exposes.
//!
//! Every failure mode maps to a stable `code` string so that clients can branch on it
//! without parsing prose.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// A failure that can be rendered as an HTTP response.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// The request was structurally fine but semantically wrong.
    #[error("{message}")]
    InvalidRequest {
        message: String,
        details: Option<serde_json::Value>,
    },

    /// The addressed resource does not exist.
    #[error("{message}")]
    NotFound { message: String },

    /// The request collides with existing state.
    #[error("{message}")]
    Conflict { message: String },

    /// A dependency (embedder, Qdrant) failed or is unreachable.
    #[error("{message}")]
    Upstream { message: String },

    /// Anything else. The message is logged but still returned, because this is a
    /// self-hosted tool and hiding the reason helps nobody.
    #[error("{message}")]
    Internal { message: String },
}

impl ApiError {
    pub fn invalid(message: impl Into<String>) -> Self {
        ApiError::InvalidRequest {
            message: message.into(),
            details: None,
        }
    }

    pub fn invalid_with(message: impl Into<String>, details: serde_json::Value) -> Self {
        ApiError::InvalidRequest {
            message: message.into(),
            details: Some(details),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        ApiError::NotFound {
            message: message.into(),
        }
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        ApiError::Conflict {
            message: message.into(),
        }
    }

    pub fn upstream(message: impl Into<String>) -> Self {
        ApiError::Upstream {
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        ApiError::Internal {
            message: message.into(),
        }
    }

    /// The stable machine-readable code for this failure.
    pub fn code(&self) -> &'static str {
        match self {
            ApiError::InvalidRequest { .. } => "invalid_request",
            ApiError::NotFound { .. } => "not_found",
            ApiError::Conflict { .. } => "conflict",
            ApiError::Upstream { .. } => "upstream_error",
            ApiError::Internal { .. } => "internal",
        }
    }

    pub fn status(&self) -> StatusCode {
        match self {
            ApiError::InvalidRequest { .. } => StatusCode::BAD_REQUEST,
            ApiError::NotFound { .. } => StatusCode::NOT_FOUND,
            ApiError::Conflict { .. } => StatusCode::CONFLICT,
            ApiError::Upstream { .. } => StatusCode::BAD_GATEWAY,
            ApiError::Internal { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        let mut body = json!({
            "error": {
                "code": self.code(),
                "message": self.to_string(),
            }
        });
        if let ApiError::InvalidRequest {
            details: Some(details),
            ..
        } = &self
        {
            body["error"]["details"] = details.clone();
        }
        (status, Json(body)).into_response()
    }
}

impl From<adrian_core::store::StoreError> for ApiError {
    fn from(err: adrian_core::store::StoreError) -> Self {
        use adrian_core::store::StoreError;
        match &err {
            StoreError::NotFound(message) => ApiError::not_found(message.clone()),
            StoreError::Conflict(message) => ApiError::conflict(message.clone()),
            StoreError::Invalid(message) => ApiError::invalid(message.clone()),
            _ => ApiError::internal(err.to_string()),
        }
    }
}

impl From<adrian_core::embedder::EmbedderError> for ApiError {
    fn from(err: adrian_core::embedder::EmbedderError) -> Self {
        ApiError::upstream(err.to_string())
    }
}

impl From<adrian_core::qdrant::QdrantError> for ApiError {
    fn from(err: adrian_core::qdrant::QdrantError) -> Self {
        ApiError::upstream(err.to_string())
    }
}

impl From<adrian_core::ingest::IngestError> for ApiError {
    fn from(err: adrian_core::ingest::IngestError) -> Self {
        use adrian_core::ingest::IngestError;
        match err {
            IngestError::Conflict(message) => ApiError::conflict(message),
            IngestError::NotFound(message) => ApiError::not_found(message),
            IngestError::Invalid(message) => ApiError::invalid(message),
            IngestError::Store(inner) => ApiError::from(inner),
            IngestError::Embedder(inner) => ApiError::upstream(inner.to_string()),
            IngestError::Qdrant(inner) => ApiError::upstream(inner.to_string()),
        }
    }
}

/// The result type every handler uses.
pub type ApiResult<T> = Result<T, ApiError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_statuses_match_the_documented_contract() {
        let cases = [
            (
                ApiError::invalid("x"),
                "invalid_request",
                StatusCode::BAD_REQUEST,
            ),
            (ApiError::not_found("x"), "not_found", StatusCode::NOT_FOUND),
            (ApiError::conflict("x"), "conflict", StatusCode::CONFLICT),
            (
                ApiError::upstream("x"),
                "upstream_error",
                StatusCode::BAD_GATEWAY,
            ),
            (
                ApiError::internal("x"),
                "internal",
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ];
        for (error, code, status) in cases {
            assert_eq!(error.code(), code);
            assert_eq!(error.status(), status);
        }
    }
}
