use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Context as _;
use tracing::{info, warn};

/// Number of in-flight operations that stage files under the runtime temp
/// directory (media uploads, account archive export/import, restore uploads).
/// File cleanup is skipped while any operation is active so a slow but healthy
/// operation can never have its staging directory removed underneath it.
static ACTIVE_TEMP_OPERATIONS: AtomicUsize = AtomicUsize::new(0);

/// Maximum directory entries inspected in one cleanup pass. Cleanup is
/// opportunistic; bounding each pass keeps a pathological temp directory from
/// stalling the maintenance scheduler.
const MAX_CLEANUP_SCAN_ENTRIES: usize = 10_000;

/// Marks one long-running operation that stages files under
/// [`RuntimePaths::tmp_dir`]. The guard must live for the whole operation.
#[must_use]
pub struct TempOperationGuard {
    _private: (),
}

/// Begins a temp-staging operation. See [`TempOperationGuard`].
pub fn begin_temp_operation() -> TempOperationGuard {
    ACTIVE_TEMP_OPERATIONS.fetch_add(1, Ordering::SeqCst);
    TempOperationGuard { _private: () }
}

impl Drop for TempOperationGuard {
    fn drop(&mut self) {
        ACTIVE_TEMP_OPERATIONS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Whether any operation is currently staging files under `tmp_dir`.
#[must_use]
pub fn temp_operations_active() -> bool {
    ACTIVE_TEMP_OPERATIONS.load(Ordering::SeqCst) > 0
}

#[derive(Debug, Clone)]
pub struct RuntimePaths {
    pub data_dir: PathBuf,
    pub settings_path: PathBuf,
    pub db_dir: PathBuf,
    pub database_path: PathBuf,
    pub uploads_originals: PathBuf,
    pub uploads_images: PathBuf,
    pub uploads_videos: PathBuf,
    pub uploads_thumbs: PathBuf,
    pub assets_dir: PathBuf,
    pub tmp_dir: PathBuf,
    pub tmp_uploads: PathBuf,
    pub backups_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub tor_dir: PathBuf,
    pub tor_onion_service_dir: PathBuf,
}

impl RuntimePaths {
    pub fn discover(configured_data_dir: Option<&Path>) -> anyhow::Result<Self> {
        let data_dir = match configured_data_dir {
            Some(path) => path.to_path_buf(),
            None => exe_dir()?.join("rustpost-data"),
        };
        let data_dir = if data_dir.is_absolute() {
            data_dir
        } else {
            env::current_dir()?.join(data_dir)
        };
        Ok(Self::from_data_dir(data_dir))
    }

    #[must_use]
    pub fn from_data_dir(data_dir: PathBuf) -> Self {
        let db_dir = data_dir.join("db");
        let uploads_dir = data_dir.join("uploads");
        let assets_dir = data_dir.join("assets");
        let tmp_dir = data_dir.join("tmp");
        Self {
            settings_path: data_dir.join("settings.toml"),
            database_path: db_dir.join("rustpost.sqlite3"),
            db_dir,
            uploads_originals: uploads_dir.join("originals"),
            uploads_images: uploads_dir.join("images"),
            uploads_videos: uploads_dir.join("videos"),
            uploads_thumbs: uploads_dir.join("thumbs"),
            assets_dir,
            tmp_uploads: tmp_dir.join("uploads"),
            tmp_dir,
            backups_dir: data_dir.join("backups"),
            logs_dir: data_dir.join("logs"),
            tor_dir: data_dir.join("tor"),
            tor_onion_service_dir: data_dir.join("tor/onion-service"),
            data_dir,
        }
    }

    #[must_use]
    pub fn with_tor_data_dir(mut self, tor_data_dir: &str) -> Self {
        self.tor_dir = self.data_dir.join(tor_data_dir);
        self.tor_onion_service_dir = self.tor_dir.join("onion-service");
        self
    }

    #[must_use]
    pub fn with_backup_dir(mut self, backup_dir: &str) -> Self {
        self.backups_dir = self.data_dir.join(backup_dir);
        self
    }

    pub fn ensure(&self) -> anyhow::Result<()> {
        for path in [
            &self.data_dir,
            &self.db_dir,
            &self.uploads_originals,
            &self.uploads_images,
            &self.uploads_videos,
            &self.uploads_thumbs,
            &self.assets_dir,
            &self.tmp_dir,
            &self.tmp_uploads,
            &self.backups_dir,
            &self.logs_dir,
            &self.tor_dir,
            &self.tor_onion_service_dir,
        ] {
            fs::create_dir_all(path).with_context(|| {
                format!("failed to create runtime directory {}", path.display())
            })?;
        }
        self.migrate_legacy_database_layout()?;
        restrict_dir(&self.data_dir)?;
        restrict_dir(&self.backups_dir)?;
        restrict_dir(&self.tor_dir)?;
        restrict_dir(&self.tor_onion_service_dir)?;
        Ok(())
    }

    #[must_use]
    pub fn staged_upload_path(&self, id: &str) -> PathBuf {
        self.tmp_uploads.join(format!("{id}.upload"))
    }

    fn migrate_legacy_database_layout(&self) -> anyhow::Result<()> {
        let legacy_database = self.legacy_database_path();
        if legacy_database.exists() && self.database_path.exists() {
            anyhow::bail!(
                "database layout conflict: both legacy database {} and new database {} exist; move one aside before starting RustPost",
                legacy_database.display(),
                self.database_path.display()
            );
        }
        if legacy_database.exists() {
            self.ensure_no_legacy_sidecar_conflict("wal")?;
            self.ensure_no_legacy_sidecar_conflict("shm")?;
            fs::rename(&legacy_database, &self.database_path).with_context(|| {
                format!(
                    "failed to migrate database from {} to {}",
                    legacy_database.display(),
                    self.database_path.display()
                )
            })?;
            info!(
                from = %legacy_database.display(),
                to = %self.database_path.display(),
                "migrated RustPost database into db directory"
            );
            self.migrate_legacy_database_sidecar("wal")?;
            self.migrate_legacy_database_sidecar("shm")?;
        } else {
            self.warn_about_orphaned_legacy_sidecar("wal");
            self.warn_about_orphaned_legacy_sidecar("shm");
        }
        Ok(())
    }

    fn ensure_no_legacy_sidecar_conflict(&self, suffix: &str) -> anyhow::Result<()> {
        let old = self.legacy_database_sidecar_path(suffix);
        let new = self.database_sidecar_path(suffix);
        if old.exists() && new.exists() {
            anyhow::bail!(
                "database layout conflict: both legacy SQLite sidecar {} and new SQLite sidecar {} exist; move one aside before starting RustPost",
                old.display(),
                new.display()
            );
        }
        Ok(())
    }

    fn migrate_legacy_database_sidecar(&self, suffix: &str) -> anyhow::Result<()> {
        let old = self.legacy_database_sidecar_path(suffix);
        if !old.exists() {
            return Ok(());
        }
        let new = self.database_sidecar_path(suffix);
        fs::rename(&old, &new).with_context(|| {
            format!(
                "failed to migrate SQLite sidecar from {} to {}",
                old.display(),
                new.display()
            )
        })?;
        info!(
            from = %old.display(),
            to = %new.display(),
            "migrated RustPost SQLite sidecar into db directory"
        );
        Ok(())
    }

    fn warn_about_orphaned_legacy_sidecar(&self, suffix: &str) {
        let old = self.legacy_database_sidecar_path(suffix);
        if old.exists() {
            warn!(
                path = %old.display(),
                "legacy SQLite sidecar exists without legacy database; left in place for operator review"
            );
        }
    }

    fn legacy_database_path(&self) -> PathBuf {
        self.data_dir.join("app.sqlite3")
    }

    fn legacy_database_sidecar_path(&self, suffix: &str) -> PathBuf {
        self.data_dir.join(format!("app.sqlite3-{suffix}"))
    }

    pub fn database_sidecar_path(&self, suffix: &str) -> PathBuf {
        self.db_dir.join(format!("rustpost.sqlite3-{suffix}"))
    }

    /// Removes leftover upload staging files, abandoned restore uploads, and
    /// staged account export/import archives older than `max_age`. Called with
    /// a grace period during startup and while running.
    ///
    /// Safety properties:
    /// * Returns without touching anything while a staged operation is active,
    ///   so in-flight uploads/imports/exports are never removed.
    /// * Only entries directly inside `tmp_dir` or `tmp_uploads` matching known
    ///   staging prefixes are candidates; symlinks are unlinked, never
    ///   followed, and directories are removed only with `remove_dir_all`
    ///   after confirming they are not symlinks.
    /// * Each pass inspects at most `MAX_CLEANUP_SCAN_ENTRIES` entries.
    pub fn cleanup_stale_temp_files(&self, max_age: std::time::Duration) -> anyhow::Result<usize> {
        if temp_operations_active() {
            tracing::debug!("skipping temp cleanup while a staged operation is active");
            return Ok(0);
        }
        let mut removed = 0usize;
        let mut scanned = 0usize;
        for directory in [&self.tmp_dir, &self.tmp_uploads] {
            if fs::symlink_metadata(directory)
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
            {
                continue;
            }
            let entries = match fs::read_dir(directory) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("failed to read temp directory {}", self.tmp_dir.display())
                    });
                }
            };
            for entry in entries {
                if scanned >= MAX_CLEANUP_SCAN_ENTRIES {
                    tracing::warn!(
                        limit = MAX_CLEANUP_SCAN_ENTRIES,
                        "temp cleanup stopped early; too many entries in the runtime temp directory"
                    );
                    return Ok(removed);
                }
                scanned += 1;
                let entry = entry?;
                let path = entry.path();
                let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };
                let is_staging_name = name.ends_with(".upload")
                    || Path::new(name)
                        .extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("tmp"))
                    || name.starts_with("restore-upload-")
                    || name.starts_with(crate::portability::EXPORT_TMP_PREFIX)
                    || name.starts_with(crate::portability::IMPORT_TMP_PREFIX);
                if !is_staging_name {
                    continue;
                }
                // Use `symlink_metadata` so a symlink is treated as a link (never
                // followed) and only real directories are recursively removed.
                let Ok(metadata) = fs::symlink_metadata(&path) else {
                    continue;
                };
                let is_symlink = metadata.file_type().is_symlink();
                if !is_symlink && !metadata.is_file() && !metadata.is_dir() {
                    continue;
                }
                let stale = metadata
                    .modified()
                    .is_ok_and(|modified| modified.elapsed().is_ok_and(|age| age >= max_age));
                if !stale {
                    continue;
                }
                let removed_entry = if is_symlink || metadata.is_file() {
                    fs::remove_file(&path).is_ok()
                } else {
                    fs::remove_dir_all(&path).is_ok()
                };
                if removed_entry {
                    removed += 1;
                } else {
                    tracing::warn!(path = %path.display(), "failed to remove stale temp entry");
                }
            }
        }
        Ok(removed)
    }
}

#[cfg(unix)]
fn restrict_dir(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_dir(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

fn exe_dir() -> anyhow::Result<PathBuf> {
    let exe = env::current_exe()?;
    exe.parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow::anyhow!("cannot resolve executable directory"))
}

/// Acquires the lifetime lease held by the HTTP server, or an offline restore.
/// OS locks release on process termination; the persistent file is never removed.
pub(crate) fn acquire_server_lease(paths: &RuntimePaths) -> anyhow::Result<fs::File> {
    let lease = fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(paths.data_dir.join("server-runtime.lock"))?;
    lease
        .try_lock()
        .context("RustPost is already running for this data directory")?;
    Ok(lease)
}

/// Refuses to open a partially restored runtime after interruption. The
/// rollback directory contains unique originals and must never be temp-cleaned.
pub(crate) fn check_restore_recovery(paths: &RuntimePaths) -> anyhow::Result<()> {
    for entry in fs::read_dir(&paths.data_dir)? {
        let entry = entry?;
        anyhow::ensure!(
            !entry
                .file_name()
                .to_string_lossy()
                .starts_with(".rustpost-restore-rollback-"),
            "interrupted restore requires operator recovery from {}; consult the backup recovery guide before starting or restoring",
            entry.path().display()
        );
    }
    Ok(())
}

/// Shared request/maintenance lease; a snapshot holds the exclusive lease.
pub(crate) fn snapshot_lease(paths: &RuntimePaths, exclusive: bool) -> anyhow::Result<fs::File> {
    let lease = fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(paths.data_dir.join("snapshot-coordination.lock"))?;
    if exclusive {
        lease.try_lock()?;
    } else {
        lease.try_lock_shared()?;
    }
    Ok(lease)
}
