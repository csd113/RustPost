use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use rusqlite::params;

use crate::auth;
use crate::db::SqlitePool;
use crate::runtime::RuntimePaths;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteAccountError {
    WrongPassword,
    UnsafeMediaPath(String),
    Database(String),
}

impl std::fmt::Display for DeleteAccountError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongPassword => formatter.write_str("password is incorrect"),
            Self::UnsafeMediaPath(path) => {
                write!(formatter, "refusing to delete unsafe media path: {path}")
            }
            Self::Database(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for DeleteAccountError {}

impl From<rusqlite::Error> for DeleteAccountError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error.to_string())
    }
}

impl From<anyhow::Error> for DeleteAccountError {
    fn from(error: anyhow::Error) -> Self {
        Self::Database(error.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountDeletionSummary {
    pub deleted_media_files: usize,
}

pub async fn delete_account(
    pool: &SqlitePool,
    paths: &RuntimePaths,
    user_id: i64,
    password: &str,
) -> Result<AccountDeletionSummary, DeleteAccountError> {
    let password_ok = auth::verify_user_password(pool, user_id, password)
        .await
        .map_err(|err| DeleteAccountError::Database(err.to_string()))?;
    if !password_ok {
        return Err(DeleteAccountError::WrongPassword);
    }

    let media_files = scrub_account_rows(pool, user_id, paths).await?;

    let deleted_media_files = remove_media_files(&media_files, user_id);
    Ok(AccountDeletionSummary {
        deleted_media_files,
    })
}

#[derive(Debug, Clone)]
struct AccountMedia {
    id: i64,
    stored_path: String,
    thumbnail_path: Option<String>,
}

fn account_media_in_tx(
    tx: &rusqlite::Transaction<'_>,
    user_id: i64,
) -> anyhow::Result<Vec<AccountMedia>> {
    let mut stmt = tx.prepare(
        r#"
            SELECT DISTINCT m.id, m.stored_path, m.thumbnail_path
            FROM media m
            WHERE m.owner_user_id = ?
               OR m.id IN (
                    SELECT pm.media_id
                    FROM post_media pm
                    JOIN posts p ON p.id = pm.post_id
                    WHERE p.user_id = ?
               )
            "#,
    )?;
    let rows = stmt
        .query_map(params![user_id, user_id], |row| {
            Ok(AccountMedia {
                id: row.get(0)?,
                stored_path: row.get(1)?,
                thumbnail_path: row.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

async fn scrub_account_rows(
    pool: &SqlitePool,
    user_id: i64,
    paths: &RuntimePaths,
) -> Result<BTreeSet<PathBuf>, DeleteAccountError> {
    let paths = paths.clone();
    pool.call(move |conn| {
        let result: Result<BTreeSet<PathBuf>, DeleteAccountError> = (|| {
            let tx = conn.transaction()?;
            tx.execute(
                "UPDATE users SET is_deleted = 1, updated_at = CURRENT_TIMESTAMP WHERE id = ? AND is_deleted = 0",
                [user_id],
            )?;
            let media = account_media_in_tx(&tx, user_id)?;
            let mut media_files = BTreeSet::new();
            for item in &media {
                media_files.extend(safe_media_path(&paths, &item.stored_path)?);
                if let Some(thumbnail_path) = &item.thumbnail_path {
                    media_files.extend(safe_media_path(&paths, thumbnail_path)?);
                }
            }
            let media_ids = media.into_iter().map(|item| item.id).collect::<Vec<_>>();
            tx.execute("DELETE FROM sessions WHERE user_id = ?", [user_id])?;
            tx.execute("DELETE FROM muted_words WHERE user_id = ?", [user_id])?;
            tx.execute(
                "DELETE FROM rate_limit_events WHERE actor = ?",
                [format!("user:{user_id}")],
            )?;
            tx.execute(
                "DELETE FROM blocks WHERE blocker_id = ? OR blocked_id = ?",
                params![user_id, user_id],
            )?;
            tx.execute(
                "DELETE FROM mutes WHERE muter_id = ? OR muted_id = ?",
                params![user_id, user_id],
            )?;
            tx.execute(
                "DELETE FROM follows WHERE follower_id = ? OR followed_id = ?",
                params![user_id, user_id],
            )?;
            tx.execute(
                "DELETE FROM notifications WHERE user_id = ? OR actor_user_id = ? OR post_id IN (SELECT id FROM posts WHERE user_id = ?)",
                params![user_id, user_id, user_id],
            )?;
            tx.execute(
                "DELETE FROM reports WHERE reporter_user_id = ? OR post_id IN (SELECT id FROM posts WHERE user_id = ?)",
                params![user_id, user_id],
            )?;
            tx.execute(
                "DELETE FROM likes WHERE user_id = ? OR post_id IN (SELECT id FROM posts WHERE user_id = ?)",
                params![user_id, user_id],
            )?;
            tx.execute(
                "DELETE FROM bookmarks WHERE user_id = ? OR post_id IN (SELECT id FROM posts WHERE user_id = ?)",
                params![user_id, user_id],
            )?;
            tx.execute(
                "DELETE FROM reposts WHERE user_id = ? OR post_id IN (SELECT id FROM posts WHERE user_id = ?)",
                params![user_id, user_id],
            )?;
            tx.execute("DELETE FROM posts WHERE user_id = ?", [user_id])?;
            for media_id in media_ids {
                tx.execute("DELETE FROM media_jobs WHERE media_id = ?", [media_id])?;
                tx.execute("DELETE FROM media WHERE id = ?", [media_id])?;
            }
            tx.execute("DELETE FROM users WHERE id = ?", [user_id])?;
            tx.commit()?;
            Ok(media_files)
        })();
        Ok(result)
    })
    .await
    .map_err(|err| DeleteAccountError::Database(err.to_string()))?
}

fn safe_media_path(
    paths: &RuntimePaths,
    raw_path: &str,
) -> Result<Option<PathBuf>, DeleteAccountError> {
    if raw_path.trim().is_empty() {
        return Ok(None);
    }
    let path = PathBuf::from(raw_path);
    if !path.is_absolute() || has_parent_component(&path) {
        return Err(DeleteAccountError::UnsafeMediaPath(raw_path.to_owned()));
    }
    if !allowed_media_roots(paths)
        .iter()
        .any(|root| path.starts_with(root))
    {
        return Err(DeleteAccountError::UnsafeMediaPath(raw_path.to_owned()));
    }
    if path.exists() {
        let canonical_path = path
            .canonicalize()
            .map_err(|err| DeleteAccountError::Database(err.to_string()))?;
        let canonical_roots = allowed_media_roots(paths)
            .iter()
            .map(|root| root.canonicalize())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| DeleteAccountError::Database(err.to_string()))?;
        if !canonical_roots
            .iter()
            .any(|root| canonical_path.starts_with(root))
        {
            return Err(DeleteAccountError::UnsafeMediaPath(raw_path.to_owned()));
        }
        return Ok(Some(canonical_path));
    }
    Ok(Some(path))
}

fn allowed_media_roots(paths: &RuntimePaths) -> [PathBuf; 4] {
    [
        paths.uploads_originals.clone(),
        paths.uploads_images.clone(),
        paths.uploads_videos.clone(),
        paths.uploads_thumbs.clone(),
    ]
}

fn has_parent_component(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, Component::ParentDir))
}

fn remove_media_files(paths: &BTreeSet<PathBuf>, user_id: i64) -> usize {
    let mut deleted = 0;
    for path in paths {
        match fs::remove_file(path) {
            Ok(()) => deleted += 1,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(
                    user_id,
                    path = %path.display(),
                    error = %error,
                    "failed to remove media file after account database deletion"
                );
            }
        }
    }
    deleted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{auth, config::Settings, db, social};
    use rusqlite::OptionalExtension as _;

    async fn fixture() -> (
        tempfile::TempDir,
        RuntimePaths,
        SqlitePool,
        Settings,
        i64,
        i64,
    ) {
        let temp = tempfile::tempdir().expect("temp dir");
        let paths = RuntimePaths::from_data_dir(temp.path().join("data"));
        paths.ensure().expect("runtime paths");
        let pool = db::connect(&paths.database_path).await.expect("connect");
        db::migrate(&pool).await.expect("migrate");
        let settings = Settings::default();
        let alice = auth::register_user(&pool, &settings, "alice", "very secure password", false)
            .await
            .expect("alice");
        let bob = auth::register_user(&pool, &settings, "bob", "very secure password", false)
            .await
            .expect("bob");
        (temp, paths, pool, settings, alice, bob)
    }

    #[tokio::test]
    async fn delete_account_rejects_wrong_password() {
        let (_temp, paths, pool, _settings, alice, _bob) = fixture().await;

        let result = delete_account(&pool, &paths, alice, "wrong password").await;

        assert_eq!(
            result.expect_err("wrong password"),
            DeleteAccountError::WrongPassword
        );
        let users: i64 = pool
            .call(|conn| Ok(conn.query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))?))
            .await
            .expect("count");
        assert_eq!(users, 2);
    }

    #[tokio::test]
    async fn delete_account_scrubs_owned_rows_and_media_files() {
        let (_temp, paths, pool, settings, alice, bob) = fixture().await;
        let post = social::create_post(&pool, &settings, Some(alice), "alice post", None, &[])
            .await
            .expect("post");
        social::follow(&pool, bob, alice).await.expect("follow");
        social::like(&pool, bob, post).await.expect("like");
        social::bookmark(&pool, bob, post).await.expect("bookmark");
        social::repost(&pool, bob, post).await.expect("repost");
        social::block(&pool, bob, alice).await.expect("block");
        social::mute(&pool, bob, alice).await.expect("mute");
        social::add_muted_word(&pool, alice, "secret")
            .await
            .expect("muted word");
        let alice_actor = format!("user:{alice}");
        crate::rate_limit::record(&pool, crate::rate_limit::Scope::Post, &alice_actor)
            .await
            .expect("rate limit");
        let media_path = paths.uploads_images.join("alice.webp");
        fs::write(&media_path, b"image").expect("media file");
        let thumb_path = paths.uploads_thumbs.join("alice-thumb.webp");
        fs::write(&thumb_path, b"thumb").expect("thumb file");
        let media_path_string = media_path.to_string_lossy().to_string();
        let thumb_path_string = thumb_path.to_string_lossy().to_string();
        pool.call(move |conn| {
            conn.execute(
                "INSERT INTO media (owner_user_id, original_filename, stored_path, public_path, mime_type, media_kind, byte_len, thumbnail_path) VALUES (?, 'alice.webp', ?, '/uploads/images/alice.webp', 'image/webp', 'image', 5, ?)",
                params![alice, media_path_string, thumb_path_string],
            )?;
            let media_id = conn.last_insert_rowid();
            conn.execute(
                "UPDATE users SET profile_picture_media_id = ? WHERE id = ?",
                params![media_id, alice],
            )?;
            conn.execute(
                "INSERT INTO media_jobs (media_id, status) VALUES (?, 'converted')",
                [media_id],
            )?;
            Ok(())
        })
        .await
        .expect("media row");

        let summary = delete_account(&pool, &paths, alice, "very secure password")
            .await
            .expect("delete account");

        assert_eq!(summary.deleted_media_files, 2);
        assert!(!media_path.exists());
        assert!(!thumb_path.exists());
        let counts: (i64, i64, i64, i64, i64, i64, i64, i64, i64, i64) = pool
            .call(move |conn| {
                Ok((
                    conn.query_row("SELECT COUNT(*) FROM users WHERE id = ?", [alice], |row| {
                        row.get(0)
                    })?,
                    conn.query_row(
                        "SELECT COUNT(*) FROM posts WHERE user_id = ?",
                        [alice],
                        |row| row.get(0),
                    )?,
                    conn.query_row(
                        "SELECT COUNT(*) FROM media WHERE owner_user_id = ?",
                        [alice],
                        |row| row.get(0),
                    )?,
                    conn.query_row("SELECT COUNT(*) FROM likes", [], |row| row.get(0))?,
                    conn.query_row("SELECT COUNT(*) FROM bookmarks", [], |row| row.get(0))?,
                    conn.query_row("SELECT COUNT(*) FROM reposts", [], |row| row.get(0))?,
                    conn.query_row("SELECT COUNT(*) FROM follows", [], |row| row.get(0))?,
                    conn.query_row("SELECT COUNT(*) FROM blocks", [], |row| row.get(0))?,
                    conn.query_row("SELECT COUNT(*) FROM mutes", [], |row| row.get(0))?,
                    conn.query_row(
                        "SELECT COUNT(*) FROM rate_limit_events WHERE actor = ?",
                        [format!("user:{alice}")],
                        |row| row.get(0),
                    )?,
                ))
            })
            .await
            .expect("counts");
        assert_eq!(counts, (0, 0, 0, 0, 0, 0, 0, 0, 0, 0));
    }

    #[tokio::test]
    async fn delete_account_scrubs_media_inserted_after_password_check() {
        let (_temp, paths, pool, _settings, alice, _bob) = fixture().await;
        assert!(
            auth::verify_user_password(&pool, alice, "very secure password")
                .await
                .expect("password")
        );
        let media_path = paths.uploads_images.join("late.webp");
        fs::write(&media_path, b"late").expect("late media file");
        let media_path_string = media_path.to_string_lossy().to_string();
        pool.call(move |conn| {
            conn.execute(
                "INSERT INTO media (owner_user_id, original_filename, stored_path, public_path, mime_type, media_kind, byte_len) VALUES (?, 'late.webp', ?, '/uploads/images/late.webp', 'image/webp', 'image', 4)",
                params![alice, media_path_string],
            )?;
            Ok(())
        })
        .await
        .expect("late media row");

        let summary = delete_account(&pool, &paths, alice, "very secure password")
            .await
            .expect("delete account");

        assert_eq!(summary.deleted_media_files, 1);
        assert!(!media_path.exists());
        let rows: i64 = pool
            .call(|conn| Ok(conn.query_row("SELECT COUNT(*) FROM media", [], |row| row.get(0))?))
            .await
            .expect("media count");
        assert_eq!(rows, 0);
    }

    #[tokio::test]
    async fn delete_account_treats_file_cleanup_failure_as_success_after_db_scrub() {
        let (_temp, paths, pool, _settings, alice, _bob) = fixture().await;
        let media_path = paths.uploads_images.join("directory-media");
        fs::create_dir(&media_path).expect("media directory");
        let media_path_string = media_path.to_string_lossy().to_string();
        pool.call(move |conn| {
            conn.execute(
                "INSERT INTO media (owner_user_id, original_filename, stored_path, public_path, mime_type, media_kind, byte_len) VALUES (?, 'directory-media', ?, '/uploads/images/directory-media', 'image/webp', 'image', 1)",
                params![alice, media_path_string],
            )?;
            Ok(())
        })
        .await
        .expect("media row");

        let summary = delete_account(&pool, &paths, alice, "very secure password")
            .await
            .expect("delete account");

        assert_eq!(summary.deleted_media_files, 0);
        assert!(media_path.exists());
        let user_exists: bool = pool
            .call(move |conn| {
                Ok(conn
                    .query_row("SELECT 1 FROM users WHERE id = ?", [alice], |_| Ok(()))
                    .optional()?
                    .is_some())
            })
            .await
            .expect("user lookup");
        assert!(!user_exists);
    }

    #[tokio::test]
    async fn delete_account_rejects_unsafe_media_path_before_db_changes() {
        let (_temp, paths, pool, _settings, alice, _bob) = fixture().await;
        pool.call(move |conn| {
            conn.execute(
                "INSERT INTO media (owner_user_id, original_filename, stored_path, public_path, mime_type, media_kind, byte_len) VALUES (?, 'bad.webp', '/tmp/rustpost-outside.webp', '/uploads/images/bad.webp', 'image/webp', 'image', 1)",
                [alice],
            )?;
            Ok(())
        })
        .await
        .expect("unsafe media row");

        let result = delete_account(&pool, &paths, alice, "very secure password").await;

        assert!(matches!(
            result,
            Err(DeleteAccountError::UnsafeMediaPath(path)) if path == "/tmp/rustpost-outside.webp"
        ));
        let exists: bool = pool
            .call(move |conn| {
                Ok(conn
                    .query_row("SELECT 1 FROM users WHERE id = ?", [alice], |_| Ok(()))
                    .optional()?
                    .is_some())
            })
            .await
            .expect("user exists");
        assert!(exists);
    }
}
