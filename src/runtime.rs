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
    /// a zero age during startup, where no request can be in flight, and with
    /// a grace period while running.
    ///
    /// Safety properties:
    /// * Returns without touching anything while a staged operation is active,
    ///   so in-flight uploads/imports/exports are never removed.
    /// * Only entries directly inside `tmp_dir` whose names match the known
    ///   staging prefixes are candidates; symlinks are unlinked, never
    ///   followed, and directories are removed only with `remove_dir_all`
    ///   after confirming they are not symlinks.
    /// * Each pass inspects at most [`MAX_CLEANUP_SCAN_ENTRIES`] entries.
    pub fn cleanup_stale_temp_files(&self, max_age: std::time::Duration) -> anyhow::Result<usize> {
        if temp_operations_active() {
            tracing::debug!("skipping temp cleanup while a staged operation is active");
            return Ok(0);
        }
        let mut removed = 0usize;
        let entries = match fs::read_dir(&self.tmp_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to read temp directory {}", self.tmp_dir.display())
                });
            }
        };
        for (scanned, entry) in entries.enumerate() {
            if scanned >= MAX_CLEANUP_SCAN_ENTRIES {
                tracing::warn!(
                    limit = MAX_CLEANUP_SCAN_ENTRIES,
                    "temp cleanup stopped early; too many entries in the runtime temp directory"
                );
                break;
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tor_data_dir_is_resolved_under_runtime_data() {
        let paths = RuntimePaths::from_data_dir(PathBuf::from("/tmp/rustpost-data"))
            .with_tor_data_dir("privacy/tor");
        assert_eq!(
            paths.tor_dir,
            PathBuf::from("/tmp/rustpost-data/privacy/tor")
        );
        assert_eq!(
            paths.tor_onion_service_dir,
            PathBuf::from("/tmp/rustpost-data/privacy/tor/onion-service")
        );
    }

    #[test]
    fn runtime_paths_use_dedicated_database_and_temp_upload_dirs() {
        let paths = RuntimePaths::from_data_dir(PathBuf::from("/tmp/rustpost-data"));

        assert_eq!(
            paths.database_path,
            PathBuf::from("/tmp/rustpost-data/db/rustpost.sqlite3")
        );
        assert_eq!(
            paths.database_sidecar_path("wal"),
            PathBuf::from("/tmp/rustpost-data/db/rustpost.sqlite3-wal")
        );
        assert_eq!(
            paths.staged_upload_path("abc"),
            PathBuf::from("/tmp/rustpost-data/tmp/uploads/abc.upload")
        );
        assert_eq!(paths.assets_dir, PathBuf::from("/tmp/rustpost-data/assets"));
    }

    #[test]
    fn ensure_creates_runtime_directories() {
        let temp = tempfile::tempdir().expect("temp dir");
        let paths = RuntimePaths::from_data_dir(temp.path().join("data"));

        paths.ensure().expect("ensure paths");

        for path in [
            &paths.db_dir,
            &paths.uploads_originals,
            &paths.uploads_images,
            &paths.uploads_videos,
            &paths.uploads_thumbs,
            &paths.tmp_uploads,
            &paths.backups_dir,
            &paths.logs_dir,
        ] {
            assert!(path.is_dir(), "{} should exist", path.display());
        }
    }

    #[cfg(unix)]
    #[test]
    fn ensure_restricts_sensitive_runtime_directories() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().expect("temp dir");
        let paths = RuntimePaths::from_data_dir(temp.path().join("data"));

        paths.ensure().expect("ensure paths");

        for path in [
            &paths.data_dir,
            &paths.backups_dir,
            &paths.tor_dir,
            &paths.tor_onion_service_dir,
        ] {
            let mode = fs::metadata(path).expect("metadata").permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{} should be private", path.display());
        }
    }

    #[test]
    fn ensure_migrates_legacy_database_when_new_database_is_absent() {
        let temp = tempfile::tempdir().expect("temp dir");
        let data_dir = temp.path().join("data");
        fs::create_dir_all(&data_dir).expect("data dir");
        fs::write(data_dir.join("app.sqlite3"), b"db").expect("legacy db");
        fs::write(data_dir.join("app.sqlite3-wal"), b"wal").expect("legacy wal");
        fs::write(data_dir.join("app.sqlite3-shm"), b"shm").expect("legacy shm");
        let paths = RuntimePaths::from_data_dir(data_dir.clone());

        paths.ensure().expect("ensure paths");

        assert!(!data_dir.join("app.sqlite3").exists());
        assert_eq!(fs::read(&paths.database_path).expect("new db"), b"db");
        assert_eq!(
            fs::read(paths.database_sidecar_path("wal")).expect("new wal"),
            b"wal"
        );
        assert_eq!(
            fs::read(paths.database_sidecar_path("shm")).expect("new shm"),
            b"shm"
        );
    }

    #[test]
    fn ensure_rejects_legacy_and_new_database_conflict_without_overwrite() {
        let temp = tempfile::tempdir().expect("temp dir");
        let data_dir = temp.path().join("data");
        fs::create_dir_all(data_dir.join("db")).expect("db dir");
        fs::write(data_dir.join("app.sqlite3"), b"old").expect("legacy db");
        fs::write(data_dir.join("db/rustpost.sqlite3"), b"new").expect("new db");
        let paths = RuntimePaths::from_data_dir(data_dir.clone());

        let error = paths.ensure().expect_err("conflict");

        assert!(error.to_string().contains("database layout conflict"));
        assert_eq!(fs::read(data_dir.join("app.sqlite3")).expect("old"), b"old");
        assert_eq!(
            fs::read(data_dir.join("db/rustpost.sqlite3")).expect("new"),
            b"new"
        );
    }

    #[test]
    fn ensure_rejects_sidecar_conflict_before_moving_database() {
        let temp = tempfile::tempdir().expect("temp dir");
        let data_dir = temp.path().join("data");
        fs::create_dir_all(data_dir.join("db")).expect("db dir");
        fs::write(data_dir.join("app.sqlite3"), b"old").expect("legacy db");
        fs::write(data_dir.join("app.sqlite3-wal"), b"old wal").expect("legacy wal");
        fs::write(data_dir.join("db/rustpost.sqlite3-wal"), b"new wal").expect("new wal");
        let paths = RuntimePaths::from_data_dir(data_dir.clone());

        let error = paths.ensure().expect_err("conflict");

        assert!(error.to_string().contains("database layout conflict"));
        assert_eq!(fs::read(data_dir.join("app.sqlite3")).expect("old"), b"old");
        assert!(!data_dir.join("db/rustpost.sqlite3").exists());
        assert_eq!(
            fs::read(data_dir.join("db/rustpost.sqlite3-wal")).expect("new wal"),
            b"new wal"
        );
    }

    fn staging_paths() -> (tempfile::TempDir, RuntimePaths) {
        let temp = tempfile::tempdir().expect("temp dir");
        let paths = RuntimePaths::from_data_dir(temp.path().join("data"));
        paths.ensure().expect("ensure paths");
        (temp, paths)
    }

    fn age_file(path: &Path, age: std::time::Duration) {
        let modified = std::time::SystemTime::now()
            .checked_sub(age)
            .expect("system time supports subtraction");
        let file = fs::File::options()
            .write(true)
            .open(path)
            .expect("open for mtime");
        file.set_modified(modified).expect("set modified");
    }

    #[test]
    fn cleanup_removes_stale_known_staging_entries_only() {
        let (_temp, paths) = staging_paths();
        let stale = std::time::Duration::from_hours(2);
        let staging_file = paths.tmp_dir.join("account-import-abc.tar.gz");
        let export_file = paths.tmp_dir.join("account-export-def.tar.gz");
        let restore_file = paths.tmp_dir.join("restore-upload-1.tar");
        let upload_file = paths.tmp_dir.join("staged.upload");
        let tmp_file = paths.tmp_dir.join("scratch.tmp");
        let unrelated = paths.tmp_dir.join("keep.txt");
        for file in [
            &staging_file,
            &export_file,
            &restore_file,
            &upload_file,
            &tmp_file,
            &unrelated,
        ] {
            fs::write(file, b"staged").expect("write staging file");
            age_file(file, stale);
        }

        let removed = paths
            .cleanup_stale_temp_files(std::time::Duration::from_hours(1))
            .expect("cleanup");

        assert_eq!(removed, 5);
        assert!(!staging_file.exists());
        assert!(!export_file.exists());
        assert!(!restore_file.exists());
        assert!(!upload_file.exists());
        assert!(!tmp_file.exists());
        assert!(unrelated.exists(), "unrelated files must be preserved");
    }

    #[test]
    fn cleanup_removes_abandoned_import_staging_directories() {
        let (_temp, paths) = staging_paths();
        let staging_dir = paths.tmp_dir.join("account-import-extract");
        fs::create_dir_all(&staging_dir).expect("staging dir");
        fs::write(staging_dir.join("media.bin"), b"staged").expect("staged media");
        let unrelated_dir = paths.tmp_dir.join("keep-dir");
        fs::create_dir_all(&unrelated_dir).expect("unrelated dir");

        let removed = paths
            .cleanup_stale_temp_files(std::time::Duration::ZERO)
            .expect("startup cleanup");

        assert_eq!(removed, 1);
        assert!(!staging_dir.exists());
        assert!(unrelated_dir.is_dir(), "unrelated directories must remain");
    }

    #[test]
    fn cleanup_respects_the_age_threshold() {
        let (_temp, paths) = staging_paths();
        let fresh = paths.tmp_dir.join("account-import-fresh.tar.gz");
        fs::write(&fresh, b"fresh").expect("staging file");

        let removed = paths
            .cleanup_stale_temp_files(std::time::Duration::from_hours(1))
            .expect("cleanup");
        assert_eq!(removed, 0);
        assert!(fresh.exists(), "a fresh upload must not be removed");

        let removed = paths
            .cleanup_stale_temp_files(std::time::Duration::ZERO)
            .expect("startup cleanup");
        assert_eq!(removed, 1);
        assert!(!fresh.exists());
    }

    #[test]
    fn cleanup_skips_while_a_staged_operation_is_active() {
        let (_temp, paths) = staging_paths();
        let stale = paths.tmp_dir.join("account-import-active.tar.gz");
        fs::write(&stale, b"active").expect("staging file");
        age_file(&stale, std::time::Duration::from_hours(4));

        let operation = begin_temp_operation();
        assert!(temp_operations_active());
        let removed = paths
            .cleanup_stale_temp_files(std::time::Duration::ZERO)
            .expect("cleanup");
        assert_eq!(removed, 0);
        assert!(
            stale.exists(),
            "an active operation's staging file must survive cleanup"
        );

        drop(operation);
        assert!(!temp_operations_active());
        let removed = paths
            .cleanup_stale_temp_files(std::time::Duration::from_hours(1))
            .expect("cleanup");
        assert_eq!(removed, 1);
        assert!(!stale.exists());
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_never_follows_staging_symlinks() {
        let (_temp, paths) = staging_paths();
        let target_dir = paths.data_dir.join("outside");
        fs::create_dir_all(&target_dir).expect("target dir");
        let target_file = target_dir.join("important.bin");
        fs::write(&target_file, b"important").expect("target file");
        let link = paths.tmp_dir.join("account-import-link");
        std::os::unix::fs::symlink(&target_dir, &link).expect("symlink");

        let removed = paths
            .cleanup_stale_temp_files(std::time::Duration::ZERO)
            .expect("cleanup");

        assert_eq!(removed, 1);
        assert!(!link.exists());
        assert!(target_dir.is_dir(), "symlink target directory must remain");
        assert_eq!(
            fs::read(&target_file).expect("target file survives"),
            b"important"
        );
    }
}
