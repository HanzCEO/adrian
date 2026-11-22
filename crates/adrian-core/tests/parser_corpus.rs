//! Corpus tests: the parser against real MADR documents.
//!
//! These are regression tests against authoring habits, not against the template. The
//! fixtures in `fixtures/madr/` are real files from public repositories plus hand-written
//! edge cases, checked in verbatim.
//!
//! Golden projections live in `fixtures/madr/expected/`. Regenerate them with:
//!
//! ```text
//! ADRIAN_UPDATE_GOLDEN=1 cargo test -p adrian-core --test parser_corpus
//! ```

use adrian_core::parser;
use adrian_core::types::{ItemKind, ParseWarning, SectionKind};
use std::path::{Path, PathBuf};

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/madr")
        .canonicalize()
        .expect("fixtures/madr must exist")
}

/// Every fixture, as (relative name, bytes), in a deterministic order.
fn all_fixtures() -> Vec<(String, Vec<u8>)> {
    let root = fixtures_root();
    let mut out = Vec::new();
    for subdir in ["real", "synthetic"] {
        let dir = root.join(subdir);
        let entries = std::fs::read_dir(&dir)
            .unwrap_or_else(|err| panic!("cannot read {}: {err}", dir.display()));
        let mut names: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.extension().map(|e| e == "md").unwrap_or(false))
            .collect();
        names.sort();
        for path in names {
            let bytes = std::fs::read(&path).expect("read fixture");
            let rel = format!("{}/{}", subdir, path.file_name().unwrap().to_string_lossy());
            out.push((rel, bytes));
        }
    }
    assert!(
        out.len() >= 33,
        "expected at least 33 fixtures, found {}",
        out.len()
    );
    out
}

#[test]
fn corpus_size_and_sources_are_documented() {
    let fixtures = all_fixtures();
    let real = fixtures
        .iter()
        .filter(|(n, _)| n.starts_with("real/"))
        .count();
    let synthetic = fixtures
        .iter()
        .filter(|(n, _)| n.starts_with("synthetic/"))
        .count();
    assert!(real >= 25, "expected at least 25 real ADRs, found {real}");
    assert!(
        synthetic >= 8,
        "expected at least 8 synthetic cases, found {synthetic}"
    );

    let sources =
        std::fs::read_to_string(fixtures_root().join("SOURCES.md")).expect("SOURCES.md must exist");
    for (name, _) in fixtures.iter().filter(|(n, _)| n.starts_with("real/")) {
        let file = name.trim_start_matches("real/");
        assert!(sources.contains(file), "SOURCES.md has no entry for {file}");
        assert!(
            sources.contains(&format!("`real/{file}`")),
            "SOURCES.md entry for {file} is not in the expected table form"
        );
    }
    for (name, _) in fixtures.iter().filter(|(n, _)| n.starts_with("synthetic/")) {
        let file = name.trim_start_matches("synthetic/");
        assert!(sources.contains(file), "SOURCES.md has no entry for {file}");
    }
}

#[test]
fn parsing_never_panics_on_the_whole_corpus() {
    for (name, bytes) in all_fixtures() {
        let adr = parser::parse(&bytes);
        // A document with an H1 must produce that H1 as its title.
        if String::from_utf8_lossy(&bytes).contains("\n# ") {
            assert!(!adr.title.is_empty(), "{name} produced no title");
        }
    }
}

#[test]
fn parsing_never_panics_on_any_truncated_prefix() {
    for (name, bytes) in all_fixtures() {
        for end in 0..=bytes.len() {
            let parsed = parser::parse(&bytes[..end]);
            // The only thing worth asserting on a truncated document is that we got a
            // value back at all; the point of the test is the absence of a panic.
            let _ = parsed.sections.len() + parsed.items.len();
        }
        // Truncation at a multi-byte boundary must also be survivable, since the source is
        // lossily decoded.
        for end in 0..=bytes.len() {
            let _ = parser::parse(&bytes[..end]);
        }
        let _ = name;
    }
}

#[test]
fn section_and_item_spans_slice_the_original_source() {
    for (name, bytes) in all_fixtures() {
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let adr = parser::parse(&bytes);
        for section in &adr.sections {
            assert!(
                section.byte_end <= text.len(),
                "{name}: section span past end of source"
            );
            assert!(
                section.byte_start <= section.byte_end,
                "{name}: inverted section span"
            );
            let slice = &text[section.byte_start..section.byte_end];
            assert!(
                slice.contains(&section.raw_heading),
                "{name}: section span does not contain its heading {:?}",
                section.raw_heading
            );
        }
        for item in &adr.items {
            assert!(
                item.byte_end <= text.len(),
                "{name}: item span past end of source"
            );
            assert!(
                item.byte_start <= item.byte_end,
                "{name}: inverted item span"
            );
            let slice = &text[item.byte_start..item.byte_end];
            let first_line = item.text.lines().next().unwrap_or("");
            assert!(
                first_line.is_empty() || slice.contains(first_line),
                "{name}: item span {slice:?} does not contain {:?}",
                first_line
            );
        }
    }
}

#[test]
fn front_matter_preserves_unknown_keys() {
    let source = br#"---
status: accepted
date: 2024-01-01
nav_order: 13
parent: Decisions
custom:
  nested: [a, b]
---
# Title
"#;
    let adr = parser::parse(source);
    assert_eq!(
        adr.extra.get("nav_order").and_then(|v| v.as_i64()),
        Some(13)
    );
    assert_eq!(
        adr.extra.get("parent").and_then(|v| v.as_str()),
        Some("Decisions")
    );
    let nested = adr
        .extra
        .get("custom")
        .and_then(|v| v.get("nested"))
        .and_then(|v| v.as_array())
        .expect("nested list preserved");
    assert_eq!(nested.len(), 2);
}

#[test]
fn front_matter_accepts_lists_and_scalars_for_the_same_key() {
    let listed = parser::parse(b"---\ndecision-makers:\n  - Alice\n  - Bob\n---\n# T\n");
    assert_eq!(listed.decision_makers, vec!["Alice", "Bob"]);

    let scalar = parser::parse(b"---\ndecision-makers: Alice, Bob\n---\n# T\n");
    assert_eq!(scalar.decision_makers, vec!["Alice", "Bob"]);

    let empty = parser::parse(b"---\ndecision-makers:\n---\n# T\n");
    assert!(empty.decision_makers.is_empty());
}

#[test]
fn empty_status_is_absent_rather_than_unknown() {
    let adr = parser::parse(b"---\nstatus:\ndate:\n---\n# T\n");
    assert!(adr.status.is_none());
    assert!(adr.status_raw.is_none());
    assert!(!adr
        .warnings
        .iter()
        .any(|w| matches!(w, ParseWarning::UnknownStatus { .. })));
}

#[test]
fn headings_inside_fenced_code_are_not_headings() {
    let source =
        std::fs::read(fixtures_root().join("synthetic/headings-in-code.md")).expect("fixture");
    let adr = parser::parse(&source);
    let headings: Vec<&str> = adr
        .sections
        .iter()
        .map(|s| s.raw_heading.as_str())
        .collect();
    assert!(
        !headings.iter().any(|h| h.contains("Server settings")),
        "a YAML comment inside a fence became a heading: {headings:?}"
    );
    assert!(
        !headings.iter().any(|h| h.contains("Start the server")),
        "a shell comment inside a fence became a heading: {headings:?}"
    );
    assert!(headings.contains(&"Considered Options"));
}

#[test]
fn option_key_joins_smart_quotes_and_trailing_punctuation() {
    let source = std::fs::read(fixtures_root().join("synthetic/smart-quotes.md")).expect("fixture");
    let adr = parser::parse(&source);

    // The fixture is built so that every considered option differs from its Pros-and-Cons
    // heading in exactly the ways the normalisation exists to absorb:
    //
    //   list: “Plain JUnit5 assertions”.        heading: Plain JUnit5 assertions
    //   list: “AssertJ”                          heading: AssertJ
    //   list: ‘Hamcrest’ – the older alternative.  heading: Hamcrest - the older alternative
    //
    // Smart double quotes, smart single quotes, an en dash against a hyphen, and a trailing
    // period. Without the normalisation none of the three would join, so this test fails if
    // the normalisation is ever removed.
    let option_texts: Vec<&str> = adr
        .items_of(ItemKind::Option)
        .map(|item| item.text.as_str())
        .collect();
    assert_eq!(option_texts.len(), 3, "options: {option_texts:?}");
    assert!(
        option_texts.iter().any(|text| text.contains('\u{201C}')),
        "the fixture must carry a smart quote in an option title: {option_texts:?}"
    );
    assert!(
        option_texts.iter().any(|text| text.contains('\u{2013}')),
        "the fixture must carry an en dash in an option title: {option_texts:?}"
    );
    assert!(
        option_texts.iter().any(|text| text.ends_with('.')),
        "the fixture must carry trailing punctuation in an option title: {option_texts:?}"
    );

    let heading_keys: Vec<String> = adr
        .sections
        .iter()
        .filter(|s| s.kind == SectionKind::ProsCons && s.option_key.is_some())
        .filter_map(|s| s.option_key.clone())
        .collect();
    assert_eq!(
        heading_keys,
        vec![
            "opt:plain junit5 assertions",
            "opt:assertj",
            "opt:hamcrest - the older alternative",
        ],
        "the Pros-and-Cons headings should normalise to plain keys"
    );

    // Every option joins, so the fixture produces no join warnings at all. This is the
    // assertion that actually exercises the normalisation: with the smart quotes left in
    // place, or with the trailing period kept, every one of these three would fail.
    let join_warnings: Vec<&ParseWarning> = adr
        .warnings
        .iter()
        .filter(|warning| {
            matches!(
                warning,
                ParseWarning::UnmatchedOption { .. } | ParseWarning::OrphanProsCons { .. }
            )
        })
        .collect();
    assert!(
        join_warnings.is_empty(),
        "every option should join its Pros-and-Cons heading, got {join_warnings:?}"
    );

    // And the arguments are attached to the option they belong to.
    for (title, expected_pros) in [
        ("\u{201C}Plain JUnit5 assertions\u{201D}.", 1usize),
        ("\u{201C}AssertJ\u{201D}", 2),
        (
            "\u{2018}Hamcrest\u{2019} \u{2013} the older alternative.",
            1,
        ),
    ] {
        let key = adrian_core::types::option_key(title);
        let pros = adr
            .items
            .iter()
            .filter(|item| item.kind == ItemKind::Pro)
            .filter(|item| item.option_key.as_deref() == Some(key.as_str()))
            .count();
        assert_eq!(
            pros, expected_pros,
            "option {title:?} should own {expected_pros} pro item(s), key {key:?}"
        );
    }
}

#[test]
fn unmatched_options_and_orphan_pros_cons_produce_warnings() {
    let orphan =
        std::fs::read(fixtures_root().join("synthetic/orphan-pros-cons.md")).expect("fixture");
    let adr = parser::parse(&orphan);
    assert!(
        adr.warnings
            .iter()
            .any(|w| matches!(w, ParseWarning::OrphanProsCons { heading } if heading.contains("RabbitMQ"))),
        "expected an OrphanProsCons warning for RabbitMQ, got {:?}",
        adr.warnings
    );

    let none = std::fs::read(fixtures_root().join("synthetic/no-options.md")).expect("fixture");
    let adr = parser::parse(&none);
    assert_eq!(adr.items_of(ItemKind::Option).count(), 0);
    // A document with no Considered Options must not warn about options it never had.
    assert!(!adr
        .warnings
        .iter()
        .any(|w| matches!(w, ParseWarning::UnmatchedOption { .. })));
}

#[test]
fn crlf_documents_parse_the_same_as_lf() {
    let crlf = std::fs::read(fixtures_root().join("synthetic/crlf.md")).expect("fixture");
    assert!(
        crlf.windows(2).any(|w| w == b"\r\n"),
        "fixture must be CRLF"
    );

    let adr = parser::parse(&crlf);
    assert_eq!(
        adr.title,
        "Use feature flags instead of long-lived branches"
    );
    assert_eq!(adr.status_raw.as_deref(), Some("proposed"));
    assert_eq!(adr.items_of(ItemKind::Driver).count(), 2);
    assert_eq!(adr.items_of(ItemKind::Option).count(), 2);
    // No carriage return may survive into extracted text.
    for item in &adr.items {
        assert!(
            !item.text.contains('\r'),
            "carriage return leaked into item text {:?}",
            item.text
        );
    }
}

#[test]
fn golden_projections_are_stable() {
    let update = std::env::var("ADRIAN_UPDATE_GOLDEN").is_ok();
    let expected_dir = fixtures_root().join("expected");
    std::fs::create_dir_all(&expected_dir).expect("create expected dir");

    let chosen = [
        "real/madr__0006-use-names-as-identifier.md",
        "real/madr__0010-support-categories.md",
        "real/madr__0013-use-yaml-front-matter-for-meta-data.md",
        "real/adg__AD0001-domain-layer-is-independent-from-upper-layers.md",
        "synthetic/minimal-template.md",
        "synthetic/smart-quotes.md",
        "synthetic/crlf.md",
    ];

    let mut compared = 0usize;
    for name in chosen {
        let path = fixtures_root().join(name);
        let bytes = std::fs::read(&path).unwrap_or_else(|err| panic!("{name}: {err}"));
        let adr = parser::parse(&bytes);
        let actual = serde_json::to_string_pretty(&adr).expect("serialise projection");

        let golden_name = name.replace('/', "__").replace(".md", ".json");
        let golden_path = expected_dir.join(&golden_name);

        if update || !golden_path.exists() {
            std::fs::write(&golden_path, format!("{actual}\n")).expect("write golden");
            continue;
        }

        let expected = std::fs::read_to_string(&golden_path).expect("read golden");
        assert_eq!(
            actual.trim_end(),
            expected.trim_end(),
            "{name}: projection drifted from the golden file. If the change is intended, \
             regenerate with ADRIAN_UPDATE_GOLDEN=1 and review the diff."
        );
        compared += 1;
    }

    if !update {
        assert!(
            compared >= 5,
            "expected at least 5 golden comparisons, made {compared}"
        );
    }
}

#[test]
fn golden_projection_is_byte_stable_across_runs() {
    let path = fixtures_root().join("synthetic/smart-quotes.md");
    let bytes = std::fs::read(path).expect("fixture");
    let first = serde_json::to_string_pretty(&parser::parse(&bytes)).expect("serialise");
    let second = serde_json::to_string_pretty(&parser::parse(&bytes)).expect("serialise");
    assert_eq!(first, second);
}
