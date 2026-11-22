//! Revising a decision replaces its index entries rather than adding to them.
//!
//! The audit never reached task-8, so this is the end-to-end statement of its criteria:
//! same bytes are a no-op, changed bytes are a new revision that closes the old one, and
//! only the new revision's chunks are queued for embedding.

mod common;

use common::{Harness, SAMPLE_ADR};

#[tokio::test]
async fn the_same_bytes_are_a_no_op() {
    let harness = Harness::new(None).await;
    harness.create_project("demo").await;

    let (status, first) = harness.create_adr("demo", SAMPLE_ADR).await;
    assert_eq!(status, 201, "body: {first}");
    assert_eq!(first["action"], "created");
    assert_eq!(first["rev"], 1);

    let (status, second) = harness.create_adr("demo", SAMPLE_ADR).await;
    assert_eq!(status, 201);
    assert_eq!(
        second["action"], "unchanged",
        "identical bytes must not create a revision"
    );
    assert_eq!(second["rev"], 1);
    assert_eq!(second["id"], first["id"]);
}

#[tokio::test]
async fn changed_bytes_create_a_revision_and_close_the_previous_one() {
    let harness = Harness::new(None).await;
    harness.create_project("demo").await;

    let (_, first) = harness.create_adr("demo", SAMPLE_ADR).await;
    let id = first["id"].as_str().expect("id").to_string();

    let revised_source = SAMPLE_ADR.replace("tokio", "async-std");
    let (status, second) = harness.create_adr("demo", &revised_source).await;
    assert_eq!(status, 201, "body: {second}");
    assert_eq!(second["action"], "revised");
    assert_eq!(second["rev"], 2);
    assert_eq!(second["id"], id, "a revision keeps the decision's identity");
    assert_eq!(
        second["status"], "accepted",
        "a revision keeps its status, so the response must report the status it kept"
    );

    // The old revision is closed but still readable, which is the whole point of the
    // append-only model.
    let (status, old) = harness
        .send("GET", &format!("/v1/adrs/{id}?rev=1"), None, None)
        .await;
    assert_eq!(status, 200, "revision 1 must remain readable: {old}");
    assert!(
        old["title"].as_str().unwrap_or_default().contains("tokio"),
        "revision 1 should still say tokio, got {}",
        old["title"]
    );

    let (_, current) = harness
        .send("GET", &format!("/v1/adrs/{id}"), None, None)
        .await;
    assert_eq!(current["rev"], 2);
    assert!(
        current["title"]
            .as_str()
            .unwrap_or_default()
            .contains("async-std"),
        "the current revision should carry the change, got {}",
        current["title"]
    );

    // And the status history recorded the initial state once, not once per revision.
    let events = current["status_events"].as_array().expect("events");
    assert_eq!(events.len(), 1, "a revision is not a status change");
}

#[tokio::test]
async fn only_the_current_revision_is_queued_for_embedding() {
    let harness = Harness::new(None).await;
    harness.create_project("demo").await;

    let (_, first) = harness.create_adr("demo", SAMPLE_ADR).await;
    let id = first["id"].as_str().expect("id").to_string();
    let adr = harness
        .state
        .store
        .get_adr(uuid::Uuid::parse_str(&id).expect("uuid"))
        .await
        .expect("adr");

    // Drain revision 1 so the queue is empty before the revision happens.
    let jobs = harness
        .state
        .store
        .claim_embed_jobs(1000)
        .await
        .expect("claim");
    assert!(!jobs.is_empty());
    for job in &jobs {
        harness
            .state
            .store
            .finish_embed_job(job.id)
            .await
            .expect("finish");
    }

    let revised_source = SAMPLE_ADR.replace("tokio", "async-std");
    let (status, second) = harness.create_adr("demo", &revised_source).await;
    assert_eq!(status, 201);
    assert_eq!(second["rev"], 2);

    let queued = harness
        .state
        .store
        .claim_embed_jobs(1000)
        .await
        .expect("claim");
    let rev1: std::collections::BTreeSet<_> = harness
        .state
        .store
        .list_chunks(adr.id, 1)
        .await
        .expect("rev 1 chunks")
        .into_iter()
        .map(|chunk| chunk.id)
        .collect();
    let rev2: std::collections::BTreeSet<_> = harness
        .state
        .store
        .list_chunks(adr.id, 2)
        .await
        .expect("rev 2 chunks")
        .into_iter()
        .map(|chunk| chunk.id)
        .collect();

    assert!(!queued.is_empty(), "the new revision should be queued");
    for job in &queued {
        assert!(
            !rev1.contains(&job.chunk_id),
            "a superseded chunk must not be re-embedded"
        );
        assert!(
            rev2.contains(&job.chunk_id),
            "only the new revision's chunks are queued"
        );
    }
    assert_eq!(
        queued.len(),
        rev2.len(),
        "every new chunk gets exactly one job"
    );
}
