//! Semantic chunking: the ADR template *is* the chunking scheme.
//!
//! A generic text splitter would be wrong here. MADR already says where the arguments
//! are, so a chunk is one decision-relevant argument rather than N characters of prose.
//!
//! The composition rule that matters is `option`: MADR keeps the option title in
//! "Considered Options" and its arguments in "Pros and Cons of the Options", joined only
//! by title string equality. A chunk containing just the title is useless to a retriever,
//! so the chunker reassembles both halves.

use crate::types::*;
use sha2::{Digest, Sha256};
use thiserror::Error;

/// A failure while chunking.
#[derive(Debug, Error)]
pub enum ChunkerError {
    #[error("{0}")]
    Invalid(String),
}

/// Counts tokens for a piece of text.
///
/// The real implementation lives in the embedder client, because the tokenizer belongs to
/// the model. Tests inject a deterministic stub so that chunk boundaries can be asserted
/// without loading a 3.6 MB tokenizer.
pub trait TokenCounter: Send + Sync {
    fn count(&self, text: &str) -> usize;
}

/// A token counter that approximates by whitespace, used when nothing better is available.
#[derive(Debug, Default, Clone, Copy)]
pub struct WhitespaceTokenCounter;

impl TokenCounter for WhitespaceTokenCounter {
    fn count(&self, text: &str) -> usize {
        text.split_whitespace().count().max(1)
    }
}

/// A counter that treats one character as one token. Only useful in tests.
#[derive(Debug, Default, Clone, Copy)]
pub struct CharTokenCounter;

impl TokenCounter for CharTokenCounter {
    fn count(&self, text: &str) -> usize {
        text.chars().count().max(1)
    }
}

/// Default budget for a single chunk, matching the embedding model's document limit.
pub const DEFAULT_MAX_TOKENS: usize = 2048;

/// What the chunker produced for one ADR revision.
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkedAdr {
    pub chunks: Vec<Chunk>,
    pub scopes: Vec<Scope>,
    pub technologies: Vec<Technology>,
}

/// Chunk one parsed ADR.
///
/// `adr_id` and `rev` are only used to derive deterministic chunk ids, so that
/// re-chunking the same revision produces the same ids and re-embedding cannot create
/// duplicate points.
pub fn chunk(
    adr: &ParsedAdr,
    adr_id: uuid::Uuid,
    rev: i64,
    counter: &dyn TokenCounter,
    max_tokens: usize,
) -> Result<ChunkedAdr, ChunkerError> {
    let mut builder = Builder {
        adr,
        adr_id,
        rev,
        counter,
        max_tokens,
        chunks: Vec::new(),
    };
    builder.run()?;

    Ok(ChunkedAdr {
        scopes: extract_scopes(adr),
        technologies: extract_technologies(adr),
        chunks: builder.chunks,
    })
}

struct Builder<'a> {
    adr: &'a ParsedAdr,
    adr_id: uuid::Uuid,
    rev: i64,
    counter: &'a dyn TokenCounter,
    max_tokens: usize,
    chunks: Vec<Chunk>,
}

impl<'a> Builder<'a> {
    fn run(&mut self) -> Result<(), ChunkerError> {
        // Exactly one document chunk per ADR, always, even if every section is empty.
        let whole = self.adr_section_text();
        self.emit(ChunkKind::Document, &whole);

        if let Some(section) = self.adr.section(SectionKind::Context) {
            self.emit(ChunkKind::Context, section.text.trim());
        }

        for item in self.adr.items_of(ItemKind::Driver) {
            self.emit(ChunkKind::Driver, item.text.trim());
        }

        // One chunk per option, reassembling the title with its arguments.
        for option in self.option_titles() {
            let composed = self.compose_option(&option);
            if !composed.trim().is_empty() {
                self.emit(ChunkKind::Option, &composed);
            }
        }

        if let Some(section) = self.adr.section(SectionKind::Outcome) {
            self.emit(ChunkKind::Outcome, section.text.trim());
        }

        for item in self.adr.items_of(ItemKind::ConsequenceGood) {
            self.emit(ChunkKind::Consequence, item.text.trim());
        }
        for item in self.adr.items_of(ItemKind::ConsequenceBad) {
            self.emit(ChunkKind::Consequence, item.text.trim());
        }
        for item in self.adr.items_of(ItemKind::ConsequenceOther) {
            self.emit(ChunkKind::Consequence, item.text.trim());
        }

        if let Some(section) = self.adr.section(SectionKind::Confirmation) {
            self.emit(ChunkKind::Confirmation, section.text.trim());
        }

        if let Some(section) = self.adr.section(SectionKind::MoreInfo) {
            self.emit(ChunkKind::MoreInfo, section.text.trim());
        }

        Ok(())
    }

    /// The whole document as submitted, which is what "find decisions like this" needs.
    fn adr_section_text(&self) -> String {
        let mut out = String::new();
        if !self.adr.title.is_empty() {
            out.push_str("# ");
            out.push_str(&self.adr.title);
            out.push_str("\n\n");
        }
        for section in &self.adr.sections {
            if section.kind == SectionKind::Other && section.text.trim().is_empty() {
                continue;
            }
            out.push_str(&section.raw_heading);
            out.push('\n');
            out.push_str(section.text.trim());
            out.push_str("\n\n");
        }
        out.trim_end().to_string()
    }

    /// Option titles in source order, deduplicated by key.
    fn option_titles(&self) -> Vec<String> {
        let mut seen = std::collections::BTreeSet::new();
        let mut out = Vec::new();
        for item in self.adr.items_of(ItemKind::Option) {
            let key = option_key(&item.text);
            if seen.insert(key) {
                out.push(item.text.trim().to_string());
            }
        }
        out
    }

    /// Build the text of one option chunk: title, then the prose under its Pros and Cons
    /// heading, then every Good/Neutral/Bad bullet that belongs to it.
    fn compose_option(&self, title: &str) -> String {
        let key = option_key(title);
        let mut parts: Vec<String> = Vec::new();

        let mut heading = format!("Option: {}", title.trim());
        // Prefer the author's own wording of the heading when it differs from the list entry.
        if let Some(section) = self
            .adr
            .sections
            .iter()
            .find(|s| s.kind == SectionKind::ProsCons && s.option_key.as_deref() == Some(&key))
        {
            if !section.raw_heading.trim().is_empty() {
                heading = section.raw_heading.trim().to_string();
            }
        }
        parts.push(heading);

        // Prose directly under the option heading, plus any deeper sub-headings that
        // inherit the same key (MADR's own ADR-0010 has `#### Examples` under an option).
        for section in &self.adr.sections {
            if section.kind == SectionKind::ProsCons && section.option_key.as_deref() == Some(&key)
            {
                let prose = prose_without_bullets(&section.text);
                if !prose.trim().is_empty() {
                    parts.push(prose.trim().to_string());
                }
            }
        }

        // Every argument attached to this option, in source order.
        let mut arguments: Vec<String> = Vec::new();
        for item in &self.adr.items {
            if item.option_key.as_deref() != Some(&key) {
                continue;
            }
            let prefix = match item.kind {
                ItemKind::Pro => "Good",
                ItemKind::Con => "Bad",
                ItemKind::Neutral => "Neutral",
                _ => continue,
            };
            let text = item.text.trim();
            // The source already says "Good, because ..." in most cases; avoid doubling it.
            if text
                .to_ascii_lowercase()
                .starts_with(&prefix.to_ascii_lowercase())
            {
                arguments.push(format!("* {text}"));
            } else {
                arguments.push(format!("* {prefix}: {text}"));
            }
        }
        if !arguments.is_empty() {
            parts.push(arguments.join("\n"));
        }

        parts.join("\n\n")
    }

    /// Emit one logical chunk, splitting it if it exceeds the token budget.
    fn emit(&mut self, kind: ChunkKind, text: &str) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        let ordinal_start = self.chunks.iter().filter(|c| c.kind == kind).count();

        let pieces = self.split_to_budget(trimmed);
        for (offset, (piece, hard_split)) in pieces.iter().enumerate() {
            let token_count = self.counter.count(piece);
            let ordinal = ordinal_start + offset;
            let id = chunk_id(self.adr_id, self.rev, kind, ordinal);
            self.chunks.push(Chunk {
                id,
                adr_id: self.adr_id,
                rev: self.rev,
                kind,
                ordinal,
                section_id: None,
                text: piece.clone(),
                text_sha256: sha256(piece.as_bytes()),
                token_count: Some(token_count),
                // Splitting on a paragraph or sentence boundary is not truncation: the
                // reader of the next chunk gets the continuation. Cutting a word because
                // nothing smaller could be split is, so it is recorded as truncated.
                truncated: *hard_split,
            });
        }
    }

    /// Split text to the token budget, preferring paragraph then sentence boundaries.
    ///
    /// Returns `(piece, was_hard_split)`. The flag is what distinguishes a boundary split,
    /// which loses nothing, from a mid-word cut, which does.
    fn split_to_budget(&self, text: &str) -> Vec<(String, bool)> {
        if self.counter.count(text) <= self.max_tokens {
            return vec![(text.to_string(), false)];
        }

        let mut out: Vec<(String, bool)> = Vec::new();
        let mut current = String::new();

        for unit in sentence_units(text) {
            let candidate_len = if current.is_empty() {
                self.counter.count(&unit)
            } else {
                self.counter.count(&current) + self.counter.count(&unit)
            };

            if candidate_len <= self.max_tokens {
                if !current.is_empty() {
                    current.push_str("\n\n");
                }
                current.push_str(&unit);
                continue;
            }

            if !current.is_empty() {
                out.push((std::mem::take(&mut current), false));
            }

            if self.counter.count(&unit) <= self.max_tokens {
                current.push_str(&unit);
                continue;
            }

            // A single unit is still too large: cut it on character boundaries.
            for piece in hard_split(&unit, self.counter, self.max_tokens) {
                out.push((piece, true));
            }
        }

        if !current.is_empty() {
            out.push((current, false));
        }

        if out.is_empty() {
            out.push((text.to_string(), false));
        }
        out
    }
}

/// Split into paragraph units, and paragraphs into sentences when a paragraph is short
/// enough that sentence packing is worth the extra boundaries.
fn sentence_units(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for paragraph in text.split("\n\n") {
        let paragraph = paragraph.trim();
        if paragraph.is_empty() {
            continue;
        }
        // Keep list blocks intact: a bullet and its continuation lines are one unit.
        if paragraph.starts_with('*') || paragraph.starts_with('-') || paragraph.starts_with('#') {
            out.push(paragraph.to_string());
            continue;
        }
        let mut sentence = String::new();
        for fragment in split_sentences(paragraph) {
            if sentence.is_empty() {
                sentence.push_str(&fragment);
            } else {
                sentence.push(' ');
                sentence.push_str(&fragment);
            }
            if sentence.ends_with('.') || sentence.ends_with('!') || sentence.ends_with('?') {
                out.push(std::mem::take(&mut sentence));
            }
        }
        if !sentence.is_empty() {
            out.push(sentence);
        }
    }
    if out.is_empty() {
        out.push(text.trim().to_string());
    }
    out
}

fn split_sentences(paragraph: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut chars = paragraph.chars().peekable();
    while let Some(c) = chars.next() {
        current.push(c);
        if matches!(c, '.' | '!' | '?') {
            match chars.peek() {
                Some(next) if next.is_whitespace() => {
                    out.push(std::mem::take(&mut current).trim().to_string());
                }
                None => {}
                _ => {}
            }
        }
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }
    out.retain(|s| !s.is_empty());
    out
}

fn hard_split(text: &str, counter: &dyn TokenCounter, max_tokens: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for c in text.chars() {
        current.push(c);
        if counter.count(&current) >= max_tokens {
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.trim().is_empty() {
        out.push(current);
    }
    out
}

/// The prose of a section with its bullet lines removed.
///
/// The bullets are extracted as items and reattached by [`Builder::compose_option`], so
/// keeping them here too would duplicate every argument in the chunk.
fn prose_without_bullets(text: &str) -> String {
    let mut out = Vec::new();
    let mut in_fence = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            out.push(line);
            continue;
        }
        if !in_fence {
            let is_bullet = trimmed.starts_with("* ")
                || trimmed.starts_with("- ")
                || trimmed.starts_with("+ ")
                || trimmed
                    .split_once('.')
                    .map(|(head, _)| !head.is_empty() && head.chars().all(|c| c.is_ascii_digit()))
                    .unwrap_or(false);
            if is_bullet {
                continue;
            }
        }
        out.push(line);
    }
    out.join("\n").trim().to_string()
}

/// A deterministic chunk id, so re-chunking the same revision is idempotent.
pub fn chunk_id(adr_id: uuid::Uuid, rev: i64, kind: ChunkKind, ordinal: usize) -> uuid::Uuid {
    let name = format!("{adr_id}||{rev}||{}||{ordinal}", kind.as_str());
    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, name.as_bytes())
}

pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

// ---------------------------------------------------------------------------
// Heuristic extraction
// ---------------------------------------------------------------------------

/// Path-like tokens that look like a file or directory the decision binds.
///
/// This is a guess and is recorded as [`ScopeOrigin::Extracted`]. Adrian cannot see the
/// repository, so anything it infers here is an assertion, not a fact.
pub fn extract_scopes(adr: &ParsedAdr) -> Vec<Scope> {
    let mut found: std::collections::BTreeMap<String, Scope> = std::collections::BTreeMap::new();
    let haystack = adr
        .sections
        .iter()
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    for token in backticked_tokens(&haystack) {
        if let Some(normalized) = normalize_scope_path(&token) {
            found.entry(normalized.clone()).or_insert(Scope {
                kind: ScopeKind::PathPrefix,
                value: normalized,
                origin: ScopeOrigin::Extracted,
                confidence: 0.5,
            });
        }
    }

    found.into_values().collect()
}

/// Every backtick-quoted span in the text.
fn backticked_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_tick = false;
    let mut current = String::new();
    for c in text.chars() {
        if c == '`' {
            if in_tick {
                out.push(std::mem::take(&mut current));
            }
            in_tick = !in_tick;
            continue;
        }
        if in_tick {
            current.push(c);
        }
    }
    out
}

/// Accept a candidate as a path only when it is relative, has no parent traversal and no
/// mid-path wildcard. Rejecting loudly here is what keeps the scope table queryable.
pub fn normalize_scope_path(candidate: &str) -> Option<String> {
    let trimmed = candidate.trim();
    if trimmed.is_empty() || trimmed.contains(char::is_whitespace) {
        return None;
    }
    // Strip a trailing wildcard, which authors write as `path/**` or `path/*`.
    let without_wildcard = trimmed
        .trim_end_matches("/**")
        .trim_end_matches("/*")
        .trim_end_matches('/');
    if without_wildcard.contains('*') {
        return None; // a mid-path wildcard cannot be turned into a prefix
    }
    if without_wildcard.starts_with('/') || without_wildcard.starts_with('~') {
        return None; // absolute
    }
    if without_wildcard.starts_with("http://") || without_wildcard.starts_with("https://") {
        return None;
    }
    let segments: Vec<&str> = without_wildcard.split('/').collect();
    if segments.contains(&"..") {
        return None;
    }
    // Require either a separator or a known source-file extension: a bare word like
    // `tokio` is a technology, not a path.
    let has_separator = without_wildcard.contains('/');
    let has_extension = segments.last().map(|last| {
        matches!(
            last.rsplit_once('.').map(|(_, ext)| ext),
            Some(
                "rs" | "toml"
                    | "md"
                    | "json"
                    | "yaml"
                    | "yml"
                    | "ts"
                    | "js"
                    | "tsx"
                    | "jsx"
                    | "go"
                    | "py"
                    | "java"
                    | "kt"
                    | "sql"
                    | "sh"
                    | "proto"
                    | "lock"
            )
        )
    });
    if !has_separator && !has_extension.unwrap_or(false) {
        return None;
    }
    Some(
        without_wildcard
            .trim_start_matches("./")
            .trim_end_matches('/')
            .to_string(),
    )
}

/// Technologies named by a decision.
///
/// Deliberately conservative: a technology has to appear as a considered option and then
/// also appear in the outcome to count as chosen. False positives are worse than misses
/// here, because they end up in a filter that silently narrows retrieval.
pub fn extract_technologies(adr: &ParsedAdr) -> Vec<Technology> {
    let outcome = adr
        .section(SectionKind::Outcome)
        .map(|s| s.text.to_ascii_lowercase())
        .unwrap_or_default();

    let mut found: std::collections::BTreeMap<String, Technology> =
        std::collections::BTreeMap::new();

    for option in adr.items_of(ItemKind::Option) {
        let title = option.text.trim();
        let canonical = canonical_technology(title);
        if canonical.is_empty() || !looks_like_technology(&canonical) {
            continue;
        }
        let role = if outcome.contains(&canonical.to_ascii_lowercase()) {
            TechnologyRole::Chosen
        } else {
            TechnologyRole::Rejected
        };
        found
            .entry(canonical.clone())
            .and_modify(|existing| {
                if role == TechnologyRole::Chosen {
                    existing.role = TechnologyRole::Chosen;
                }
            })
            .or_insert(Technology { canonical, role });
    }

    found.into_values().collect()
}

/// Reduce an option title to a bare identifier when it is one.
fn canonical_technology(title: &str) -> String {
    let trimmed = title.trim().trim_matches('`').trim();
    // Only accept short, single-token titles: "Use YAML front matter" is a choice, not a
    // technology name, and treating it as one pollutes the filter.
    if trimmed.split_whitespace().count() > 2 {
        return String::new();
    }
    let cleaned = trimmed
        .trim_matches(|c: char| c == '.' || c == ',' || c == ':' || c == '"' || c == '\'')
        .trim();
    if cleaned.is_empty() || cleaned.len() > 40 {
        return String::new();
    }
    cleaned.to_string()
}

fn looks_like_technology(name: &str) -> bool {
    // Reject things that are obviously prose or paths.
    if name.contains('/') || name.contains(char::is_whitespace) && name.len() > 20 {
        return false;
    }
    // A technology name is a single token, possibly with punctuation such as `tokio`,
    // `async-std`, `Vue.js`, `PostgreSQL 16`.
    let words: Vec<&str> = name.split_whitespace().collect();
    if words.is_empty() || words.len() > 2 {
        return false;
    }
    words.iter().all(|w| {
        w.chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '+' | '#'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;

    fn counter() -> CharTokenCounter {
        CharTokenCounter
    }

    fn chunk_source(source: &str) -> ChunkedAdr {
        let adr = parser::parse(source.as_bytes());
        chunk(
            &adr,
            uuid::Uuid::now_v7(),
            1,
            &counter(),
            DEFAULT_MAX_TOKENS,
        )
        .expect("chunk")
    }

    const SAMPLE: &str = r#"---
status: accepted
---

# Use tokio as the async runtime

## Context and Problem Statement

The API server needs an async runtime with a mature ecosystem.

## Decision Drivers

* Performance under load
* Ecosystem maturity

## Considered Options

* tokio
* async-std

## Decision Outcome

Chosen option: "tokio", because it is the de-facto standard.

### Consequences

* Good, because ecosystem support is excellent
* Bad, because the dependency tree is large

## Pros and Cons of the Options

### tokio

The most widely used runtime.

* Good, because it has the largest ecosystem
* Bad, because it is opinionated

### async-std

A smaller alternative.

* Good, because the API mirrors std
* Bad, because adoption is low
"#;

    #[test]
    fn emits_one_document_chunk_per_adr() {
        let chunked = chunk_source(SAMPLE);
        let documents: Vec<&Chunk> = chunked
            .chunks
            .iter()
            .filter(|c| c.kind == ChunkKind::Document)
            .collect();
        assert_eq!(documents.len(), 1);
        assert_eq!(documents[0].ordinal, 0);
    }

    #[test]
    fn emits_a_document_chunk_even_when_every_section_is_empty() {
        let chunked = chunk_source("# Only a title\n");
        let documents: Vec<&Chunk> = chunked
            .chunks
            .iter()
            .filter(|c| c.kind == ChunkKind::Document)
            .collect();
        assert_eq!(documents.len(), 1);
        assert!(documents[0].text.contains("Only a title"));
    }

    #[test]
    fn option_chunks_contain_the_title_and_all_its_arguments() {
        let chunked = chunk_source(SAMPLE);
        let options: Vec<&Chunk> = chunked
            .chunks
            .iter()
            .filter(|c| c.kind == ChunkKind::Option)
            .collect();
        assert_eq!(options.len(), 2, "one chunk per considered option");

        let tokio = &options[0];
        assert!(
            tokio.text.contains("tokio"),
            "title missing: {}",
            tokio.text
        );
        assert!(
            tokio.text.contains("largest ecosystem"),
            "pro missing: {}",
            tokio.text
        );
        assert!(
            tokio.text.contains("opinionated"),
            "con missing: {}",
            tokio.text
        );
        assert!(
            tokio.text.contains("most widely used runtime"),
            "prose missing: {}",
            tokio.text
        );
        // The other option's arguments must not leak in.
        assert!(!tokio.text.contains("mirrors std"));

        let async_std = &options[1];
        assert!(async_std.text.contains("async-std"));
        assert!(async_std.text.contains("mirrors std"));
        assert!(!async_std.text.contains("largest ecosystem"));
    }

    #[test]
    fn chunk_ids_are_deterministic() {
        let adr = parser::parse(SAMPLE.as_bytes());
        let id = uuid::Uuid::now_v7();
        let first = chunk(&adr, id, 1, &counter(), DEFAULT_MAX_TOKENS).unwrap();
        let second = chunk(&adr, id, 1, &counter(), DEFAULT_MAX_TOKENS).unwrap();
        assert_eq!(first.chunks.len(), second.chunks.len());
        for (a, b) in first.chunks.iter().zip(second.chunks.iter()) {
            assert_eq!(a.id, b.id);
            assert_eq!(a.text, b.text);
            assert_eq!(a.ordinal, b.ordinal);
            assert_eq!(a.kind, b.kind);
        }
    }

    #[test]
    fn chunk_ids_change_with_the_revision() {
        let adr = parser::parse(SAMPLE.as_bytes());
        let id = uuid::Uuid::now_v7();
        let one = chunk(&adr, id, 1, &counter(), DEFAULT_MAX_TOKENS).unwrap();
        let two = chunk(&adr, id, 2, &counter(), DEFAULT_MAX_TOKENS).unwrap();
        assert_ne!(one.chunks[0].id, two.chunks[0].id);
    }

    #[test]
    fn no_chunk_is_empty() {
        for (name, bytes) in corpus() {
            let adr = parser::parse(&bytes);
            let chunked = chunk(
                &adr,
                uuid::Uuid::now_v7(),
                1,
                &WhitespaceTokenCounter,
                DEFAULT_MAX_TOKENS,
            )
            .expect("chunk");
            for c in &chunked.chunks {
                assert!(
                    !c.text.trim().is_empty(),
                    "{name}: emitted an empty {} chunk",
                    c.kind
                );
            }
        }
    }

    #[test]
    fn document_chunk_equals_the_source_when_under_budget() {
        let source = "# A title\n\n## Context and Problem Statement\n\nShort body.\n";
        let chunked = chunk_source(source);
        let document = chunked
            .chunks
            .iter()
            .find(|c| c.kind == ChunkKind::Document)
            .expect("document chunk");
        // The document chunk is a reassembly of title plus sections, so it must contain
        // every word of the source and no invented text.
        for word in source.split_whitespace() {
            let word = word.trim_matches(['#', '.']);
            if word.is_empty() {
                continue;
            }
            assert!(
                document.text.contains(word),
                "document chunk lost the word {word:?}: {}",
                document.text
            );
        }
    }

    #[test]
    fn oversized_sections_split_without_losing_text() {
        let paragraph = "This sentence is repeated to force a split. ";
        let mut source = String::from("# Big\n\n## Context and Problem Statement\n\n");
        for _ in 0..40 {
            source.push_str(paragraph);
            source.push_str("\n\n");
        }
        let adr = parser::parse(source.as_bytes());
        let chunked =
            chunk(&adr, uuid::Uuid::now_v7(), 1, &WhitespaceTokenCounter, 40).expect("chunk");

        let contexts: Vec<&Chunk> = chunked
            .chunks
            .iter()
            .filter(|c| c.kind == ChunkKind::Context)
            .collect();
        assert!(contexts.len() > 1, "the section should have been split");

        // Every word of the original section survives across the pieces.
        let joined: String = contexts
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        let original_words: std::collections::BTreeSet<&str> =
            "This sentence is repeated to force a split."
                .split(' ')
                .collect();
        for word in original_words {
            assert!(joined.contains(word), "lost {word:?} while splitting");
        }

        // Ordinals continue across the split.
        let ordinals: Vec<usize> = contexts.iter().map(|c| c.ordinal).collect();
        assert_eq!(ordinals, (0..contexts.len()).collect::<Vec<_>>());
    }

    #[test]
    fn truncated_is_only_set_when_an_indivisible_unit_exceeds_the_budget() {
        // A single 200-character word with a budget of 10 characters cannot be split on a
        // boundary, so the pieces are marked truncated.
        let source = format!(
            "# T\n\n## Context and Problem Statement\n\n{}\n",
            "x".repeat(200)
        );
        let adr = parser::parse(source.as_bytes());
        let chunked = chunk(&adr, uuid::Uuid::now_v7(), 1, &counter(), 10).expect("chunk");
        let contexts: Vec<&Chunk> = chunked
            .chunks
            .iter()
            .filter(|c| c.kind == ChunkKind::Context)
            .collect();
        assert!(contexts.len() > 1);
        assert!(
            contexts.iter().all(|c| c.truncated),
            "an indivisible unit over budget must be marked truncated"
        );
    }

    #[test]
    fn splitting_alone_does_not_mark_truncated() {
        let mut source = String::from("# T\n\n## Context and Problem Statement\n\n");
        for _ in 0..20 {
            source.push_str("A short sentence here. ");
        }
        let adr = parser::parse(source.as_bytes());
        let chunked = chunk(&adr, uuid::Uuid::now_v7(), 1, &counter(), 30).expect("chunk");
        let contexts: Vec<&Chunk> = chunked
            .chunks
            .iter()
            .filter(|c| c.kind == ChunkKind::Context)
            .collect();
        assert!(contexts.len() > 1);
        assert!(
            contexts.iter().all(|c| !c.truncated),
            "splitting on boundaries is not truncation"
        );
    }

    #[test]
    fn scope_extraction_rejects_unsafe_paths() {
        assert_eq!(
            normalize_scope_path("crates/api"),
            Some("crates/api".to_string())
        );
        assert_eq!(
            normalize_scope_path("crates/api/**"),
            Some("crates/api".to_string())
        );
        assert_eq!(
            normalize_scope_path("./src/main.rs"),
            Some("src/main.rs".to_string())
        );
        assert_eq!(normalize_scope_path("/etc/passwd"), None);
        assert_eq!(normalize_scope_path("~/secrets"), None);
        assert_eq!(normalize_scope_path("crates/../etc"), None);
        assert_eq!(normalize_scope_path("crates/*/src"), None);
        assert_eq!(normalize_scope_path("https://example.com/x"), None);
        assert_eq!(normalize_scope_path("tokio"), None);
        assert_eq!(normalize_scope_path(""), None);
    }

    #[test]
    fn scope_extraction_finds_backticked_paths_only() {
        let source = "# T\n\n## Context and Problem Statement\n\nThe rule applies to `crates/api/**` and to plain prose about crates/web.\n";
        let adr = parser::parse(source.as_bytes());
        let scopes = extract_scopes(&adr);
        assert_eq!(scopes.len(), 1, "got {scopes:?}");
        assert_eq!(scopes[0].value, "crates/api");
        assert_eq!(scopes[0].origin, ScopeOrigin::Extracted);
    }

    #[test]
    fn technology_extraction_is_conservative() {
        let chunked = chunk_source(SAMPLE);
        let names: Vec<&str> = chunked
            .technologies
            .iter()
            .map(|t| t.canonical.as_str())
            .collect();
        assert!(names.contains(&"tokio"), "got {names:?}");
        assert!(names.contains(&"async-std"), "got {names:?}");
        let tokio = chunked
            .technologies
            .iter()
            .find(|t| t.canonical == "tokio")
            .expect("tokio");
        assert_eq!(tokio.role, TechnologyRole::Chosen);
        let async_std = chunked
            .technologies
            .iter()
            .find(|t| t.canonical == "async-std")
            .expect("async-std");
        assert_eq!(async_std.role, TechnologyRole::Rejected);
    }

    #[test]
    fn prose_paragraphs_are_not_treated_as_technologies() {
        let source = "# T\n\n## Considered Options\n\n* Use YAML front matter\n* Use plain Markdown everywhere\n\n## Decision Outcome\n\nChosen option: \"Use YAML front matter\", because it is shorter.\n";
        let adr = parser::parse(source.as_bytes());
        let technologies = extract_technologies(&adr);
        assert!(
            technologies.is_empty(),
            "prose choices must not become technologies: {technologies:?}"
        );
    }

    fn corpus() -> Vec<(String, Vec<u8>)> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/madr");
        let mut out = Vec::new();
        for subdir in ["real", "synthetic"] {
            let dir = root.join(subdir);
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            let mut paths: Vec<std::path::PathBuf> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().map(|x| x == "md").unwrap_or(false))
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
        out
    }
}
