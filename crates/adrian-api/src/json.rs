//! Extractors that fail in the documented shape.
//!
//! `axum::Json` and `axum::Query` answer a bad request with their own plain-text rejection
//! body, which would mean a client that only knows how to parse
//! `{"error": {"code", "message"}}` cannot read the reason. Every rejection they can produce
//! is therefore mapped onto [`ApiError`]:
//!
//! | Cause | Status | `code` |
//! | --- | --- | --- |
//! | body is not valid JSON | 400 | `invalid_request` |
//! | body is valid JSON but the wrong shape, or a required field is missing | 400 | `invalid_request` |
//! | `Content-Type` is not `application/json` | 415 | `unsupported_media_type` |
//! | body exceeds the configured limit | 413 | `payload_too_large` |
//! | query string does not deserialize (e.g. `?limit=abc`) | 400 | `invalid_request` |
//!
//! A missing required field is reported as 400 rather than 422 on purpose: the contract
//! documents 400/401/404/409/413/415/500/502 and nothing else, so a 422 would be a status a
//! client has no rule for.

use crate::error::ApiError;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::request::Parts;
use axum::Json;
use serde::de::DeserializeOwned;

/// A JSON body extractor whose failures are rendered as [`ApiError`].
pub struct ApiJson<T>(pub T);

impl<T, S> FromRequest<S> for ApiJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(request, state).await {
            Ok(Json(value)) => Ok(ApiJson(value)),
            Err(rejection) => Err(map_rejection(rejection)),
        }
    }
}

/// A query-string extractor whose failures are rendered as [`ApiError`].
///
/// `?limit=abc` is a client mistake, not a server fault, so it must arrive as the same JSON
/// body as every other 400 rather than as axum's plain-text `Failed to deserialize query
/// string`.
pub struct ApiQuery<T>(pub T);

impl<T, S> FromRequestParts<S> for ApiQuery<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match axum::extract::Query::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Query(value)) => Ok(ApiQuery(value)),
            Err(rejection) => Err(map_query_rejection(rejection)),
        }
    }
}

fn map_query_rejection(rejection: QueryRejection) -> ApiError {
    match rejection {
        QueryRejection::FailedToDeserializeQueryString(err) => ApiError::invalid(format!(
            "the query string does not match the expected parameters: {err}"
        )),
        other => ApiError::invalid(other.body_text()),
    }
}

fn map_rejection(rejection: JsonRejection) -> ApiError {
    match rejection {
        JsonRejection::JsonDataError(err) => ApiError::invalid(format!(
            "the request body does not match the expected shape: {err}"
        )),
        JsonRejection::JsonSyntaxError(err) => {
            ApiError::invalid(format!("the request body is not valid JSON: {err}"))
        }
        JsonRejection::MissingJsonContentType(_) => ApiError::unsupported_media_type(
            "the request must be sent with Content-Type: application/json".to_string(),
        ),
        JsonRejection::BytesRejection(err) => {
            // The only way `Json` fails while reading bytes is a body that is too large.
            ApiError::payload_too_large(format!("the request body was rejected: {err}"))
        }
        other => ApiError::invalid(other.body_text()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{header, StatusCode};
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Payload {
        #[allow(dead_code)]
        name: String,
    }

    async fn extract(
        body: &'static str,
        content_type: Option<&'static str>,
    ) -> Result<Payload, ApiError> {
        let mut builder = Request::builder().method("POST").uri("/");
        if let Some(content_type) = content_type {
            builder = builder.header(header::CONTENT_TYPE, content_type);
        }
        let request = builder.body(Body::from(body)).expect("request");
        ApiJson::<Payload>::from_request(request, &())
            .await
            .map(|ApiJson(payload)| payload)
    }

    #[tokio::test]
    async fn a_valid_body_is_accepted() {
        let payload = extract(r#"{"name":"ok"}"#, Some("application/json"))
            .await
            .expect("valid");
        assert_eq!(payload.name, "ok");
    }

    #[tokio::test]
    async fn malformed_json_is_a_400_in_the_documented_shape() {
        let err = extract("{not json", Some("application/json"))
            .await
            .expect_err("must reject");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert_eq!(err.code(), "invalid_request");
    }

    #[tokio::test]
    async fn a_missing_field_is_a_400_not_a_422() {
        let err = extract(r#"{"other":1}"#, Some("application/json"))
            .await
            .expect_err("must reject");
        assert_eq!(
            err.status(),
            StatusCode::BAD_REQUEST,
            "a missing field must not surface as 422, which the contract does not document"
        );
        assert_eq!(err.code(), "invalid_request");
    }

    #[tokio::test]
    async fn a_wrong_content_type_is_a_415() {
        let err = extract(r#"{"name":"ok"}"#, Some("text/plain"))
            .await
            .expect_err("must reject");
        assert_eq!(err.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(err.code(), "unsupported_media_type");
    }

    #[tokio::test]
    async fn a_missing_content_type_is_a_415() {
        let err = extract(r#"{"name":"ok"}"#, None)
            .await
            .expect_err("must reject");
        assert_eq!(err.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[derive(Debug, Deserialize)]
    struct Params {
        #[allow(dead_code)]
        limit: i64,
    }

    async fn extract_query(uri: &'static str) -> Result<Params, ApiError> {
        let request = Request::builder()
            .uri(uri)
            .body(Body::empty())
            .expect("request");
        let (mut parts, _) = request.into_parts();
        ApiQuery::<Params>::from_request_parts(&mut parts, &())
            .await
            .map(|ApiQuery(params)| params)
    }

    #[tokio::test]
    async fn a_valid_query_string_is_accepted() {
        let params = extract_query("/?limit=10").await.expect("valid");
        assert_eq!(params.limit, 10);
    }

    #[tokio::test]
    async fn a_malformed_query_string_is_a_400_in_the_documented_shape() {
        // `?limit=abc` used to reach the client as axum's plain-text rejection.
        let err = extract_query("/?limit=abc").await.expect_err("must reject");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert_eq!(err.code(), "invalid_request");
    }
}
