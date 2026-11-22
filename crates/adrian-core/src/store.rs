//! SQLite persistence: the system of record.
//!
//! Two access paths exist on purpose:
//!
//! * [`Store::read`] runs a closure on a pooled connection. Many of these may run at once.
//! * [`Store::write`] runs a closure while holding a process-wide write gate, which is how
//!   SQLite wants to be used: one writer, many readers, WAL journal.
//!
//! Every closure runs on a blocking thread via [`tokio::task::spawn_blocking`], because
//! `rusqlite` is synchronous and pretending otherwise would block the reactor.

use crate::types::*;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{params, types::Value as SqlValue, Connection, OptionalExtension, Row};
use std::path::Path;
use std::sync::{Arc, Mutex};
use thiserror::Error;
use uuid::Uuid;

/// A failure from the persistence layer.
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("connection pool error: {0}")]
    Pool(#[from] r2d2::Error),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("invalid: {0}")]
    Invalid(String),

    #[error("migration failed: {0}")]
    Migration(String),

    #[error("blocking task failed: {0}")]
    Join(String),

    #[error("corrupt row: {0}")]
    Corrupt(String),
}

type Result<T> = std::result::Result<T, StoreError>;

/// Converts a `rusqlite::Result` into a [`StoreError`] result.
///
/// `rusqlite`'s `.optional()` yields `rusqlite::Result<Option<T>>`, which does not unify
/// with this module's `Result` in a closure tail position. This trait keeps those call
/// sites a single line.
trait OrStore<T> {
    fn or_store(self) -> Result<T>;
}

impl<T> OrStore<T> for rusqlite::Result<T> {
    fn or_store(self) -> Result<T> {
        self.map_err(StoreError::Sqlite)
    }
}

/// The schema version this build expects.
pub const SCHEMA_VERSION: i64 = 1;

/// What a project holds, counted before and after a delete.
///
/// It is returned by both the preview and the delete so that an operator sees the same
/// numbers whether they were asking or committing, and so a test can assert the delete
/// took everything the preview promised.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProjectFootprint {
    pub adrs: i64,
    pub revisions: i64,
    pub chunks: i64,
    pub vectors: i64,
    pub embed_jobs: i64,
}

const MIGRATION_0001: &str = include_str!("../../../migrations/0001_init.sql");

/// A handle to the database.
#[derive(Clone)]
pub struct Store {
    pool: r2d2::Pool<SqliteConnectionManager>,
    write_gate: Arc<Mutex<()>>,
}

impl Store {
    /// Open (or create) a database file and apply pragmas.
    pub fn open(path: impl AsRef<Path>) -> Result<Store> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|err| {
                    StoreError::Invalid(format!(
                        "cannot create database directory {}: {err}",
                        parent.display()
                    ))
                })?;
            }
        }
        let manager = SqliteConnectionManager::file(&path).with_init(|conn| {
            configure(conn)?;
            Ok(())
        });
        let pool = r2d2::Pool::builder()
            .max_size(8)
            .connection_timeout(std::time::Duration::from_secs(10))
            .build(manager)?;
        Ok(Store {
            pool,
            write_gate: Arc::new(Mutex::new(())),
        })
    }

    /// Open a private in-memory database. Used by tests.
    pub fn open_in_memory() -> Result<Store> {
        // A shared-cache in-memory database so that every pooled connection sees the same
        // data; a plain `:memory:` would give each connection its own empty database.
        let name = format!(
            "file:adrian-test-{}?mode=memory&cache=shared",
            Uuid::new_v4()
        );
        let manager = SqliteConnectionManager::file(&name)
            .with_init(|conn| {
                configure(conn)?;
                Ok(())
            })
            .with_flags(
                rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                    | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
                    | rusqlite::OpenFlags::SQLITE_OPEN_URI
                    | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
            );
        let pool = r2d2::Pool::builder()
            .max_size(4)
            .connection_timeout(std::time::Duration::from_secs(5))
            .build(manager)?;
        // Keep one connection alive for the lifetime of the pool so the shared in-memory
        // database is not dropped when a connection returns to the pool and is closed.
        let keeper = pool.get()?;
        std::mem::forget(keeper);
        Ok(Store {
            pool,
            write_gate: Arc::new(Mutex::new(())),
        })
    }

    /// Run a closure with a pooled read connection.
    pub async fn read<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get()?;
            f(&conn)
        })
        .await
        .map_err(|err| StoreError::Join(err.to_string()))?
    }

    /// Run a closure with a write gate held, so writers serialise inside the process.
    pub async fn write<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let pool = self.pool.clone();
        let gate = self.write_gate.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = gate
                .lock()
                .map_err(|_| StoreError::Join("write gate poisoned".to_string()))?;
            let conn = pool.get()?;
            f(&conn)
        })
        .await
        .map_err(|err| StoreError::Join(err.to_string()))?
    }

    /// Apply pragmas and any pending migrations. Idempotent.
    pub async fn migrate(&self) -> Result<()> {
        self.write(migrate_sync).await
    }

    /// The current schema version recorded in the file.
    pub async fn schema_version(&self) -> Result<i64> {
        self.read(|conn| {
            let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            Ok(version)
        })
        .await
    }
}

fn configure(conn: &Connection) -> rusqlite::Result<()> {
    conn.busy_timeout(std::time::Duration::from_millis(5000))?;
    // `journal_mode` returns a row, so it has to go through a query rather than execute.
    let _: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
    conn.execute_batch(
        "PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;
         PRAGMA page_size = 8192;",
    )?;
    Ok(())
}

/// Apply migrations on an already-open connection.
pub fn migrate_sync(conn: &Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version >= SCHEMA_VERSION {
        return Ok(());
    }
    conn.execute_batch("BEGIN IMMEDIATE;")
        .map_err(|err| StoreError::Migration(err.to_string()))?;
    let outcome = (|| -> Result<()> {
        if version < 1 {
            conn.execute_batch(MIGRATION_0001)?;
        }
        conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
        Ok(())
    })();
    match outcome {
        Ok(()) => {
            conn.execute_batch("COMMIT;")
                .map_err(|err| StoreError::Migration(err.to_string()))?;
            Ok(())
        }
        Err(err) => {
            let _ = conn.execute_batch("ROLLBACK;");
            Err(StoreError::Migration(err.to_string()))
        }
    }
}

// ---------------------------------------------------------------------------
// Workspace
// ---------------------------------------------------------------------------

impl Store {
    /// Create a workspace. v0.1.0 uses exactly one, created on first start.
    pub async fn create_workspace(&self, slug: String) -> Result<Uuid> {
        self.write(move |conn| {
            if let Some(existing) = conn
                .query_row(
                    "SELECT id FROM workspace WHERE slug = ?1",
                    params![slug],
                    |row| row.get::<_, Uuid>(0),
                )
                .optional()?
            {
                return Err(StoreError::Conflict(format!(
                    "workspace `{slug}` already exists as {existing}"
                )));
            }
            let id = Uuid::now_v7();
            conn.execute(
                "INSERT INTO workspace (id, slug, created_at) VALUES (?1, ?2, ?3)",
                params![id, slug, now()],
            )?;
            Ok(id)
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// Row helpers
// ---------------------------------------------------------------------------

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn read_uuid(row: &Row<'_>, index: usize) -> rusqlite::Result<Uuid> {
    row.get::<_, Uuid>(index)
}

fn read_optional_uuid(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<Uuid>> {
    row.get::<_, Option<Uuid>>(index)
}

fn read_status(row: &Row<'_>, index: usize) -> rusqlite::Result<Status> {
    let raw: String = row.get(index)?;
    raw.parse::<Status>().map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other(err)),
        )
    })
}

fn read_sha(row: &Row<'_>, index: usize) -> rusqlite::Result<[u8; 32]> {
    let bytes: Vec<u8> = row.get(index)?;
    if bytes.len() != 32 {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Blob,
            Box::new(std::io::Error::other(format!(
                "expected a 32-byte hash, got {} bytes",
                bytes.len()
            ))),
        ));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

fn to_sql_blob(bytes: &[u8]) -> SqlValue {
    SqlValue::Blob(bytes.to_vec())
}

// ---------------------------------------------------------------------------
// Workspace and project
// ---------------------------------------------------------------------------

impl Store {
    /// Ensure the single local workspace exists and return its id.
    pub async fn ensure_default_workspace(&self) -> Result<Uuid> {
        self.write(ensure_default_workspace_sync).await
    }

    pub async fn create_project(&self, project: Project) -> Result<Uuid> {
        self.write(move |conn| {
            conn.execute(
                "INSERT INTO project
                   (id, workspace_id, slug, name, repo_url, default_template, embed_model,
                    embed_version, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    project.id,
                    project.workspace_id,
                    project.slug,
                    project.name,
                    project.repo_url,
                    project.default_template,
                    project.embed_model,
                    project.embed_version,
                    project.created_at,
                ],
            )?;
            Ok(project.id)
        })
        .await
    }

    pub async fn get_project_by_slug(&self, slug: String) -> Result<Option<Project>> {
        self.read(move |conn| {
            conn.query_row(
                "SELECT id, workspace_id, slug, name, repo_url, default_template, embed_model,
                        embed_version, created_at
                 FROM project WHERE slug = ?1",
                params![slug],
                map_project,
            )
            .optional()
            .or_store()
        })
        .await
    }

    pub async fn list_projects(&self) -> Result<Vec<(Project, i64)>> {
        self.read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT p.id, p.workspace_id, p.slug, p.name, p.repo_url, p.default_template,
                        p.embed_model, p.embed_version, p.created_at,
                        (SELECT COUNT(*) FROM adr a WHERE a.project_id = p.id)
                 FROM project p ORDER BY p.slug",
            )?;
            let rows = stmt.query_map([], |row| Ok((map_project(row)?, row.get::<_, i64>(9)?)))?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await
    }

    /// Count what a project holds, without touching it.
    ///
    /// The same numbers come back from [`Store::delete_project`], so a preview and the
    /// commit agree by construction rather than by two queries that drifted apart.
    pub async fn project_footprint(&self, project_id: Uuid) -> Result<ProjectFootprint> {
        self.read(move |conn| footprint_sync(conn, project_id))
            .await
    }

    /// Delete a project and everything that belongs to it.
    ///
    /// A project delete has to reach eleven tables and the full-text index, in an order
    /// the foreign keys allow. `PRAGMA foreign_keys = ON` is set per connection and the
    /// schema declares references on `adr`, `adr_rev`, `section`, `chunk_vector` and
    /// `status_event`, so deleting the project row first would simply fail. The whole thing
    /// runs inside one transaction, because a half-deleted project is worse than an
    /// undeleted one.
    ///
    /// Vector points in Qdrant are the caller's problem. This function cannot reach the
    /// index, so it returns the footprint and the caller removes the points.
    pub async fn delete_project(&self, project_id: Uuid) -> Result<ProjectFootprint> {
        self.write(move |conn| {
            conn.execute_batch("BEGIN IMMEDIATE;")?;
            match delete_project_sync(conn, project_id) {
                Ok(footprint) => {
                    conn.execute_batch("COMMIT;")?;
                    Ok(footprint)
                }
                Err(err) => {
                    let _ = conn.execute_batch("ROLLBACK;");
                    Err(err)
                }
            }
        })
        .await
    }
}

/// The `adr_id` subquery every cascade step keys on.
const PROJECT_ADRS: &str = "SELECT id FROM adr WHERE project_id = ?1";

fn footprint_sync(conn: &Connection, project_id: Uuid) -> Result<ProjectFootprint> {
    let count = |sql: &str| -> Result<i64> {
        conn.query_row(sql, params![project_id], |row| row.get::<_, i64>(0))
            .or_store()
    };
    Ok(ProjectFootprint {
        adrs: count("SELECT COUNT(*) FROM adr WHERE project_id = ?1")?,
        revisions: count(&format!(
            "SELECT COUNT(*) FROM adr_rev WHERE adr_id IN ({PROJECT_ADRS})"
        ))?,
        chunks: count(&format!(
            "SELECT COUNT(*) FROM chunk WHERE adr_id IN ({PROJECT_ADRS})"
        ))?,
        vectors: count(&format!(
            "SELECT COUNT(*) FROM chunk_vector WHERE chunk_id IN \
             (SELECT id FROM chunk WHERE adr_id IN ({PROJECT_ADRS}))"
        ))?,
        embed_jobs: count(&format!(
            "SELECT COUNT(*) FROM embed_job WHERE chunk_id IN \
             (SELECT id FROM chunk WHERE adr_id IN ({PROJECT_ADRS}))"
        ))?,
    })
}

fn delete_project_sync(conn: &Connection, project_id: Uuid) -> Result<ProjectFootprint> {
    let exists = conn
        .query_row(
            "SELECT 1 FROM project WHERE id = ?1",
            params![project_id],
            |_| Ok(()),
        )
        .optional()
        .or_store()?
        .is_some();
    if !exists {
        return Err(StoreError::NotFound(format!("project {project_id}")));
    }

    let footprint = footprint_sync(conn, project_id)?;

    // FTS5 cannot take a BLOB primary key as its rowid, so the index is keyed on the
    // chunk table's implicit rowid. Deleting chunk rows without this step would leave
    // search hits pointing at rows that no longer exist.
    conn.execute(
        &format!(
            "DELETE FROM chunk_fts WHERE rowid IN \
             (SELECT c.rowid FROM chunk c WHERE c.adr_id IN ({PROJECT_ADRS}))"
        ),
        params![project_id],
    )?;
    conn.execute(
        &format!(
            "DELETE FROM chunk_vector WHERE chunk_id IN \
             (SELECT id FROM chunk WHERE adr_id IN ({PROJECT_ADRS}))"
        ),
        params![project_id],
    )?;
    conn.execute(
        &format!(
            "DELETE FROM embed_job WHERE chunk_id IN \
             (SELECT id FROM chunk WHERE adr_id IN ({PROJECT_ADRS}))"
        ),
        params![project_id],
    )?;
    conn.execute(
        &format!("DELETE FROM item WHERE adr_id IN ({PROJECT_ADRS})"),
        params![project_id],
    )?;
    conn.execute(
        &format!("DELETE FROM section WHERE adr_id IN ({PROJECT_ADRS})"),
        params![project_id],
    )?;
    conn.execute(
        &format!("DELETE FROM chunk WHERE adr_id IN ({PROJECT_ADRS})"),
        params![project_id],
    )?;
    conn.execute(
        &format!("DELETE FROM status_event WHERE adr_id IN ({PROJECT_ADRS})"),
        params![project_id],
    )?;
    conn.execute(
        &format!("DELETE FROM scope WHERE adr_id IN ({PROJECT_ADRS})"),
        params![project_id],
    )?;
    conn.execute(
        &format!("DELETE FROM technology WHERE adr_id IN ({PROJECT_ADRS})"),
        params![project_id],
    )?;
    conn.execute(
        &format!("DELETE FROM adr_rev WHERE adr_id IN ({PROJECT_ADRS})"),
        params![project_id],
    )?;
    conn.execute(
        &format!("DELETE FROM adr WHERE id IN ({PROJECT_ADRS})"),
        params![project_id],
    )?;
    conn.execute(
        "DELETE FROM ingest_request WHERE project_id = ?1",
        params![project_id],
    )?;
    conn.execute("DELETE FROM project WHERE id = ?1", params![project_id])?;

    Ok(footprint)
}

fn ensure_default_workspace_sync(conn: &Connection) -> Result<Uuid> {
    if let Some(id) = conn
        .query_row("SELECT id FROM workspace WHERE slug = 'local'", [], |row| {
            row.get::<_, Uuid>(0)
        })
        .optional()?
    {
        return Ok(id);
    }
    let id = Uuid::now_v7();
    conn.execute(
        "INSERT INTO workspace (id, slug, created_at) VALUES (?1, 'local', ?2)",
        params![id, now()],
    )?;
    Ok(id)
}

fn map_project(row: &Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: read_uuid(row, 0)?,
        workspace_id: read_uuid(row, 1)?,
        slug: row.get(2)?,
        name: row.get(3)?,
        repo_url: row.get(4)?,
        default_template: row.get(5)?,
        embed_model: row.get(6)?,
        embed_version: row.get(7)?,
        created_at: row.get(8)?,
    })
}

// ---------------------------------------------------------------------------
// ADR writes
// ---------------------------------------------------------------------------

/// Everything needed to persist one new ADR revision in a single transaction.
#[derive(Debug, Clone)]
pub struct NewRevision {
    pub adr_id: Uuid,
    pub project_id: Uuid,
    pub rev: i64,
    pub title: String,
    pub template: String,
    pub source_format: String,
    pub source_bytes: Vec<u8>,
    pub source_sha256: [u8; 32],
    pub content_sha256: [u8; 32],
    pub decided_on: Option<String>,
    /// `true` for the first revision of a new ADR, which also inserts the `adr` row.
    pub create_adr: bool,
    pub external_ref: String,
    pub number: Option<i64>,
    pub slug: String,
    pub created_by: String,
    pub status: Status,
    pub actor: String,
    pub sections: Vec<SectionRow>,
    pub items: Vec<ItemRow>,
    pub chunks: Vec<ChunkRow>,
    pub scopes: Vec<Scope>,
    pub technologies: Vec<Technology>,
    pub embed_model: String,
    pub embed_version: i64,
}

/// A section as persisted.
#[derive(Debug, Clone)]
pub struct SectionRow {
    pub id: Uuid,
    pub parent_id: Option<Uuid>,
    pub kind: SectionKind,
    pub raw_heading: String,
    pub ordinal: usize,
    pub text: String,
    pub option_key: Option<String>,
    pub byte_start: usize,
    pub byte_end: usize,
    pub content_sha256: [u8; 32],
}

/// An item as persisted.
#[derive(Debug, Clone)]
pub struct ItemRow {
    pub id: Uuid,
    pub section_id: Uuid,
    pub kind: ItemKind,
    pub ordinal: usize,
    pub text: String,
    pub option_key: Option<String>,
    pub byte_start: usize,
    pub byte_end: usize,
}

/// A chunk as persisted.
#[derive(Debug, Clone)]
pub struct ChunkRow {
    pub id: Uuid,
    pub kind: ChunkKind,
    pub ordinal: usize,
    pub section_id: Option<Uuid>,
    pub text: String,
    pub text_sha256: [u8; 32],
    pub token_count: Option<usize>,
    pub truncated: bool,
}

impl Store {
    /// Persist a new revision atomically.
    ///
    /// Either every row lands or none does. The caller has already decided that this
    /// revision is genuinely new; duplicate detection happens in [`crate::ingest`].
    /// Persist a new revision atomically and drop the vectors of the revisions it
    /// supersedes.
    ///
    /// Returns the `(collection, point_id)` pairs that were removed from `chunk_vector`, so
    /// the caller can delete the matching points from the vector index. The old revisions
    /// stay in SQLite: the history is the point of the design, only the index is current.
    pub async fn insert_adr_with_revision(
        &self,
        revision: NewRevision,
    ) -> Result<Vec<(String, Uuid)>> {
        self.write(move |conn| insert_revision_sync(conn, &revision))
            .await
    }

    /// Close the previous revision's transaction interval.
    pub async fn close_previous_revision(&self, adr_id: Uuid, rev: i64) -> Result<()> {
        self.write(move |conn| {
            conn.execute(
                "UPDATE adr_rev SET tx_to = ?1 WHERE adr_id = ?2 AND rev = ?3 AND tx_to IS NULL",
                params![now(), adr_id, rev],
            )?;
            Ok(())
        })
        .await
    }
}

fn insert_revision_sync(conn: &Connection, revision: &NewRevision) -> Result<Vec<(String, Uuid)>> {
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let outcome = insert_revision_inner(conn, revision)
        .and_then(|()| supersede_older_revisions(conn, revision.adr_id, revision.rev));
    match outcome {
        Ok(purged) => {
            conn.execute_batch("COMMIT;")?;
            Ok(purged)
        }
        Err(err) => {
            let _ = conn.execute_batch("ROLLBACK;");
            Err(err)
        }
    }
}

/// Drop the queued work and the index entries of every revision older than `keep_rev`.
///
/// A superseded revision's chunks must not be embedded: they no longer describe the current
/// decision, and leaving them queued would re-embed the whole history on every edit. The
/// rows themselves stay in `chunk`, because export and history still need the text.
fn supersede_older_revisions(
    conn: &Connection,
    adr_id: Uuid,
    keep_rev: i64,
) -> Result<Vec<(String, Uuid)>> {
    let mut stmt = conn.prepare(
        "SELECT v.collection, v.point_id
         FROM chunk_vector v
         JOIN chunk c ON c.id = v.chunk_id
         WHERE c.adr_id = ?1 AND c.rev < ?2",
    )?;
    let rows = stmt.query_map(params![adr_id, keep_rev], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Uuid>(1)?))
    })?;
    let mut purged = Vec::new();
    for row in rows {
        purged.push(row?);
    }
    drop(stmt);

    conn.execute(
        "DELETE FROM chunk_vector
         WHERE chunk_id IN (SELECT id FROM chunk WHERE adr_id = ?1 AND rev < ?2)",
        params![adr_id, keep_rev],
    )?;

    // Cancel queued or failed work for the superseded chunks. A job that is `running`
    // right now is left alone; the worker re-checks the revision before it indexes.
    conn.execute(
        "DELETE FROM embed_job
         WHERE state IN ('pending', 'failed')
           AND chunk_id IN (SELECT id FROM chunk WHERE adr_id = ?1 AND rev < ?2)",
        params![adr_id, keep_rev],
    )?;

    Ok(purged)
}

fn insert_revision_inner(conn: &Connection, revision: &NewRevision) -> Result<()> {
    let created_at = now();

    if revision.create_adr {
        conn.execute(
            "INSERT INTO adr
               (id, project_id, external_ref, number, slug, created_at, created_by,
                cur_rev, cur_status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                revision.adr_id,
                revision.project_id,
                revision.external_ref,
                revision.number,
                revision.slug,
                created_at,
                revision.created_by,
                revision.rev,
                revision.status.as_str(),
            ],
        )?;
        conn.execute(
            "INSERT INTO status_event
               (id, adr_id, from_status, to_status, effective_from, recorded_at, actor, reason,
                superseded_by)
             VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6, ?7, NULL)",
            params![
                Uuid::now_v7(),
                revision.adr_id,
                revision.status.as_str(),
                revision.decided_on,
                created_at,
                revision.actor,
                "initial status",
            ],
        )?;
    } else {
        conn.execute(
            "UPDATE adr SET cur_rev = ?1 WHERE id = ?2",
            params![revision.rev, revision.adr_id],
        )?;
    }

    conn.execute(
        "INSERT INTO adr_rev
           (adr_id, rev, title, template, source_format, source_bytes, source_sha256,
            content_sha256, decided_on, tx_from, tx_to)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL)",
        params![
            revision.adr_id,
            revision.rev,
            revision.title,
            revision.template,
            revision.source_format,
            to_sql_blob(&revision.source_bytes),
            to_sql_blob(&revision.source_sha256),
            to_sql_blob(&revision.content_sha256),
            revision.decided_on,
            created_at,
        ],
    )?;

    for section in &revision.sections {
        conn.execute(
            "INSERT INTO section
               (id, adr_id, rev, parent_id, kind, raw_heading, ordinal, text, option_key,
                byte_start, byte_end, content_sha256)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                section.id,
                revision.adr_id,
                revision.rev,
                section.parent_id,
                section.kind.as_str(),
                section.raw_heading,
                section.ordinal as i64,
                section.text,
                section.option_key,
                section.byte_start as i64,
                section.byte_end as i64,
                to_sql_blob(&section.content_sha256),
            ],
        )?;
    }

    for item in &revision.items {
        conn.execute(
            "INSERT INTO item
               (id, adr_id, rev, section_id, kind, ordinal, text, option_key, byte_start, byte_end)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                item.id,
                revision.adr_id,
                revision.rev,
                item.section_id,
                item.kind.as_str(),
                item.ordinal as i64,
                item.text,
                item.option_key,
                item.byte_start as i64,
                item.byte_end as i64,
            ],
        )?;
    }

    for chunk in &revision.chunks {
        conn.execute(
            "INSERT INTO chunk
               (id, adr_id, rev, kind, ordinal, section_id, text, text_sha256, token_count,
                truncated, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                chunk.id,
                revision.adr_id,
                revision.rev,
                chunk.kind.as_str(),
                chunk.ordinal as i64,
                chunk.section_id,
                chunk.text,
                to_sql_blob(&chunk.text_sha256),
                chunk.token_count.map(|n| n as i64),
                if chunk.truncated { 1i64 } else { 0i64 },
                created_at,
            ],
        )?;

        let rowid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO chunk_fts (rowid, text, chunk_id) VALUES (?1, ?2, ?3)",
            params![rowid, chunk.text, chunk.id.to_string()],
        )?;

        conn.execute(
            "INSERT INTO embed_job
               (chunk_id, model, version, role, state, attempts, last_error, created_at)
             VALUES (?1, ?2, ?3, 'document', 'pending', 0, NULL, ?4)",
            params![
                chunk.id,
                revision.embed_model,
                revision.embed_version,
                created_at
            ],
        )?;
    }

    for scope in &revision.scopes {
        conn.execute(
            "INSERT OR REPLACE INTO scope (adr_id, kind, value, origin, confidence)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                revision.adr_id,
                scope.kind.as_str(),
                scope.value,
                scope.origin.as_str(),
                scope.confidence as f64,
            ],
        )?;
    }

    for technology in &revision.technologies {
        conn.execute(
            "INSERT OR REPLACE INTO technology (adr_id, canonical, role)
             VALUES (?1, ?2, ?3)",
            params![
                revision.adr_id,
                technology.canonical,
                technology.role.as_str()
            ],
        )?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// ADR reads
// ---------------------------------------------------------------------------

impl Store {
    pub async fn get_adr(&self, id: Uuid) -> Result<Adr> {
        self.read(move |conn| {
            conn.query_row(
                "SELECT id, project_id, external_ref, number, slug, created_at, created_by,
                        cur_rev, cur_status
                 FROM adr WHERE id = ?1",
                params![id],
                map_adr,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("adr {id} does not exist")))
        })
        .await
    }

    pub async fn get_adr_by_external_ref(
        &self,
        project_id: Uuid,
        external_ref: String,
    ) -> Result<Option<Adr>> {
        self.read(move |conn| {
            conn.query_row(
                "SELECT id, project_id, external_ref, number, slug, created_at, created_by,
                        cur_rev, cur_status
                 FROM adr WHERE project_id = ?1 AND external_ref = ?2",
                params![project_id, external_ref],
                map_adr,
            )
            .optional()
            .or_store()
        })
        .await
    }

    /// Look a decision up by its display number within a project.
    ///
    /// Numbers are a filename artifact and may collide, so this returns the most recently
    /// created match rather than pretending the number is a key.
    pub async fn get_adr_by_number(&self, project_id: Uuid, number: i64) -> Result<Option<Adr>> {
        self.read(move |conn| {
            conn.query_row(
                "SELECT id, project_id, external_ref, number, slug, created_at, created_by,
                        cur_rev, cur_status
                 FROM adr WHERE project_id = ?1 AND number = ?2
                 ORDER BY created_at DESC LIMIT 1",
                params![project_id, number],
                map_adr,
            )
            .optional()
            .or_store()
        })
        .await
    }

    /// Find an ADR in the same project whose latest revision has this exact source hash.
    /// Used to recognise a rename: the bytes are identical, only the path changed.
    pub async fn find_adr_by_source_sha(
        &self,
        project_id: Uuid,
        sha: [u8; 32],
    ) -> Result<Option<Adr>> {
        self.read(move |conn| {
            conn.query_row(
                "SELECT a.id, a.project_id, a.external_ref, a.number, a.slug, a.created_at,
                        a.created_by, a.cur_rev, a.cur_status
                 FROM adr a
                 JOIN adr_rev r ON r.adr_id = a.id AND r.rev = a.cur_rev
                 WHERE a.project_id = ?1 AND r.source_sha256 = ?2
                 LIMIT 1",
                params![project_id, to_sql_blob(&sha)],
                map_adr,
            )
            .optional()
            .or_store()
        })
        .await
    }

    pub async fn rename_external_ref(&self, id: Uuid, external_ref: String) -> Result<()> {
        self.write(move |conn| {
            conn.execute(
                "UPDATE adr SET external_ref = ?1 WHERE id = ?2",
                params![external_ref, id],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn list_adrs(
        &self,
        project_id: Uuid,
        status: Option<Status>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<Adr>> {
        self.read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, project_id, external_ref, number, slug, created_at, created_by,
                        cur_rev, cur_status
                 FROM adr
                 WHERE project_id = ?1 AND (?2 IS NULL OR cur_status = ?2)
                 ORDER BY COALESCE(number, 0) DESC, created_at DESC
                 LIMIT ?3 OFFSET ?4",
            )?;
            let status_text = status.map(|s| s.as_str());
            let rows = stmt.query_map(params![project_id, status_text, limit, offset], map_adr)?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await
    }

    pub async fn get_revision(&self, adr_id: Uuid, rev: i64) -> Result<AdrRevision> {
        self.read(move |conn| {
            conn.query_row(
                "SELECT adr_id, rev, title, template, source_format, source_bytes,
                        source_sha256, content_sha256, decided_on, tx_from, tx_to
                 FROM adr_rev WHERE adr_id = ?1 AND rev = ?2",
                params![adr_id, rev],
                map_revision,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("adr {adr_id} has no revision {rev}")))
        })
        .await
    }

    pub async fn get_sections(&self, adr_id: Uuid, rev: i64) -> Result<Vec<ParsedSection>> {
        self.read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, parent_id, kind, raw_heading, ordinal, text, option_key,
                        byte_start, byte_end
                 FROM section WHERE adr_id = ?1 AND rev = ?2 ORDER BY ordinal",
            )?;
            let rows = stmt.query_map(params![adr_id, rev], |row| {
                let kind_raw: String = row.get(2)?;
                Ok((
                    row.get::<_, Uuid>(0)?,
                    row.get::<_, Option<Uuid>>(1)?,
                    kind_raw,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (_, parent_id, kind_raw, raw_heading, ordinal, text, option_key, start, end) =
                    row?;
                out.push((
                    parent_id,
                    SectionKind::from_str_lossy(&kind_raw).unwrap_or(SectionKind::Other),
                    raw_heading,
                    ordinal,
                    text,
                    option_key,
                    start,
                    end,
                ));
            }
            // Parent links are stored as section ids; translate them to ordinals so the
            // caller gets the same shape the parser produced.
            let id_to_ordinal: std::collections::BTreeMap<Uuid, usize> = {
                let mut stmt =
                    conn.prepare("SELECT id, ordinal FROM section WHERE adr_id = ?1 AND rev = ?2")?;
                let rows = stmt.query_map(params![adr_id, rev], |row| {
                    Ok((row.get::<_, Uuid>(0)?, row.get::<_, i64>(1)?))
                })?;
                let mut map = std::collections::BTreeMap::new();
                for row in rows {
                    let (id, ordinal) = row?;
                    map.insert(id, ordinal as usize);
                }
                map
            };

            Ok(out
                .into_iter()
                .map(
                    |(parent_id, kind, raw_heading, ordinal, text, option_key, start, end)| {
                        ParsedSection {
                            kind,
                            raw_heading,
                            ordinal: ordinal as usize,
                            parent: parent_id.and_then(|id| id_to_ordinal.get(&id).copied()),
                            text,
                            option_key,
                            byte_start: start as usize,
                            byte_end: end as usize,
                        }
                    },
                )
                .collect())
        })
        .await
    }

    pub async fn get_items(&self, adr_id: Uuid, rev: i64) -> Result<Vec<ParsedItem>> {
        self.read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT i.kind, i.ordinal, i.text, i.option_key, i.byte_start, i.byte_end,
                        s.ordinal
                 FROM item i JOIN section s ON s.id = i.section_id
                 WHERE i.adr_id = ?1 AND i.rev = ?2
                 ORDER BY s.ordinal, i.ordinal",
            )?;
            let rows = stmt.query_map(params![adr_id, rev], |row| {
                let kind_raw: String = row.get(0)?;
                Ok((
                    kind_raw,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (kind_raw, ordinal, text, option_key, start, end, section_ordinal) = row?;
                out.push(ParsedItem {
                    kind: ItemKind::from_str_lossy(&kind_raw).unwrap_or(ItemKind::Driver),
                    section: section_ordinal as usize,
                    ordinal: ordinal as usize,
                    text,
                    option_key,
                    byte_start: start as usize,
                    byte_end: end as usize,
                });
            }
            Ok(out)
        })
        .await
    }

    pub async fn get_scopes(&self, adr_id: Uuid) -> Result<Vec<Scope>> {
        self.read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT kind, value, origin, confidence FROM scope WHERE adr_id = ?1
                 ORDER BY kind, value",
            )?;
            let rows = stmt.query_map(params![adr_id], |row| {
                let kind_raw: String = row.get(0)?;
                let origin_raw: String = row.get(2)?;
                Ok(Scope {
                    kind: ScopeKind::from_str_lossy(&kind_raw).unwrap_or(ScopeKind::PathPrefix),
                    value: row.get(1)?,
                    origin: ScopeOrigin::from_str_lossy(&origin_raw)
                        .unwrap_or(ScopeOrigin::Declared),
                    confidence: row.get::<_, f64>(3)? as f32,
                })
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await
    }

    pub async fn get_technologies(&self, adr_id: Uuid) -> Result<Vec<Technology>> {
        self.read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT canonical, role FROM technology WHERE adr_id = ?1 ORDER BY canonical",
            )?;
            let rows = stmt.query_map(params![adr_id], |row| {
                let role_raw: String = row.get(1)?;
                Ok(Technology {
                    canonical: row.get(0)?,
                    role: TechnologyRole::from_str_lossy(&role_raw)
                        .unwrap_or(TechnologyRole::Constrained),
                })
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await
    }

    pub async fn list_status_events(&self, adr_id: Uuid) -> Result<Vec<StatusEvent>> {
        self.read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, adr_id, from_status, to_status, effective_from, recorded_at,
                        actor, reason, superseded_by
                 FROM status_event WHERE adr_id = ?1 ORDER BY recorded_at, id",
            )?;
            let rows = stmt.query_map(params![adr_id], |row| {
                let from_raw: Option<String> = row.get(2)?;
                let to_raw: String = row.get(3)?;
                Ok(StatusEvent {
                    id: read_uuid(row, 0)?,
                    adr_id: read_uuid(row, 1)?,
                    from_status: from_raw.and_then(|s| s.parse::<Status>().ok()),
                    to_status: to_raw.parse::<Status>().unwrap_or(Status::Proposed),
                    effective_from: row.get(4)?,
                    recorded_at: row.get(5)?,
                    actor: row.get(6)?,
                    reason: row.get(7)?,
                    superseded_by: read_optional_uuid(row, 8)?,
                })
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await
    }

    pub async fn list_chunks(&self, adr_id: Uuid, rev: i64) -> Result<Vec<Chunk>> {
        self.read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, adr_id, rev, kind, ordinal, section_id, text, text_sha256,
                        token_count, truncated
                 FROM chunk WHERE adr_id = ?1 AND rev = ?2
                 ORDER BY kind, ordinal",
            )?;
            let rows = stmt.query_map(params![adr_id, rev], map_chunk)?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await
    }

    pub async fn get_chunk(&self, id: Uuid) -> Result<Option<Chunk>> {
        self.read(move |conn| {
            conn.query_row(
                "SELECT id, adr_id, rev, kind, ordinal, section_id, text, text_sha256,
                        token_count, truncated
                 FROM chunk WHERE id = ?1",
                params![id],
                map_chunk,
            )
            .optional()
            .or_store()
        })
        .await
    }

    /// Whether a chunk already has a vector for the given model version.
    pub async fn chunk_embedded(
        &self,
        chunk_id: Uuid,
        model: String,
        version: i64,
    ) -> Result<bool> {
        self.read(move |conn| {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM chunk_vector
                 WHERE chunk_id = ?1 AND model = ?2 AND version = ?3",
                params![chunk_id, model, version],
                |row| row.get(0),
            )?;
            Ok(count > 0)
        })
        .await
    }
}

fn map_adr(row: &Row<'_>) -> rusqlite::Result<Adr> {
    Ok(Adr {
        id: read_uuid(row, 0)?,
        project_id: read_uuid(row, 1)?,
        external_ref: row.get(2)?,
        number: row.get(3)?,
        slug: row.get(4)?,
        created_at: row.get(5)?,
        created_by: row.get(6)?,
        cur_rev: row.get(7)?,
        cur_status: read_status(row, 8)?,
    })
}

fn map_revision(row: &Row<'_>) -> rusqlite::Result<AdrRevision> {
    Ok(AdrRevision {
        adr_id: read_uuid(row, 0)?,
        rev: row.get(1)?,
        title: row.get(2)?,
        template: row.get(3)?,
        source_format: row.get(4)?,
        source_bytes: row.get(5)?,
        source_sha256: read_sha(row, 6)?,
        content_sha256: read_sha(row, 7)?,
        decided_on: row.get(8)?,
        tx_from: row.get(9)?,
        tx_to: row.get(10)?,
    })
}

fn map_chunk(row: &Row<'_>) -> rusqlite::Result<Chunk> {
    let kind_raw: String = row.get(3)?;
    Ok(Chunk {
        id: read_uuid(row, 0)?,
        adr_id: read_uuid(row, 1)?,
        rev: row.get(2)?,
        kind: ChunkKind::from_str_lossy(&kind_raw).unwrap_or(ChunkKind::Document),
        ordinal: row.get::<_, i64>(4)? as usize,
        section_id: read_optional_uuid(row, 5)?,
        text: row.get(6)?,
        text_sha256: read_sha(row, 7)?,
        token_count: row.get::<_, Option<i64>>(8)?.map(|n| n as usize),
        truncated: row.get::<_, i64>(9)? != 0,
    })
}

impl Store {
    /// The revision number `adr.cur_rev` points at.
    pub async fn get_current_revision(&self, adr_id: Uuid) -> Result<i64> {
        self.read(move |conn| {
            conn.query_row(
                "SELECT cur_rev FROM adr WHERE id = ?1",
                params![adr_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("adr {adr_id} does not exist")))
        })
        .await
    }

    /// A chunk by id, or `None`.
    pub async fn chunk_by_id(&self, id: Uuid) -> Result<Option<Chunk>> {
        self.get_chunk(id).await
    }

    /// Replace the scope rows of an ADR.
    pub async fn set_scopes(&self, adr_id: Uuid, scopes: Vec<Scope>) -> Result<()> {
        self.write(move |conn| {
            conn.execute("DELETE FROM scope WHERE adr_id = ?1", params![adr_id])?;
            for scope in &scopes {
                conn.execute(
                    "INSERT OR REPLACE INTO scope (adr_id, kind, value, origin, confidence)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        adr_id,
                        scope.kind.as_str(),
                        scope.value,
                        scope.origin.as_str(),
                        scope.confidence as f64,
                    ],
                )?;
            }
            Ok(())
        })
        .await
    }

    /// Replace the technology rows of an ADR.
    pub async fn set_technologies(
        &self,
        adr_id: Uuid,
        technologies: Vec<Technology>,
    ) -> Result<()> {
        self.write(move |conn| {
            conn.execute("DELETE FROM technology WHERE adr_id = ?1", params![adr_id])?;
            for technology in &technologies {
                conn.execute(
                    "INSERT OR REPLACE INTO technology (adr_id, canonical, role)
                     VALUES (?1, ?2, ?3)",
                    params![adr_id, technology.canonical, technology.role.as_str()],
                )?;
            }
            Ok(())
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// Status transitions
// ---------------------------------------------------------------------------

impl Store {
    /// Record a status change and update the convenience column.
    pub async fn record_status_event(
        &self,
        adr_id: Uuid,
        to: Status,
        actor: String,
        reason: Option<String>,
    ) -> Result<Uuid> {
        self.write(move |conn| {
            let from_raw: String = conn
                .query_row(
                    "SELECT cur_status FROM adr WHERE id = ?1",
                    params![adr_id],
                    |row| row.get(0),
                )
                .optional()?
                .ok_or_else(|| StoreError::NotFound(format!("adr {adr_id} does not exist")))?;
            let from = from_raw.parse::<Status>().ok();
            let id = Uuid::now_v7();
            conn.execute(
                "INSERT INTO status_event
                   (id, adr_id, from_status, to_status, effective_from, recorded_at, actor,
                    reason, superseded_by)
                 VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, ?7, NULL)",
                params![
                    id,
                    adr_id,
                    from.map(|s| s.as_str()),
                    to.as_str(),
                    now(),
                    actor,
                    reason,
                ],
            )?;
            conn.execute(
                "UPDATE adr SET cur_status = ?1 WHERE id = ?2",
                params![to.as_str(), adr_id],
            )?;
            Ok(id)
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// Embed jobs
// ---------------------------------------------------------------------------

/// One claimed unit of embedding work.
#[derive(Debug, Clone)]
pub struct EmbedJob {
    pub id: i64,
    pub chunk_id: Uuid,
    pub model: String,
    pub version: i64,
    pub role: EmbedRole,
    pub attempts: i64,
}

impl Store {
    /// Atomically claim up to `limit` pending jobs by flipping them to `running`.
    ///
    /// The claim is a single `UPDATE ... RETURNING`, so two workers cannot pick up the
    /// same row.
    pub async fn claim_embed_jobs(&self, limit: i64) -> Result<Vec<EmbedJob>> {
        self.write(move |conn| {
            let mut stmt = conn.prepare(
                "UPDATE embed_job
                 SET state = 'running'
                 WHERE id IN (
                     SELECT id FROM embed_job WHERE state = 'pending' ORDER BY id LIMIT ?1
                 )
                 RETURNING id, chunk_id, model, version, role, attempts",
            )?;
            let rows = stmt.query_map(params![limit], |row| {
                let role_raw: String = row.get(4)?;
                Ok(EmbedJob {
                    id: row.get(0)?,
                    chunk_id: read_uuid(row, 1)?,
                    model: row.get(2)?,
                    version: row.get(3)?,
                    role: if role_raw == "query" {
                        EmbedRole::Query
                    } else {
                        EmbedRole::Document
                    },
                    attempts: row.get(5)?,
                })
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await
    }

    /// Mark a job finished.
    pub async fn finish_embed_job(&self, id: i64) -> Result<()> {
        self.write(move |conn| {
            conn.execute("DELETE FROM embed_job WHERE id = ?1", params![id])?;
            Ok(())
        })
        .await
    }

    /// Record a failure. After [`MAX_EMBED_ATTEMPTS`] the job is parked as `failed`.
    pub async fn fail_embed_job(&self, id: i64, error: String) -> Result<i64> {
        self.write(move |conn| {
            conn.execute(
                "UPDATE embed_job
                 SET attempts = attempts + 1,
                     last_error = ?1,
                     state = CASE WHEN attempts + 1 >= ?2 THEN 'failed' ELSE 'pending' END
                 WHERE id = ?3",
                params![error, MAX_EMBED_ATTEMPTS, id],
            )?;
            let attempts: i64 = conn.query_row(
                "SELECT attempts FROM embed_job WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )?;
            Ok(attempts)
        })
        .await
    }

    /// Jobs that gave up, for `adrian check`.
    pub async fn failed_embed_jobs(&self) -> Result<Vec<(i64, Uuid, String)>> {
        self.read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, chunk_id, COALESCE(last_error, '') FROM embed_job
                 WHERE state = 'failed' ORDER BY id",
            )?;
            let rows =
                stmt.query_map([], |row| Ok((row.get(0)?, read_uuid(row, 1)?, row.get(2)?)))?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await
    }

    /// Chunks that have no vector for the given model version.
    pub async fn chunks_without_vectors(&self, model: String, version: i64) -> Result<Vec<Uuid>> {
        self.read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT c.id FROM chunk c
                 LEFT JOIN chunk_vector v
                   ON v.chunk_id = c.id AND v.model = ?1 AND v.version = ?2
                 WHERE v.chunk_id IS NULL
                 ORDER BY c.created_at, c.id",
            )?;
            let rows = stmt.query_map(params![model, version], |row| row.get::<_, Uuid>(0))?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await
    }

    /// Record that a chunk now has vectors in the index.
    #[allow(clippy::too_many_arguments)]
    pub async fn record_chunk_vector(
        &self,
        chunk_id: Uuid,
        model: String,
        version: i64,
        collection: String,
        point_id: Uuid,
        token_vectors: i64,
        dense: bool,
    ) -> Result<()> {
        self.write(move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO chunk_vector
                   (chunk_id, model, version, collection, point_id, token_vectors, dense,
                    created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    chunk_id,
                    model,
                    version,
                    collection,
                    point_id,
                    token_vectors,
                    if dense { 1i64 } else { 0i64 },
                    now(),
                ],
            )?;
            Ok(())
        })
        .await
    }

    /// Every vector row for a chunk, used by `adrian check`.
    pub async fn vectors_for_chunk(&self, chunk_id: Uuid) -> Result<Vec<(String, i64)>> {
        self.read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT model, version FROM chunk_vector WHERE chunk_id = ?1 ORDER BY model",
            )?;
            let rows = stmt.query_map(params![chunk_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await
    }
}

/// How many times the worker retries a chunk before parking it.
pub const MAX_EMBED_ATTEMPTS: i64 = 5;

// ---------------------------------------------------------------------------
// Lexical search
// ---------------------------------------------------------------------------

impl Store {
    /// Full-text search over chunk text using FTS5 and BM25.
    ///
    /// The caller fuses this with the vector results; this function knows nothing about
    /// embeddings.
    pub async fn search_lexical(
        &self,
        project_id: Uuid,
        query: String,
        limit: i64,
    ) -> Result<Vec<ScoredChunk>> {
        self.read(move |conn| {
            let Some(match_expr) = fts_query(&query) else {
                return Ok(Vec::new());
            };
            let mut stmt = conn.prepare(
                "SELECT c.id, c.adr_id, bm25(chunk_fts) AS rank
                 FROM chunk_fts
                 JOIN chunk c ON c.rowid = chunk_fts.rowid
                 JOIN adr a ON a.id = c.adr_id
                 WHERE chunk_fts MATCH ?1 AND a.project_id = ?2
                 ORDER BY rank
                 LIMIT ?3",
            )?;
            let rows = stmt.query_map(params![match_expr, project_id, limit], |row| {
                Ok(ScoredChunk {
                    chunk_id: read_uuid(row, 0)?,
                    adr_id: read_uuid(row, 1)?,
                    // BM25 in SQLite is negative and unbounded; expose it as-is and let
                    // the fusion step work on ranks rather than raw scores.
                    score: -(row.get::<_, f64>(2)? as f32),
                })
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await
    }

    /// Chunks whose scope value is an ancestor prefix of `path`, most specific first.
    pub async fn context_for_path(
        &self,
        project_id: Uuid,
        path: String,
        limit: i64,
    ) -> Result<Vec<Adr>> {
        self.read(move |conn| {
            let prefixes = ancestor_prefixes(&path);
            if prefixes.is_empty() {
                return Ok(Vec::new());
            }
            // Build a parameterised IN list over the ancestor prefixes.
            let placeholders = (0..prefixes.len())
                .map(|i| format!("?{}", i + 2))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT DISTINCT a.id, a.project_id, a.external_ref, a.number, a.slug,
                        a.created_at, a.created_by, a.cur_rev, a.cur_status,
                        (SELECT MAX(LENGTH(s.value)) FROM scope s
                          WHERE s.adr_id = a.id AND s.kind = 'path_prefix'
                            AND s.value IN ({placeholders})) AS specificity
                 FROM adr a
                 JOIN scope s2 ON s2.adr_id = a.id
                 WHERE a.project_id = ?1
                   AND s2.kind = 'path_prefix'
                   AND s2.value IN ({placeholders})
                 ORDER BY specificity DESC, a.created_at DESC
                 LIMIT ?{}",
                prefixes.len() + 2
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
            values.push(Box::new(project_id));
            for prefix in &prefixes {
                values.push(Box::new(prefix.clone()));
            }
            values.push(Box::new(limit));
            let refs: Vec<&dyn rusqlite::ToSql> = values.iter().map(|v| v.as_ref()).collect();
            let rows = stmt.query_map(refs.as_slice(), map_adr)?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await
    }
}

/// Every ancestor prefix of a path, including the path itself, in increasing specificity.
///
/// `crates/api/src/main.rs` yields `crates/`, `crates/api/`, `crates/api/src/`,
/// `crates/api/src/main.rs`. Scope values are normalised to have no trailing slash, so the
/// final element carries no slash either.
pub fn ancestor_prefixes(path: &str) -> Vec<String> {
    let cleaned = path.trim().trim_start_matches("./").trim_start_matches('/');
    if cleaned.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut current = String::new();
    let parts: Vec<&str> = cleaned.split('/').filter(|p| !p.is_empty()).collect();
    for (index, part) in parts.iter().enumerate() {
        if index + 1 == parts.len() {
            current.push_str(part);
        } else {
            current.push_str(part);
            current.push('/');
        }
        out.push(current.trim_end_matches('/').to_string());
    }
    out.retain(|p| !p.is_empty());
    out
}

/// Turn a user query into a safe FTS5 MATCH expression.
///
/// FTS5 has its own operator syntax, so arbitrary input must be quoted rather than passed
/// through. Each token becomes a quoted phrase and the tokens are ANDed.
fn fts_query(query: &str) -> Option<String> {
    let tokens: Vec<String> = query
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{}\"", t.replace('"', "")))
        .collect();
    if tokens.is_empty() {
        None
    } else {
        Some(tokens.join(" "))
    }
}

// ---------------------------------------------------------------------------
// Idempotency
// ---------------------------------------------------------------------------

impl Store {
    pub async fn lookup_ingest_response(
        &self,
        project_id: Uuid,
        key: String,
    ) -> Result<Option<([u8; 32], String)>> {
        self.read(move |conn| {
            conn.query_row(
                "SELECT request_sha256, response_json FROM ingest_request
                 WHERE project_id = ?1 AND idempotency_key = ?2",
                params![project_id, key],
                |row| Ok((read_sha(row, 0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .or_store()
        })
        .await
    }

    pub async fn store_ingest_response(
        &self,
        project_id: Uuid,
        key: String,
        sha: [u8; 32],
        response_json: String,
    ) -> Result<()> {
        self.write(move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO ingest_request
                   (project_id, idempotency_key, request_sha256, response_json, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![project_id, key, to_sql_blob(&sha), response_json, now()],
            )?;
            Ok(())
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// Aggregates for `adrian check`
// ---------------------------------------------------------------------------

impl Store {
    /// Total chunks and total vector rows, for the reconcile report.
    pub async fn counts(&self) -> Result<(i64, i64)> {
        self.read(|conn| {
            let chunks: i64 = conn.query_row("SELECT COUNT(*) FROM chunk", [], |row| row.get(0))?;
            let vectors: i64 =
                conn.query_row("SELECT COUNT(*) FROM chunk_vector", [], |row| row.get(0))?;
            Ok((chunks, vectors))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ScopeOrigin;

    async fn fresh_store() -> Store {
        let store = Store::open_in_memory().expect("in-memory store");
        store.migrate().await.expect("migrate");
        store
    }

    fn sample_project() -> Project {
        Project {
            id: Uuid::now_v7(),
            workspace_id: Uuid::nil(),
            slug: "demo".to_string(),
            name: "Demo".to_string(),
            repo_url: None,
            default_template: "madr".to_string(),
            embed_model: crate::EMBED_MODEL.to_string(),
            embed_version: crate::EMBED_VERSION,
            created_at: now(),
        }
    }

    fn sample_revision(project: &Project, adr_id: Uuid, chunk_text: &str) -> NewRevision {
        let section_id = Uuid::now_v7();
        let chunk_id = Uuid::new_v5(&Uuid::NAMESPACE_OID, b"chunk");
        NewRevision {
            adr_id,
            project_id: project.id,
            rev: 1,
            title: "Use tokio".to_string(),
            template: "madr".to_string(),
            source_format: "markdown".to_string(),
            source_bytes: b"# Use tokio\n".to_vec(),
            source_sha256: [7u8; 32],
            content_sha256: [9u8; 32],
            decided_on: Some("2024-03-01".to_string()),
            create_adr: true,
            external_ref: "docs/adr/0001-use-tokio.md".to_string(),
            number: Some(1),
            slug: "use-tokio".to_string(),
            created_by: "test".to_string(),
            status: Status::Proposed,
            actor: "test".to_string(),
            sections: vec![SectionRow {
                id: section_id,
                parent_id: None,
                kind: SectionKind::Context,
                raw_heading: "Context and Problem Statement".to_string(),
                ordinal: 0,
                text: "We need a runtime.".to_string(),
                option_key: None,
                byte_start: 0,
                byte_end: 20,
                content_sha256: [1u8; 32],
            }],
            items: vec![],
            chunks: vec![ChunkRow {
                id: chunk_id,
                kind: ChunkKind::Document,
                ordinal: 0,
                section_id: Some(section_id),
                text: chunk_text.to_string(),
                text_sha256: [2u8; 32],
                token_count: Some(3),
                truncated: false,
            }],
            scopes: vec![Scope {
                kind: ScopeKind::PathPrefix,
                value: "crates/api".to_string(),
                origin: ScopeOrigin::Declared,
                confidence: 1.0,
            }],
            technologies: vec![Technology {
                canonical: "tokio".to_string(),
                role: TechnologyRole::Chosen,
            }],
            embed_model: crate::EMBED_MODEL.to_string(),
            embed_version: crate::EMBED_VERSION,
        }
    }

    async fn seeded() -> (Store, Project, Uuid) {
        let store = fresh_store().await;
        let workspace_id = store.ensure_default_workspace().await.expect("workspace");
        let mut project = sample_project();
        project.workspace_id = workspace_id;
        store
            .create_project(project.clone())
            .await
            .expect("project");
        let adr_id = Uuid::now_v7();
        store
            .insert_adr_with_revision(sample_revision(&project, adr_id, "tokio async runtime"))
            .await
            .expect("revision");
        (store, project, adr_id)
    }

    #[tokio::test]
    async fn migration_is_idempotent() {
        let store = fresh_store().await;
        assert_eq!(store.schema_version().await.unwrap(), SCHEMA_VERSION);
        store.migrate().await.expect("second migrate is a no-op");
        assert_eq!(store.schema_version().await.unwrap(), SCHEMA_VERSION);
    }

    #[tokio::test]
    async fn create_workspace_rejects_duplicates() {
        let store = fresh_store().await;
        let first = store.create_workspace("local".to_string()).await.unwrap();
        let err = store
            .create_workspace("local".to_string())
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Conflict(_)));
        // The default workspace helper must then find the existing one.
        assert_eq!(store.ensure_default_workspace().await.unwrap(), first);
    }

    #[tokio::test]
    async fn current_revision_and_chunk_lookup() {
        let (store, _, adr_id) = seeded().await;
        assert_eq!(store.get_current_revision(adr_id).await.unwrap(), 1);
        let chunks = store.list_chunks(adr_id, 1).await.unwrap();
        assert_eq!(chunks.len(), 1);
        let by_id = store
            .chunk_by_id(chunks[0].id)
            .await
            .unwrap()
            .expect("chunk");
        assert_eq!(by_id.id, chunks[0].id);
        assert!(store.chunk_by_id(Uuid::now_v7()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn scopes_and_technologies_can_be_replaced() {
        let (store, _, adr_id) = seeded().await;
        assert_eq!(store.get_scopes(adr_id).await.unwrap().len(), 1);
        store
            .set_scopes(
                adr_id,
                vec![
                    Scope {
                        kind: ScopeKind::PathPrefix,
                        value: "src".to_string(),
                        origin: ScopeOrigin::Declared,
                        confidence: 1.0,
                    },
                    Scope {
                        kind: ScopeKind::Crate,
                        value: "api".to_string(),
                        origin: ScopeOrigin::Extracted,
                        confidence: 0.6,
                    },
                ],
            )
            .await
            .unwrap();
        let scopes = store.get_scopes(adr_id).await.unwrap();
        assert_eq!(scopes.len(), 2);
        assert!(!scopes.iter().any(|s| s.value == "crates/api"));

        store.set_technologies(adr_id, vec![]).await.unwrap();
        assert!(store.get_technologies(adr_id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn pragmas_are_applied() {
        let store = fresh_store().await;
        let (journal, foreign, busy) = store
            .read(|conn| {
                let journal: String =
                    conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
                let foreign: i64 = conn.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
                let busy: i64 = conn.query_row("PRAGMA busy_timeout", [], |row| row.get(0))?;
                Ok((journal, foreign, busy))
            })
            .await
            .expect("pragmas");
        // An in-memory database cannot use WAL, so it reports `memory`; the file-backed
        // path is what matters and is asserted in the file test below.
        assert!(
            journal == "wal" || journal == "memory",
            "journal_mode was {journal}"
        );
        assert_eq!(foreign, 1);
        assert_eq!(busy, 5000);
    }

    #[tokio::test]
    async fn file_database_uses_wal() {
        let dir = std::env::temp_dir().join(format!("adrian-test-{}", Uuid::new_v4()));
        let path = dir.join("adrian.db");
        let store = Store::open(&path).expect("open");
        store.migrate().await.expect("migrate");
        let journal: String = store
            .read(|conn| {
                let value: String = conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
                Ok(value)
            })
            .await
            .expect("journal");
        assert_eq!(journal, "wal");
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn insert_revision_is_atomic() {
        let store = fresh_store().await;
        let workspace_id = store.ensure_default_workspace().await.unwrap();
        let mut project = sample_project();
        project.workspace_id = workspace_id;
        store.create_project(project.clone()).await.unwrap();

        let adr_id = Uuid::now_v7();
        let mut revision = sample_revision(&project, adr_id, "text");
        // Two chunks with the same (adr, rev, kind, ordinal) violate the unique index,
        // so the second insert must abort the whole transaction.
        let duplicate = revision.chunks[0].clone();
        revision.chunks.push(duplicate);

        let result = store.insert_adr_with_revision(revision).await;
        assert!(
            result.is_err(),
            "duplicate chunk must abort the transaction"
        );

        // Every table the revision touches must be empty again. A partial write would leave
        // an ADR with no sections, or sections with no chunks, and nothing would ever
        // repair it.
        let (adrs, revs, sections, items, chunks, vectors, jobs): (
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
        ) = store
            .read(|conn| {
                Ok((
                    conn.query_row("SELECT COUNT(*) FROM adr", [], |r| r.get(0))?,
                    conn.query_row("SELECT COUNT(*) FROM adr_rev", [], |r| r.get(0))?,
                    conn.query_row("SELECT COUNT(*) FROM section", [], |r| r.get(0))?,
                    conn.query_row("SELECT COUNT(*) FROM item", [], |r| r.get(0))?,
                    conn.query_row("SELECT COUNT(*) FROM chunk", [], |r| r.get(0))?,
                    conn.query_row("SELECT COUNT(*) FROM chunk_vector", [], |r| r.get(0))?,
                    conn.query_row("SELECT COUNT(*) FROM embed_job", [], |r| r.get(0))?,
                ))
            })
            .await
            .unwrap();
        assert_eq!(
            (adrs, revs, sections, items, chunks, vectors, jobs),
            (0, 0, 0, 0, 0, 0, 0),
            "a failed revision must leave no rows behind in any table"
        );

        // The same revision, without the duplicate, still succeeds afterwards: the failed
        // transaction must not have poisoned the connection.
        let adr_id = Uuid::now_v7();
        let clean = sample_revision(&project, adr_id, "text");
        store
            .insert_adr_with_revision(clean)
            .await
            .expect("a valid revision after a rolled back one");
        let adrs: i64 = store
            .read(|conn| Ok(conn.query_row("SELECT COUNT(*) FROM adr", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(adrs, 1);
    }

    #[tokio::test]
    async fn fts_index_tracks_inserts() {
        let (store, project, adr_id) = seeded().await;

        // The FTS row must resolve back to the exact chunk that carries the text, not just
        // to the right ADR: search hydrates chunk text by chunk_id, so a wrong id would
        // return the wrong paragraph.
        let expected: Vec<Uuid> = store
            .list_chunks(adr_id, 1)
            .await
            .unwrap()
            .into_iter()
            .filter(|chunk| chunk.kind == ChunkKind::Document)
            .map(|chunk| chunk.id)
            .collect();
        assert_eq!(expected.len(), 1, "the seed should have one document chunk");

        let hits = store
            .search_lexical(project.id, "tokio runtime".to_string(), 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].chunk_id, expected[0], "wrong chunk_id from FTS");
        assert_eq!(hits[0].adr_id, adr_id);

        let misses = store
            .search_lexical(project.id, "kubernetes".to_string(), 10)
            .await
            .unwrap();
        assert!(misses.is_empty());

        // And the index follows later inserts, so it is a live index rather than a snapshot
        // taken once at migration time.
        let second_id = Uuid::now_v7();
        let mut second = sample_revision(&project, second_id, "kubernetes manifests");
        second.external_ref = "docs/adr/0002-use-kubernetes.md".to_string();
        second.slug = "use-kubernetes".to_string();
        second.number = Some(2);
        // A second ADR needs its own row ids; the sample reuses fixed ones.
        second.sections[0].id = Uuid::now_v7();
        second.chunks.truncate(1);
        for (index, chunk) in second.chunks.iter_mut().enumerate() {
            chunk.id = Uuid::now_v7();
            chunk.ordinal = index;
            chunk.section_id = Some(second.sections[0].id);
        }
        store.insert_adr_with_revision(second).await.unwrap();

        let after = store
            .search_lexical(project.id, "kubernetes".to_string(), 10)
            .await
            .unwrap();
        assert_eq!(after.len(), 1, "the newly inserted chunk should be found");
        assert_eq!(after[0].adr_id, second_id);

        // The first document is still findable, so the insert did not clobber the index.
        let still_there = store
            .search_lexical(project.id, "tokio runtime".to_string(), 10)
            .await
            .unwrap();
        assert_eq!(still_there.len(), 1);
        assert_eq!(still_there[0].chunk_id, expected[0]);
    }

    #[tokio::test]
    async fn fts_respects_project_boundaries() {
        let (store, project, _) = seeded().await;
        let other_id = Uuid::now_v7();
        let mut other = sample_project();
        other.id = other_id;
        other.slug = "other".to_string();
        other.workspace_id = project.workspace_id;
        store.create_project(other.clone()).await.unwrap();

        let hits = store
            .search_lexical(other_id, "tokio".to_string(), 10)
            .await
            .unwrap();
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn embed_jobs_are_claimed_once() {
        let (store, _, _) = seeded().await;
        let first = store.claim_embed_jobs(10).await.unwrap();
        assert_eq!(first.len(), 1);
        let second = store.claim_embed_jobs(10).await.unwrap();
        assert!(
            second.is_empty(),
            "a claimed job must not be handed out twice"
        );
    }

    #[tokio::test]
    async fn concurrent_claimers_never_receive_the_same_job() {
        // The real contention case: several workers polling the same file database. Claiming
        // is `UPDATE ... RETURNING` inside a write transaction, so it has to serialize rather
        // than hand the same row to two callers.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("claim.db");

        let setup = Store::open(&path).expect("open");
        setup.migrate().await.expect("migrate");
        let workspace_id = setup.ensure_default_workspace().await.unwrap();
        let mut project = sample_project();
        project.workspace_id = workspace_id;
        setup.create_project(project.clone()).await.unwrap();

        // Enough jobs that every claimer can win one if claiming is broken.
        const REVISIONS: usize = 8;
        for index in 0..REVISIONS {
            let adr_id = Uuid::now_v7();
            let mut revision = sample_revision(&project, adr_id, "text");
            revision.external_ref = format!("docs/adr/{index:04}.md");
            revision.slug = format!("adr-{index}");
            revision.number = Some(index as i64);
            revision.sections[0].id = Uuid::now_v7();
            for (ordinal, chunk) in revision.chunks.iter_mut().enumerate() {
                chunk.id = Uuid::now_v7();
                chunk.ordinal = ordinal;
                chunk.section_id = Some(revision.sections[0].id);
            }
            setup.insert_adr_with_revision(revision).await.unwrap();
        }

        const CLAIMERS: usize = 8;
        let mut handles = Vec::with_capacity(CLAIMERS);
        for _ in 0..CLAIMERS {
            let path = path.clone();
            handles.push(tokio::task::spawn_blocking(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("runtime");
                runtime.block_on(async move {
                    let store = Store::open(&path).expect("open");
                    store.claim_embed_jobs(2).await.expect("claim")
                })
            }));
        }

        let mut claimed: Vec<i64> = Vec::new();
        for handle in handles {
            for job in handle.await.expect("join") {
                claimed.push(job.id);
            }
        }

        let total = claimed.len();
        claimed.sort();
        claimed.dedup();
        assert_eq!(
            total,
            claimed.len(),
            "two concurrent callers were handed the same job"
        );
        assert_eq!(
            total, REVISIONS,
            "every pending job should be claimed exactly once"
        );
    }

    #[tokio::test]
    async fn failing_a_job_five_times_parks_it() {
        let (store, _, _) = seeded().await;
        let jobs = store.claim_embed_jobs(10).await.unwrap();
        let id = jobs[0].id;
        for attempt in 1..=MAX_EMBED_ATTEMPTS {
            let attempts = store
                .fail_embed_job(id, format!("boom {attempt}"))
                .await
                .unwrap();
            assert_eq!(attempts, attempt);
        }
        let failed = store.failed_embed_jobs().await.unwrap();
        assert_eq!(failed.len(), 1);
        assert!(store.claim_embed_jobs(10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn status_transition_updates_the_convenience_column() {
        let (store, _, adr_id) = seeded().await;
        assert_eq!(
            store.get_adr(adr_id).await.unwrap().cur_status,
            Status::Proposed
        );
        store
            .record_status_event(
                adr_id,
                Status::Accepted,
                "human:hanz".to_string(),
                Some("reviewed".to_string()),
            )
            .await
            .unwrap();
        assert_eq!(
            store.get_adr(adr_id).await.unwrap().cur_status,
            Status::Accepted
        );
        let events = store.list_status_events(adr_id).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].to_status, Status::Accepted);
        assert_eq!(events[1].from_status, Some(Status::Proposed));
    }

    #[tokio::test]
    async fn status_transition_on_unknown_adr_is_not_found() {
        let store = fresh_store().await;
        let err = store
            .record_status_event(Uuid::now_v7(), Status::Accepted, "x".into(), None)
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::NotFound(_)));
    }

    #[tokio::test]
    async fn rename_lookup_finds_the_same_bytes() {
        let (store, project, adr_id) = seeded().await;
        let found = store
            .find_adr_by_source_sha(project.id, [7u8; 32])
            .await
            .unwrap()
            .expect("rename detection should find the original");
        assert_eq!(found.id, adr_id);
        store
            .rename_external_ref(adr_id, "docs/adr/0042-use-tokio.md".to_string())
            .await
            .unwrap();
        let updated = store.get_adr(adr_id).await.unwrap();
        assert_eq!(updated.external_ref, "docs/adr/0042-use-tokio.md");
    }

    #[tokio::test]
    async fn context_for_path_matches_ancestor_prefixes() {
        let (store, project, adr_id) = seeded().await;
        let hits = store
            .context_for_path(project.id, "crates/api/src/handler.rs".to_string(), 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, adr_id);

        let misses = store
            .context_for_path(project.id, "crates/web/src/lib.rs".to_string(), 10)
            .await
            .unwrap();
        assert!(misses.is_empty());
    }

    #[tokio::test]
    async fn ancestor_prefixes_are_cumulative() {
        assert_eq!(
            ancestor_prefixes("crates/api/src/main.rs"),
            vec![
                "crates".to_string(),
                "crates/api".to_string(),
                "crates/api/src".to_string(),
                "crates/api/src/main.rs".to_string(),
            ]
        );
        assert_eq!(
            ancestor_prefixes("./src/lib.rs"),
            vec!["src".to_string(), "src/lib.rs".to_string()]
        );
        assert!(ancestor_prefixes("").is_empty());
    }

    #[tokio::test]
    async fn idempotency_records_round_trip() {
        let (store, project, _) = seeded().await;
        assert!(store
            .lookup_ingest_response(project.id, "k1".to_string())
            .await
            .unwrap()
            .is_none());
        store
            .store_ingest_response(project.id, "k1".to_string(), [3u8; 32], "{}".to_string())
            .await
            .unwrap();
        let found = store
            .lookup_ingest_response(project.id, "k1".to_string())
            .await
            .unwrap()
            .expect("stored");
        assert_eq!(found.0, [3u8; 32]);
        assert_eq!(found.1, "{}");
    }

    #[tokio::test]
    async fn chunks_without_vectors_lists_everything_initially() {
        let (store, _, _) = seeded().await;
        let missing = store
            .chunks_without_vectors(crate::EMBED_MODEL.to_string(), crate::EMBED_VERSION)
            .await
            .unwrap();
        assert_eq!(missing.len(), 1);

        store
            .record_chunk_vector(
                missing[0],
                crate::EMBED_MODEL.to_string(),
                crate::EMBED_VERSION,
                "c".to_string(),
                Uuid::now_v7(),
                5,
                true,
            )
            .await
            .unwrap();
        let missing = store
            .chunks_without_vectors(crate::EMBED_MODEL.to_string(), crate::EMBED_VERSION)
            .await
            .unwrap();
        assert!(missing.is_empty());
    }

    // -----------------------------------------------------------------------
    // Project deletion
    // -----------------------------------------------------------------------

    /// A revision whose section and chunk ids are unique to `(adr_id, rev)`.
    ///
    /// [`sample_revision`] reuses one deterministic chunk id, which is fine for a single
    /// revision and impossible for a corpus with a superseded revision in it.
    fn revision_for(project: &Project, adr_id: Uuid, rev: i64, seed: &str) -> NewRevision {
        let mut revision = sample_revision(project, adr_id, &format!("tokio runtime {seed}"));
        revision.rev = rev;
        revision.create_adr = rev == 1;
        revision.external_ref = format!("docs/adr/{seed}.md");
        revision.slug = format!("adr-{seed}");
        revision.number = Some(rev);
        revision.sections[0].id = Uuid::new_v5(
            &Uuid::NAMESPACE_OID,
            format!("section-{adr_id}-{rev}").as_bytes(),
        );
        let section_id = revision.sections[0].id;
        for (ordinal, chunk) in revision.chunks.iter_mut().enumerate() {
            chunk.id = Uuid::new_v5(
                &Uuid::NAMESPACE_OID,
                format!("chunk-{adr_id}-{rev}-{ordinal}").as_bytes(),
            );
            chunk.ordinal = ordinal;
            chunk.section_id = Some(section_id);
        }
        revision
    }

    const EVERY_TABLE: [&str; 13] = [
        "project",
        "adr",
        "adr_rev",
        "section",
        "item",
        "chunk",
        "chunk_fts",
        "chunk_vector",
        "status_event",
        "scope",
        "technology",
        "embed_job",
        "ingest_request",
    ];

    /// Row counts for every table the cascade touches, so a delete can be checked as a
    /// whole rather than one table at a time.
    async fn table_counts(store: &Store) -> Vec<(&'static str, i64)> {
        store
            .read(|conn| {
                let mut out = Vec::new();
                for table in EVERY_TABLE {
                    let count: i64 =
                        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                            row.get(0)
                        })?;
                    out.push((table, count));
                }
                Ok(out)
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn deleting_a_project_removes_every_row_and_leaves_others_alone() {
        let store = fresh_store().await;
        let workspace = store.ensure_default_workspace().await.unwrap();

        let mut demo = sample_project();
        demo.workspace_id = workspace;
        demo.slug = "demo".to_string();
        store.create_project(demo.clone()).await.unwrap();

        let mut other = sample_project();
        other.id = Uuid::now_v7();
        other.workspace_id = workspace;
        other.slug = "other".to_string();
        store.create_project(other.clone()).await.unwrap();

        // demo holds one ADR at revision 2 and one at revision 1, so the cascade has to
        // clear a superseded revision as well as the current ones.
        let first = Uuid::now_v7();
        store
            .insert_adr_with_revision(revision_for(&demo, first, 1, "0001"))
            .await
            .unwrap();
        store.close_previous_revision(first, 1).await.unwrap();
        store
            .insert_adr_with_revision(revision_for(&demo, first, 2, "0001"))
            .await
            .unwrap();
        let second = Uuid::now_v7();
        store
            .insert_adr_with_revision(revision_for(&demo, second, 1, "0002"))
            .await
            .unwrap();

        // The other project keeps one ADR, so "untouched" is not vacuously true.
        let keeper = Uuid::now_v7();
        store
            .insert_adr_with_revision(revision_for(&other, keeper, 1, "0001"))
            .await
            .unwrap();

        let chunks = store.list_chunks(first, 2).await.unwrap();
        store
            .record_chunk_vector(
                chunks[0].id,
                crate::EMBED_MODEL.to_string(),
                crate::EMBED_VERSION,
                "test".to_string(),
                Uuid::now_v7(),
                4,
                true,
            )
            .await
            .unwrap();
        store
            .store_ingest_response(demo.id, "key-1".to_string(), [3u8; 32], "{}".to_string())
            .await
            .unwrap();

        let before = store.project_footprint(demo.id).await.unwrap();
        assert_eq!(before.adrs, 2);
        assert_eq!(
            before.revisions, 3,
            "one superseded revision plus two current ones"
        );
        assert_eq!(before.chunks, 3);
        assert_eq!(before.vectors, 1);
        // Three chunks were written, but superseding revision 1 cancelled its queued job,
        // so only the two current revisions still have work pending.
        assert_eq!(before.embed_jobs, 2);

        let hits = store
            .search_lexical(demo.id, "tokio".to_string(), 10)
            .await
            .unwrap();
        assert!(
            !hits.is_empty(),
            "the corpus should be searchable before the delete"
        );

        let deleted = store.delete_project(demo.id).await.unwrap();
        assert_eq!(deleted, before, "the delete must report what it removed");

        assert!(store
            .get_project_by_slug("demo".to_string())
            .await
            .unwrap()
            .is_none());
        assert!(store
            .list_adrs(demo.id, None, 100, 0)
            .await
            .unwrap()
            .is_empty());
        assert!(
            store
                .search_lexical(demo.id, "tokio".to_string(), 10)
                .await
                .unwrap()
                .is_empty(),
            "the full-text index must not keep the deleted project's chunks"
        );

        // What survives is exactly the other project's single ADR, in every table.
        let expected = vec![
            ("project", 1),
            ("adr", 1),
            ("adr_rev", 1),
            ("section", 1),
            ("item", 0),
            ("chunk", 1),
            ("chunk_fts", 1),
            ("chunk_vector", 0),
            ("status_event", 1),
            ("scope", 1),
            ("technology", 1),
            ("embed_job", 1),
            ("ingest_request", 0),
        ];
        assert_eq!(table_counts(&store).await, expected);

        // And the survivor is still readable and searchable.
        let survivor = store.get_adr(keeper).await.unwrap();
        assert_eq!(survivor.project_id, other.id);
        assert!(!store
            .search_lexical(other.id, "tokio".to_string(), 10)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn deleting_an_unknown_project_is_not_found_and_changes_nothing() {
        let (store, project, _) = seeded().await;
        let before = table_counts(&store).await;

        let err = store.delete_project(Uuid::now_v7()).await.unwrap_err();
        assert!(matches!(err, StoreError::NotFound(_)), "got {err:?}");
        assert_eq!(table_counts(&store).await, before);

        // The real project is still there, so the failure was not a partial cascade.
        assert!(store
            .get_project_by_slug(project.slug.clone())
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn a_project_with_nothing_in_it_still_deletes() {
        let store = fresh_store().await;
        let workspace = store.ensure_default_workspace().await.unwrap();
        let mut project = sample_project();
        project.workspace_id = workspace;
        store.create_project(project.clone()).await.unwrap();

        let footprint = store.delete_project(project.id).await.unwrap();
        assert_eq!(footprint, ProjectFootprint::default());
        assert!(store.list_projects().await.unwrap().is_empty());
    }
}
