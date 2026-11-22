//! The engine's HTTP contract.
//!
//! `role` is a required field with no default. The model treats a query and a document
//! differently, and a caller that gets it wrong receives no error at all: the scores
//! simply degrade. Making the field mandatory is the only way to force the decision to be
//! made explicitly.

use crate::model::{ModelInfo, Role, SharedEngine};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

/// The largest batch accepted in one request.
const MAX_BATCH: usize = 256;

pub fn router(engine: SharedEngine) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/models", get(models))
        .route("/v1/embed", post(embed))
        .with_state(engine)
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
}

async fn healthz() -> Json<Health> {
    Json(Health { status: "ok" })
}

async fn models(State(engine): State<SharedEngine>) -> Json<ModelInfo> {
    Json(engine.info())
}

#[derive(Debug, Deserialize)]
struct EmbedRequest {
    /// `document` or `query`. Required.
    role: Option<String>,
    texts: Option<Vec<String>>,
    #[serde(default)]
    return_dense: bool,
}

#[derive(Debug, Serialize)]
struct EmbedItem {
    tokens: usize,
    truncated: bool,
    vectors: Vec<Vec<f32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dense: Option<Vec<f32>>,
}

#[derive(Debug, Serialize)]
struct EmbedResponse {
    model: String,
    version: i64,
    dim: usize,
    dense_dim: usize,
    items: Vec<EmbedItem>,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: ErrorDetail,
}

#[derive(Debug, Serialize)]
struct ErrorDetail {
    code: &'static str,
    message: String,
}

fn error(status: StatusCode, code: &'static str, message: impl Into<String>) -> Response {
    (
        status,
        Json(ErrorBody {
            error: ErrorDetail {
                code,
                message: message.into(),
            },
        }),
    )
        .into_response()
}

async fn embed(State(engine): State<SharedEngine>, body: String) -> Response {
    let request: EmbedRequest = match serde_json::from_str(&body) {
        Ok(request) => request,
        Err(err) => {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                format!("malformed JSON body: {err}"),
            )
        }
    };

    let Some(role_text) = request.role.as_deref() else {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "`role` is required and must be \"document\" or \"query\"; it is never inferred \
             from the text, because the model embeds the two differently",
        );
    };

    let role = match role_text {
        "document" => Role::Document,
        "query" => Role::Query,
        other => {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                format!("`role` must be \"document\" or \"query\", got {other:?}"),
            )
        }
    };

    let texts = request.texts.unwrap_or_default();
    if texts.is_empty() {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unprocessable",
            "`texts` must contain at least one string",
        );
    }
    if texts.len() > MAX_BATCH {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unprocessable",
            format!(
                "`texts` may contain at most {MAX_BATCH} entries, got {}",
                texts.len()
            ),
        );
    }

    // Inference is CPU bound and blocking; keep it off the reactor.
    let token_dim = engine.token_dim();
    let dense_dim = engine.dense_dim();
    let engine = engine.clone();
    let dense_wanted = request.return_dense;
    let result = tokio::task::spawn_blocking(move || {
        let mut items = Vec::with_capacity(texts.len());
        for text in &texts {
            let embedding = engine.embed(text, role)?;
            items.push(EmbedItem {
                tokens: embedding.tokens,
                truncated: embedding.truncated,
                vectors: embedding.vectors,
                dense: if dense_wanted {
                    Some(embedding.dense)
                } else {
                    None
                },
            });
        }
        Ok::<_, anyhow::Error>(items)
    })
    .await;

    match result {
        Ok(Ok(items)) => Json(EmbedResponse {
            model: crate::model::MODEL_NAME.to_string(),
            version: crate::MODEL_VERSION,
            dim: token_dim,
            dense_dim,
            items,
        })
        .into_response(),
        Ok(Err(err)) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("{err:#}"),
        ),
        Err(err) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("inference task failed: {err}"),
        ),
    }
}
