//! Qdrant integration: the collection shape and the retrieval path.
//!
//! Gated on `ADRIAN_QDRANT_URL` because it needs a live instance. It skips loudly rather
//! than silently passing, so a run without Qdrant is obviously not a run with it.

use adrian_core::qdrant::{ChunkPayload, ChunkPoint, QdrantClient};
use uuid::Uuid;

fn client() -> Option<(QdrantClient, String)> {
    let Ok(url) = std::env::var("ADRIAN_QDRANT_URL") else {
        eprintln!("SKIPPING QDRANT TEST: ADRIAN_QDRANT_URL is not set");
        return None;
    };
    let collection = format!("adrian_test_{}", Uuid::now_v7().simple());
    Some((QdrantClient::new(url).expect("client"), collection))
}

fn payload(adr_id: Uuid, project_id: Uuid, chunk_id: Uuid) -> ChunkPayload {
    ChunkPayload {
        adr_id: adr_id.to_string(),
        project_id: project_id.to_string(),
        number: Some(1),
        rev: 1,
        chunk_id: chunk_id.to_string(),
        chunk_kind: "option".to_string(),
        option_key: Some("opt:tokio".to_string()),
        template: "madr".to_string(),
        status: "accepted".to_string(),
        model: adrian_core::EMBED_MODEL.to_string(),
        model_version: adrian_core::EMBED_VERSION,
        chunk_version: adrian_core::CHUNK_VERSION,
        scope_prefixes: vec!["crates/api".to_string()],
        technologies: vec!["tokio".to_string()],
    }
}

/// A deterministic unit-ish vector of the requested width.
fn vector(seed: usize, width: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(width);
    for index in 0..width {
        let value = (((seed * 31 + index * 17) % 97) as f32 / 97.0) - 0.5;
        out.push(value);
    }
    let norm: f32 = out.iter().map(|x| x * x).sum::<f32>().sqrt();
    out.iter().map(|x| x / norm).collect()
}

#[tokio::test]
async fn collection_has_both_named_vectors_and_the_expected_comparator() {
    let Some((client, collection)) = client() else {
        return;
    };
    client.ensure_collection(&collection).await.expect("create");

    let url = format!("{}/collections/{}", client.base_url(), collection);
    let value: serde_json::Value = reqwest::get(&url)
        .await
        .expect("get collection")
        .json()
        .await
        .expect("parse collection");

    let vectors = &value["result"]["config"]["params"]["vectors"];
    assert_eq!(vectors["colbert"]["size"], 128);
    assert_eq!(
        vectors["colbert"]["multivector_config"]["comparator"],
        "max_sim"
    );
    assert_eq!(vectors["dense"]["size"], 768);
    assert_eq!(value["result"]["status"], "green");

    client
        .delete_adr_points(&collection, Uuid::nil())
        .await
        .expect("delete");
    reqwest::Client::new()
        .delete(format!("{}/collections/{}", client.base_url(), collection))
        .send()
        .await
        .expect("drop collection");
}

#[tokio::test]
async fn two_stage_search_returns_the_closer_chunk_first() {
    let Some((client, collection)) = client() else {
        return;
    };
    client.ensure_collection(&collection).await.expect("create");

    let project = Uuid::now_v7();
    let adr = Uuid::now_v7();
    let chunk_a = Uuid::now_v7();
    let chunk_b = Uuid::now_v7();

    let base = vector(1, 128);
    let far = vector(2, 128);

    client
        .upsert_chunks(
            &collection,
            vec![
                ChunkPoint {
                    id: QdrantClient::point_id(chunk_a, "m", 1),
                    payload: payload(adr, project, chunk_a),
                    token_vectors: vec![base.clone(), base.clone(), base.clone(), base.clone()],
                    dense: vector(1, 768),
                },
                ChunkPoint {
                    id: QdrantClient::point_id(chunk_b, "m", 1),
                    payload: payload(adr, project, chunk_b),
                    token_vectors: vec![far.clone(), far.clone(), far.clone(), far.clone()],
                    dense: vector(9, 768),
                },
            ],
        )
        .await
        .expect("upsert");

    // Qdrant's point ids are the deterministic ones, so the search must come back with
    // those rather than with the chunk ids directly.
    let hits = client
        .search(
            &collection,
            project,
            vec![base.clone(), base.clone()],
            vector(1, 768),
            5,
        )
        .await
        .expect("search");

    assert!(!hits.is_empty(), "search returned nothing");
    assert_eq!(
        hits[0].chunk_id, chunk_a,
        "the closer chunk should rank first"
    );
    assert!(hits[0].score >= hits.last().expect("last").score);

    // A different project must see nothing.
    let other = client
        .search(&collection, Uuid::now_v7(), vec![base], vector(1, 768), 5)
        .await
        .expect("search");
    assert!(other.is_empty(), "project isolation failed");

    reqwest::Client::new()
        .delete(format!("{}/collections/{}", client.base_url(), collection))
        .send()
        .await
        .expect("drop collection");
}
