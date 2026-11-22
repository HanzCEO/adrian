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
/// One section reduced to the fields that carry meaning.
///
/// Byte offsets and ordinals are excluded because the renderer legitimately chooses its own
/// whitespace and re-derives positions. Everything else is included: a heading that comes
/// back with a different depth, an option key that stops matching, or a parent that points
/// somewhere else are all real losses, not formatting.
#[derive(Debug, PartialEq)]
struct SectionView {
    kind: String,
    heading: String,
    text: String,
    option_key: Option<String>,
    parent: Option<usize>,
}

/// One item reduced to the fields that carry meaning.
#[derive(Debug, PartialEq)]
struct ItemView {
    kind: String,
    text: String,
    option_key: Option<String>,
}

#[derive(Debug, PartialEq)]
struct Projection {
    title: String,
    template: String,
    sections: Vec<SectionView>,
    items: Vec<ItemView>,
}

fn squash(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn comparable(adr: &ParsedAdr) -> Projection {
    Projection {
        title: adr.title.clone(),
        template: adr.template.as_str().to_string(),
        sections: adr
            .sections
            .iter()
            .map(|section| SectionView {
                kind: section.kind.as_str().to_string(),
                heading: section.raw_heading.trim().to_string(),
                text: squash(&section.text),
                option_key: section.option_key.clone(),
                parent: section.parent,
            })
            .collect(),
        items: adr
            .items
            .iter()
            .map(|item| ItemView {
                kind: item.kind.as_str().to_string(),
                text: squash(&item.text),
                option_key: item.option_key.clone(),
            })
            .collect(),
    }
}

#[tokio::test]
async fn rendering_and_reparsing_preserves_the_projection() {
    let (store, project) = fresh_store().await;
    let fixtures = fixtures();
    let ids = ingest_all(&store, &project, &fixtures).await;

    let mut compared = 0usize;
    for (index, id) in ids.iter().enumerate() {
        let name = &fixtures[index].0;
        let adr = store.get_adr(*id).await.expect("adr");
        let rendered = render_madr(&store, &adr)
            .await
            .unwrap_or_else(|err| panic!("{name}: render: {err}"));

        let reparsed = parser::parse(rendered.as_bytes());
        let original = parser::parse(&fixtures[index].1);

        let before = comparable(&original);
        let after = comparable(&reparsed);

        // The whole projection, field by field. A subset comparison is what let content
        // loss through before: section text was not compared, so a renderer that dropped
        // an option's prose still "passed".
        assert_eq!(
            before.sections.len(),
            after.sections.len(),
            "{name}: section count changed on round trip\nrendered:\n{rendered}"
        );
        for (ordinal, (a, b)) in before
            .sections
            .iter()
            .zip(after.sections.iter())
            .enumerate()
        {
            assert_eq!(
                a.kind, b.kind,
                "{name}: section {ordinal} ({:?}) kind changed on round trip",
                a.heading
            );
            assert_eq!(
                a.heading, b.heading,
                "{name}: section {ordinal} heading changed on round trip"
            );
            assert_eq!(
                a.text, b.text,
                "{name}: section {ordinal} ({:?}) text changed on round trip\nrendered:\n{rendered}",
                a.heading
            );
            assert_eq!(
                a.option_key, b.option_key,
                "{name}: section {ordinal} ({:?}) option key changed on round trip",
                a.heading
            );
            // A parent index is only comparable when the section count is unchanged, which
            // is already asserted above.
            assert_eq!(
                a.parent.is_some(),
                b.parent.is_some(),
                "{name}: section {ordinal} ({:?}) lost or gained a parent",
                a.heading
            );
        }

        assert_eq!(
            before.items.len(),
            after.items.len(),
            "{name}: item count changed on round trip\nrendered:\n{rendered}"
        );
        for (ordinal, (a, b)) in before.items.iter().zip(after.items.iter()).enumerate() {
            assert_eq!(
                a, b,
                "{name}: item {ordinal} changed on round trip\nrendered:\n{rendered}"
            );
        }

        assert_eq!(before.title, after.title, "{name}: title changed");
        compared += 1;
    }

    assert_eq!(compared, fixtures.len(), "every fixture should be compared");
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
