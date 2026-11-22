//! `adrian import`.
//!
//! The import is the bootstrap path for an existing corpus, so what matters is that it
//! honours the documents rather than the caller: front-matter statuses survive, a second
//! run is a no-op, and a dry run writes nothing.

use std::path::PathBuf;
use std::process::{Command, Stdio};

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_adrian")
}

/// A temp directory that cleans up after itself.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> TempDir {
        let path = std::env::temp_dir().join(format!("adrian-import-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&path).expect("temp dir");
        TempDir(path)
    }

    fn db(&self) -> PathBuf {
        self.0.join("adrian.db")
    }

    fn corpus(&self) -> PathBuf {
        self.0.join("corpus")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(dir: &TempDir, args: &[&str]) -> (i32, String) {
    let output = Command::new(binary())
        .args(args)
        .current_dir(&dir.0)
        .env("ADRIAN_DB_PATH", dir.db())
        .env("ADRIAN_QDRANT_URL", "http://127.0.0.1:1")
        .env("ADRIAN_LOG", "warn")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run adrian");
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status.code().unwrap_or(-1), text)
}

const ACCEPTED: &str = "---\nstatus: accepted\ndate: 2024-03-01\n---\n\n# Use tokio\n\n## Context and Problem Statement\n\nWe need an async runtime.\n\n## Considered Options\n\n* tokio\n* async-std\n\n## Decision Outcome\n\nChosen option: \"tokio\", because it is the de-facto standard.\n";

const PROPOSED: &str = "---\nstatus: proposed\n---\n\n# Adopt conventional commits\n\n## Context and Problem Statement\n\nThe log is unreadable.\n";

const UNKNOWN: &str = "---\nstatus: banana\n---\n\n# Something undecided\n\n## Context and Problem Statement\n\nNobody wrote a status anyone recognises.\n";

fn write_corpus(dir: &TempDir) {
    let corpus = dir.corpus();
    std::fs::create_dir_all(&corpus).expect("corpus dir");
    std::fs::write(corpus.join("0001-use-tokio.md"), ACCEPTED).expect("write");
    std::fs::write(corpus.join("0002-conventional-commits.md"), PROPOSED).expect("write");
    std::fs::write(corpus.join("0003-undecided.md"), UNKNOWN).expect("write");
    // A file that is not Markdown must be ignored rather than imported.
    std::fs::write(corpus.join("notes.txt"), "ignore me").expect("write");
}

async fn counts(dir: &TempDir) -> (i64, i64) {
    use adrian_core::store::Store;
    let store = Store::open(dir.db()).expect("store");
    store
        .read(|conn| {
            let adrs: i64 = conn.query_row("SELECT COUNT(*) FROM adr", [], |row| row.get(0))?;
            let revisions: i64 =
                conn.query_row("SELECT COUNT(*) FROM adr_rev", [], |row| row.get(0))?;
            Ok((adrs, revisions))
        })
        .await
        .expect("counts")
}

async fn status_of(dir: &TempDir, external_ref: &str) -> Option<String> {
    use adrian_core::store::Store;
    let store = Store::open(dir.db()).expect("store");
    let project = store
        .get_project_by_slug("demo".to_string())
        .await
        .expect("lookup")
        .expect("project");
    let adr = store
        .get_adr_by_external_ref(project.id, external_ref.to_string())
        .await
        .expect("lookup");
    adr.map(|adr| adr.cur_status.as_str().to_string())
}

#[tokio::test(flavor = "multi_thread")]
async fn dry_run_reports_the_corpus_and_writes_nothing() {
    let dir = TempDir::new();
    write_corpus(&dir);

    let (code, text) = run(
        &dir,
        &["project", "add", "--slug", "demo", "--name", "Demo"],
    );
    assert_eq!(code, 0, "add should succeed:\n{text}");

    let (code, text) = run(
        &dir,
        &[
            "import",
            "--project",
            "demo",
            "--dir",
            "corpus",
            "--dry-run",
        ],
    );
    assert_eq!(code, 0, "a dry run should succeed:\n{text}");
    assert!(text.contains("3 file(s) under `corpus`"), "got:\n{text}");
    assert!(
        text.contains("corpus/0001-use-tokio.md"),
        "the ref should inherit the typed prefix, got:\n{text}"
    );
    assert!(
        text.contains("#1"),
        "the display number should be shown:\n{text}"
    );
    assert!(text.contains("accepted"), "got:\n{text}");
    assert!(
        text.contains("unknown") || text.contains("UnknownStatus"),
        "an unrecognised status should be reported as a warning, got:\n{text}"
    );
    assert!(text.contains("3 new"), "got:\n{text}");
    assert!(text.contains("dry run wrote nothing"), "got:\n{text}");

    assert_eq!(
        counts(&dir).await,
        (0, 0),
        "a dry run must not write a decision"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn import_honours_front_matter_statuses_and_is_idempotent() {
    let dir = TempDir::new();
    write_corpus(&dir);

    let (code, text) = run(
        &dir,
        &["project", "add", "--slug", "demo", "--name", "Demo"],
    );
    assert_eq!(code, 0, "add should succeed:\n{text}");

    let (code, text) = run(&dir, &["import", "--project", "demo", "--dir", "corpus"]);
    assert_eq!(code, 0, "the import should succeed:\n{text}");
    assert!(text.contains("3 created"), "got:\n{text}");
    assert_eq!(counts(&dir).await, (3, 3));

    // The accepted document stays accepted. `adr_submit` would have forced `proposed`.
    assert_eq!(
        status_of(&dir, "corpus/0001-use-tokio.md").await.as_deref(),
        Some("accepted")
    );
    assert_eq!(
        status_of(&dir, "corpus/0002-conventional-commits.md")
            .await
            .as_deref(),
        Some("proposed")
    );
    // An unrecognised status falls back to `proposed` rather than guessing.
    assert_eq!(
        status_of(&dir, "corpus/0003-undecided.md").await.as_deref(),
        Some("proposed")
    );

    let (code, text) = run(&dir, &["import", "--project", "demo", "--dir", "corpus"]);
    assert_eq!(code, 0, "a second import should succeed:\n{text}");
    assert!(text.contains("3 unchanged"), "got:\n{text}");
    assert!(
        text.contains("0 created"),
        "a re-run must not create anything, got:\n{text}"
    );
    assert_eq!(
        counts(&dir).await,
        (3, 3),
        "a re-run must not add a revision"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn import_reports_colliding_display_numbers() {
    let dir = TempDir::new();
    let corpus = dir.corpus();
    std::fs::create_dir_all(&corpus).expect("corpus dir");
    // Two different files claiming the same leading number.
    std::fs::write(corpus.join("0072-first.md"), ACCEPTED).expect("write");
    std::fs::write(corpus.join("0072-second.md"), PROPOSED).expect("write");

    let (code, text) = run(
        &dir,
        &["project", "add", "--slug", "demo", "--name", "Demo"],
    );
    assert_eq!(code, 0, "add should succeed:\n{text}");

    let (code, text) = run(
        &dir,
        &[
            "import",
            "--project",
            "demo",
            "--dir",
            "corpus",
            "--dry-run",
        ],
    );
    assert_eq!(code, 0, "a dry run should succeed:\n{text}");
    assert!(
        text.contains("2 files share display number #72"),
        "the collision should be named, got:\n{text}"
    );
    assert!(
        text.contains("display-only"),
        "the warning should say the number is not identity, got:\n{text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn import_requires_an_existing_project_and_a_real_directory() {
    let dir = TempDir::new();
    write_corpus(&dir);

    let (code, text) = run(&dir, &["import", "--project", "demo", "--dir", "corpus"]);
    assert_ne!(code, 0, "a missing project must fail:\n{text}");
    assert!(
        text.contains("adrian project add"),
        "the failure should name the fix, got:\n{text}"
    );

    let (code, text) = run(
        &dir,
        &["project", "add", "--slug", "demo", "--name", "Demo"],
    );
    assert_eq!(code, 0, "add should succeed:\n{text}");

    let (code, text) = run(&dir, &["import", "--project", "demo", "--dir", "nowhere"]);
    assert_ne!(code, 0, "a missing directory must fail:\n{text}");
    assert!(
        text.contains("is not a directory"),
        "the failure should be clear, got:\n{text}"
    );
}
