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

        assert_eq!(versions, 1);
        assert_eq!(foreign_keys, 1);
        assert_eq!(journal_mode, "wal");
        assert!(busy_timeout >= 5_000);
    }
}
