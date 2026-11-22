-- Adrian v0.1.0 schema.
--
-- SQLite is the system of record. The vector index in Qdrant is derived and may be
-- rebuilt from this file at any time. If the two disagree, Qdrant is wrong.
--
-- Conventions:
--   * Identifiers are 16-byte UUIDs stored as BLOB. ADR ids are UUIDv7 (time ordered);
--     chunk ids are UUIDv5 so that re-chunking the same revision is idempotent.
--   * Timestamps are unix seconds in INTEGER columns.
--   * Calendar dates are TEXT in 'YYYY-MM-DD' form, because they are business time and
--     are compared lexicographically.
--   * ADRs are append-only. A content change creates a new row in adr_rev; a status
--     change creates a row in status_event. Nothing is updated in place except the
--     convenience columns on `adr`.

PRAGMA foreign_keys = ON;

CREATE TABLE workspace (
    id         BLOB PRIMARY KEY,
    slug       TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL
);

CREATE TABLE project (
    id               BLOB PRIMARY KEY,
    workspace_id     BLOB NOT NULL REFERENCES workspace(id),
    slug             TEXT NOT NULL,
    name             TEXT NOT NULL,
    repo_url         TEXT,
    default_template TEXT NOT NULL DEFAULT 'madr',
    embed_model      TEXT NOT NULL DEFAULT 'uibk-embedding-v1',
    embed_version    INTEGER NOT NULL DEFAULT 1,
    created_at       INTEGER NOT NULL,
    UNIQUE (workspace_id, slug)
);

CREATE TABLE adr (
    id           BLOB PRIMARY KEY,
    project_id   BLOB NOT NULL REFERENCES project(id),
    -- The client's stable key for this decision. Numbers are local to a directory and
    -- Adrian cannot see directories, so they are display-only and may collide.
    external_ref TEXT NOT NULL,
    number       INTEGER,
    slug         TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    created_by   TEXT NOT NULL,
    -- Derived convenience columns. Authoritative history lives in adr_rev/status_event.
    cur_rev      INTEGER NOT NULL,
    cur_status   TEXT NOT NULL,
    UNIQUE (project_id, external_ref)
);

CREATE TABLE adr_rev (
    adr_id         BLOB NOT NULL REFERENCES adr(id),
    rev            INTEGER NOT NULL,
    title          TEXT NOT NULL,
    template       TEXT NOT NULL,
    source_format  TEXT NOT NULL,
    -- The verbatim bytes as submitted. Kept forever so that export can be lossless and
    -- so that a parse bug never destroys the original.
    source_bytes   BLOB NOT NULL,
    source_sha256  BLOB NOT NULL,
    content_sha256 BLOB NOT NULL,
    decided_on     TEXT,
    tx_from        INTEGER NOT NULL,
    tx_to          INTEGER,
    PRIMARY KEY (adr_id, rev)
);

CREATE TABLE section (
    id             BLOB PRIMARY KEY,
    adr_id         BLOB NOT NULL,
    rev            INTEGER NOT NULL,
    parent_id      BLOB,
    kind           TEXT NOT NULL,
    -- The heading as written, so rendering can reproduce the author's wording.
    raw_heading    TEXT NOT NULL,
    ordinal        INTEGER NOT NULL,
    text           TEXT NOT NULL,
    option_key     TEXT,
    byte_start     INTEGER NOT NULL,
    byte_end       INTEGER NOT NULL,
    content_sha256 BLOB NOT NULL
);

CREATE TABLE item (
    id         BLOB PRIMARY KEY,
    adr_id     BLOB NOT NULL,
    rev        INTEGER NOT NULL,
    section_id BLOB NOT NULL REFERENCES section(id),
    kind       TEXT NOT NULL,
    ordinal    INTEGER NOT NULL,
    text       TEXT NOT NULL,
    option_key TEXT,
    byte_start INTEGER NOT NULL,
    byte_end   INTEGER NOT NULL
);

CREATE TABLE status_event (
    id             BLOB PRIMARY KEY,
    adr_id         BLOB NOT NULL REFERENCES adr(id),
    from_status    TEXT,
    to_status      TEXT NOT NULL,
    effective_from TEXT,
    recorded_at    INTEGER NOT NULL,
    actor          TEXT NOT NULL,
    reason         TEXT,
    superseded_by  BLOB
);

CREATE TABLE scope (
    adr_id     BLOB NOT NULL,
    kind       TEXT NOT NULL,
    value      TEXT NOT NULL,
    origin     TEXT NOT NULL,
    confidence REAL NOT NULL DEFAULT 1.0,
    PRIMARY KEY (adr_id, kind, value)
);

CREATE TABLE technology (
    adr_id    BLOB NOT NULL,
    canonical TEXT NOT NULL,
    role      TEXT NOT NULL,
    PRIMARY KEY (adr_id, canonical, role)
);

CREATE TABLE chunk (
    id           BLOB PRIMARY KEY,
    adr_id       BLOB NOT NULL,
    rev          INTEGER NOT NULL,
    kind         TEXT NOT NULL,
    ordinal      INTEGER NOT NULL,
    section_id   BLOB,
    text         TEXT NOT NULL,
    text_sha256  BLOB NOT NULL,
    token_count  INTEGER,
    truncated    INTEGER NOT NULL DEFAULT 0,
    created_at   INTEGER NOT NULL,
    UNIQUE (adr_id, rev, kind, ordinal)
);

CREATE TABLE chunk_vector (
    chunk_id      BLOB NOT NULL REFERENCES chunk(id),
    model         TEXT NOT NULL,
    version       INTEGER NOT NULL,
    collection    TEXT NOT NULL,
    point_id      BLOB NOT NULL,
    token_vectors INTEGER NOT NULL,
    dense         INTEGER NOT NULL,
    created_at    INTEGER NOT NULL,
    PRIMARY KEY (chunk_id, model, version)
);

CREATE TABLE embed_job (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    chunk_id   BLOB NOT NULL,
    model      TEXT NOT NULL,
    version    INTEGER NOT NULL,
    role       TEXT NOT NULL DEFAULT 'document',
    state      TEXT NOT NULL,
    attempts   INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    created_at INTEGER NOT NULL
);

CREATE TABLE ingest_request (
    project_id      BLOB NOT NULL,
    idempotency_key TEXT NOT NULL,
    request_sha256  BLOB NOT NULL,
    response_json   TEXT NOT NULL,
    created_at      INTEGER NOT NULL,
    PRIMARY KEY (project_id, idempotency_key)
);

-- Lexical arm of retrieval. FTS5 cannot use a BLOB primary key as its rowid, so the
-- chunk's implicit rowid is the join key and chunk_id is carried along for readability.
CREATE VIRTUAL TABLE chunk_fts USING fts5 (
    text,
    chunk_id UNINDEXED,
    tokenize = 'unicode61 remove_diacritics 2'
);

CREATE INDEX ix_adr_project_status ON adr (project_id, cur_status);
CREATE INDEX ix_adr_project_number ON adr (project_id, number);
CREATE INDEX ix_adr_rev_sha ON adr_rev (source_sha256);
CREATE INDEX ix_section_adr_rev ON section (adr_id, rev);
CREATE INDEX ix_item_adr_rev ON item (adr_id, rev);
CREATE INDEX ix_chunk_adr_rev ON chunk (adr_id, rev);
CREATE INDEX ix_embed_job_state ON embed_job (state, id);
CREATE INDEX ix_scope_kind_value ON scope (kind, value);
CREATE INDEX ix_scope_adr ON scope (adr_id);
CREATE INDEX ix_status_event_adr ON status_event (adr_id, recorded_at);
CREATE INDEX ix_technology_canonical ON technology (canonical);
