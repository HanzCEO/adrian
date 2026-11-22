//! Parity against the reference embeddings produced by the model's own Python.
//!
//! The thresholds are deliberately tight. A late-interaction model that is off by a
//! little still returns plausible rankings, so "close enough" is not a useful signal:
//! either the arithmetic matches the reference or the port has a bug.

use adrian_embedder::model::{Engine, Role};
use adrian_embedder::weights::ModelFiles;
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
struct Case {
    name: String,
    role: String,
    text: String,
    input_ids: Vec<u32>,
    keep_positions: Vec<usize>,
    token_vectors: Vec<Vec<f32>>,
    dense: Vec<f32>,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn weights_dir() -> PathBuf {
    match std::env::var("ADRIAN_WEIGHTS_DIR") {
        Ok(value) => PathBuf::from(value),
        Err(_) => Path::new(env!("CARGO_MANIFEST_DIR")).join("weights"),
    }
}

fn load_engine() -> Option<Engine> {
    let dir = weights_dir();
    match ModelFiles::discover(&dir) {
        Ok(files) => {
            let device = adrian_embedder::select_device(false).expect("device");
            Some(Engine::load(&files, device).expect("load model"))
        }
        Err(err) => {
            if std::env::var("ADRIAN_SKIP_PARITY").is_ok() {
                eprintln!(
                    "SKIPPING PARITY TEST: ADRIAN_SKIP_PARITY is set and the model is not \
                     available at {} ({err:#})",
                    dir.display()
                );
                None
            } else {
                panic!(
                    "cannot load the model from {}: {err:#}\n\
                     Run scripts/fetch_weights.sh, or set ADRIAN_SKIP_PARITY=1 to skip \
                     deliberately.",
                    dir.display()
                );
            }
        }
    }
}

fn cases() -> Vec<Case> {
    let path = repo_root().join("fixtures/golden/cases.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
    serde_json::from_str(&text).expect("parse cases.json")
}

fn role_of(case: &Case) -> Role {
    match case.role.as_str() {
        "query" => Role::Query,
        "document" => Role::Document,
        other => panic!("unknown role {other:?}"),
    }
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na * nb)
}

#[test]
fn tokenization_and_surviving_positions_match_the_oracle() {
    let Some(engine) = load_engine() else { return };
    for case in cases() {
        let role = role_of(&case);
        let (ids, keep, truncated) = engine
            .debug_encode(&case.text, role)
            .unwrap_or_else(|err| panic!("{}: {err:#}", case.name));

        assert_eq!(
            ids, case.input_ids,
            "{}: token ids differ from the oracle",
            case.name
        );
        assert_eq!(
            keep, case.keep_positions,
            "{}: surviving positions differ from the oracle",
            case.name
        );
        assert!(!truncated, "{}: unexpectedly truncated", case.name);
    }
}

#[test]
fn vectors_match_the_oracle() {
    let Some(engine) = load_engine() else { return };
    for case in cases() {
        let role = role_of(&case);
        let embedding = engine
            .embed(&case.text, role)
            .unwrap_or_else(|err| panic!("{}: {err:#}", case.name));

        assert_eq!(
            embedding.vectors.len(),
            case.token_vectors.len(),
            "{}: wrong number of vectors",
            case.name
        );

        for (index, (got, want)) in embedding
            .vectors
            .iter()
            .zip(case.token_vectors.iter())
            .enumerate()
        {
            assert_eq!(got.len(), 128, "{}[{}]: wrong width", case.name, index);
            let similarity = cosine(got, want);
            assert!(
                similarity >= 0.9999,
                "{}[{}]: cosine {similarity} is below 0.9999",
                case.name,
                index
            );
            let max_diff = got
                .iter()
                .zip(want.iter())
                .map(|(a, b)| (a - b).abs())
                .fold(0.0f32, f32::max);
            assert!(
                max_diff <= 1e-3,
                "{}[{}]: max absolute difference {max_diff} exceeds 1e-3",
                case.name,
                index
            );
        }

        assert_eq!(embedding.dense.len(), 768, "{}: dense width", case.name);
        let dense_similarity = cosine(&embedding.dense, &case.dense);
        assert!(
            dense_similarity >= 0.9999,
            "{}: dense cosine {dense_similarity} is below 0.9999",
            case.name
        );
    }
}

#[test]
fn every_vector_is_unit_norm() {
    let Some(engine) = load_engine() else { return };
    for case in cases() {
        let embedding = engine.embed(&case.text, role_of(&case)).expect("embed");
        for (index, vector) in embedding.vectors.iter().enumerate() {
            let norm: f32 = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
            assert!(
                (norm - 1.0).abs() < 1e-4,
                "{}[{}]: norm {norm} is not 1",
                case.name,
                index
            );
        }
        let norm: f32 = embedding.dense.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-4,
            "{}: dense norm {norm} is not 1",
            case.name
        );
    }
}

#[test]
fn query_role_is_capped_and_document_role_is_not() {
    let Some(engine) = load_engine() else { return };
    let long = cases()
        .into_iter()
        .find(|case| case.name == "query_over_local_cap")
        .expect("the long query case must exist");

    assert!(
        long.input_ids.len() > 64,
        "the fixture must exceed the cap to be meaningful"
    );

    let as_query = engine.embed(&long.text, Role::Query).expect("embed query");
    assert_eq!(
        as_query.vectors.len(),
        64,
        "a query must keep exactly the first 64 vectors"
    );

    let as_document = engine
        .embed(&long.text, Role::Document)
        .expect("embed document");
    assert!(
        as_document.vectors.len() > 64,
        "a document must keep every position, got {}",
        as_document.vectors.len()
    );
}

#[test]
fn slot_positions_carry_the_negative_tag() {
    let Some(engine) = load_engine() else { return };
    let embedding = engine.embed("hello world", Role::Document).expect("embed");

    let slot_tag = -((1.0f32 - 0.225).sqrt());
    let token_tag = (0.1f32).sqrt();

    for (index, vector) in embedding.vectors.iter().enumerate() {
        if index < 3 {
            assert!(
                (vector[0] - slot_tag).abs() < 1e-4,
                "position {index} should be a slot, dim 0 was {}",
                vector[0]
            );
        } else {
            assert!(
                (vector[0] - token_tag).abs() < 1e-4,
                "position {index} should be a token, dim 0 was {}",
                vector[0]
            );
        }
    }
}
