use std::collections::BTreeSet;
use std::fs;
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};

use anyhow::Context as _;
use rusqlite::{OptionalExtension as _, params};
use uuid::Uuid;

use crate::auth;
use crate::config::Settings;
use crate::db::SqlitePool;
use crate::runtime::RuntimePaths;

/// File-name prefix for durable pending-media-deletion journals.
///
/// The runtime temp cleanup never matches this prefix, so a pending journal is
/// never removed behind the account deletion path's back.
const PENDING_MEDIA_DELETION_PREFIX: &str = "pending-media-deletion-";
/// File-name suffix for durable pending-media-deletion journals.
const PENDING_MEDIA_DELETION_SUFFIX: &str = ".journal";
/// Maximum pending-media-deletion journals recovered in one pass.
const MAX_PENDING_MEDIA_DELETION_JOURNALS_PER_PASS: usize = 1000;

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

    let outcome = scrub_account_rows(pool, user_id, paths).await?;

    let deleted_media_files = finish_media_cleanup(&outcome, user_id);
    Ok(AccountDeletionSummary {
        deleted_media_files,
    })
}

/// A pending account deletion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletionRequest {
    /// When the deletion was requested.
    pub requested_at: String,
    /// When the account becomes eligible for permanent removal.
    pub scheduled_at: String,
}

/// Records a deletion request and the final deadline.
///
/// The account keeps authenticating while the deadline is in the future and
/// [`cancel_deletion`] clears the request. A configured grace period of zero
/// schedules the deadline immediately.
pub async fn request_deletion(
    pool: &SqlitePool,
    settings: &Settings,
    user_id: i64,
) -> anyhow::Result<DeletionRequest> {
    let grace_days = settings.accounts.deletion_grace_period_days;
    let modifier = format!("+{grace_days} days");
    pool.call(move |conn| {
        let tx = conn.transaction()?;
        let existing = tx
            .query_row(
                "SELECT deletion_requested_at, deletion_scheduled_at FROM users WHERE id = ? AND is_deleted = 0",
                [user_id],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                    ))
                },
            )
            .optional()?;
        let Some((requested_at, scheduled_at)) = existing else {
            anyhow::bail!("account not found");
        };
        if let (Some(requested_at), Some(scheduled_at)) = (requested_at, scheduled_at) {
            tx.commit()?;
            return Ok(DeletionRequest {
                requested_at,
                scheduled_at,
            });
        }
        tx.execute(
            "UPDATE users SET deletion_requested_at = datetime('now'), deletion_scheduled_at = datetime('now', ?1), updated_at = CURRENT_TIMESTAMP WHERE id = ?",
            params![modifier, user_id],
        )?;
        let (requested_at, scheduled_at) = tx.query_row(
            "SELECT deletion_requested_at, deletion_scheduled_at FROM users WHERE id = ?",
            [user_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?;
        tx.commit()?;
        Ok(DeletionRequest {
            requested_at,
            scheduled_at,
        })
    })
    .await
}

/// Clears a pending deletion request. Returns `true` when a request existed.
///
/// Cancellation is a no-op for accounts that are already permanently removed or
/// marked deleted, so it can never resurrect a deleted account.
pub async fn cancel_deletion(pool: &SqlitePool, user_id: i64) -> anyhow::Result<bool> {
    pool.call(move |conn| {
        let changed = conn.execute(
            "UPDATE users SET deletion_requested_at = NULL, deletion_scheduled_at = NULL, updated_at = CURRENT_TIMESTAMP WHERE id = ? AND is_deleted = 0 AND deletion_requested_at IS NOT NULL",
            [user_id],
        )?;
        Ok(changed > 0)
    })
    .await
}

/// Reads the pending deletion window for an account, if any.
pub async fn deletion_status(
    pool: &SqlitePool,
    user_id: i64,
) -> anyhow::Result<Option<DeletionRequest>> {
    pool.call(move |conn| {
        conn.query_row(
            "SELECT deletion_requested_at, deletion_scheduled_at FROM users WHERE id = ? AND is_deleted = 0 AND deletion_requested_at IS NOT NULL AND deletion_scheduled_at IS NOT NULL",
            [user_id],
            |row| {
                Ok(DeletionRequest {
                    requested_at: row.get(0)?,
                    scheduled_at: row.get(1)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    })
    .await
}

/// Permanently removes every account whose deletion deadline has passed.
///
/// Runs the same scrubbing logic as [`delete_account`] without a password
/// check, is safe to call repeatedly, and is driven by persisted timestamps so
/// it works after restarts. Failures for one account are logged and retried on
/// the next call; the returned count is the number of accounts fully removed.
pub async fn finalize_due_deletions(
    pool: &SqlitePool,
    paths: &RuntimePaths,
) -> anyhow::Result<usize> {
    let due_ids = pool
        .call(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id FROM users WHERE is_deleted = 0 AND deletion_scheduled_at IS NOT NULL AND datetime(deletion_scheduled_at) <= CURRENT_TIMESTAMP ORDER BY id",
            )?;
            let ids = stmt
                .query_map([], |row| row.get::<_, i64>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(ids)
        })
        .await?;
    let mut removed = 0;
    for user_id in due_ids {
        match finalize_due_deletion(pool, paths, user_id).await {
            Ok(true) => removed += 1,
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(
                    user_id,
                    error = %error,
                    "failed to finalize due account deletion; will retry on the next pass"
                );
            }
        }
    }
    Ok(removed)
}

/// Finalizes one due deletion, re-checking the persisted deadline and request
/// marker inside the scrubbing transaction so a concurrent cancellation wins.
///
/// Returns `true` when the account was removed in this call. Two concurrent
/// callers can never both claim the same account: the claim and the row
/// deletion share one transaction, and the loser observes no due row.
async fn finalize_due_deletion(
    pool: &SqlitePool,
    paths: &RuntimePaths,
    user_id: i64,
) -> anyhow::Result<bool> {
    let paths = paths.clone();
    let result = pool
        .call(move |conn| {
            let result: Result<Option<ScrubOutcome>, DeleteAccountError> = (|| {
                let tx = conn.transaction()?;
                let due = tx
                    .query_row(
                        "SELECT 1 FROM users WHERE id = ? AND is_deleted = 0 AND deletion_requested_at IS NOT NULL AND deletion_scheduled_at IS NOT NULL AND datetime(deletion_scheduled_at) <= CURRENT_TIMESTAMP",
                        [user_id],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
                if !due {
                    tx.commit()?;
                    return Ok(None);
                }
                let outcome = scrub_account_rows_in_tx(&tx, user_id, &paths)?;
                commit_scrub_transaction(tx, &outcome, user_id)?;
                Ok(Some(outcome))
            })();
            Ok(result)
        })
        .await?;
    let result = result?;
    let Some(outcome) = result else {
        return Ok(false);
    };
    let deleted_media_files = finish_media_cleanup(&outcome, user_id);
    tracing::info!(
        user_id,
        deleted_media_files,
        "finalized scheduled account deletion"
    );
    Ok(true)
}

#[derive(Debug, Clone)]
struct AccountMedia {
    id: i64,
    original_path: Option<String>,
    stored_path: String,
    thumbnail_path: Option<String>,
}

/// Collects every normalized handle the account ever held so release
/// tombstones can be recorded in the deletion transaction.
///
/// The current handle and the full username history are both included; the
/// caller records them before deleting the rows that store them.
fn account_normalized_usernames_in_tx(
    tx: &rusqlite::Transaction<'_>,
    user_id: i64,
) -> anyhow::Result<Vec<String>> {
    let mut usernames = BTreeSet::new();
    let current: Option<String> = tx
        .query_row(
            "SELECT normalized_username FROM users WHERE id = ?",
            [user_id],
            |row| row.get(0),
        )
        .optional()?;
    usernames.extend(current);
    let mut stmt =
        tx.prepare("SELECT normalized_username FROM username_history WHERE user_id = ?")?;
    let history = stmt
        .query_map([user_id], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    usernames.extend(history);
    Ok(usernames.into_iter().collect())
}

fn account_media_in_tx(
    tx: &rusqlite::Transaction<'_>,
    user_id: i64,
) -> anyhow::Result<Vec<AccountMedia>> {
    let mut stmt = tx.prepare(
        r#"
            SELECT DISTINCT m.id, m.original_path, m.stored_path, m.thumbnail_path
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
                original_path: row.get(1)?,
                stored_path: row.get(2)?,
                thumbnail_path: row.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

async fn scrub_account_rows(
    pool: &SqlitePool,
    user_id: i64,
    paths: &RuntimePaths,
) -> Result<ScrubOutcome, DeleteAccountError> {
    let paths = paths.clone();
    pool.call(move |conn| {
        let result: Result<ScrubOutcome, DeleteAccountError> = (|| {
            let tx = conn.transaction()?;
            let outcome = scrub_account_rows_in_tx(&tx, user_id, &paths)?;
            commit_scrub_transaction(tx, &outcome, user_id)?;
            Ok(outcome)
        })();
        Ok(result)
    })
    .await
    .map_err(|err| DeleteAccountError::Database(err.to_string()))?
}

/// Files whose database rows were removed, plus the durable journal that
/// records them until every file is gone.
struct ScrubOutcome {
    media_files: BTreeSet<PathBuf>,
    journal_path: PathBuf,
}

/// Commits a scrub transaction.
///
/// When the commit fails the database rolls back and the media rows still
/// exist, so the just-written pending-deletion journal is discarded: recovery
/// must never delete files that survived a rollback.
fn commit_scrub_transaction(
    tx: rusqlite::Transaction<'_>,
    outcome: &ScrubOutcome,
    user_id: i64,
) -> Result<(), DeleteAccountError> {
    if let Err(error) = tx.commit() {
        remove_journal(&outcome.journal_path, user_id);
        return Err(DeleteAccountError::Database(error.to_string()));
    }
    Ok(())
}

/// Scrubs every row owned by one account inside an existing transaction.
///
/// Shared by the password-confirmed immediate deletion path and the scheduled
/// finalization path. The caller commits; any error rolls the transaction back.
/// Before returning, a pending-media-deletion journal is written and fsynced so
/// a crash between the commit and the file removal leaves a durable record.
/// A journal write failure aborts the scrub and rolls the transaction back.
///
/// Documented policy: permanent deletion releases the account's username
/// reservations. Release tombstones are recorded in `instance_settings` before
/// the `username_history` rows are deleted in this transaction, so old profile
/// URLs keep explaining that the handle belonged to a deleted account while
/// every handle becomes available again.
fn scrub_account_rows_in_tx(
    tx: &rusqlite::Transaction<'_>,
    user_id: i64,
    paths: &RuntimePaths,
) -> Result<ScrubOutcome, DeleteAccountError> {
    tx.execute(
                "UPDATE users SET is_deleted = 1, updated_at = CURRENT_TIMESTAMP WHERE id = ? AND is_deleted = 0",
                [user_id],
            )?;
    let media = account_media_in_tx(tx, user_id)?;
    let mut media_paths = Vec::new();
    for item in &media {
        if let Some(original_path) = &item.original_path {
            media_paths.push(original_path.clone());
        }
        media_paths.push(item.stored_path.clone());
        if let Some(thumbnail_path) = &item.thumbnail_path {
            media_paths.push(thumbnail_path.clone());
        }
    }
    let mut safe_paths = Vec::new();
    for path in &media_paths {
        if let Some(safe_path) = safe_media_path(paths, path)? {
            safe_paths.push((path.clone(), safe_path));
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
        "DELETE FROM follow_requests WHERE requester_id = ? OR target_id = ?",
        params![user_id, user_id],
    )?;
    let released_usernames = account_normalized_usernames_in_tx(tx, user_id)?;
    tx.execute("DELETE FROM username_history WHERE user_id = ?", [user_id])?;
    crate::instance::record_released_usernames_tx(tx, &released_usernames)?;
    tx.execute("DELETE FROM account_imports WHERE user_id = ?", [user_id])?;
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
        promote_canonical_references(tx, media_id)?;
        tx.execute("DELETE FROM media_jobs WHERE media_id = ?", [media_id])?;
        tx.execute("DELETE FROM media WHERE id = ?", [media_id])?;
    }
    let mut media_files = BTreeSet::new();
    for (raw_path, safe_path) in safe_paths {
        let remaining: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM media WHERE original_path = ? OR stored_path = ? OR thumbnail_path = ?",
                    params![raw_path, raw_path, raw_path],
                    |row| row.get(0),
                )?;
        if remaining == 0 {
            media_files.insert(safe_path);
        }
    }
    tx.execute("DELETE FROM users WHERE id = ?", [user_id])?;
    let journal_path = write_pending_media_deletion_journal(paths, user_id, &media_files)?;
    Ok(ScrubOutcome {
        media_files,
        journal_path,
    })
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
    let roots = allowed_media_roots(paths);
    let raw_under_roots = roots.iter().any(|root| path.starts_with(root));
    if path.exists() {
        let canonical_path = path
            .canonicalize()
            .map_err(|err| DeleteAccountError::Database(err.to_string()))?;
        let canonical_roots = roots
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
        // A canonical path can legitimately live outside the configured roots
        // when a root's ancestor is a symlink (for example macOS `/var` versus
        // `/private/var`). Accept it only when the raw path is under a
        // configured root or already expressed in canonical root form, so a
        // path that merely resolves into the uploads from elsewhere is still
        // rejected.
        let raw_under_canonical_roots = canonical_roots.iter().any(|root| path.starts_with(root));
        if !raw_under_roots && !raw_under_canonical_roots {
            return Err(DeleteAccountError::UnsafeMediaPath(raw_path.to_owned()));
        }
        return Ok(Some(canonical_path));
    }
    if !raw_under_roots {
        return Err(DeleteAccountError::UnsafeMediaPath(raw_path.to_owned()));
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

fn promote_canonical_references(
    tx: &rusqlite::Transaction<'_>,
    media_id: i64,
) -> anyhow::Result<()> {
    let replacement: Option<i64> = tx
        .query_row(
            "SELECT id FROM media WHERE canonical_media_id = ? ORDER BY id ASC LIMIT 1",
            [media_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(replacement) = replacement else {
        return Ok(());
    };
    tx.execute(
        "UPDATE media SET original_sha256 = '', normalized_sha256 = NULL WHERE id = ?",
        [media_id],
    )?;
    tx.execute(
        "UPDATE media SET canonical_media_id = NULL WHERE id = ?",
        [replacement],
    )?;
    tx.execute(
        "UPDATE media SET canonical_media_id = ? WHERE canonical_media_id = ?",
        params![replacement, media_id],
    )?;
    Ok(())
}

/// Removes the files recorded by one scrub and disposes of its journal.
///
/// The journal is removed only when every file was removed or was already
/// missing. Any failure keeps the journal so
/// [`recover_pending_media_deletions`] can retry the cleanup after a restart.
fn finish_media_cleanup(outcome: &ScrubOutcome, user_id: i64) -> usize {
    let removal = remove_media_files(&outcome.media_files, user_id);
    if removal.complete {
        remove_journal(&outcome.journal_path, user_id);
    } else {
        tracing::warn!(
            user_id,
            journal = %outcome.journal_path.display(),
            "media file removal incomplete; keeping pending media deletion journal for retry"
        );
    }
    removal.deleted
}

struct MediaFileRemoval {
    deleted: usize,
    complete: bool,
}

fn remove_media_files(paths: &BTreeSet<PathBuf>, user_id: i64) -> MediaFileRemoval {
    let mut deletion = MediaFileRemoval {
        deleted: 0,
        complete: true,
    };
    for path in paths {
        match fs::remove_file(path) {
            Ok(()) => deletion.deleted += 1,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                deletion.complete = false;
                tracing::warn!(
                    user_id,
                    path = %path.display(),
                    error = %error,
                    "failed to remove media file after account database deletion"
                );
            }
        }
    }
    deletion
}

fn remove_journal(journal_path: &Path, user_id: i64) {
    match fs::remove_file(journal_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::warn!(
                user_id,
                journal = %journal_path.display(),
                error = %error,
                "failed to remove pending media deletion journal"
            );
        }
    }
}

/// Writes and fsyncs a pending-media-deletion journal for one scrub.
///
/// The journal lives under [`RuntimePaths::tmp_dir`] and records the numeric
/// owner id on the first line followed by one absolute media path per line.
/// Only paths that already passed [`safe_media_path`] are written, so recovery
/// can trust the validation and re-check it. Callers must roll the database
/// transaction back when this fails.
fn write_pending_media_deletion_journal(
    paths: &RuntimePaths,
    user_id: i64,
    media_files: &BTreeSet<PathBuf>,
) -> Result<PathBuf, DeleteAccountError> {
    let journal_path = paths.tmp_dir.join(format!(
        "{PENDING_MEDIA_DELETION_PREFIX}{user_id}-{}{PENDING_MEDIA_DELETION_SUFFIX}",
        Uuid::new_v4().simple()
    ));
    if let Err(error) = write_journal_contents(&journal_path, user_id, media_files) {
        let _ = fs::remove_file(&journal_path);
        return Err(DeleteAccountError::Database(format!(
            "failed to write pending media deletion journal {}: {error}",
            journal_path.display()
        )));
    }
    if let Err(error) = sync_directory(&paths.tmp_dir) {
        tracing::debug!(
            dir = %paths.tmp_dir.display(),
            error = %error,
            "failed to fsync directory after writing pending media deletion journal"
        );
    }
    Ok(journal_path)
}

fn write_journal_contents(
    journal_path: &Path,
    user_id: i64,
    media_files: &BTreeSet<PathBuf>,
) -> anyhow::Result<()> {
    if let Some(parent) = journal_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut contents = String::new();
    contents.push_str(&format!("{user_id}\n"));
    for path in media_files {
        let Some(raw) = path.to_str() else {
            anyhow::bail!("media path is not valid UTF-8");
        };
        contents.push_str(raw);
        contents.push('\n');
    }
    let mut file = fs::File::create(journal_path)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

/// Fsyncs a directory so a newly created or removed journal survives a crash.
fn sync_directory(path: &Path) -> std::io::Result<()> {
    fs::File::open(path)?.sync_all()
}

/// Removes media files left behind by interrupted account deletions.
///
/// Scans [`RuntimePaths::tmp_dir`] for pending-media-deletion journals, at most
/// [`MAX_PENDING_MEDIA_DELETION_JOURNALS_PER_PASS`] per call. Each recorded
/// path is re-validated with [`safe_media_path`]; unsafe entries are skipped
/// and logged, and malformed journals are retained for operator review. A
/// journal is removed only when every recorded file was removed or was already
/// missing. Returns the number of journals fully recovered.
///
/// Safe to call repeatedly, both at startup and from a periodic scheduler; a
/// pass that loses a race with an in-flight finalization still converges on the
/// same end state because removals are idempotent.
pub async fn recover_pending_media_deletions(paths: &RuntimePaths) -> anyhow::Result<usize> {
    let paths = paths.clone();
    tokio::task::spawn_blocking(move || recover_pending_media_deletions_blocking(&paths))
        .await
        .map_err(|error| anyhow::anyhow!("pending media deletion recovery task failed: {error}"))?
}

fn recover_pending_media_deletions_blocking(paths: &RuntimePaths) -> anyhow::Result<usize> {
    let entries = match fs::read_dir(&paths.tmp_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(error).context(format!(
                "failed to read temp directory {}",
                paths.tmp_dir.display()
            ));
        }
    };
    let mut attempted = 0usize;
    let mut recovered = 0usize;
    for entry in entries {
        if attempted >= MAX_PENDING_MEDIA_DELETION_JOURNALS_PER_PASS {
            tracing::warn!(
                limit = MAX_PENDING_MEDIA_DELETION_JOURNALS_PER_PASS,
                "pending media deletion recovery stopped early; too many journals to process"
            );
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!(error = %error, "failed to read temp directory entry");
                continue;
            }
        };
        let journal_path = entry.path();
        if !is_pending_media_deletion_journal(&journal_path) {
            continue;
        }
        attempted += 1;
        match recover_pending_media_deletion_journal(paths, &journal_path) {
            Ok(true) => recovered += 1,
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(
                    journal = %journal_path.display(),
                    error = %error,
                    "failed to recover pending media deletion journal; keeping it for review"
                );
            }
        }
    }
    Ok(recovered)
}

fn is_pending_media_deletion_journal(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name.starts_with(PENDING_MEDIA_DELETION_PREFIX) && name.ends_with(PENDING_MEDIA_DELETION_SUFFIX)
}

/// Removes one journal's files and then the journal itself.
///
/// Returns `Ok(true)` when the journal was fully processed and removed, and
/// `Ok(false)` when it was skipped or must be retained. Symlinked journal files
/// are never read, and every recorded path is re-validated before removal.
fn recover_pending_media_deletion_journal(
    paths: &RuntimePaths,
    journal_path: &Path,
) -> anyhow::Result<bool> {
    let metadata = fs::symlink_metadata(journal_path)?;
    if !metadata.file_type().is_file() {
        tracing::warn!(
            journal = %journal_path.display(),
            "ignoring pending media deletion journal that is not a regular file"
        );
        return Ok(false);
    }
    let contents = fs::read_to_string(journal_path)?;
    let (user_id, media_files) = parse_pending_media_deletion_journal(paths, &contents)
        .with_context(|| {
            format!(
                "malformed pending media deletion journal {}",
                journal_path.display()
            )
        })?;
    let mut deleted = 0usize;
    let mut complete = true;
    for path in &media_files {
        match fs::remove_file(path) {
            Ok(()) => deleted += 1,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                complete = false;
                tracing::warn!(
                    user_id,
                    path = %path.display(),
                    error = %error,
                    "failed to remove media file from pending deletion journal"
                );
            }
        }
    }
    if !complete {
        return Ok(false);
    }
    match fs::remove_file(journal_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to remove pending media deletion journal {}",
                    journal_path.display()
                )
            });
        }
    }
    tracing::info!(
        user_id,
        deleted,
        journal = %journal_path.display(),
        "recovered pending media deletion journal"
    );
    Ok(true)
}

/// Parses a journal into its owner id and already-validated absolute paths.
///
/// The first line must be the numeric owner id; structurally malformed
/// journals produce an error so the caller retains them. Individual unsafe
/// path lines are skipped with a warning instead, and a path that cannot be
/// validated because of a transient filesystem error also retains the journal.
fn parse_pending_media_deletion_journal(
    paths: &RuntimePaths,
    contents: &str,
) -> anyhow::Result<(i64, Vec<PathBuf>)> {
    let mut lines = contents.lines();
    let Some(user_id_line) = lines.next() else {
        anyhow::bail!("journal is empty");
    };
    let user_id = user_id_line
        .trim()
        .parse::<i64>()
        .with_context(|| format!("journal user id line is not an integer: {user_id_line:?}"))?;
    let mut media_files = Vec::new();
    for line in lines {
        let raw = line.trim();
        if raw.is_empty() {
            continue;
        }
        match safe_media_path(paths, raw) {
            Ok(Some(path)) => media_files.push(path),
            Ok(None) => {}
            Err(DeleteAccountError::UnsafeMediaPath(path)) => {
                tracing::warn!(
                    path = %path,
                    "skipping unsafe media path in pending deletion journal"
                );
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok((user_id, media_files))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{auth, config::Settings, db, social};

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
    async fn delete_account_keeps_shared_media_files_referenced_by_other_rows() {
        let (_temp, paths, pool, _settings, alice, bob) = fixture().await;
        let media_path = paths.uploads_images.join("shared.webp");
        fs::write(&media_path, b"shared").expect("shared media file");
        let media_path_string = media_path.to_string_lossy().to_string();
        pool.call(move |conn| {
            conn.execute(
                "INSERT INTO media (owner_user_id, original_filename, stored_path, public_path, mime_type, media_kind, byte_len) VALUES (?, 'alice.webp', ?, '/uploads/images/shared.webp', 'image/webp', 'image', 6)",
                params![alice, media_path_string],
            )?;
            let media_path_string = media_path_string.clone();
            conn.execute(
                "INSERT INTO media (owner_user_id, original_filename, stored_path, public_path, mime_type, media_kind, byte_len) VALUES (?, 'bob.webp', ?, '/uploads/images/shared.webp', 'image/webp', 'image', 6)",
                params![bob, media_path_string],
            )?;
            Ok(())
        })
        .await
        .expect("shared media rows");

        let summary = delete_account(&pool, &paths, alice, "very secure password")
            .await
            .expect("delete account");

        assert_eq!(summary.deleted_media_files, 0);
        assert!(media_path.exists());
        let bob_rows: i64 = pool
            .call(move |conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM media WHERE owner_user_id = ?",
                    [bob],
                    |row| row.get(0),
                )?)
            })
            .await
            .expect("bob media count");
        assert_eq!(bob_rows, 1);
    }

    #[tokio::test]
    async fn delete_account_promotes_shared_canonical_media_for_remaining_rows() {
        let (_temp, paths, pool, _settings, alice, bob) = fixture().await;
        let media_path = paths.uploads_images.join("promoted.webp");
        fs::write(&media_path, b"shared").expect("shared media file");
        let media_path_string = media_path.to_string_lossy().to_string();
        pool.call(move |conn| {
            conn.execute(
                "INSERT INTO media (id, owner_user_id, original_filename, stored_path, public_path, mime_type, media_kind, byte_len, original_sha256, normalized_sha256) VALUES (100, ?, 'alice.webp', ?, '/uploads/images/promoted.webp', 'image/webp', 'image', 6, 'alice-raw', 'shared-normalized')",
                params![alice, media_path_string],
            )?;
            let media_path_string = media_path_string.clone();
            conn.execute(
                "INSERT INTO media (owner_user_id, original_filename, stored_path, public_path, mime_type, media_kind, byte_len, original_sha256, normalized_sha256, canonical_media_id) VALUES (?, 'bob.webp', ?, '/uploads/images/promoted.webp', 'image/webp', 'image', 6, 'bob-raw', 'shared-normalized', 100)",
                params![bob, media_path_string],
            )?;
            Ok(())
        })
        .await
        .expect("shared media rows");

        let summary = delete_account(&pool, &paths, alice, "very secure password")
            .await
            .expect("delete account");

        assert_eq!(summary.deleted_media_files, 0);
        assert!(media_path.exists());
        let bob_canonical: Option<i64> = pool
            .call(move |conn| {
                Ok(conn.query_row(
                    "SELECT canonical_media_id FROM media WHERE owner_user_id = ?",
                    [bob],
                    |row| row.get(0),
                )?)
            })
            .await
            .expect("bob canonical");
        assert_eq!(bob_canonical, None);
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

    async fn user_exists(pool: &SqlitePool, user_id: i64) -> bool {
        pool.call(move |conn| {
            Ok(conn
                .query_row("SELECT 1 FROM users WHERE id = ?", [user_id], |_| Ok(()))
                .optional()?
                .is_some())
        })
        .await
        .expect("user lookup")
    }

    async fn expire_deletion_deadline(pool: &SqlitePool, user_id: i64) {
        pool.call(move |conn| {
            conn.execute(
                "UPDATE users SET deletion_scheduled_at = datetime('now', '-1 second') WHERE id = ?",
                [user_id],
            )?;
            Ok(())
        })
        .await
        .expect("expire deadline");
    }

    fn pending_media_journals(paths: &RuntimePaths) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(&paths.tmp_dir) else {
            return Vec::new();
        };
        let mut journals = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if is_pending_media_deletion_journal(&path) {
                journals.push(path);
            }
        }
        journals.sort();
        journals
    }

    async fn insert_media_row(pool: &SqlitePool, user_id: i64, stored_path: &str) -> i64 {
        let stored_path = stored_path.to_owned();
        pool.call(move |conn| {
            conn.execute(
                "INSERT INTO media (owner_user_id, original_filename, stored_path, public_path, mime_type, media_kind, byte_len) VALUES (?, 'media.webp', ?, '/uploads/images/media.webp', 'image/webp', 'image', 5)",
                params![user_id, stored_path],
            )?;
            Ok(conn.last_insert_rowid())
        })
        .await
        .expect("media row")
    }

    #[tokio::test]
    async fn request_deletion_sets_window_and_is_idempotent() {
        let (_temp, _paths, pool, settings, alice, bob) = fixture().await;

        assert!(
            deletion_status(&pool, alice)
                .await
                .expect("status")
                .is_none()
        );
        let first = request_deletion(&pool, &settings, alice)
            .await
            .expect("request");
        assert!(!first.requested_at.is_empty());
        assert!(first.scheduled_at > first.requested_at);
        let second = request_deletion(&pool, &settings, alice)
            .await
            .expect("repeat request");
        assert_eq!(first, second);
        assert_eq!(
            deletion_status(&pool, alice).await.expect("status"),
            Some(first)
        );
        assert!(
            deletion_status(&pool, bob)
                .await
                .expect("bob status")
                .is_none()
        );
    }

    #[tokio::test]
    async fn request_deletion_with_zero_grace_schedules_immediately() {
        let (_temp, _paths, pool, mut settings, alice, _bob) = fixture().await;
        settings.accounts.deletion_grace_period_days = 0;

        let request = request_deletion(&pool, &settings, alice)
            .await
            .expect("request");

        assert_eq!(request.requested_at, request.scheduled_at);
    }

    #[tokio::test]
    async fn request_deletion_rejects_missing_and_deleted_accounts() {
        let (_temp, _paths, pool, settings, alice, _bob) = fixture().await;

        let missing = request_deletion(&pool, &settings, 9999).await;
        assert_eq!(
            missing.expect_err("missing account").to_string(),
            "account not found"
        );

        pool.call(move |conn| {
            conn.execute("UPDATE users SET is_deleted = 1 WHERE id = ?", [alice])?;
            Ok(())
        })
        .await
        .expect("mark deleted");
        let deleted = request_deletion(&pool, &settings, alice).await;
        assert_eq!(
            deleted.expect_err("deleted account").to_string(),
            "account not found"
        );
    }

    #[tokio::test]
    async fn cancel_deletion_clears_and_is_idempotent() {
        let (_temp, _paths, pool, settings, alice, _bob) = fixture().await;
        request_deletion(&pool, &settings, alice)
            .await
            .expect("request");

        assert!(cancel_deletion(&pool, alice).await.expect("cancel"));
        assert!(!cancel_deletion(&pool, alice).await.expect("cancel again"));
        assert!(
            deletion_status(&pool, alice)
                .await
                .expect("status")
                .is_none()
        );

        let re_requested = request_deletion(&pool, &settings, alice)
            .await
            .expect("re-request");
        assert!(!re_requested.requested_at.is_empty());
    }

    #[tokio::test]
    async fn finalize_due_deletions_ignores_future_and_null_deadlines() {
        let (_temp, paths, pool, settings, alice, bob) = fixture().await;
        request_deletion(&pool, &settings, alice)
            .await
            .expect("request");

        assert_eq!(
            finalize_due_deletions(&pool, &paths)
                .await
                .expect("finalize before deadline"),
            0
        );
        assert!(user_exists(&pool, alice).await);

        pool.call(move |conn| {
            conn.execute(
                "UPDATE users SET deletion_scheduled_at = datetime('now', '+1 day') WHERE id = ?",
                [alice],
            )?;
            Ok(())
        })
        .await
        .expect("push deadline");
        assert_eq!(
            finalize_due_deletions(&pool, &paths)
                .await
                .expect("finalize with future deadline"),
            0
        );
        assert!(user_exists(&pool, alice).await);
        assert!(user_exists(&pool, bob).await);
    }

    #[tokio::test]
    async fn finalize_due_deletions_includes_the_exact_deadline() {
        let (_temp, paths, pool, settings, alice, _bob) = fixture().await;
        request_deletion(&pool, &settings, alice)
            .await
            .expect("request");
        pool.call(move |conn| {
            conn.execute(
                "UPDATE users SET deletion_scheduled_at = CURRENT_TIMESTAMP WHERE id = ?",
                [alice],
            )?;
            Ok(())
        })
        .await
        .expect("set exact deadline");

        assert_eq!(
            finalize_due_deletions(&pool, &paths)
                .await
                .expect("finalize at the deadline"),
            1
        );
    }

    #[tokio::test]
    async fn finalize_due_deletions_removes_due_accounts_and_releases_handle() {
        let (_temp, paths, pool, settings, alice, bob) = fixture().await;
        social::create_post(&pool, &settings, Some(alice), "alice post", None, &[])
            .await
            .expect("post");
        social::follow(&pool, bob, alice).await.expect("follow");
        let media_path = paths.uploads_images.join("alice-due.webp");
        fs::write(&media_path, b"image").expect("media file");
        let media_path_string = media_path.to_string_lossy().to_string();
        pool.call(move |conn| {
            conn.execute(
                "INSERT INTO media (owner_user_id, original_filename, stored_path, public_path, mime_type, media_kind, byte_len) VALUES (?, 'alice-due.webp', ?, '/uploads/images/alice-due.webp', 'image/webp', 'image', 5)",
                params![alice, media_path_string],
            )?;
            conn.execute(
                "INSERT INTO follow_requests (requester_id, target_id) VALUES (?, ?)",
                params![bob, alice],
            )?;
            conn.execute(
                "INSERT INTO follow_requests (requester_id, target_id) VALUES (?, ?)",
                params![alice, bob],
            )?;
            conn.execute(
                "INSERT INTO account_imports (user_id, archive_id, format_version) VALUES (?, 'archive-1', 1)",
                [alice],
            )?;
            Ok(())
        })
        .await
        .expect("account rows");
        crate::identity::change_username(&pool, &settings, alice, "alice_scheduled")
            .await
            .expect("rename alice");
        request_deletion(&pool, &settings, alice)
            .await
            .expect("request deletion");
        expire_deletion_deadline(&pool, alice).await;

        let removed = finalize_due_deletions(&pool, &paths)
            .await
            .expect("finalize");

        assert_eq!(removed, 1);
        assert!(!media_path.exists());
        let counts: (i64, i64, i64, i64, i64, i64) = pool
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
                    conn.query_row(
                        "SELECT COUNT(*) FROM follow_requests WHERE requester_id = ? OR target_id = ?",
                        params![alice, alice],
                        |row| row.get(0),
                    )?,
                    conn.query_row(
                        "SELECT COUNT(*) FROM username_history WHERE user_id = ?",
                        [alice],
                        |row| row.get(0),
                    )?,
                    conn.query_row(
                        "SELECT COUNT(*) FROM account_imports WHERE user_id = ?",
                        [alice],
                        |row| row.get(0),
                    )?,
                ))
            })
            .await
            .expect("counts");
        assert_eq!(counts, (0, 0, 0, 0, 0, 0));
        assert!(user_exists(&pool, bob).await);
        crate::auth::register_user(&pool, &settings, "alice", "very secure password", false)
            .await
            .expect("historical handle released");
        crate::auth::register_user(
            &pool,
            &settings,
            "alice_scheduled",
            "very secure password",
            false,
        )
        .await
        .expect("current handle released");
        assert_eq!(
            finalize_due_deletions(&pool, &paths)
                .await
                .expect("finalize again"),
            0
        );
    }

    #[tokio::test]
    async fn finalize_due_deletions_yields_to_cancellation() {
        let (_temp, paths, pool, settings, alice, _bob) = fixture().await;
        request_deletion(&pool, &settings, alice)
            .await
            .expect("request");
        expire_deletion_deadline(&pool, alice).await;

        assert!(cancel_deletion(&pool, alice).await.expect("cancel first"));
        assert_eq!(
            finalize_due_deletions(&pool, &paths)
                .await
                .expect("finalize"),
            0
        );
        assert!(user_exists(&pool, alice).await);
        assert!(!cancel_deletion(&pool, alice).await.expect("cancel again"));
    }

    #[tokio::test]
    async fn delete_account_scrubs_follow_requests_username_history_and_imports() {
        let (_temp, paths, pool, settings, alice, bob) = fixture().await;
        pool.call(move |conn| {
            conn.execute(
                "INSERT INTO follow_requests (requester_id, target_id) VALUES (?, ?)",
                params![bob, alice],
            )?;
            conn.execute(
                "INSERT INTO follow_requests (requester_id, target_id) VALUES (?, ?)",
                params![alice, bob],
            )?;
            conn.execute(
                "INSERT INTO account_imports (user_id, archive_id, format_version) VALUES (?, 'archive-1', 1)",
                [alice],
            )?;
            Ok(())
        })
        .await
        .expect("account rows");
        crate::identity::change_username(&pool, &settings, alice, "alice_immediate")
            .await
            .expect("rename alice");

        delete_account(&pool, &paths, alice, "very secure password")
            .await
            .expect("delete account");

        let counts: (i64, i64, i64) = pool
            .call(move |conn| {
                Ok((
                    conn.query_row(
                        "SELECT COUNT(*) FROM follow_requests WHERE requester_id = ? OR target_id = ?",
                        params![alice, alice],
                        |row| row.get(0),
                    )?,
                    conn.query_row(
                        "SELECT COUNT(*) FROM username_history WHERE user_id = ?",
                        [alice],
                        |row| row.get(0),
                    )?,
                    conn.query_row(
                        "SELECT COUNT(*) FROM account_imports WHERE user_id = ?",
                        [alice],
                        |row| row.get(0),
                    )?,
                ))
            })
            .await
            .expect("counts");
        assert_eq!(counts, (0, 0, 0));
    }

    #[tokio::test]
    async fn concurrent_finalizations_delete_exactly_once() {
        let (_temp, paths, pool, settings, alice, _bob) = fixture().await;
        social::create_post(&pool, &settings, Some(alice), "alice post", None, &[])
            .await
            .expect("post");
        let media_path = paths.uploads_images.join("concurrent.webp");
        fs::write(&media_path, b"image").expect("media file");
        insert_media_row(&pool, alice, &media_path.to_string_lossy()).await;
        request_deletion(&pool, &settings, alice)
            .await
            .expect("request");
        expire_deletion_deadline(&pool, alice).await;

        let (first, second) = tokio::join!(
            finalize_due_deletions(&pool, &paths),
            finalize_due_deletions(&pool, &paths)
        );
        let first = first.expect("first finalize");
        let second = second.expect("second finalize");

        assert_eq!(
            first + second,
            1,
            "exactly one finalizer must claim the account"
        );
        assert!(!user_exists(&pool, alice).await);
        assert!(!media_path.exists());
        assert!(pending_media_journals(&paths).is_empty());
        let counts: (i64, i64) = pool
            .call(move |conn| {
                Ok((
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
                ))
            })
            .await
            .expect("counts");
        assert_eq!(counts, (0, 0));
        assert_eq!(
            finalize_due_deletions(&pool, &paths)
                .await
                .expect("third finalize"),
            0
        );
    }

    #[tokio::test]
    async fn cancellation_before_claim_keeps_account_intact() {
        let (_temp, paths, pool, settings, alice, _bob) = fixture().await;
        social::create_post(&pool, &settings, Some(alice), "alice post", None, &[])
            .await
            .expect("post");
        let media_path = paths.uploads_images.join("cancelled.webp");
        fs::write(&media_path, b"image").expect("media file");
        insert_media_row(&pool, alice, &media_path.to_string_lossy()).await;
        request_deletion(&pool, &settings, alice)
            .await
            .expect("request");
        expire_deletion_deadline(&pool, alice).await;

        assert!(cancel_deletion(&pool, alice).await.expect("cancel"));
        assert!(
            deletion_status(&pool, alice)
                .await
                .expect("status")
                .is_none()
        );
        assert_eq!(
            finalize_due_deletions(&pool, &paths)
                .await
                .expect("finalize after cancellation"),
            0
        );
        assert!(user_exists(&pool, alice).await);
        assert!(media_path.exists());
        let posts: i64 = pool
            .call(move |conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM posts WHERE user_id = ?",
                    [alice],
                    |row| row.get(0),
                )?)
            })
            .await
            .expect("posts");
        assert_eq!(posts, 1);
        assert_eq!(pending_media_journals(&paths).len(), 0);
    }

    #[tokio::test]
    async fn cancellation_after_finalization_cannot_resurrect_account() {
        let (_temp, paths, pool, settings, alice, _bob) = fixture().await;
        request_deletion(&pool, &settings, alice)
            .await
            .expect("request");
        expire_deletion_deadline(&pool, alice).await;
        assert_eq!(
            finalize_due_deletions(&pool, &paths)
                .await
                .expect("finalize"),
            1
        );

        assert!(!cancel_deletion(&pool, alice).await.expect("cancel"));
        assert!(!user_exists(&pool, alice).await);
        assert!(
            deletion_status(&pool, alice)
                .await
                .expect("status")
                .is_none()
        );
    }

    #[tokio::test]
    async fn concurrent_cancel_and_finalize_never_half_scrub() {
        let (_temp, paths, pool, settings, alice, _bob) = fixture().await;
        social::create_post(&pool, &settings, Some(alice), "alice post", None, &[])
            .await
            .expect("post");
        let media_path = paths.uploads_images.join("racing.webp");
        fs::write(&media_path, b"image").expect("media file");
        insert_media_row(&pool, alice, &media_path.to_string_lossy()).await;
        let _session = auth::create_session(&pool, alice).await.expect("session");
        request_deletion(&pool, &settings, alice)
            .await
            .expect("request");
        expire_deletion_deadline(&pool, alice).await;

        let (cancelled, finalized) = tokio::join!(
            cancel_deletion(&pool, alice),
            finalize_due_deletions(&pool, &paths)
        );
        let cancelled = cancelled.expect("cancel");
        let finalized = finalized.expect("finalize");

        let present = user_exists(&pool, alice).await;
        let counts: (i64, i64, i64) = pool
            .call(move |conn| {
                Ok((
                    conn.query_row(
                        "SELECT COUNT(*) FROM posts WHERE user_id = ?",
                        [alice],
                        |row| row.get(0),
                    )?,
                    conn.query_row(
                        "SELECT COUNT(*) FROM sessions WHERE user_id = ?",
                        [alice],
                        |row| row.get(0),
                    )?,
                    conn.query_row(
                        "SELECT COUNT(*) FROM media WHERE owner_user_id = ?",
                        [alice],
                        |row| row.get(0),
                    )?,
                ))
            })
            .await
            .expect("counts");
        if present {
            assert!(cancelled, "a surviving account must have been cancelled");
            assert_eq!(finalized, 0);
            assert_eq!(
                counts,
                (1, 1, 1),
                "cancellation must leave every row intact"
            );
            assert!(media_path.exists());
            assert!(
                deletion_status(&pool, alice)
                    .await
                    .expect("status")
                    .is_none()
            );
        } else {
            assert!(!cancelled, "a removed account must reject cancellation");
            assert_eq!(finalized, 1);
            assert_eq!(counts, (0, 0, 0), "deletion must remove every owned row");
            assert!(!media_path.exists());
        }
        assert!(pending_media_journals(&paths).is_empty());
        assert_eq!(
            finalize_due_deletions(&pool, &paths)
                .await
                .expect("sweep after the race"),
            0
        );
        assert_eq!(user_exists(&pool, alice).await, present);
    }

    #[tokio::test]
    async fn unsafe_media_path_rolls_finalization_back_until_fixed() {
        let (_temp, paths, pool, settings, alice, _bob) = fixture().await;
        social::create_post(&pool, &settings, Some(alice), "alice post", None, &[])
            .await
            .expect("post");
        insert_media_row(&pool, alice, "../escape.webp").await;
        request_deletion(&pool, &settings, alice)
            .await
            .expect("request");
        expire_deletion_deadline(&pool, alice).await;
        let deadline = deletion_status(&pool, alice)
            .await
            .expect("status")
            .expect("pending")
            .scheduled_at;

        assert_eq!(
            finalize_due_deletions(&pool, &paths)
                .await
                .expect("finalize with unsafe path"),
            0
        );
        assert!(user_exists(&pool, alice).await);
        assert_eq!(
            deletion_status(&pool, alice)
                .await
                .expect("status")
                .expect("still pending")
                .scheduled_at,
            deadline,
            "a rolled-back finalization must keep the original deadline"
        );
        let counts: (i64, i64) = pool
            .call(move |conn| {
                Ok((
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
                ))
            })
            .await
            .expect("counts");
        assert_eq!(counts, (1, 1));
        assert!(pending_media_journals(&paths).is_empty());

        let fixed_path = paths.uploads_images.join("fixed.webp");
        fs::write(&fixed_path, b"image").expect("fixed media file");
        let fixed = fixed_path.to_string_lossy().to_string();
        pool.call(move |conn| {
            conn.execute(
                "UPDATE media SET stored_path = ? WHERE owner_user_id = ?",
                params![fixed, alice],
            )?;
            Ok(())
        })
        .await
        .expect("fix stored path");

        assert_eq!(
            finalize_due_deletions(&pool, &paths)
                .await
                .expect("finalize after fix"),
            1
        );
        assert!(!user_exists(&pool, alice).await);
        assert!(!fixed_path.exists());
        assert!(pending_media_journals(&paths).is_empty());
    }

    #[tokio::test]
    async fn failed_media_removal_retains_journal_until_recovery() {
        let (_temp, paths, pool, settings, alice, _bob) = fixture().await;
        let blocked_path = paths.uploads_images.join("blocked-media");
        fs::create_dir(&blocked_path).expect("media directory");
        fs::write(blocked_path.join("inner.webp"), b"inner").expect("inner file");
        insert_media_row(&pool, alice, &blocked_path.to_string_lossy()).await;
        request_deletion(&pool, &settings, alice)
            .await
            .expect("request");
        expire_deletion_deadline(&pool, alice).await;

        assert_eq!(
            finalize_due_deletions(&pool, &paths)
                .await
                .expect("finalize"),
            1
        );
        assert!(!user_exists(&pool, alice).await);
        assert_eq!(
            pending_media_journals(&paths).len(),
            1,
            "a failed removal must keep its journal"
        );
        assert!(blocked_path.exists());

        fs::remove_dir_all(&blocked_path).expect("clear blocked directory");
        fs::write(&blocked_path, b"image").expect("regular file");
        assert_eq!(
            recover_pending_media_deletions(&paths)
                .await
                .expect("recover"),
            1
        );
        assert!(!blocked_path.exists());
        assert!(pending_media_journals(&paths).is_empty());
        assert_eq!(
            recover_pending_media_deletions(&paths)
                .await
                .expect("recover again"),
            0
        );
    }

    #[tokio::test]
    async fn recovery_processes_crash_journal_and_skips_unsafe_lines() {
        let (temp, paths, _pool, _settings, alice, _bob) = fixture().await;
        let real_path = paths.uploads_videos.join("crashed.mp4");
        fs::write(&real_path, b"video").expect("real media file");
        let missing_path = paths.uploads_thumbs.join("missing.webp");
        let outside_path = temp.path().join("outside.webp");
        fs::write(&outside_path, b"outside").expect("outside file");
        let journal_path = paths.tmp_dir.join(format!(
            "{PENDING_MEDIA_DELETION_PREFIX}{alice}-crash{PENDING_MEDIA_DELETION_SUFFIX}"
        ));
        fs::write(
            &journal_path,
            format!(
                "{alice}\n{}\n{}\n{}\n../relative.webp\n",
                real_path.display(),
                missing_path.display(),
                outside_path.display()
            ),
        )
        .expect("crash journal");

        let cleanup_removed = paths
            .cleanup_stale_temp_files(std::time::Duration::ZERO)
            .expect("temp cleanup");
        assert_eq!(cleanup_removed, 0);
        assert!(
            journal_path.exists(),
            "runtime temp cleanup must not remove deletion journals"
        );

        assert_eq!(
            recover_pending_media_deletions(&paths)
                .await
                .expect("recover"),
            1
        );
        assert!(!real_path.exists());
        assert!(!missing_path.exists());
        assert!(
            outside_path.exists(),
            "paths outside the upload roots must never be removed"
        );
        assert!(!journal_path.exists());
        assert_eq!(
            recover_pending_media_deletions(&paths)
                .await
                .expect("recover again"),
            0
        );
    }

    #[tokio::test]
    async fn recovery_retains_malformed_journals() {
        let (_temp, paths, _pool, _settings, alice, _bob) = fixture().await;
        let real_path = paths.uploads_images.join("review-target.webp");
        fs::write(&real_path, b"image").expect("media file");
        let journal_path = paths.tmp_dir.join(format!(
            "{PENDING_MEDIA_DELETION_PREFIX}malformed{PENDING_MEDIA_DELETION_SUFFIX}"
        ));

        fs::write(&journal_path, "").expect("empty journal");
        assert_eq!(
            recover_pending_media_deletions(&paths)
                .await
                .expect("recover empty"),
            0
        );
        assert!(journal_path.exists());

        fs::write(
            &journal_path,
            format!("not-a-user-id\n{}\n", real_path.display()),
        )
        .expect("garbage journal");
        assert_eq!(
            recover_pending_media_deletions(&paths)
                .await
                .expect("recover garbage"),
            0
        );
        assert!(journal_path.exists());
        assert!(real_path.exists());

        fs::write(&journal_path, format!("{alice}\n{}\n", real_path.display()))
            .expect("fixed journal");
        assert_eq!(
            recover_pending_media_deletions(&paths)
                .await
                .expect("recover fixed"),
            1
        );
        assert!(!real_path.exists());
        assert!(!journal_path.exists());
    }

    #[tokio::test]
    async fn startup_and_periodic_sweeps_are_idempotent() {
        let (_temp, paths, pool, settings, alice, bob) = fixture().await;
        let alice_media = paths.uploads_images.join("restart-sweep.webp");
        fs::write(&alice_media, b"image").expect("alice media file");
        insert_media_row(&pool, alice, &alice_media.to_string_lossy()).await;
        request_deletion(&pool, &settings, alice)
            .await
            .expect("request alice");
        expire_deletion_deadline(&pool, alice).await;

        assert_eq!(
            finalize_due_deletions(&pool, &paths)
                .await
                .expect("startup finalize"),
            1
        );
        assert_eq!(
            recover_pending_media_deletions(&paths)
                .await
                .expect("startup recovery"),
            0
        );
        assert_eq!(
            finalize_due_deletions(&pool, &paths)
                .await
                .expect("periodic finalize"),
            0
        );
        assert_eq!(
            recover_pending_media_deletions(&paths)
                .await
                .expect("periodic recovery"),
            0
        );
        assert!(!user_exists(&pool, alice).await);
        assert!(!alice_media.exists());

        let bob_media = paths.uploads_images.join("concurrent-sweep.webp");
        fs::write(&bob_media, b"image").expect("bob media file");
        insert_media_row(&pool, bob, &bob_media.to_string_lossy()).await;
        request_deletion(&pool, &settings, bob)
            .await
            .expect("request bob");
        expire_deletion_deadline(&pool, bob).await;

        let (finalized, recovered) = tokio::join!(
            finalize_due_deletions(&pool, &paths),
            recover_pending_media_deletions(&paths)
        );
        assert_eq!(finalized.expect("concurrent finalize"), 1);
        recovered.expect("concurrent recovery");
        assert!(!user_exists(&pool, bob).await);
        assert!(!bob_media.exists());
        assert!(pending_media_journals(&paths).is_empty());
        assert_eq!(
            recover_pending_media_deletions(&paths)
                .await
                .expect("recover after sweep"),
            0
        );
    }

    #[tokio::test]
    async fn zero_grace_deletion_removes_account_and_media_immediately() {
        let (_temp, paths, pool, mut settings, alice, _bob) = fixture().await;
        settings.accounts.deletion_grace_period_days = 0;
        let media_path = paths.uploads_images.join("zero-grace.webp");
        fs::write(&media_path, b"image").expect("media file");
        insert_media_row(&pool, alice, &media_path.to_string_lossy()).await;

        let request = request_deletion(&pool, &settings, alice)
            .await
            .expect("request");
        assert_eq!(request.requested_at, request.scheduled_at);
        assert_eq!(
            finalize_due_deletions(&pool, &paths)
                .await
                .expect("finalize immediately"),
            1
        );
        assert!(!user_exists(&pool, alice).await);
        assert!(!media_path.exists());
        assert!(pending_media_journals(&paths).is_empty());
    }

    #[tokio::test]
    async fn zero_grace_cancellation_before_finalization_wins() {
        let (_temp, paths, pool, mut settings, alice, _bob) = fixture().await;
        settings.accounts.deletion_grace_period_days = 0;
        let media_path = paths.uploads_images.join("zero-grace-cancel.webp");
        fs::write(&media_path, b"image").expect("media file");
        insert_media_row(&pool, alice, &media_path.to_string_lossy()).await;
        request_deletion(&pool, &settings, alice)
            .await
            .expect("request");

        assert!(cancel_deletion(&pool, alice).await.expect("cancel"));
        assert_eq!(
            finalize_due_deletions(&pool, &paths)
                .await
                .expect("finalize after cancellation"),
            0
        );
        assert!(user_exists(&pool, alice).await);
        assert!(media_path.exists());
        assert!(
            deletion_status(&pool, alice)
                .await
                .expect("status")
                .is_none()
        );
    }

    #[tokio::test]
    async fn finalization_removes_every_session_for_the_account() {
        let (_temp, paths, pool, settings, alice, _bob) = fixture().await;
        let first = auth::create_session(&pool, alice)
            .await
            .expect("first session");
        let second = auth::create_session(&pool, alice)
            .await
            .expect("second session");
        request_deletion(&pool, &settings, alice)
            .await
            .expect("request");
        expire_deletion_deadline(&pool, alice).await;

        assert_eq!(
            finalize_due_deletions(&pool, &paths)
                .await
                .expect("finalize"),
            1
        );

        let sessions: i64 = pool
            .call(move |conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM sessions WHERE user_id = ?",
                    [alice],
                    |row| row.get(0),
                )?)
            })
            .await
            .expect("sessions");
        assert_eq!(sessions, 0);
        let token_hashes = vec![
            auth::hash_token(&first.token),
            auth::hash_token(&second.token),
        ];
        let resolved: i64 = pool
            .call(move |conn| {
                let mut resolved = 0;
                for token_hash in &token_hashes {
                    resolved += conn.query_row(
                        "SELECT COUNT(*) FROM sessions WHERE token_hash = ?",
                        [token_hash.as_str()],
                        |row| row.get::<_, i64>(0),
                    )?;
                }
                Ok(resolved)
            })
            .await
            .expect("token lookups");
        assert_eq!(
            resolved, 0,
            "session tokens must not resolve after deletion"
        );
    }

    #[tokio::test]
    async fn concurrent_request_deletion_is_idempotent() {
        let (_temp, _paths, pool, settings, alice, _bob) = fixture().await;
        let (first, second) = tokio::join!(
            request_deletion(&pool, &settings, alice),
            request_deletion(&pool, &settings, alice)
        );
        let first = first.expect("first request");
        let second = second.expect("second request");
        assert_eq!(first, second);

        let stored = deletion_status(&pool, alice)
            .await
            .expect("status")
            .expect("pending");
        assert_eq!(
            stored, first,
            "concurrent requests must not move the deadline"
        );

        let repeated = request_deletion(&pool, &settings, alice)
            .await
            .expect("repeated request");
        assert_eq!(repeated, first);
        assert_eq!(
            deletion_status(&pool, alice)
                .await
                .expect("status")
                .expect("still pending"),
            first
        );
    }
}
