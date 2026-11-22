//! The punctuation skip list must match the one the oracle used, or document vectors are
//! filtered differently on the two sides and parity fails for a reason that looks like a
//! numerical bug.

use adrian_embedder::model::Engine;
use adrian_embedder::weights::ModelFiles;
use std::path::Path;

#[test]
fn skiplist_matches_the_oracle_manifest() {
    let manifest_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/golden/manifest.json");
    let text = std::fs::read_to_string(&manifest_path).expect("read manifest");
    let manifest: serde_json::Value = serde_json::from_str(&text).expect("parse manifest");
    let expected: Vec<u32> = manifest["skiplist_token_ids"]
        .as_array()
        .expect("skiplist_token_ids")
        .iter()
        .map(|v| v.as_u64().expect("id") as u32)
        .collect();

    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("weights");
    let files = ModelFiles::discover(&dir).expect("weights");
    let device = adrian_embedder::select_device(false).expect("device");
    let engine = Engine::load(&files, device).expect("load");
    assert_eq!(engine.skiplist().to_vec(), expected);
}
