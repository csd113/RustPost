use std::path::Path;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use anyhow::Context as _;
use rusqlite::{Connection, OptionalExtension as _};

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
        tx.execute_batch(
            r#"
        CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY,
            applied_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );
        "#,
        )?;
        let applied: Option<i64> = tx
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .optional()?
            .flatten();
        if applied.unwrap_or(0) < 1 {
            tx.execute_batch(MIGRATION_1)?;
            tx.execute("INSERT INTO schema_migrations (version) VALUES (1)", [])?;
        }
        if applied.unwrap_or(0) < 2 {
            tx.execute_batch(MIGRATION_2)?;
            tx.execute("INSERT INTO schema_migrations (version) VALUES (2)", [])?;
        }
        if applied.unwrap_or(0) < 3 {
            tx.execute_batch(MIGRATION_3)?;
            tx.execute("INSERT INTO schema_migrations (version) VALUES (3)", [])?;
        }
        if applied.unwrap_or(0) < 4 {
            tx.execute_batch(MIGRATION_4)?;
            tx.execute("INSERT INTO schema_migrations (version) VALUES (4)", [])?;
        }
        if applied.unwrap_or(0) < 5 {
            tx.execute_batch(MIGRATION_5)?;
            tx.execute("INSERT INTO schema_migrations (version) VALUES (5)", [])?;
        }
        if applied.unwrap_or(0) < 6 {
            tx.execute_batch(MIGRATION_6)?;
            tx.execute("INSERT INTO schema_migrations (version) VALUES (6)", [])?;
        }
        if applied.unwrap_or(0) < 7 {
            tx.execute_batch(MIGRATION_7)?;
            tx.execute("INSERT INTO schema_migrations (version) VALUES (7)", [])?;
        }
        if applied.unwrap_or(0) < 8 {
            tx.execute_batch(MIGRATION_8)?;
            tx.execute("INSERT INTO schema_migrations (version) VALUES (8)", [])?;
        }
        if applied.unwrap_or(0) < 9 {
            tx.execute_batch(MIGRATION_9)?;
            tx.execute("INSERT INTO schema_migrations (version) VALUES (9)", [])?;
        }
        if applied.unwrap_or(0) < 10 {
            tx.execute_batch(MIGRATION_10)?;
            tx.execute("INSERT INTO schema_migrations (version) VALUES (10)", [])?;
        }
        if applied.unwrap_or(0) < 11 {
            tx.execute_batch(MIGRATION_11)?;
            tx.execute("INSERT INTO schema_migrations (version) VALUES (11)", [])?;
        }
        if applied.unwrap_or(0) < 12 {
            tx.execute_batch(MIGRATION_12)?;
            tx.execute("INSERT INTO schema_migrations (version) VALUES (12)", [])?;
        }
        if applied.unwrap_or(0) < 13 {
            tx.execute_batch(MIGRATION_13)?;
            tx.execute("INSERT INTO schema_migrations (version) VALUES (13)", [])?;
        }
        tx.commit()?;
        Ok(())
    })
    .await
}

const MIGRATION_1: &str = r#"
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
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE sessions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_hash TEXT NOT NULL UNIQUE,
    csrf_token_hash TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    expires_at TEXT NOT NULL,
    revoked_at TEXT
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
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
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
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
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
"#;

const MIGRATION_2: &str = r#"
ALTER TABLE sessions ADD COLUMN previous_csrf_token_hash TEXT;
"#;

const MIGRATION_3: &str = r#"
ALTER TABLE media ADD COLUMN thumbnail_path TEXT;
ALTER TABLE media ADD COLUMN thumbnail_public_path TEXT;
"#;

const MIGRATION_4: &str = r#"
ALTER TABLE users ADD COLUMN theme TEXT NOT NULL DEFAULT 'light' CHECK (theme IN ('light', 'dark'));
"#;

const MIGRATION_5: &str = r#"
CREATE TABLE IF NOT EXISTS muted_words (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    term TEXT NOT NULL,
    normalized_term TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(user_id, normalized_term)
);

CREATE INDEX IF NOT EXISTS idx_muted_words_user ON muted_words(user_id, normalized_term);
"#;

const MIGRATION_6: &str = r#"
ALTER TABLE users ADD COLUMN location TEXT NOT NULL DEFAULT '';
"#;

const MIGRATION_7: &str = r#"
ALTER TABLE sessions ADD COLUMN delete_account_token_hash TEXT;
ALTER TABLE sessions ADD COLUMN delete_account_token_expires_at TEXT;
"#;

const MIGRATION_8: &str = r#"
ALTER TABLE posts ADD COLUMN quote_post_id INTEGER REFERENCES posts(id) ON DELETE SET NULL;

CREATE INDEX idx_posts_quote ON posts(quote_post_id, created_at DESC, id DESC);
CREATE UNIQUE INDEX idx_posts_quote_dedupe
    ON posts(user_id, quote_post_id, text)
    WHERE quote_post_id IS NOT NULL AND is_deleted = 0;
"#;

const MIGRATION_9: &str = r#"
CREATE INDEX IF NOT EXISTS idx_notifications_user_id_desc ON notifications(user_id, id DESC);
CREATE INDEX IF NOT EXISTS idx_notifications_dedupe ON notifications(user_id, actor_user_id, post_id, kind);
"#;

const MIGRATION_10: &str = r#"
ALTER TABLE media ADD COLUMN is_nsfw INTEGER NOT NULL DEFAULT 0;
ALTER TABLE users ADD COLUMN nsfw_blur_enabled INTEGER NOT NULL DEFAULT 1;

CREATE INDEX IF NOT EXISTS idx_media_is_nsfw ON media(is_nsfw);
"#;

const MIGRATION_11: &str = r#"
ALTER TABLE media ADD COLUMN original_path TEXT;
ALTER TABLE media ADD COLUMN original_public_path TEXT;
ALTER TABLE media ADD COLUMN original_sha256 TEXT NOT NULL DEFAULT '';
ALTER TABLE media ADD COLUMN normalized_sha256 TEXT;
ALTER TABLE media ADD COLUMN canonical_media_id INTEGER REFERENCES media(id) ON DELETE SET NULL;

CREATE INDEX IF NOT EXISTS idx_media_original_sha256
    ON media(media_kind, original_sha256)
    WHERE original_sha256 != '';
CREATE INDEX IF NOT EXISTS idx_media_normalized_sha256
    ON media(media_kind, normalized_sha256)
    WHERE normalized_sha256 IS NOT NULL AND normalized_sha256 != '';
CREATE INDEX IF NOT EXISTS idx_media_canonical_media_id ON media(canonical_media_id);

CREATE UNIQUE INDEX IF NOT EXISTS idx_media_unique_canonical_original_sha256
    ON media(media_kind, original_sha256)
    WHERE canonical_media_id IS NULL AND original_sha256 != '';
CREATE UNIQUE INDEX IF NOT EXISTS idx_media_unique_canonical_normalized_sha256
    ON media(media_kind, normalized_sha256)
    WHERE canonical_media_id IS NULL AND normalized_sha256 IS NOT NULL AND normalized_sha256 != '';

CREATE TRIGGER IF NOT EXISTS media_canonical_ref_insert
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

CREATE TRIGGER IF NOT EXISTS media_canonical_ref_update
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

const MIGRATION_12: &str = r#"
ALTER TABLE users ADD COLUMN onboarding_completed_at TEXT;
UPDATE users
SET onboarding_completed_at = CURRENT_TIMESTAMP
WHERE onboarding_completed_at IS NULL AND is_deleted = 0;
"#;

const MIGRATION_13: &str = r#"
ALTER TABLE users ADD COLUMN liked_posts_public INTEGER NOT NULL DEFAULT 1 CHECK (liked_posts_public IN (0, 1));
"#;

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
        let count: i64 = pool
            .call(|conn| Ok(conn.query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))?))
            .await
            .expect("count");
        assert_eq!(count, 0);
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

        assert_eq!(versions, 13);
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
    async fn liked_posts_public_migration_defaults_existing_users_public() {
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
                    display_name TEXT NOT NULL DEFAULT '',
                    is_deleted INTEGER NOT NULL DEFAULT 0
                );
                INSERT INTO users (username, normalized_username, password_hash, display_name)
                    VALUES ('Alice', 'alice', 'hash', 'Alice');
                INSERT INTO users (username, normalized_username, password_hash, display_name, is_deleted)
                    VALUES ('Deleted', 'deleted', 'hash', 'Deleted', 1);
                "#,
            )?;
            Ok(())
        })
        .await
        .expect("legacy schema");

        migrate(&pool).await.expect("migrate to liked_posts_public");
        migrate(&pool).await.expect("second migrate");

        let (alice_public, deleted_public, column_count, version): (i64, i64, i64, i64) = pool
            .call(|conn| {
                let alice = conn.query_row(
                    "SELECT liked_posts_public FROM users WHERE normalized_username = 'alice'",
                    [],
                    |row| row.get(0),
                )?;
                let deleted = conn.query_row(
                    "SELECT liked_posts_public FROM users WHERE normalized_username = 'deleted'",
                    [],
                    |row| row.get(0),
                )?;
                let column_count = conn.query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('users') WHERE name = 'liked_posts_public'",
                    [],
                    |row| row.get(0),
                )?;
                let version =
                    conn.query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
                        row.get(0)
                    })?;
                Ok((alice, deleted, column_count, version))
            })
            .await
            .expect("liked posts migration state");

        assert_eq!(alice_public, 1);
        assert_eq!(deleted_public, 1);
        assert_eq!(column_count, 1);
        assert_eq!(version, 13);
    }

    #[tokio::test]
    async fn onboarding_migration_marks_existing_active_users_complete() {
        let temp = tempfile::tempdir().expect("temp dir");
        let pool = connect(&temp.path().join("test.sqlite3"))
            .await
            .expect("connect");
        pool.call(|conn| {
            conn.execute_batch(
                r#"
                CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY);
                INSERT INTO schema_migrations (version) VALUES (11);
                CREATE TABLE users (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    username TEXT NOT NULL,
                    normalized_username TEXT NOT NULL UNIQUE,
                    password_hash TEXT NOT NULL,
                    display_name TEXT NOT NULL DEFAULT '',
                    is_deleted INTEGER NOT NULL DEFAULT 0
                );
                INSERT INTO users (username, normalized_username, password_hash, display_name)
                    VALUES ('Alice', 'alice', 'hash', 'Alice');
                INSERT INTO users (username, normalized_username, password_hash, display_name, is_deleted)
                    VALUES ('Deleted', 'deleted', 'hash', 'Deleted', 1);
                "#,
            )?;
            Ok(())
        })
        .await
        .expect("legacy schema");

        migrate(&pool).await.expect("migrate");

        let (active_completed_at, deleted_completed_at): (Option<String>, Option<String>) = pool
            .call(|conn| {
                let active = conn.query_row(
                    "SELECT onboarding_completed_at FROM users WHERE normalized_username = 'alice'",
                    [],
                    |row| row.get(0),
                )?;
                let deleted = conn.query_row(
                    "SELECT onboarding_completed_at FROM users WHERE normalized_username = 'deleted'",
                    [],
                    |row| row.get(0),
                )?;
                Ok((active, deleted))
            })
            .await
            .expect("onboarding state");

        assert!(active_completed_at.is_some());
        assert!(deleted_completed_at.is_none());
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
}
