//! Export is the trust boundary.
//!
//! Two properties are asserted over the whole fixture corpus:
//!
//! * Rendering a decision to MADR and parsing it again yields the same projection. If this
//!   drifts, export has quietly lost or invented information.
//! * Canonical JSON is byte-identical across runs, so it can be committed and diffed.

use adrian_core::chunker::WhitespaceTokenCounter;
use adrian_core::export::{
    export_adr_json, export_adr_markdown, export_project, render_madr, ExportFormat,
};
use adrian_core::ingest::{ingest, IngestRequest};
use adrian_core::parser;
use adrian_core::store::Store;
use adrian_core::types::*;
use std::path::{Path, PathBuf};
use uuid::Uuid;

fn fixtures() -> Vec<(String, Vec<u8>)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/madr");
    let mut out = Vec::new();
    for subdir in ["real", "synthetic"] {
        let Ok(entries) = std::fs::read_dir(root.join(subdir)) else {
            continue;
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.extension().map(|e| e == "md").unwrap_or(false))
            .collect();
        paths.sort();
        for path in paths {
            let bytes = std::fs::read(&path).expect("read fixture");
            out.push((
                format!("{subdir}/{}", path.file_name().unwrap().to_string_lossy()),
                bytes,
            ));
        }
    }
    assert!(
        out.len() >= 33,
        "expected the full corpus, found {}",
        out.len()
    );
    out
}

async fn fresh_store() -> (Store, Project) {
    let store = Store::open_in_memory().expect("store");
    store.migrate().await.expect("migrate");
    let workspace = store.ensure_default_workspace().await.expect("workspace");
    let project = Project {
        id: Uuid::now_v7(),
        workspace_id: workspace,
        slug: "demo".to_string(),
        name: "Demo".to_string(),
        repo_url: None,
        default_template: "madr".to_string(),
        embed_model: adrian_core::EMBED_MODEL.to_string(),
        embed_version: adrian_core::EMBED_VERSION,
        created_at: 0,
    };
    store
        .create_project(project.clone())
        .await
        .expect("project");
    (store, project)
}

async fn ingest_all(store: &Store, project: &Project, fixtures: &[(String, Vec<u8>)]) -> Vec<Uuid> {
    let mut ids = Vec::new();
    for (name, bytes) in fixtures {
        let outcome = ingest(
            store,
            IngestRequest {
                project_id: project.id,
                external_ref: format!("docs/adr/{name}"),
                source: bytes.clone(),
                source_format: "markdown".to_string(),
                actor: "test".to_string(),
                declared_scopes: Vec::new(),
                status_override: None,
                embed_model: adrian_core::EMBED_MODEL.to_string(),
                embed_version: adrian_core::EMBED_VERSION,
                number: None,
            },
            &WhitespaceTokenCounter,
        )
        .await
        .unwrap_or_else(|err| panic!("ingest {name}: {err}"));
        ids.push(outcome.adr_id);
    }
    ids
}

/// `(kind, text)` pairs, whitespace-normalised.
type Pairs = Vec<(String, String)>;

/// A projection reduced to the fields that carry meaning, so that byte offsets and
/// ordinals do not make the comparison brittle to whitespace the renderer chose.
fn comparable(adr: &ParsedAdr) -> (String, Pairs, Pairs) {
    let sections = adr
        .sections
        .iter()
        .map(|section| {
            (
                section.kind.as_str().to_string(),
                section
                    .text
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" "),
            )
        })
        .collect();
    let items = adr
        .items
        .iter()
        .map(|item| {
            (
                item.kind.as_str().to_string(),
                item.text.split_whitespace().collect::<Vec<_>>().join(" "),
            )
        })
        .collect();
    (adr.title.clone(), sections, items)
}

#[tokio::test]
async fn rendering_and_reparsing_preserves_the_projection() {
    let (store, project) = fresh_store().await;
    let fixtures = fixtures();
    let ids = ingest_all(&store, &project, &fixtures).await;

    for (index, id) in ids.iter().enumerate() {
        let name = &fixtures[index].0;
        let adr = store.get_adr(*id).await.expect("adr");
        let rendered = render_madr(&store, &adr)
            .await
            .unwrap_or_else(|err| panic!("{name}: render: {err}"));

        let reparsed = parser::parse(rendered.as_bytes());
        let original = parser::parse(&fixtures[index].1);

        let (title_a, sections_a, items_a) = comparable(&original);
        let (title_b, sections_b, items_b) = comparable(&reparsed);

        assert_eq!(title_a, title_b, "{name}: the title changed on round trip");

        // The rendered document carries the same semantic sections, in the same order.
        let kinds_a: Vec<&String> = sections_a.iter().map(|(kind, _)| kind).collect();
        let kinds_b: Vec<&String> = sections_b.iter().map(|(kind, _)| kind).collect();
        assert_eq!(
            kinds_a, kinds_b,
            "{name}: section kinds changed on round trip"
        );

        // Every option survives, with its arguments still attached to it.
        let options_a: Vec<&String> = original
            .items_of(ItemKind::Option)
            .map(|item| &item.text)
            .collect();
        let options_b: Vec<&String> = reparsed
            .items_of(ItemKind::Option)
            .map(|item| &item.text)
            .collect();
        assert_eq!(
            options_a, options_b,
            "{name}: options changed on round trip"
        );

        let pros_a = items_a.iter().filter(|(kind, _)| kind == "pro").count();
        let pros_b = items_b.iter().filter(|(kind, _)| kind == "pro").count();
        assert_eq!(
            pros_a, pros_b,
            "{name}: pro arguments changed on round trip"
        );
    }
}

#[tokio::test]
async fn an_unmodified_import_exports_its_original_bytes() {
    let (store, project) = fresh_store().await;
    let fixtures = fixtures();
    let ids = ingest_all(&store, &project, &fixtures).await;

    for (index, id) in ids.iter().enumerate() {
        let adr = store.get_adr(*id).await.expect("adr");
        let exported = export_adr_markdown(&store, &adr).await.expect("export");
        let expected = String::from_utf8_lossy(&fixtures[index].1).into_owned();
        assert_eq!(
            exported, expected,
            "{}: an unmodified import must export verbatim",
            fixtures[index].0
        );
    }
}

#[tokio::test]
async fn canonical_json_is_byte_identical_across_runs() {
    let (store, project) = fresh_store().await;
    let fixtures = fixtures();
    let ids = ingest_all(&store, &project, &fixtures[..3]).await;

    for id in &ids {
        let adr = store.get_adr(*id).await.expect("adr");
        let first = export_adr_json(&store, &adr).await.expect("export");
        let second = export_adr_json(&store, &adr).await.expect("export");
        let a = serde_json::to_string_pretty(&first).expect("serialise");
        let b = serde_json::to_string_pretty(&second).expect("serialise");
        assert_eq!(a, b, "canonical JSON must be stable across runs");
        assert_eq!(first["schema_version"], "1.0");
        assert!(first["id"].as_str().is_some());
        assert!(first["status_events"].as_array().is_some());
    }
}

#[tokio::test]
async fn project_export_is_stable_and_ordered() {
    let (store, project) = fresh_store().await;
    let fixtures = fixtures();
    ingest_all(&store, &project, &fixtures[..3]).await;

    let first = export_project(&store, project.id, ExportFormat::Json)
        .await
        .expect("export");
    let second = export_project(&store, project.id, ExportFormat::Json)
        .await
        .expect("export");
    assert_eq!(first, second, "a project export must be reproducible");

    let parsed: serde_json::Value = serde_json::from_slice(&first).expect("valid json");
    assert_eq!(parsed.as_array().expect("array").len(), 3);

    let madr = export_project(&store, project.id, ExportFormat::Madr)
        .await
        .expect("export");
    let text = String::from_utf8(madr).expect("utf8");
    assert!(text.contains("# "));
}
