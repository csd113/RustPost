use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::Context as _;
use rusqlite::{Connection, OpenFlags, OptionalExtension as _};

pub const CURRENT_SCHEMA_VERSION: i64 = 4;

/// Pre-1.0 schema version 1 (without the `YouTube` embed tables).
const RELEASE_V1_SCHEMA_VERSION: i64 = 1;
/// Pre-1.0 schema version 2 (with the `YouTube` embed tables).
const RELEASE_V2_SCHEMA_VERSION: i64 = 2;
/// Pre-1.0 schema version 3 (with the release indexes).
const RELEASE_V3_SCHEMA_VERSION: i64 = 3;
const OLD_ALPHA_SCHEMA_VERSION: i64 = 13;
const INCOMPATIBLE_DATABASE_HINT: &str = "back up or export the instance, recreate a fresh RustPost data directory, and restore from a known-good backup instead of attempting a blind migration";

#[derive(Clone)]
pub struct Db {
    sender: Arc<tokio::sync::mpsc::Sender<Job>>,
    snapshot_lock_path: Arc<PathBuf>,
}

type Job = Box<dyn FnOnce(&mut Connection) + Send + 'static>;

impl Db {
    pub async fn call<F, T>(&self, f: F) -> anyhow::Result<T>
    where
        F: FnOnce(&mut Connection) -> anyhow::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        // Move the lease into the queued job so cancellation cannot release
        // snapshot coordination before an already-admitted write commits.
        let snapshot_lease = std::fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.snapshot_lock_path.as_ref())?;
        snapshot_lease
            .try_lock_shared()
            .context("a backup snapshot is in progress")?;
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        self.sender
            .send(Box::new(move |conn| {
                let _snapshot_lease = snapshot_lease;
                let _ = result_tx.send(f(conn));
            }))
            .await
            .map_err(|_send_err| anyhow::anyhow!("database worker stopped"))?;
        result_rx
            .await
            .map_err(|_recv_err| anyhow::anyhow!("database worker stopped"))?
    }
}

pub type SqlitePool = Db;

pub async fn connect(path: &Path) -> anyhow::Result<Db> {
    let snapshot_lock_path = path
        .parent()
        .and_then(Path::parent)
        .context("database path must be inside the runtime db directory")?
        .join("snapshot-coordination.lock");
    let path = path.to_owned();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::channel::<Job>(256);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    thread::Builder::new()
        .name("rustpost-db".to_owned())
        .spawn(move || {
            let ready = open_connection(&path);
            let Ok(mut conn) = ready else {
                let _ = ready_tx.send(ready.map(|_| ()));
                return;
            };
            let _ = ready_tx.send(Ok(()));
            while let Some(job) = job_rx.blocking_recv() {
                job(&mut conn);
            }
        })
        .context("failed to spawn database worker")?;
    ready_rx
        .await
        .map_err(|_recv_err| anyhow::anyhow!("database worker stopped during startup"))??;
    Ok(Db {
        sender: Arc::new(job_tx),
        snapshot_lock_path: Arc::new(snapshot_lock_path),
    })
}

fn open_connection(path: &Path) -> anyhow::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    register_sql_helpers(&conn)?;
    Ok(conn)
}

/// Registers the Rust-backed SQL helpers used by application queries.
///
/// `rustpost_muted_word_match(text, term)` performs a Unicode-aware,
/// case-insensitive substring test. `SQLite`'s built-in `lower()` only folds
/// ASCII characters, which made muted-word matching miss uppercase accented
/// text. Registering this on the connection keeps the check inside SQL so
/// muted rows are filtered before `LIMIT` is applied.
fn register_sql_helpers(conn: &Connection) -> anyhow::Result<()> {
    conn.create_scalar_function(
        "rustpost_muted_word_match",
        2,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8
            | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
        |context| {
            let text = context.get::<Option<String>>(0)?.unwrap_or_default();
            let term = context.get::<Option<String>>(1)?.unwrap_or_default();
            if term.is_empty() {
                return Ok(false);
            }
            Ok(text.to_lowercase().contains(&term.to_lowercase()))
        },
    )?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaReport {
    version: Option<i64>,
    issues: Vec<String>,
}

impl SchemaReport {
    #[must_use]
    pub const fn version(&self) -> Option<i64> {
        self.version
    }

    #[must_use]
    pub fn issues(&self) -> &[String] {
        &self.issues
    }

    #[must_use]
    pub fn is_compatible(&self) -> bool {
        self.issues.is_empty()
    }

    #[must_use]
    pub fn summary(&self) -> String {
        if self.is_compatible() {
            return format!("release baseline schema version {CURRENT_SCHEMA_VERSION}");
        }
        summarize_issues(&self.issues)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MigrationState {
    Empty,
    Baseline,
    ReleaseVersion3,
    ReleaseVersion2,
    ReleaseVersion1,
    AlphaLatest,
    Incompatible(String),
}

#[derive(Debug, Clone, Copy)]
struct RequiredColumn {
    table: &'static str,
    name: &'static str,
    type_name: &'static str,
    not_null: bool,
    default_value: Option<&'static str>,
    primary_key_position: i64,
}

#[derive(Debug, Clone, Copy)]
struct RequiredIndex {
    table: &'static str,
    name: &'static str,
    unique: bool,
}

#[derive(Debug, Clone, Copy)]
struct RequiredTrigger {
    table: &'static str,
    name: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ColumnInfo {
    name: String,
    type_name: String,
    not_null: bool,
    default_value: Option<String>,
    primary_key_position: i64,
}

mod migrations;
pub use migrations::migrate;
use migrations::{
    migrate_release_v1_to_current, migrate_release_v2_to_current, migrate_release_v3_to_current,
    migration_state, reset_schema_migrations_to,
};
mod inspect;
pub use inspect::{
    adoptable_schema_summary_from_path, normalize_restorable_schema, schema_report,
    schema_report_from_path, schema_version_from_connection, validate_restorable_schema,
    validate_schema,
};
use inspect::{
    inspect_alpha_schema_objects, migration_versions, quoted_identifier, summarize_issues,
    table_columns, table_exists, user_schema_objects,
};
mod schema;
use schema::{
    BASELINE_SCHEMA, FTS_SHADOW_TABLES, RELEASE_V3_INDEXES, RELEASE_V4_SCHEMA, REQUIRED_COLUMNS,
    REQUIRED_INDEXES, REQUIRED_TABLES, REQUIRED_TRIGGERS, USER_COLUMNS_ADDED_IN_V4,
    YOUTUBE_EMBEDS_SCHEMA,
};
