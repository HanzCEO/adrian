//! The background embed worker.
//!
//! Chunks are enqueued inside the same transaction that creates them, so a crash between
//! the two cannot lose work. The worker claims jobs atomically, embeds them in batches, and
//! writes one Qdrant point per chunk. A job that fails repeatedly is parked rather than
//! retried forever, and `adrian check` reports it.

use crate::embedder::EmbedderClient;
use crate::qdrant::{ChunkPayload, ChunkPoint, QdrantClient};
use crate::store::{EmbedJob, Store, StoreError, MAX_EMBED_ATTEMPTS};
use crate::types::EmbedRole;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::watch;

/// A failure in the worker loop.
#[derive(Debug, Error)]
pub enum WorkerError {
    #[error(transparent)]
    Store(#[from] StoreError),

    #[error(transparent)]
    Embedder(#[from] crate::embedder::EmbedderError),

    #[error(transparent)]
    Qdrant(#[from] crate::qdrant::QdrantError),
}

/// How many jobs to claim per pass.
const CLAIM_BATCH: i64 = 16;

/// How long to wait when there is nothing to do.
const IDLE_SLEEP: std::time::Duration = std::time::Duration::from_millis(250);

/// Everything the worker needs.
#[derive(Clone)]
pub struct WorkerConfig {
    pub collection: String,
    pub embed_model: String,
    pub embed_version: i64,
    /// Chunking rules version, recorded in the payload so stale points are identifiable.
    pub chunk_version: i64,
}

/// Run the worker until `shutdown` flips to true.
pub async fn run(
    store: Store,
    embedder: EmbedderClient,
    qdrant: QdrantClient,
    config: WorkerConfig,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), WorkerError> {
    tracing::info!(
        collection = %config.collection,
        model = %config.embed_model,
        "embed worker started"
    );

    loop {
        if *shutdown.borrow() {
            tracing::info!("embed worker stopping on shutdown signal");
            return Ok(());
        }

        let jobs = store.claim_embed_jobs(CLAIM_BATCH).await?;
        if jobs.is_empty() {
            tokio::select! {
                _ = tokio::time::sleep(IDLE_SLEEP) => {},
                _ = shutdown.changed() => {},
            }
            continue;
        }

        // A batch is processed to completion before the loop checks the shutdown flag, so
        // a signal never leaves a claimed job in limbo.
        process_batch(&store, &embedder, &qdrant, &config, jobs).await;
    }
}

async fn process_batch(
    store: &Store,
    embedder: &EmbedderClient,
    qdrant: &QdrantClient,
    config: &WorkerConfig,
    jobs: Vec<EmbedJob>,
) {
    // Load the chunk text for every job. A job whose chunk disappeared is dropped.
    let mut pending = Vec::new();
    for job in jobs {
        match store.get_chunk(job.chunk_id).await {
            Ok(Some(chunk)) => pending.push((job, chunk)),
            Ok(None) => {
                tracing::warn!(job = job.id, "chunk for this job no longer exists");
                let _ = store.finish_embed_job(job.id).await;
            }
            Err(err) => {
                tracing::warn!(job = job.id, error = %err, "cannot load chunk");
                let _ = store.fail_embed_job(job.id, err.to_string()).await;
            }
        }
    }

    if pending.is_empty() {
        return;
    }

    // v0.1.0 only ever embeds documents. A query vector is computed at search time and
    // never stored, because the model keeps only 64 positions on that side.
    let role = EmbedRole::Document;
    let texts: Vec<String> = pending
        .iter()
        .map(|(_, chunk)| chunk.text.clone())
        .collect();

    let response = match embedder.embed(role, &texts, true).await {
        Ok(response) => response,
        Err(err) => {
            let message = err.to_string();
            for (job, _) in &pending {
                let attempts = store.fail_embed_job(job.id, message.clone()).await;
                if let Ok(attempts) = attempts {
                    if attempts >= MAX_EMBED_ATTEMPTS {
                        tracing::error!(
                            job = job.id,
                            attempts,
                            error = %message,
                            "giving up on this chunk"
                        );
                    }
                }
            }
            return;
        }
    };

    if response.items.len() != pending.len() {
        let message = format!(
            "embedder returned {} items for {} texts",
            response.items.len(),
            pending.len()
        );
        for (job, _) in &pending {
            let _ = store.fail_embed_job(job.id, message.clone()).await;
        }
        return;
    }

    for ((job, chunk), item) in pending.into_iter().zip(response.items.into_iter()) {
        if item.truncated {
            tracing::warn!(
                chunk = %chunk.id,
                tokens = item.tokens,
                "chunk exceeded the model's token limit and was truncated"
            );
        }

        let adr = match store.get_adr(chunk.adr_id).await {
            Ok(adr) => adr,
            Err(err) => {
                let _ = store.fail_embed_job(job.id, err.to_string()).await;
                continue;
            }
        };
        let revision = match store.get_revision(chunk.adr_id, chunk.rev).await {
            Ok(revision) => revision,
            Err(err) => {
                let _ = store.fail_embed_job(job.id, err.to_string()).await;
                continue;
            }
        };
        let scopes = store.get_scopes(chunk.adr_id).await.unwrap_or_default();
        let technologies = store
            .get_technologies(chunk.adr_id)
            .await
            .unwrap_or_default();

        let Some(dense) = item.dense.clone() else {
            let _ = store
                .fail_embed_job(job.id, "embedder returned no dense vector".to_string())
                .await;
            continue;
        };

        let point_id = QdrantClient::point_id(chunk.id, &config.embed_model, config.embed_version);
        let payload = ChunkPayload {
            adr_id: chunk.adr_id.to_string(),
            project_id: adr.project_id.to_string(),
            number: adr.number,
            rev: chunk.rev,
            chunk_id: chunk.id.to_string(),
            chunk_kind: chunk.kind.as_str().to_string(),
            option_key: None,
            template: revision.template.clone(),
            status: adr.cur_status.as_str().to_string(),
            model: config.embed_model.clone(),
            model_version: config.embed_version,
            chunk_version: config.chunk_version,
            scope_prefixes: scopes
                .iter()
                .filter(|scope| scope.kind == crate::types::ScopeKind::PathPrefix)
                .map(|scope| scope.value.clone())
                .collect(),
            technologies: technologies
                .iter()
                .map(|technology| technology.canonical.clone())
                .collect(),
        };

        let point = ChunkPoint {
            id: point_id,
            payload,
            token_vectors: item.vectors.clone(),
            dense,
        };

        match qdrant.upsert_chunk(&config.collection, point).await {
            Ok(()) => {
                if let Err(err) = store
                    .record_chunk_vector(
                        chunk.id,
                        config.embed_model.clone(),
                        config.embed_version,
                        config.collection.clone(),
                        point_id,
                        item.vectors.len() as i64,
                        true,
                    )
                    .await
                {
                    tracing::warn!(chunk = %chunk.id, error = %err, "cannot record the vector row");
                }
                if let Err(err) = store.finish_embed_job(job.id).await {
                    tracing::warn!(job = job.id, error = %err, "cannot finish the job");
                }
            }
            Err(err) => {
                let message = err.to_string();
                let _ = store.fail_embed_job(job.id, message.clone()).await;
                tracing::warn!(chunk = %chunk.id, error = %message, "indexing failed");
            }
        }
    }
}

/// Spawn the worker on the current runtime.
pub fn spawn(
    store: Store,
    embedder: EmbedderClient,
    qdrant: QdrantClient,
    config: WorkerConfig,
    shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(err) = run(store, embedder, qdrant, config, shutdown).await {
            tracing::error!("embed worker stopped: {err}");
        }
    })
}

/// Shared so that the binary and any future supervisor agree on the handle type.
pub type WorkerHandle = Arc<tokio::task::JoinHandle<()>>;
