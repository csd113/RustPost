use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use tracing::{info, warn};

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
}
