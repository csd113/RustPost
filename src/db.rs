use std::path::Path;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use anyhow::Context as _;
use rusqlite::{Connection, OptionalExtension as _};

pub const CURRENT_SCHEMA_VERSION: i64 = 1;

const OLD_ALPHA_SCHEMA_VERSION: i64 = 13;
const INCOMPATIBLE_DATABASE_HINT: &str = "back up or export the instance, recreate a fresh RustPost data directory, and restore from a known-good backup instead of attempting a blind migration";

#[derive(Clone)]
pub struct Db {
    sender: Arc<mpsc::Sender<Job>>,
}

type Job = Box<dyn FnOnce(&mut Connection) + Send + 'static>;

impl Db {
    pub async fn call<F, T>(&self, f: F) -> anyhow::Result<T>
    where
        F: FnOnce(&mut Connection) -> anyhow::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        self.sender
            .send(Box::new(move |conn| {
                let _ = result_tx.send(f(conn));
            }))
            .map_err(|_send_err| anyhow::anyhow!("database worker stopped"))?;
        result_rx
            .await
            .map_err(|_recv_err| anyhow::anyhow!("database worker stopped"))?
    }
}

pub type SqlitePool = Db;

pub async fn connect(path: &Path) -> anyhow::Result<Db> {
    let path = path.to_owned();
    let (job_tx, job_rx) = mpsc::channel::<Job>();
    let (ready_tx, ready_rx) = mpsc::channel();
    thread::Builder::new()
        .name("rustpost-db".to_owned())
        .spawn(move || {
            let ready = open_connection(&path);
            let Ok(mut conn) = ready else {
                let _ = ready_tx.send(ready.map(|_| ()));
                return;
            };
            let _ = ready_tx.send(Ok(()));
            while let Ok(job) = job_rx.recv() {
                job(&mut conn);
            }
        })
        .context("failed to spawn database worker")?;
    ready_rx
        .recv()
        .map_err(|_recv_err| anyhow::anyhow!("database worker stopped during startup"))??;
    Ok(Db {
        sender: Arc::new(job_tx),
    })
}

fn open_connection(path: &Path) -> anyhow::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    Ok(conn)
}

pub async fn migrate(pool: &Db) -> anyhow::Result<()> {
    pool.call(|conn| {
        let tx = conn.transaction()?;
        ensure_release_baseline(&tx)?;
        tx.commit()?;
        Ok(())
    })
    .await
}

pub async fn schema_report(pool: &Db) -> anyhow::Result<SchemaReport> {
    pool.call(|conn| inspect_schema(conn)).await
}

pub fn validate_schema(conn: &Connection) -> anyhow::Result<()> {
    let report = inspect_schema(conn)?;
    if report.is_compatible() {
        return Ok(());
    }
    anyhow::bail!(
        "database schema is incompatible with RustPost release baseline: {}",
        report.summary()
    );
}

pub fn validate_restorable_schema(conn: &Connection) -> anyhow::Result<()> {
    let version = schema_version_from_connection(conn)?;
    if version == CURRENT_SCHEMA_VERSION {
        return validate_schema(conn);
    }
    if version == OLD_ALPHA_SCHEMA_VERSION {
        let issues = inspect_schema_objects(conn)?;
        if issues.is_empty() {
            return Ok(());
        }
        anyhow::bail!(
            "restored database has incomplete old alpha schema: {}",
            summarize_issues(&issues)
        );
    }
    anyhow::bail!(
        "database schema version {version} is not supported by this RustPost release baseline"
    );
}

pub fn schema_version_from_connection(conn: &Connection) -> anyhow::Result<i64> {
    if !table_exists(conn, "schema_migrations")? {
        anyhow::bail!("database has no schema_migrations table");
    }
    migration_versions(conn)?
        .into_iter()
        .max()
        .ok_or_else(|| anyhow::anyhow!("database has no schema migration version"))
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

fn ensure_release_baseline(conn: &Connection) -> anyhow::Result<()> {
    match migration_state(conn)? {
        MigrationState::Empty => initialize_release_baseline(conn)?,
        MigrationState::Baseline => {}
        MigrationState::AlphaLatest => adopt_latest_alpha_schema(conn)?,
        MigrationState::Incompatible(reason) => {
            anyhow::bail!("{reason}. {INCOMPATIBLE_DATABASE_HINT}");
        }
    }

    validate_schema(conn).with_context(|| INCOMPATIBLE_DATABASE_HINT)
}

fn migration_state(conn: &Connection) -> anyhow::Result<MigrationState> {
    let schema_objects = user_schema_objects(conn)?;
    if !table_exists(conn, "schema_migrations")? {
        if schema_objects.is_empty() {
            return Ok(MigrationState::Empty);
        }
        return Ok(MigrationState::Incompatible(format!(
            "database has schema objects but no migration metadata: {}",
            summarize_issues(&schema_objects)
        )));
    }

    let versions = migration_versions(conn)?;
    if versions.is_empty() {
        if schema_objects
            .iter()
            .all(|name| name == "schema_migrations")
        {
            return Ok(MigrationState::Empty);
        }
        return Ok(MigrationState::Incompatible(
            "database has schema objects but no migration version".to_owned(),
        ));
    }

    let Some(latest) = versions.last().copied() else {
        return Ok(MigrationState::Incompatible(
            "database has no migration version".to_owned(),
        ));
    };
    if versions == [CURRENT_SCHEMA_VERSION] {
        return Ok(MigrationState::Baseline);
    }
    if latest == OLD_ALPHA_SCHEMA_VERSION {
        return Ok(MigrationState::AlphaLatest);
    }
    Ok(MigrationState::Incompatible(format!(
        "database schema version {latest} is not a supported release baseline"
    )))
}

fn initialize_release_baseline(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        r#"
DROP TABLE IF EXISTS schema_migrations;
"#,
    )?;
    conn.execute_batch(BASELINE_SCHEMA)?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn install_release_baseline_for_test(conn: &Connection) -> anyhow::Result<()> {
    initialize_release_baseline(conn)
}

fn adopt_latest_alpha_schema(conn: &Connection) -> anyhow::Result<()> {
    let issues = inspect_schema_objects(conn)?;
    if !issues.is_empty() {
        anyhow::bail!(
            "old alpha database is not safe to adopt as release baseline: {}. {}",
            summarize_issues(&issues),
            INCOMPATIBLE_DATABASE_HINT
        );
    }
    reset_schema_migrations(conn)
}

fn reset_schema_migrations(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        r#"
DROP TABLE IF EXISTS schema_migrations;
CREATE TABLE schema_migrations (
    version INTEGER PRIMARY KEY,
    applied_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
INSERT INTO schema_migrations (version) VALUES (1);
"#,
    )?;
    Ok(())
}

fn inspect_schema(conn: &Connection) -> anyhow::Result<SchemaReport> {
    let mut issues = Vec::new();
    let mut version = None;

    if table_exists(conn, "schema_migrations")? {
        let versions = migration_versions(conn)?;
        version = versions.iter().max().copied();
        if versions != [CURRENT_SCHEMA_VERSION] {
            issues.push(format!(
                "expected schema_migrations to contain only version {CURRENT_SCHEMA_VERSION}, found {}",
                if versions.is_empty() {
                    "none".to_owned()
                } else {
                    versions
                        .iter()
                        .map(i64::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            ));
        }
    } else {
        issues.push("missing table schema_migrations".to_owned());
    }

    issues.extend(inspect_schema_objects(conn)?);
    Ok(SchemaReport { version, issues })
}

fn inspect_schema_objects(conn: &Connection) -> anyhow::Result<Vec<String>> {
    let mut issues = Vec::new();

    for table in REQUIRED_TABLES {
        if !table_exists(conn, table)? {
            issues.push(format!("missing table {table}"));
        }
    }

    for table in REQUIRED_TABLES {
        if table_exists(conn, table)? {
            check_table_columns(conn, table, &mut issues)?;
        }
    }

    for index in REQUIRED_INDEXES {
        check_index(conn, index, &mut issues)?;
    }

    for trigger in REQUIRED_TRIGGERS {
        check_trigger(conn, trigger, &mut issues)?;
    }

    for object in user_schema_objects(conn)? {
        if !is_expected_schema_object(&object) {
            issues.push(format!("unexpected schema object {object}"));
        }
    }

    Ok(issues)
}

fn check_table_columns(
    conn: &Connection,
    table: &str,
    issues: &mut Vec<String>,
) -> anyhow::Result<()> {
    let columns = table_columns(conn, table)?;
    let expected = required_columns_for(table);
    let actual_names = columns
        .iter()
        .map(|column| column.name.as_str())
        .collect::<Vec<_>>();
    let expected_names = expected
        .iter()
        .map(|column| column.name)
        .collect::<Vec<_>>();
    if actual_names != expected_names {
        issues.push(format!(
            "table {table} columns differ; expected {}, found {}",
            expected_names.join(", "),
            actual_names.join(", ")
        ));
    }

    for expected_column in expected {
        let Some(actual) = columns
            .iter()
            .find(|column| column.name == expected_column.name)
        else {
            issues.push(format!(
                "missing column {}.{}",
                expected_column.table, expected_column.name
            ));
            continue;
        };
        if !actual
            .type_name
            .eq_ignore_ascii_case(expected_column.type_name)
        {
            issues.push(format!(
                "column {}.{} has type {}, expected {}",
                expected_column.table,
                expected_column.name,
                actual.type_name,
                expected_column.type_name
            ));
        }
        if actual.not_null != expected_column.not_null {
            issues.push(format!(
                "column {}.{} nullability differs",
                expected_column.table, expected_column.name
            ));
        }
        if actual.default_value.as_deref() != expected_column.default_value {
            issues.push(format!(
                "column {}.{} default differs",
                expected_column.table, expected_column.name
            ));
        }
        if actual.primary_key_position != expected_column.primary_key_position {
            issues.push(format!(
                "column {}.{} primary key position differs",
                expected_column.table, expected_column.name
            ));
        }
    }
    Ok(())
}

fn check_index(
    conn: &Connection,
    expected: &RequiredIndex,
    issues: &mut Vec<String>,
) -> anyhow::Result<()> {
    if !table_exists(conn, expected.table)? {
        return Ok(());
    }
    let indexes = table_indexes(conn, expected.table)?;
    let Some((_name, unique)) = indexes.iter().find(|(name, _unique)| name == expected.name) else {
        issues.push(format!("missing index {}", expected.name));
        return Ok(());
    };
    if *unique != expected.unique {
        issues.push(format!("index {} uniqueness differs", expected.name));
    }
    Ok(())
}

fn check_trigger(
    conn: &Connection,
    expected: &RequiredTrigger,
    issues: &mut Vec<String>,
) -> anyhow::Result<()> {
    let trigger_table: Option<String> = conn
        .query_row(
            "SELECT tbl_name FROM sqlite_master WHERE type = 'trigger' AND name = ?",
            [expected.name],
            |row| row.get(0),
        )
        .optional()?;
    match trigger_table {
        Some(table) if table == expected.table => {}
        Some(_) => issues.push(format!("trigger {} table differs", expected.name)),
        None => issues.push(format!("missing trigger {}", expected.name)),
    }
    Ok(())
}

fn table_exists(conn: &Connection, table: &str) -> anyhow::Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?",
        [table],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

fn migration_versions(conn: &Connection) -> anyhow::Result<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT version FROM schema_migrations ORDER BY version")?;
    let versions = stmt
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<i64>>>()?;
    Ok(versions)
}

fn user_schema_objects(conn: &Connection) -> anyhow::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT name
        FROM sqlite_master
        WHERE name NOT LIKE 'sqlite_%'
        ORDER BY name
        "#,
    )?;
    let objects = stmt
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    Ok(objects)
}

fn table_columns(conn: &Connection, table: &str) -> anyhow::Result<Vec<ColumnInfo>> {
    let sql = format!("PRAGMA table_info({})", quoted_identifier(table));
    let mut stmt = conn.prepare(&sql)?;
    let columns = stmt
        .query_map([], |row| {
            Ok(ColumnInfo {
                name: row.get(1)?,
                type_name: row.get(2)?,
                not_null: row.get::<_, i64>(3)? != 0,
                default_value: row.get(4)?,
                primary_key_position: row.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(columns)
}

fn table_indexes(conn: &Connection, table: &str) -> anyhow::Result<Vec<(String, bool)>> {
    let sql = format!("PRAGMA index_list({})", quoted_identifier(table));
    let mut stmt = conn.prepare(&sql)?;
    let indexes = stmt
        .query_map([], |row| Ok((row.get(1)?, row.get::<_, i64>(2)? != 0)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(indexes)
}

fn quoted_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn required_columns_for(table: &str) -> Vec<RequiredColumn> {
    REQUIRED_COLUMNS
        .iter()
        .copied()
        .filter(|column| column.table == table)
        .collect()
}

fn is_expected_schema_object(name: &str) -> bool {
    REQUIRED_TABLES.contains(&name)
        || REQUIRED_INDEXES.iter().any(|index| index.name == name)
        || REQUIRED_TRIGGERS.iter().any(|trigger| trigger.name == name)
        || FTS_SHADOW_TABLES.contains(&name)
}

fn summarize_issues(issues: &[String]) -> String {
    const MAX_ISSUES: usize = 6;
    let mut summary = issues
        .iter()
        .take(MAX_ISSUES)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("; ");
    let remaining = issues.len().saturating_sub(MAX_ISSUES);
    if remaining > 0 {
        summary.push_str(&format!("; and {remaining} more issue(s)"));
    }
    summary
}

const BASELINE_SCHEMA: &str = r#"
CREATE TABLE schema_migrations (
    version INTEGER PRIMARY KEY,
    applied_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

INSERT INTO schema_migrations (version) VALUES (1);

CREATE TABLE users (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    username TEXT NOT NULL,
    normalized_username TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    display_name TEXT NOT NULL DEFAULT '',
    bio TEXT NOT NULL DEFAULT '',
    website TEXT NOT NULL DEFAULT '',
    profile_picture_media_id INTEGER,
    banner_media_id INTEGER,
    pinned_post_id INTEGER,
    is_admin INTEGER NOT NULL DEFAULT 0,
    is_suspended INTEGER NOT NULL DEFAULT 0,
    is_deleted INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    theme TEXT NOT NULL DEFAULT 'light' CHECK (theme IN ('light', 'dark')),
    location TEXT NOT NULL DEFAULT '',
    nsfw_blur_enabled INTEGER NOT NULL DEFAULT 1,
    onboarding_completed_at TEXT,
    liked_posts_public INTEGER NOT NULL DEFAULT 1 CHECK (liked_posts_public IN (0, 1))
);

CREATE TABLE sessions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_hash TEXT NOT NULL UNIQUE,
    csrf_token_hash TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    expires_at TEXT NOT NULL,
    revoked_at TEXT,
    previous_csrf_token_hash TEXT,
    delete_account_token_hash TEXT,
    delete_account_token_expires_at TEXT
);

CREATE TABLE posts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    anonymous_label TEXT,
    text TEXT NOT NULL,
    parent_post_id INTEGER REFERENCES posts(id) ON DELETE SET NULL,
    root_post_id INTEGER REFERENCES posts(id) ON DELETE SET NULL,
    is_deleted INTEGER NOT NULL DEFAULT 0,
    deleted_at TEXT,
    edited_at TEXT,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    quote_post_id INTEGER REFERENCES posts(id) ON DELETE SET NULL
);

CREATE TABLE media (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    owner_user_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    original_filename TEXT NOT NULL,
    stored_path TEXT NOT NULL,
    public_path TEXT NOT NULL,
    mime_type TEXT NOT NULL,
    media_kind TEXT NOT NULL,
    byte_len INTEGER NOT NULL,
    alt_text TEXT NOT NULL DEFAULT '',
    conversion_state TEXT NOT NULL DEFAULT 'original',
    ffmpeg_stderr TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    thumbnail_path TEXT,
    thumbnail_public_path TEXT,
    is_nsfw INTEGER NOT NULL DEFAULT 0,
    original_path TEXT,
    original_public_path TEXT,
    original_sha256 TEXT NOT NULL DEFAULT '',
    normalized_sha256 TEXT,
    canonical_media_id INTEGER REFERENCES media(id) ON DELETE SET NULL
);

CREATE TABLE post_media (
    post_id INTEGER NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
    media_id INTEGER NOT NULL REFERENCES media(id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    PRIMARY KEY (post_id, media_id)
);

CREATE TABLE reposts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    post_id INTEGER NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(user_id, post_id)
);

CREATE TABLE follows (
    follower_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    followed_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (follower_id, followed_id)
);

CREATE TABLE blocks (
    blocker_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    blocked_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (blocker_id, blocked_id)
);

CREATE TABLE mutes (
    muter_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    muted_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (muter_id, muted_id)
);

CREATE TABLE likes (
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    post_id INTEGER NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (user_id, post_id)
);

CREATE TABLE bookmarks (
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    post_id INTEGER NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (user_id, post_id)
);

CREATE TABLE notifications (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    actor_user_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    post_id INTEGER REFERENCES posts(id) ON DELETE SET NULL,
    kind TEXT NOT NULL,
    message TEXT NOT NULL,
    read_at TEXT,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE rate_limit_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    scope TEXT NOT NULL,
    actor TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE admin_audit_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    admin_user_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    action TEXT NOT NULL,
    target TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE media_jobs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    media_id INTEGER REFERENCES media(id) ON DELETE SET NULL,
    status TEXT NOT NULL,
    stderr_summary TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    finished_at TEXT
);

CREATE TABLE reports (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    reporter_user_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    post_id INTEGER REFERENCES posts(id) ON DELETE SET NULL,
    reason TEXT NOT NULL,
    dismissed_at TEXT,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE VIRTUAL TABLE IF NOT EXISTS posts_fts USING fts5(text, content='posts', content_rowid='id');

CREATE TRIGGER posts_ai AFTER INSERT ON posts BEGIN
  INSERT INTO posts_fts(rowid, text) VALUES (new.id, new.text);
END;
CREATE TRIGGER posts_ad AFTER DELETE ON posts BEGIN
  INSERT INTO posts_fts(posts_fts, rowid, text) VALUES('delete', old.id, old.text);
END;
CREATE TRIGGER posts_au AFTER UPDATE ON posts BEGIN
  INSERT INTO posts_fts(posts_fts, rowid, text) VALUES('delete', old.id, old.text);
  INSERT INTO posts_fts(rowid, text) VALUES (new.id, new.text);
END;

CREATE INDEX idx_posts_created ON posts(created_at DESC, id DESC);
CREATE INDEX idx_posts_user_created ON posts(user_id, created_at DESC, id DESC);
CREATE INDEX idx_posts_parent ON posts(parent_post_id, created_at ASC, id ASC);
CREATE INDEX idx_follows_followed ON follows(followed_id, follower_id);
CREATE INDEX idx_blocks_blocked ON blocks(blocked_id, blocker_id);
CREATE INDEX idx_mutes_muted ON mutes(muted_id, muter_id);
CREATE INDEX idx_notifications_user_read_created ON notifications(user_id, read_at, created_at DESC);
CREATE INDEX idx_reposts_post ON reposts(post_id, user_id);
CREATE INDEX idx_likes_post ON likes(post_id, user_id);
CREATE INDEX idx_bookmarks_post ON bookmarks(post_id, user_id);
CREATE INDEX idx_rate_limit_scope_actor_created ON rate_limit_events(scope, actor, created_at);
CREATE TABLE muted_words (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    term TEXT NOT NULL,
    normalized_term TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(user_id, normalized_term)
);

CREATE INDEX idx_muted_words_user ON muted_words(user_id, normalized_term);

CREATE INDEX idx_posts_quote ON posts(quote_post_id, created_at DESC, id DESC);
CREATE UNIQUE INDEX idx_posts_quote_dedupe
    ON posts(user_id, quote_post_id, text)
    WHERE quote_post_id IS NOT NULL AND is_deleted = 0;

CREATE INDEX idx_notifications_user_id_desc ON notifications(user_id, id DESC);
CREATE INDEX idx_notifications_dedupe ON notifications(user_id, actor_user_id, post_id, kind);

CREATE INDEX idx_media_is_nsfw ON media(is_nsfw);

CREATE INDEX idx_media_original_sha256
    ON media(media_kind, original_sha256)
    WHERE original_sha256 != '';
CREATE INDEX idx_media_normalized_sha256
    ON media(media_kind, normalized_sha256)
    WHERE normalized_sha256 IS NOT NULL AND normalized_sha256 != '';
CREATE INDEX idx_media_canonical_media_id ON media(canonical_media_id);

CREATE UNIQUE INDEX idx_media_unique_canonical_original_sha256
    ON media(media_kind, original_sha256)
    WHERE canonical_media_id IS NULL AND original_sha256 != '';
CREATE UNIQUE INDEX idx_media_unique_canonical_normalized_sha256
    ON media(media_kind, normalized_sha256)
    WHERE canonical_media_id IS NULL AND normalized_sha256 IS NOT NULL AND normalized_sha256 != '';

CREATE TRIGGER media_canonical_ref_insert
BEFORE INSERT ON media
WHEN NEW.canonical_media_id IS NOT NULL
BEGIN
    SELECT CASE
        WHEN NEW.id = NEW.canonical_media_id
        THEN RAISE(ABORT, 'media cannot reference itself as canonical')
    END;
    SELECT CASE
        WHEN NOT EXISTS (
            SELECT 1
            FROM media canonical
            WHERE canonical.id = NEW.canonical_media_id
              AND canonical.canonical_media_id IS NULL
              AND canonical.media_kind = NEW.media_kind
        )
        THEN RAISE(ABORT, 'media canonical reference must point to same-kind canonical media')
    END;
END;

CREATE TRIGGER media_canonical_ref_update
BEFORE UPDATE OF canonical_media_id, media_kind ON media
WHEN NEW.canonical_media_id IS NOT NULL
BEGIN
    SELECT CASE
        WHEN NEW.id = NEW.canonical_media_id
        THEN RAISE(ABORT, 'media cannot reference itself as canonical')
    END;
    SELECT CASE
        WHEN NOT EXISTS (
            SELECT 1
            FROM media canonical
            WHERE canonical.id = NEW.canonical_media_id
              AND canonical.canonical_media_id IS NULL
              AND canonical.media_kind = NEW.media_kind
        )
        THEN RAISE(ABORT, 'media canonical reference must point to same-kind canonical media')
    END;
END;
"#;

const REQUIRED_TABLES: &[&str] = &[
    "schema_migrations",
    "users",
    "sessions",
    "posts",
    "media",
    "post_media",
    "reposts",
    "follows",
    "blocks",
    "mutes",
    "likes",
    "bookmarks",
    "notifications",
    "rate_limit_events",
    "admin_audit_log",
    "media_jobs",
    "reports",
    "posts_fts",
    "muted_words",
];

const FTS_SHADOW_TABLES: &[&str] = &[
    "posts_fts_data",
    "posts_fts_idx",
    "posts_fts_content",
    "posts_fts_docsize",
    "posts_fts_config",
];

const REQUIRED_INDEXES: &[RequiredIndex] = &[
    RequiredIndex {
        table: "posts",
        name: "idx_posts_created",
        unique: false,
    },
    RequiredIndex {
        table: "posts",
        name: "idx_posts_user_created",
        unique: false,
    },
    RequiredIndex {
        table: "posts",
        name: "idx_posts_parent",
        unique: false,
    },
    RequiredIndex {
        table: "follows",
        name: "idx_follows_followed",
        unique: false,
    },
    RequiredIndex {
        table: "blocks",
        name: "idx_blocks_blocked",
        unique: false,
    },
    RequiredIndex {
        table: "mutes",
        name: "idx_mutes_muted",
        unique: false,
    },
    RequiredIndex {
        table: "notifications",
        name: "idx_notifications_user_read_created",
        unique: false,
    },
    RequiredIndex {
        table: "reposts",
        name: "idx_reposts_post",
        unique: false,
    },
    RequiredIndex {
        table: "likes",
        name: "idx_likes_post",
        unique: false,
    },
    RequiredIndex {
        table: "bookmarks",
        name: "idx_bookmarks_post",
        unique: false,
    },
    RequiredIndex {
        table: "rate_limit_events",
        name: "idx_rate_limit_scope_actor_created",
        unique: false,
    },
    RequiredIndex {
        table: "muted_words",
        name: "idx_muted_words_user",
        unique: false,
    },
    RequiredIndex {
        table: "posts",
        name: "idx_posts_quote",
        unique: false,
    },
    RequiredIndex {
        table: "posts",
        name: "idx_posts_quote_dedupe",
        unique: true,
    },
    RequiredIndex {
        table: "notifications",
        name: "idx_notifications_user_id_desc",
        unique: false,
    },
    RequiredIndex {
        table: "notifications",
        name: "idx_notifications_dedupe",
        unique: false,
    },
    RequiredIndex {
        table: "media",
        name: "idx_media_is_nsfw",
        unique: false,
    },
    RequiredIndex {
        table: "media",
        name: "idx_media_original_sha256",
        unique: false,
    },
    RequiredIndex {
        table: "media",
        name: "idx_media_normalized_sha256",
        unique: false,
    },
    RequiredIndex {
        table: "media",
        name: "idx_media_canonical_media_id",
        unique: false,
    },
    RequiredIndex {
        table: "media",
        name: "idx_media_unique_canonical_original_sha256",
        unique: true,
    },
    RequiredIndex {
        table: "media",
        name: "idx_media_unique_canonical_normalized_sha256",
        unique: true,
    },
];

const REQUIRED_TRIGGERS: &[RequiredTrigger] = &[
    RequiredTrigger {
        table: "posts",
        name: "posts_ai",
    },
    RequiredTrigger {
        table: "posts",
        name: "posts_ad",
    },
    RequiredTrigger {
        table: "posts",
        name: "posts_au",
    },
    RequiredTrigger {
        table: "media",
        name: "media_canonical_ref_insert",
    },
    RequiredTrigger {
        table: "media",
        name: "media_canonical_ref_update",
    },
];

const REQUIRED_COLUMNS: &[RequiredColumn] = &[
    RequiredColumn {
        table: "schema_migrations",
        name: "version",
        type_name: "INTEGER",
        not_null: false,
        default_value: None,
        primary_key_position: 1,
    },
    RequiredColumn {
        table: "schema_migrations",
        name: "applied_at",
        type_name: "TEXT",
        not_null: true,
        default_value: Some("CURRENT_TIMESTAMP"),
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "users",
        name: "id",
        type_name: "INTEGER",
        not_null: false,
        default_value: None,
        primary_key_position: 1,
    },
    RequiredColumn {
        table: "users",
        name: "username",
        type_name: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "users",
        name: "normalized_username",
        type_name: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "users",
        name: "password_hash",
        type_name: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "users",
        name: "display_name",
        type_name: "TEXT",
        not_null: true,
        default_value: Some("''"),
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "users",
        name: "bio",
        type_name: "TEXT",
        not_null: true,
        default_value: Some("''"),
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "users",
        name: "website",
        type_name: "TEXT",
        not_null: true,
        default_value: Some("''"),
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "users",
        name: "profile_picture_media_id",
        type_name: "INTEGER",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "users",
        name: "banner_media_id",
        type_name: "INTEGER",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "users",
        name: "pinned_post_id",
        type_name: "INTEGER",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "users",
        name: "is_admin",
        type_name: "INTEGER",
        not_null: true,
        default_value: Some("0"),
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "users",
        name: "is_suspended",
        type_name: "INTEGER",
        not_null: true,
        default_value: Some("0"),
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "users",
        name: "is_deleted",
        type_name: "INTEGER",
        not_null: true,
        default_value: Some("0"),
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "users",
        name: "created_at",
        type_name: "TEXT",
        not_null: true,
        default_value: Some("CURRENT_TIMESTAMP"),
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "users",
        name: "updated_at",
        type_name: "TEXT",
        not_null: true,
        default_value: Some("CURRENT_TIMESTAMP"),
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "users",
        name: "theme",
        type_name: "TEXT",
        not_null: true,
        default_value: Some("'light'"),
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "users",
        name: "location",
        type_name: "TEXT",
        not_null: true,
        default_value: Some("''"),
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "users",
        name: "nsfw_blur_enabled",
        type_name: "INTEGER",
        not_null: true,
        default_value: Some("1"),
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "users",
        name: "onboarding_completed_at",
        type_name: "TEXT",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "users",
        name: "liked_posts_public",
        type_name: "INTEGER",
        not_null: true,
        default_value: Some("1"),
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "sessions",
        name: "id",
        type_name: "INTEGER",
        not_null: false,
        default_value: None,
        primary_key_position: 1,
    },
    RequiredColumn {
        table: "sessions",
        name: "user_id",
        type_name: "INTEGER",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "sessions",
        name: "token_hash",
        type_name: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "sessions",
        name: "csrf_token_hash",
        type_name: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "sessions",
        name: "created_at",
        type_name: "TEXT",
        not_null: true,
        default_value: Some("CURRENT_TIMESTAMP"),
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "sessions",
        name: "expires_at",
        type_name: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "sessions",
        name: "revoked_at",
        type_name: "TEXT",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "sessions",
        name: "previous_csrf_token_hash",
        type_name: "TEXT",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "sessions",
        name: "delete_account_token_hash",
        type_name: "TEXT",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "sessions",
        name: "delete_account_token_expires_at",
        type_name: "TEXT",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "posts",
        name: "id",
        type_name: "INTEGER",
        not_null: false,
        default_value: None,
        primary_key_position: 1,
    },
    RequiredColumn {
        table: "posts",
        name: "user_id",
        type_name: "INTEGER",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "posts",
        name: "anonymous_label",
        type_name: "TEXT",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "posts",
        name: "text",
        type_name: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "posts",
        name: "parent_post_id",
        type_name: "INTEGER",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "posts",
        name: "root_post_id",
        type_name: "INTEGER",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "posts",
        name: "is_deleted",
        type_name: "INTEGER",
        not_null: true,
        default_value: Some("0"),
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "posts",
        name: "deleted_at",
        type_name: "TEXT",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "posts",
        name: "edited_at",
        type_name: "TEXT",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "posts",
        name: "created_at",
        type_name: "TEXT",
        not_null: true,
        default_value: Some("CURRENT_TIMESTAMP"),
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "posts",
        name: "quote_post_id",
        type_name: "INTEGER",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "media",
        name: "id",
        type_name: "INTEGER",
        not_null: false,
        default_value: None,
        primary_key_position: 1,
    },
    RequiredColumn {
        table: "media",
        name: "owner_user_id",
        type_name: "INTEGER",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "media",
        name: "original_filename",
        type_name: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "media",
        name: "stored_path",
        type_name: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "media",
        name: "public_path",
        type_name: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "media",
        name: "mime_type",
        type_name: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "media",
        name: "media_kind",
        type_name: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "media",
        name: "byte_len",
        type_name: "INTEGER",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "media",
        name: "alt_text",
        type_name: "TEXT",
        not_null: true,
        default_value: Some("''"),
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "media",
        name: "conversion_state",
        type_name: "TEXT",
        not_null: true,
        default_value: Some("'original'"),
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "media",
        name: "ffmpeg_stderr",
        type_name: "TEXT",
        not_null: true,
        default_value: Some("''"),
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "media",
        name: "created_at",
        type_name: "TEXT",
        not_null: true,
        default_value: Some("CURRENT_TIMESTAMP"),
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "media",
        name: "thumbnail_path",
        type_name: "TEXT",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "media",
        name: "thumbnail_public_path",
        type_name: "TEXT",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "media",
        name: "is_nsfw",
        type_name: "INTEGER",
        not_null: true,
        default_value: Some("0"),
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "media",
        name: "original_path",
        type_name: "TEXT",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "media",
        name: "original_public_path",
        type_name: "TEXT",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "media",
        name: "original_sha256",
        type_name: "TEXT",
        not_null: true,
        default_value: Some("''"),
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "media",
        name: "normalized_sha256",
        type_name: "TEXT",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "media",
        name: "canonical_media_id",
        type_name: "INTEGER",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "post_media",
        name: "post_id",
        type_name: "INTEGER",
        not_null: true,
        default_value: None,
        primary_key_position: 1,
    },
    RequiredColumn {
        table: "post_media",
        name: "media_id",
        type_name: "INTEGER",
        not_null: true,
        default_value: None,
        primary_key_position: 2,
    },
    RequiredColumn {
        table: "post_media",
        name: "position",
        type_name: "INTEGER",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "reposts",
        name: "id",
        type_name: "INTEGER",
        not_null: false,
        default_value: None,
        primary_key_position: 1,
    },
    RequiredColumn {
        table: "reposts",
        name: "user_id",
        type_name: "INTEGER",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "reposts",
        name: "post_id",
        type_name: "INTEGER",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "reposts",
        name: "created_at",
        type_name: "TEXT",
        not_null: true,
        default_value: Some("CURRENT_TIMESTAMP"),
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "follows",
        name: "follower_id",
        type_name: "INTEGER",
        not_null: true,
        default_value: None,
        primary_key_position: 1,
    },
    RequiredColumn {
        table: "follows",
        name: "followed_id",
        type_name: "INTEGER",
        not_null: true,
        default_value: None,
        primary_key_position: 2,
    },
    RequiredColumn {
        table: "follows",
        name: "created_at",
        type_name: "TEXT",
        not_null: true,
        default_value: Some("CURRENT_TIMESTAMP"),
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "blocks",
        name: "blocker_id",
        type_name: "INTEGER",
        not_null: true,
        default_value: None,
        primary_key_position: 1,
    },
    RequiredColumn {
        table: "blocks",
        name: "blocked_id",
        type_name: "INTEGER",
        not_null: true,
        default_value: None,
        primary_key_position: 2,
    },
    RequiredColumn {
        table: "blocks",
        name: "created_at",
        type_name: "TEXT",
        not_null: true,
        default_value: Some("CURRENT_TIMESTAMP"),
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "mutes",
        name: "muter_id",
        type_name: "INTEGER",
        not_null: true,
        default_value: None,
        primary_key_position: 1,
    },
    RequiredColumn {
        table: "mutes",
        name: "muted_id",
        type_name: "INTEGER",
        not_null: true,
        default_value: None,
        primary_key_position: 2,
    },
    RequiredColumn {
        table: "mutes",
        name: "created_at",
        type_name: "TEXT",
        not_null: true,
        default_value: Some("CURRENT_TIMESTAMP"),
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "likes",
        name: "user_id",
        type_name: "INTEGER",
        not_null: true,
        default_value: None,
        primary_key_position: 1,
    },
    RequiredColumn {
        table: "likes",
        name: "post_id",
        type_name: "INTEGER",
        not_null: true,
        default_value: None,
        primary_key_position: 2,
    },
    RequiredColumn {
        table: "likes",
        name: "created_at",
        type_name: "TEXT",
        not_null: true,
        default_value: Some("CURRENT_TIMESTAMP"),
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "bookmarks",
        name: "user_id",
        type_name: "INTEGER",
        not_null: true,
        default_value: None,
        primary_key_position: 1,
    },
    RequiredColumn {
        table: "bookmarks",
        name: "post_id",
        type_name: "INTEGER",
        not_null: true,
        default_value: None,
        primary_key_position: 2,
    },
    RequiredColumn {
        table: "bookmarks",
        name: "created_at",
        type_name: "TEXT",
        not_null: true,
        default_value: Some("CURRENT_TIMESTAMP"),
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "notifications",
        name: "id",
        type_name: "INTEGER",
        not_null: false,
        default_value: None,
        primary_key_position: 1,
    },
    RequiredColumn {
        table: "notifications",
        name: "user_id",
        type_name: "INTEGER",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "notifications",
        name: "actor_user_id",
        type_name: "INTEGER",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "notifications",
        name: "post_id",
        type_name: "INTEGER",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "notifications",
        name: "kind",
        type_name: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "notifications",
        name: "message",
        type_name: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "notifications",
        name: "read_at",
        type_name: "TEXT",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "notifications",
        name: "created_at",
        type_name: "TEXT",
        not_null: true,
        default_value: Some("CURRENT_TIMESTAMP"),
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "rate_limit_events",
        name: "id",
        type_name: "INTEGER",
        not_null: false,
        default_value: None,
        primary_key_position: 1,
    },
    RequiredColumn {
        table: "rate_limit_events",
        name: "scope",
        type_name: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "rate_limit_events",
        name: "actor",
        type_name: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "rate_limit_events",
        name: "created_at",
        type_name: "TEXT",
        not_null: true,
        default_value: Some("CURRENT_TIMESTAMP"),
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "admin_audit_log",
        name: "id",
        type_name: "INTEGER",
        not_null: false,
        default_value: None,
        primary_key_position: 1,
    },
    RequiredColumn {
        table: "admin_audit_log",
        name: "admin_user_id",
        type_name: "INTEGER",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "admin_audit_log",
        name: "action",
        type_name: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "admin_audit_log",
        name: "target",
        type_name: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "admin_audit_log",
        name: "created_at",
        type_name: "TEXT",
        not_null: true,
        default_value: Some("CURRENT_TIMESTAMP"),
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "media_jobs",
        name: "id",
        type_name: "INTEGER",
        not_null: false,
        default_value: None,
        primary_key_position: 1,
    },
    RequiredColumn {
        table: "media_jobs",
        name: "media_id",
        type_name: "INTEGER",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "media_jobs",
        name: "status",
        type_name: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "media_jobs",
        name: "stderr_summary",
        type_name: "TEXT",
        not_null: true,
        default_value: Some("''"),
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "media_jobs",
        name: "created_at",
        type_name: "TEXT",
        not_null: true,
        default_value: Some("CURRENT_TIMESTAMP"),
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "media_jobs",
        name: "finished_at",
        type_name: "TEXT",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "reports",
        name: "id",
        type_name: "INTEGER",
        not_null: false,
        default_value: None,
        primary_key_position: 1,
    },
    RequiredColumn {
        table: "reports",
        name: "reporter_user_id",
        type_name: "INTEGER",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "reports",
        name: "post_id",
        type_name: "INTEGER",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "reports",
        name: "reason",
        type_name: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "reports",
        name: "dismissed_at",
        type_name: "TEXT",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "reports",
        name: "created_at",
        type_name: "TEXT",
        not_null: true,
        default_value: Some("CURRENT_TIMESTAMP"),
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "posts_fts",
        name: "text",
        type_name: "",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "muted_words",
        name: "id",
        type_name: "INTEGER",
        not_null: false,
        default_value: None,
        primary_key_position: 1,
    },
    RequiredColumn {
        table: "muted_words",
        name: "user_id",
        type_name: "INTEGER",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "muted_words",
        name: "term",
        type_name: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "muted_words",
        name: "normalized_term",
        type_name: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    RequiredColumn {
        table: "muted_words",
        name: "created_at",
        type_name: "TEXT",
        not_null: true,
        default_value: Some("CURRENT_TIMESTAMP"),
        primary_key_position: 0,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migrations_apply() {
        let temp = tempfile::tempdir().expect("temp dir");
        let pool = connect(&temp.path().join("test.sqlite3"))
            .await
            .expect("connect");
        migrate(&pool).await.expect("migrate");
        let (user_count, version): (i64, i64) = pool
            .call(|conn| {
                let user_count =
                    conn.query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))?;
                let version = schema_version_from_connection(conn)?;
                Ok((user_count, version))
            })
            .await
            .expect("baseline");
        let report = schema_report(&pool).await.expect("schema report");
        assert_eq!(user_count, 0);
        assert_eq!(version, CURRENT_SCHEMA_VERSION);
        assert!(report.is_compatible(), "{}", report.summary());
    }

    #[tokio::test]
    async fn migrations_are_idempotent_and_enable_sqlite_settings() {
        let temp = tempfile::tempdir().expect("temp dir");
        let pool = connect(&temp.path().join("test.sqlite3"))
            .await
            .expect("connect");
        migrate(&pool).await.expect("first migrate");
        migrate(&pool).await.expect("second migrate");

        let (versions, foreign_keys, journal_mode, busy_timeout): (i64, i64, String, i64) = pool
            .call(|conn| {
                let versions =
                    conn.query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
                        row.get(0)
                    })?;
                let foreign_keys = conn.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
                let journal_mode = conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
                let busy_timeout = conn.query_row("PRAGMA busy_timeout", [], |row| row.get(0))?;
                Ok((versions, foreign_keys, journal_mode, busy_timeout))
            })
            .await
            .expect("settings");

        assert_eq!(versions, 1);
        assert_eq!(foreign_keys, 1);
        assert_eq!(journal_mode, "wal");
        assert!(busy_timeout >= 5_000);
    }

    #[tokio::test]
    async fn canonical_media_constraints_reject_unsafe_references() {
        let temp = tempfile::tempdir().expect("temp dir");
        let pool = connect(&temp.path().join("test.sqlite3"))
            .await
            .expect("connect");
        migrate(&pool).await.expect("migrate");

        pool.call(|conn| {
            conn.execute(
                "INSERT INTO media (id, original_filename, stored_path, public_path, mime_type, media_kind, byte_len, original_sha256, normalized_sha256) VALUES (10, 'image.webp', '/tmp/image.webp', '/uploads/images/image.webp', 'image/webp', 'image', 1, 'raw', 'norm')",
                [],
            )?;
            let duplicate_canonical = conn.execute(
                "INSERT INTO media (original_filename, stored_path, public_path, mime_type, media_kind, byte_len, original_sha256) VALUES ('copy.webp', '/tmp/copy.webp', '/uploads/images/copy.webp', 'image/webp', 'image', 1, 'raw')",
                [],
            );
            assert!(duplicate_canonical.is_err());

            let cross_kind = conn.execute(
                "INSERT INTO media (original_filename, stored_path, public_path, mime_type, media_kind, byte_len, canonical_media_id) VALUES ('clip.webm', '/tmp/clip.webm', '/uploads/videos/clip.webm', 'video/webm', 'video', 1, 10)",
                [],
            );
            assert!(cross_kind.is_err());

            let self_reference = conn.execute(
                "INSERT INTO media (id, original_filename, stored_path, public_path, mime_type, media_kind, byte_len, canonical_media_id) VALUES (11, 'self.webp', '/tmp/self.webp', '/uploads/images/self.webp', 'image/webp', 'image', 1, 11)",
                [],
            );
            assert!(self_reference.is_err());
            Ok(())
        })
        .await
        .expect("constraint checks");
    }

    #[tokio::test]
    async fn users_get_light_theme_by_default() {
        let temp = tempfile::tempdir().expect("temp dir");
        let pool = connect(&temp.path().join("test.sqlite3"))
            .await
            .expect("connect");
        migrate(&pool).await.expect("migrate");

        let theme: String = pool
            .call(|conn| {
                conn.execute(
                    "INSERT INTO users (username, normalized_username, password_hash, display_name) VALUES ('Alice', 'alice', 'hash', 'Alice')",
                    [],
                )?;
                Ok(conn.query_row("SELECT theme FROM users WHERE normalized_username = 'alice'", [], |row| row.get(0))?)
            })
            .await
            .expect("theme");

        assert_eq!(theme, "light");
    }

    #[tokio::test]
    async fn muted_words_table_is_created() {
        let temp = tempfile::tempdir().expect("temp dir");
        let pool = connect(&temp.path().join("test.sqlite3"))
            .await
            .expect("connect");
        migrate(&pool).await.expect("migrate");

        let count: i64 = pool
            .call(|conn| {
                Ok(conn.query_row("SELECT COUNT(*) FROM muted_words", [], |row| row.get(0))?)
            })
            .await
            .expect("count");

        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn users_get_empty_location_by_default() {
        let temp = tempfile::tempdir().expect("temp dir");
        let pool = connect(&temp.path().join("test.sqlite3"))
            .await
            .expect("connect");
        migrate(&pool).await.expect("migrate");

        let location: String = pool
            .call(|conn| {
                conn.execute(
                    "INSERT INTO users (username, normalized_username, password_hash, display_name) VALUES ('Alice', 'alice', 'hash', 'Alice')",
                    [],
                )?;
                Ok(conn.query_row("SELECT location FROM users WHERE normalized_username = 'alice'", [], |row| row.get(0))?)
            })
            .await
            .expect("location");

        assert_eq!(location, "");
    }

    #[tokio::test]
    async fn nsfw_defaults_are_safe_for_users_and_media() {
        let temp = tempfile::tempdir().expect("temp dir");
        let pool = connect(&temp.path().join("test.sqlite3"))
            .await
            .expect("connect");
        migrate(&pool).await.expect("migrate");

        let (blur_enabled, is_nsfw): (i64, i64) = pool
            .call(|conn| {
                conn.execute(
                    "INSERT INTO users (username, normalized_username, password_hash, display_name) VALUES ('Alice', 'alice', 'hash', 'Alice')",
                    [],
                )?;
                conn.execute(
                    "INSERT INTO media (owner_user_id, original_filename, stored_path, public_path, mime_type, media_kind, byte_len) VALUES (1, 'x.png', '/tmp/x.png', '/uploads/images/x.png', 'image/png', 'image', 1)",
                    [],
                )?;
                Ok((
                    conn.query_row(
                        "SELECT nsfw_blur_enabled FROM users WHERE normalized_username = 'alice'",
                        [],
                        |row| row.get(0),
                    )?,
                    conn.query_row("SELECT is_nsfw FROM media WHERE id = 1", [], |row| {
                        row.get(0)
                    })?,
                ))
            })
            .await
            .expect("defaults");

        assert_eq!(blur_enabled, 1);
        assert_eq!(is_nsfw, 0);
    }

    #[tokio::test]
    async fn onboarding_completion_defaults_to_incomplete() {
        let temp = tempfile::tempdir().expect("temp dir");
        let pool = connect(&temp.path().join("test.sqlite3"))
            .await
            .expect("connect");
        migrate(&pool).await.expect("migrate");

        let completed_at: Option<String> = pool
            .call(|conn| {
                conn.execute(
                    "INSERT INTO users (username, normalized_username, password_hash, display_name) VALUES ('Alice', 'alice', 'hash', 'Alice')",
                    [],
                )?;
                Ok(conn.query_row(
                    "SELECT onboarding_completed_at FROM users WHERE normalized_username = 'alice'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await
            .expect("onboarding state");

        assert!(completed_at.is_none());
    }

    #[tokio::test]
    async fn liked_posts_are_public_by_default() {
        let temp = tempfile::tempdir().expect("temp dir");
        let pool = connect(&temp.path().join("test.sqlite3"))
            .await
            .expect("connect");
        migrate(&pool).await.expect("migrate");

        let liked_posts_public: i64 = pool
            .call(|conn| {
                conn.execute(
                    "INSERT INTO users (username, normalized_username, password_hash, display_name) VALUES ('Alice', 'alice', 'hash', 'Alice')",
                    [],
                )?;
                Ok(conn.query_row(
                    "SELECT liked_posts_public FROM users WHERE normalized_username = 'alice'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await
            .expect("liked posts visibility");

        assert_eq!(liked_posts_public, 1);
    }

    #[tokio::test]
    async fn latest_alpha_schema_is_marked_as_baseline_without_data_loss() {
        let temp = tempfile::tempdir().expect("temp dir");
        let pool = connect(&temp.path().join("test.sqlite3"))
            .await
            .expect("connect");
        pool.call(|conn| {
            install_latest_alpha_fixture(conn)?;
            insert_preservation_fixture(conn)?;
            Ok(())
        })
        .await
        .expect("alpha fixture");

        migrate(&pool).await.expect("adopt latest alpha");
        migrate(&pool).await.expect("second migrate");

        let state: PreservedAlphaState = pool
            .call(|conn| {
                let versions = migration_versions(conn)?;
                let counts = [
                    "users",
                    "sessions",
                    "posts",
                    "media",
                    "post_media",
                    "reposts",
                    "follows",
                    "blocks",
                    "mutes",
                    "likes",
                    "bookmarks",
                    "notifications",
                    "muted_words",
                    "admin_audit_log",
                    "media_jobs",
                    "reports",
                ]
                .iter()
                .map(|table| table_count(conn, table))
                .collect::<anyhow::Result<Vec<_>>>()?;
                let privacy = conn.query_row(
                    "SELECT liked_posts_public, nsfw_blur_enabled FROM users WHERE normalized_username = 'alice'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                let media = conn.query_row(
                    "SELECT is_nsfw, original_path FROM media WHERE id = 1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                Ok(PreservedAlphaState {
                    versions,
                    counts,
                    privacy,
                    media,
                })
            })
            .await
            .expect("preserved data");

        assert_eq!(state.versions, [CURRENT_SCHEMA_VERSION]);
        assert_eq!(
            state.counts,
            [2, 1, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1]
        );
        assert_eq!(state.privacy, (0, 0));
        assert_eq!(state.media, (1, "/uploads/originals/alpha.png".to_owned()));
    }

    #[tokio::test]
    async fn partial_old_alpha_schema_fails_without_data_loss() {
        let temp = tempfile::tempdir().expect("temp dir");
        let pool = connect(&temp.path().join("test.sqlite3"))
            .await
            .expect("connect");
        pool.call(|conn| {
            conn.execute_batch(
                r#"
                CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY);
                INSERT INTO schema_migrations (version) VALUES (12);
                CREATE TABLE users (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    username TEXT NOT NULL,
                    normalized_username TEXT NOT NULL UNIQUE,
                    password_hash TEXT NOT NULL,
                    display_name TEXT NOT NULL DEFAULT ''
                );
                INSERT INTO users (username, normalized_username, password_hash, display_name)
                    VALUES ('Alice', 'alice', 'hash', 'Alice');
                "#,
            )?;
            Ok(())
        })
        .await
        .expect("partial alpha schema");

        let error = migrate(&pool)
            .await
            .expect_err("partial alpha schema must fail");
        let remaining_users: i64 = pool
            .call(|conn| table_count(conn, "users"))
            .await
            .expect("users remain");

        assert!(error.to_string().contains("version 12"));
        assert!(error.to_string().contains("blind migration"));
        assert_eq!(remaining_users, 1);
    }

    #[tokio::test]
    async fn structurally_unsafe_latest_alpha_schema_fails() {
        let temp = tempfile::tempdir().expect("temp dir");
        let pool = connect(&temp.path().join("test.sqlite3"))
            .await
            .expect("connect");
        pool.call(|conn| {
            install_latest_alpha_fixture(conn)?;
            conn.execute("DROP INDEX idx_posts_created", [])?;
            Ok(())
        })
        .await
        .expect("unsafe alpha fixture");

        let error = migrate(&pool)
            .await
            .expect_err("unsafe latest alpha schema must fail");
        let version: i64 = pool
            .call(|conn| schema_version_from_connection(conn))
            .await
            .expect("alpha version remains");

        assert!(error.to_string().contains("not safe to adopt"));
        assert!(
            error
                .to_string()
                .contains("missing index idx_posts_created")
        );
        assert_eq!(version, OLD_ALPHA_SCHEMA_VERSION);
    }

    #[tokio::test]
    async fn schema_report_lists_missing_required_objects() {
        let temp = tempfile::tempdir().expect("temp dir");
        let pool = connect(&temp.path().join("test.sqlite3"))
            .await
            .expect("connect");
        migrate(&pool).await.expect("migrate");
        pool.call(|conn| {
            conn.execute("DROP INDEX idx_posts_created", [])?;
            Ok(())
        })
        .await
        .expect("drop index");

        let report = schema_report(&pool).await.expect("schema report");

        assert_eq!(report.version(), Some(CURRENT_SCHEMA_VERSION));
        assert!(!report.is_compatible());
        assert!(report.summary().contains("missing index idx_posts_created"));
    }

    #[tokio::test]
    async fn posts_support_quote_repost_references() {
        let temp = tempfile::tempdir().expect("temp dir");
        let pool = connect(&temp.path().join("test.sqlite3"))
            .await
            .expect("connect");
        migrate(&pool).await.expect("migrate");

        let quote_count: i64 = pool
            .call(|conn| {
                conn.execute(
                    "INSERT INTO posts (text, quote_post_id) VALUES ('quote', 1)",
                    [],
                )?;
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM posts WHERE quote_post_id = 1",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await
            .expect("quote count");

        assert_eq!(quote_count, 1);
    }

    #[tokio::test]
    async fn baseline_supports_core_app_records_and_search() {
        let temp = tempfile::tempdir().expect("temp dir");
        let pool = connect(&temp.path().join("test.sqlite3"))
            .await
            .expect("connect");
        migrate(&pool).await.expect("migrate");

        let (search_count, social_count): (i64, i64) = pool
            .call(|conn| {
                conn.execute_batch(
                    r#"
                    INSERT INTO users (id, username, normalized_username, password_hash, display_name)
                        VALUES (1, 'Alice', 'alice', 'hash', 'Alice');
                    INSERT INTO users (id, username, normalized_username, password_hash, display_name)
                        VALUES (2, 'Bob', 'bob', 'hash', 'Bob');
                    INSERT INTO posts (id, user_id, text)
                        VALUES (1, 1, 'baseline search token');
                    INSERT INTO media (id, owner_user_id, original_filename, stored_path, public_path, mime_type, media_kind, byte_len, is_nsfw)
                        VALUES (1, 1, 'image.webp', '/media/image.webp', '/uploads/images/image.webp', 'image/webp', 'image', 10, 1);
                    INSERT INTO post_media (post_id, media_id, position) VALUES (1, 1, 0);
                    INSERT INTO follows (follower_id, followed_id) VALUES (1, 2);
                    INSERT INTO likes (user_id, post_id) VALUES (2, 1);
                    INSERT INTO bookmarks (user_id, post_id) VALUES (1, 1);
                    INSERT INTO reposts (user_id, post_id) VALUES (2, 1);
                    INSERT INTO notifications (user_id, actor_user_id, post_id, kind, message)
                        VALUES (1, 2, 1, 'like', 'Bob liked your post');
                    "#,
                )?;
                let search_count = conn.query_row(
                    "SELECT COUNT(*) FROM posts_fts WHERE posts_fts MATCH 'baseline'",
                    [],
                    |row| row.get(0),
                )?;
                let social_count = table_count(conn, "follows")?
                    + table_count(conn, "likes")?
                    + table_count(conn, "bookmarks")?
                    + table_count(conn, "reposts")?
                    + table_count(conn, "notifications")?;
                Ok((search_count, social_count))
            })
            .await
            .expect("core records");

        assert_eq!(search_count, 1);
        assert_eq!(social_count, 5);
    }

    fn install_latest_alpha_fixture(conn: &Connection) -> anyhow::Result<()> {
        conn.execute_batch(BASELINE_SCHEMA)?;
        conn.execute("DELETE FROM schema_migrations", [])?;
        for version in 1..=OLD_ALPHA_SCHEMA_VERSION {
            conn.execute(
                "INSERT INTO schema_migrations (version) VALUES (?)",
                [version],
            )?;
        }
        Ok(())
    }

    fn insert_preservation_fixture(conn: &Connection) -> anyhow::Result<()> {
        insert_preservation_users(conn)?;
        insert_preservation_session(conn)?;
        insert_preservation_posts_and_media(conn)?;
        insert_preservation_social_rows(conn)
    }

    fn insert_preservation_users(conn: &Connection) -> anyhow::Result<()> {
        conn.execute_batch(
            r#"
            INSERT INTO users (
                id,
                username,
                normalized_username,
                password_hash,
                display_name,
                bio,
                website,
                is_admin,
                theme,
                location,
                nsfw_blur_enabled,
                onboarding_completed_at,
                liked_posts_public
            )
            VALUES (
                1,
                'Alice',
                'alice',
                'hash',
                'Alice',
                'bio',
                'https://example.test',
                1,
                'dark',
                'Vancouver',
                0,
                '2026-01-01T00:00:00Z',
                0
            );
            INSERT INTO users (id, username, normalized_username, password_hash, display_name)
                VALUES (2, 'Bob', 'bob', 'hash', 'Bob');
            "#,
        )?;
        Ok(())
    }

    fn insert_preservation_session(conn: &Connection) -> anyhow::Result<()> {
        conn.execute_batch(
            r#"
            INSERT INTO sessions (
                user_id,
                token_hash,
                csrf_token_hash,
                expires_at,
                previous_csrf_token_hash,
                delete_account_token_hash,
                delete_account_token_expires_at
            )
            VALUES (1, 'session-token', 'csrf-token', '2099-01-01T00:00:00Z', 'old-csrf', 'delete-token', '2099-01-01T00:05:00Z');
            "#,
        )?;
        Ok(())
    }

    fn insert_preservation_posts_and_media(conn: &Connection) -> anyhow::Result<()> {
        conn.execute_batch(
            r#"
            INSERT INTO posts (id, user_id, text)
                VALUES (1, 1, 'alpha post');
            INSERT INTO posts (id, user_id, text, quote_post_id)
                VALUES (2, 2, 'quoted alpha post', 1);
            INSERT INTO media (
                id,
                owner_user_id,
                original_filename,
                original_path,
                original_public_path,
                stored_path,
                public_path,
                mime_type,
                media_kind,
                byte_len,
                alt_text,
                conversion_state,
                ffmpeg_stderr,
                thumbnail_path,
                thumbnail_public_path,
                is_nsfw,
                original_sha256,
                normalized_sha256
            )
            VALUES (
                1,
                1,
                'alpha.png',
                '/uploads/originals/alpha.png',
                '/uploads/originals/alpha.png',
                '/uploads/images/alpha.webp',
                '/uploads/images/alpha.webp',
                'image/webp',
                'image',
                10,
                'alt text',
                'converted',
                'stderr',
                '/uploads/images/alpha-thumb.webp',
                '/uploads/images/alpha-thumb.webp',
                1,
                'raw-alpha',
                'normalized-alpha'
            );
            INSERT INTO post_media (post_id, media_id, position) VALUES (1, 1, 0);
            "#,
        )?;
        Ok(())
    }

    fn insert_preservation_social_rows(conn: &Connection) -> anyhow::Result<()> {
        conn.execute_batch(
            r#"
            INSERT INTO reposts (user_id, post_id) VALUES (1, 2);
            INSERT INTO follows (follower_id, followed_id) VALUES (1, 2);
            INSERT INTO blocks (blocker_id, blocked_id) VALUES (1, 2);
            INSERT INTO mutes (muter_id, muted_id) VALUES (2, 1);
            INSERT INTO likes (user_id, post_id) VALUES (1, 2);
            INSERT INTO bookmarks (user_id, post_id) VALUES (1, 2);
            INSERT INTO notifications (user_id, actor_user_id, post_id, kind, message, read_at)
                VALUES (1, 2, 2, 'reply', 'Bob replied', '2026-01-01T00:00:00Z');
            INSERT INTO muted_words (user_id, term, normalized_term) VALUES (1, 'Spoiler', 'spoiler');
            INSERT INTO admin_audit_log (admin_user_id, action, target) VALUES (1, 'suspend', 'bob');
            INSERT INTO media_jobs (media_id, status, stderr_summary, finished_at)
                VALUES (1, 'complete', 'ok', '2026-01-01T00:00:00Z');
            INSERT INTO reports (reporter_user_id, post_id, reason) VALUES (2, 1, 'spam');
            "#,
        )?;
        Ok(())
    }

    struct PreservedAlphaState {
        versions: Vec<i64>,
        counts: Vec<i64>,
        privacy: (i64, i64),
        media: (i64, String),
    }

    fn table_count(conn: &Connection, table: &str) -> anyhow::Result<i64> {
        let sql = format!("SELECT COUNT(*) FROM {}", quoted_identifier(table));
        Ok(conn.query_row(&sql, [], |row| row.get(0))?)
    }
}
