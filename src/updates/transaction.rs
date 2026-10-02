//! Durable update journal. Once activation may have begun, recovery ALWAYS
//! stops the application and restores both the immutable database snapshot and
//! configuration before starting the old executable. No reverse migrations.
use super::release::Discovery;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    #[default]
    Idle,
    Downloading,
    Verifying,
    Stopping,
    BackingUp,
    Staged,
    Activating,
    Restarting,
    HealthChecking,
    RollingBack,
    RestartingPrevious,
    ResumingPrevious,
    Succeeded,
    RolledBack,
    Failed,
    FailedManualIntervention,
}
impl Phase {
    pub(crate) const fn active(self) -> bool {
        matches!(
            self,
            Self::Downloading
                | Self::Verifying
                | Self::Stopping
                | Self::BackingUp
                | Self::Staged
                | Self::Activating
                | Self::Restarting
                | Self::HealthChecking
                | Self::RollingBack
                | Self::RestartingPrevious
                | Self::ResumingPrevious
        )
    }
    #[cfg(unix)]
    pub(super) const fn needs_restore(self) -> bool {
        matches!(
            self,
            Self::Activating
                | Self::Restarting
                | Self::HealthChecking
                | Self::RollingBack
                | Self::RestartingPrevious
        )
    }
    #[cfg(unix)]
    pub(super) const fn may_start(self) -> bool {
        matches!(
            self,
            Self::Idle
                | Self::Downloading
                | Self::Verifying
                | Self::Succeeded
                | Self::RolledBack
                | Self::Failed
                | Self::Restarting
                | Self::HealthChecking
                | Self::RestartingPrevious
                | Self::ResumingPrevious
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupInfo {
    pub id: String,
    pub created_at: String,
    pub previous_version: String,
    pub target_version: String,
    pub previous_schema: i64,
    pub target_schema: i64,
    pub size: u64,
    pub database_sha256: String,
    pub configuration_sha256: String,
    pub verified: bool,
    pub reason: String,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Status {
    pub phase: Phase,
    pub installed: String,
    pub discovery: Option<Discovery>,
    pub approval: Option<String>,
    pub job: Option<String>,
    pub administrator: Option<i64>,
    pub previous_version: Option<String>,
    pub target_version: Option<String>,
    pub message: String,
    pub backup: Option<BackupInfo>,
    pub backups: Vec<BackupInfo>,
    pub updated_at: String,
}

#[cfg(unix)]
pub(super) mod native {
    use super::{BackupInfo, Discovery, Phase, Status};
    use crate::updates::release::{self, Manifest};
    use anyhow::Context as _;
    use rusqlite::{Connection, OpenFlags};
    use serde::Deserialize;
    use std::fs::{self, File};
    use std::io::{Read as _, Write as _};
    use std::path::{Path, PathBuf};
    use std::time::Duration;
    use uuid::Uuid;

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct Config {
        pub install_dir: PathBuf,
        pub state_dir: PathBuf,
        pub data_dir: PathBuf,
        pub settings_path: PathBuf,
        pub health_port: u16,
        pub web_uid: u32,
        pub public_key: PathBuf,
        pub retention: usize,
    }

    pub struct Engine {
        pub config: Config,
        pub key: Vec<u8>,
    }

    trait ArtifactSource {
        fn fetch(&self, manifest: &Manifest) -> anyhow::Result<Vec<u8>>;
    }
    struct OfficialSource;
    impl ArtifactSource for OfficialSource {
        fn fetch(&self, manifest: &Manifest) -> anyhow::Result<Vec<u8>> {
            release::fetch(
                &release::asset_url(&manifest.version, &manifest.filename)?,
                manifest.size,
            )
        }
    }

    pub trait Service {
        fn preflight(&self) -> anyhow::Result<()> {
            Ok(())
        }
        fn stop(&self) -> anyhow::Result<()>;
        fn start(&self) -> anyhow::Result<()>;
        fn health(&self, version: &str, schema: i64) -> anyhow::Result<()>;
    }

    pub(super) fn atomic_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
        let parent = path.parent().context("missing managed parent directory")?;
        let mut staged = tempfile::NamedTempFile::new_in(parent)?;
        staged.write_all(bytes)?;
        staged.as_file().sync_all()?;
        staged.persist(path).map_err(|error| error.error)?;
        sync_dir(parent)
    }
    fn sync_dir(path: &Path) -> anyhow::Result<()> {
        File::open(path)?.sync_all()?;
        Ok(())
    }

    fn plain(path: &Path, directory: bool) -> anyhow::Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        anyhow::ensure!(
            !metadata.file_type().is_symlink()
                && if directory {
                    metadata.is_dir()
                } else {
                    metadata.is_file()
                },
            "managed path has an unexpected type"
        );
        Ok(())
    }
    fn no_symlink_ancestors(path: &Path) -> anyhow::Result<()> {
        anyhow::ensure!(
            path.is_absolute()
                && !path
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir)),
            "managed paths must be absolute without traversal"
        );
        for ancestor in path.ancestors() {
            plain(ancestor, ancestor != path || path.is_dir())?;
        }
        Ok(())
    }

    /// Directory ownership protects names, not merely file contents. Root-owned
    /// sticky shared temp parents are safe because another UID cannot rename
    /// their protected child; every other writable parent is refused.
    pub(in crate::updates) fn protected_ancestors(path: &Path, web_uid: u32) -> anyhow::Result<()> {
        use std::os::unix::fs::MetadataExt as _;
        no_symlink_ancestors(path)?;
        let owner = fs::symlink_metadata(path)?.uid();
        for ancestor in path.ancestors() {
            let metadata = fs::symlink_metadata(ancestor)?;
            let sticky_root =
                metadata.is_dir() && metadata.uid() == 0 && metadata.mode() & 0o1000 != 0;
            anyhow::ensure!(
                metadata.uid() != web_uid
                    && (metadata.uid() == 0 || metadata.uid() == owner)
                    && (metadata.mode() & 0o022 == 0 || sticky_root),
                "protected path has an untrusted or writable ancestor"
            );
        }
        Ok(())
    }

    impl Engine {
        pub fn validate(&self) -> anyhow::Result<()> {
            for path in [
                &self.config.install_dir,
                &self.config.state_dir,
                &self.config.data_dir,
                &self.config.settings_path,
                &self.config.public_key,
            ] {
                no_symlink_ancestors(path)?;
            }
            anyhow::ensure!(
                self.config.retention >= 2
                    && self.config.retention <= 20
                    && self.key.len() == 32
                    && self.config.health_port != 0,
                "invalid updater configuration"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt as _;
                for path in [&self.config.install_dir, &self.config.state_dir] {
                    protected_ancestors(path, self.config.web_uid)?;
                    let metadata = fs::metadata(path)?;
                    anyhow::ensure!(
                        metadata.uid() != self.config.web_uid && metadata.mode() & 0o022 == 0,
                        "web account must not own or write updater installation/state"
                    );
                }
            }
            plain(&self.config.install_dir.join("versions"), true)?;
            protected_ancestors(&self.config.state_dir.join("backups"), self.config.web_uid)?;
            anyhow::ensure!(
                self.config.web_uid != 0,
                "the RustPost web account must not be root"
            );
            self.current_version()?;
            Ok(())
        }
        pub fn current_version(&self) -> anyhow::Result<String> {
            let pointer = fs::read_link(self.config.install_dir.join("current"))?;
            let version = pointer
                .strip_prefix("versions")?
                .to_str()
                .context("invalid active version")?;
            let parsed = release::stable_version(version)?;
            anyhow::ensure!(
                pointer == Path::new("versions").join(parsed.to_string()),
                "active installation pointer is outside versions"
            );
            plain(&self.config.install_dir.join(&pointer), true)?;
            plain(
                &self.config.install_dir.join(pointer).join("rustpost-cli"),
                false,
            )?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt as _;
                for path in [
                    self.config.install_dir.join("versions"),
                    self.config
                        .install_dir
                        .join("versions")
                        .join(parsed.to_string()),
                    self.config
                        .install_dir
                        .join("versions")
                        .join(parsed.to_string())
                        .join("rustpost-cli"),
                ] {
                    let metadata = fs::symlink_metadata(path)?;
                    anyhow::ensure!(
                        metadata.uid() != self.config.web_uid && metadata.mode() & 0o022 == 0,
                        "web account must not own or write versioned program files"
                    );
                }
            }
            Ok(parsed.to_string())
        }
        pub fn status(&self) -> anyhow::Result<Status> {
            let path = self.config.state_dir.join("status.json");
            if !path.exists() {
                return Ok(Status {
                    installed: self.current_version()?,
                    ..Status::default()
                });
            }
            plain(&path, false)?;
            let bytes = read_limited(&path, 512 * 1024)?;
            let status: Status = serde_json::from_slice(&bytes)?;
            for value in [&status.job, &status.approval].into_iter().flatten() {
                Uuid::parse_str(value)?;
            }
            for value in [&status.previous_version, &status.target_version]
                .into_iter()
                .flatten()
            {
                release::stable_version(value)?;
            }
            Ok(status)
        }
        pub fn lock(&self) -> anyhow::Result<File> {
            let file = File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(self.config.state_dir.join("update.lock"))?;
            file.try_lock()
                .context("another update operation is running")?;
            Ok(file) // OS releases the lock even on crash; no stale PID guessing.
        }
        pub fn save(&self, status: &mut Status, phase: Phase, message: &str) -> anyhow::Result<()> {
            status.phase = phase;
            status.message = message.into();
            status.updated_at = chrono::Utc::now().to_rfc3339();
            tracing::info!(?phase, job = ?status.job, administrator = ?status.administrator, source = ?status.previous_version, target = ?status.target_version, "update transaction");
            atomic_write(
                &self.config.state_dir.join("status.json"),
                &serde_json::to_vec(status)?,
            )
        }
        pub fn check(&self) -> anyhow::Result<Status> {
            let _lock = self.lock()?;
            let mut status = self.status()?;
            anyhow::ensure!(
                !status.phase.active() && status.phase != Phase::FailedManualIntervention,
                "resolve the current update before checking again"
            );
            status.installed = self.current_version()?;
            status.discovery = Some(release::discover_for_schema(
                &status.installed,
                Some(&self.key),
                Some(verify_database(
                    &self.config.data_dir.join("db/rustpost.sqlite3"),
                )?),
            ));
            status.approval = match &status.discovery {
                Some(Discovery::Available(release)) if release.compatible => {
                    Some(Uuid::new_v4().to_string())
                }
                _ => None,
            };
            if status.job.is_some() {
                atomic_write(
                    &self.config.state_dir.join("status.json"),
                    &serde_json::to_vec(&status)?,
                )?;
            } else {
                let phase = status.phase;
                self.save(&mut status, phase, "Release check completed.")?;
            }
            Ok(status)
        }
        pub fn approve(
            &self,
            approval: &str,
            administrator: i64,
        ) -> anyhow::Result<(Status, Manifest)> {
            let mut status = self.status()?;
            anyhow::ensure!(
                !status.phase.active()
                    && status.phase != Phase::FailedManualIntervention
                    && status.approval.as_deref() == Some(approval)
                    && administrator > 0,
                "update approval is stale, consumed, or conflicting"
            );
            Uuid::parse_str(approval)?;
            let Some(Discovery::Available(release)) = &status.discovery else {
                anyhow::bail!("no approved release");
            };
            let manifest = release.manifest.clone().context("release is unverified")?;
            let current = self.current_version()?;
            anyhow::ensure!(
                release::stable_version(&manifest.version)? > release::stable_version(&current)?,
                "downgrades and reinstallation are disabled"
            );
            anyhow::ensure!(
                Some(manifest.target.as_str()) == release::platform_target()
                    && cfg!(target_os = "linux"),
                "incompatible installation target"
            );
            status.previous_version = Some(current);
            status.target_version = Some(manifest.version.clone());
            status.administrator = Some(administrator);
            status.job = Some(Uuid::new_v4().to_string());
            status.approval = None;
            status.backup = None;
            self.save(
                &mut status,
                Phase::Downloading,
                "Downloading the approved release.",
            )?;
            Ok((status, manifest))
        }
        pub fn install(
            &self,
            status: &mut Status,
            manifest: &Manifest,
            service: &impl Service,
        ) -> anyhow::Result<()> {
            self.install_with_source(status, manifest, service, &OfficialSource)
        }
        fn install_with_source(
            &self,
            status: &mut Status,
            manifest: &Manifest,
            service: &impl Service,
            source: &impl ArtifactSource,
        ) -> anyhow::Result<()> {
            let coordination = crate::backup::update_coordination_lock(
                &crate::runtime::RuntimePaths::from_data_dir(self.config.data_dir.clone()),
            );
            let outcome = match &coordination {
                Ok(_) => self.install_inner(status, manifest, service, source),
                Err(error) => Err(anyhow::anyhow!("backup coordination failed: {error}")),
            };
            if let Err(error) = outcome {
                tracing::error!(error = %error, "update failed");
                if status.phase.needs_restore() || status.phase == Phase::Succeeded {
                    self.rollback(
                        status,
                        service,
                        "Upgrade failed; restored previous software and database.",
                    )?;
                    self.retain_terminal(status);
                    return Ok(());
                }
                if matches!(
                    status.phase,
                    Phase::Stopping | Phase::BackingUp | Phase::Staged | Phase::ResumingPrevious
                ) {
                    self.save(
                        status,
                        Phase::ResumingPrevious,
                        "Update preparation failed; restarting previous software.",
                    )?;
                    if let Err(restart_error) = service.start().and_then(|()| {
                        service.health(
                            status
                                .previous_version
                                .as_deref()
                                .context("missing previous version")?,
                            status
                                .backup
                                .as_ref()
                                .map_or(crate::db::CURRENT_SCHEMA_VERSION, |b| b.previous_schema),
                        )
                    }) {
                        tracing::error!(error = %restart_error, "previous version restart failed");
                        return self.save(status, Phase::FailedManualIntervention, "Preparation failed and the previous service could not restart. Operator intervention required.");
                    }
                }
                self.save(status, Phase::Failed, public_failure(&error))?;
                self.retain_terminal(status);
            }
            Ok(())
        }
        fn install_inner(
            &self,
            status: &mut Status,
            manifest: &Manifest,
            service: &impl Service,
            source: &impl ArtifactSource,
        ) -> anyhow::Result<()> {
            self.preflight(manifest)?;
            service.preflight()?;
            service.health(
                status
                    .previous_version
                    .as_deref()
                    .context("missing previous version")?,
                verify_database(&self.config.data_dir.join("db/rustpost.sqlite3"))?,
            )?;
            let archive = source.fetch(manifest)?;
            self.save(
                status,
                Phase::Verifying,
                "Verifying release integrity and archive structure.",
            )?;
            let stage = tempfile::Builder::new()
                .prefix(".update-stage-")
                .tempdir_in(self.config.install_dir.join("versions"))?;
            stage_archive(&archive, manifest, stage.path())?;
            self.preflight(manifest)?;
            // Verify a live snapshot before any outage. Refresh after shutdown so
            // rollback includes every write acknowledged by the old application.
            self.snapshot(status, manifest)?;
            self.save(
                status,
                Phase::Stopping,
                "Stopping RustPost for the final consistent backup.",
            )?;
            service.stop()?;
            self.save(
                status,
                Phase::BackingUp,
                "Verifying the final database and configuration backup.",
            )?;
            self.snapshot(status, manifest)?;
            let version_dir = self
                .config
                .install_dir
                .join("versions")
                .join(&manifest.version);
            if version_dir.exists() {
                // A previous failed attempt may already have fully staged the
                // identical signed program. Reuse only its exact layout/hash,
                // allowing a normal administrator to retry without host access.
                verify_staged_install(&version_dir, manifest, self.config.web_uid)?;
            } else {
                let stage_path = stage.keep();
                fs::rename(&stage_path, &version_dir)?;
                sync_dir(&self.config.install_dir.join("versions"))?;
            }
            self.save(
                status,
                Phase::Staged,
                "New version staged; verified backup retained.",
            )?;
            // Journal intent BEFORE switching: a crash on either side restores
            // the old DB/config and pointer, idempotently, on daemon restart.
            self.save(status, Phase::Activating, "Activating the new release.")?;
            self.activate(&manifest.version)?;
            self.save(
                status,
                Phase::Restarting,
                "Starting the new release and its transactional migrations.",
            )?;
            service.start()?;
            self.save(
                status,
                Phase::HealthChecking,
                "Checking the new version, database schema and persistent directories.",
            )?;
            service.health(&manifest.version, manifest.schema)?;
            status.installed.clone_from(&manifest.version);
            self.save(
                status,
                Phase::Succeeded,
                "RustPost updated successfully after health verification.",
            )?;
            self.retain_terminal(status);
            Ok(())
        }
        fn preflight(&self, manifest: &Manifest) -> anyhow::Result<()> {
            self.validate()?;
            let existing = self
                .config
                .install_dir
                .join("versions")
                .join(&manifest.version);
            if existing.exists() {
                verify_staged_install(&existing, manifest, self.config.web_uid)?;
            }

            for path in [
                &self.config.data_dir.join("db"),
                &self.config.data_dir.join("db/rustpost.sqlite3"),
            ] {
                no_symlink_ancestors(path)?;
            }
            let schema = verify_database(&self.config.data_dir.join("db/rustpost.sqlite3"))?;
            anyhow::ensure!(
                schema >= manifest.minimum_schema && schema <= manifest.schema,
                "database schema is not compatible with this release"
            );
            let database_bytes =
                fs::metadata(self.config.data_dir.join("db/rustpost.sqlite3"))?.len();
            let config_bytes = fs::metadata(&self.config.settings_path)?.len();
            let wal_path = self.config.data_dir.join("db/rustpost.sqlite3-wal");
            let wal_bytes = if wal_path.exists() {
                plain(&wal_path, false)?;
                fs::metadata(&wal_path)?.len()
            } else {
                0
            };
            let required_space = manifest
                .size
                .saturating_add(manifest.executable_size)
                .saturating_mul(2)
                .saturating_add(database_bytes.saturating_add(wal_bytes).saturating_mul(6))
                .saturating_add(config_bytes.saturating_mul(4));
            for path in [
                &self.config.install_dir,
                &self.config.state_dir,
                &self.config.data_dir,
            ] {
                let disk = rustix::fs::statvfs(path)?;
                anyhow::ensure!(
                    disk.f_bavail.saturating_mul(disk.f_frsize)
                        > required_space.saturating_add(64 * 1024 * 1024),
                    "insufficient free disk space for a safe update"
                );
                tempfile::NamedTempFile::new_in(path)?;
            }
            anyhow::ensure!(
                !self
                    .config
                    .data_dir
                    .join("tmp/backup-restore.lock")
                    .exists(),
                "a backup or restore is in progress"
            );
            Ok(())
        }
        fn snapshot(&self, status: &mut Status, manifest: &Manifest) -> anyhow::Result<()> {
            let id = status.job.as_deref().context("missing update job")?;
            let backups = self.config.state_dir.join("backups");
            let stage = tempfile::Builder::new()
                .prefix(".update-backup-")
                .tempdir_in(&backups)?;
            let conn = Connection::open_with_flags(
                self.config.data_dir.join("db/rustpost.sqlite3"),
                OpenFlags::SQLITE_OPEN_READ_ONLY,
            )?;
            conn.busy_timeout(Duration::from_secs(10))?;
            let db_path = stage.path().join("database.sqlite3");
            conn.execute(
                "VACUUM INTO ?",
                [db_path.to_str().context("invalid backup path")?],
            )?;
            drop(conn);
            let schema = verify_database(&db_path)?;
            let settings = read_limited(&self.config.settings_path, 4 * 1024 * 1024)?;
            let text = std::str::from_utf8(&settings)
                .map_err(|_| anyhow::anyhow!("configuration backup is not valid UTF-8"))?;
            let configuration: crate::config::Settings = toml::from_str(text)
                .map_err(|_| anyhow::anyhow!("configuration backup is not valid TOML"))?;
            configuration.validate()?;
            atomic_write(&stage.path().join("settings.toml"), &settings)?;
            File::open(&db_path)?.sync_all()?;
            let info = BackupInfo {
                id: id.into(),
                created_at: chrono::Utc::now().to_rfc3339(),
                previous_version: status
                    .previous_version
                    .clone()
                    .context("missing previous version")?,
                target_version: manifest.version.clone(),
                previous_schema: schema,
                target_schema: manifest.schema,
                size: fs::metadata(&db_path)?.len() + u64::try_from(settings.len())?,
                database_sha256: hash_file(&db_path)?,
                configuration_sha256: release::digest(&settings),
                verified: true,
                reason: "pre_upgrade".into(),
            };
            atomic_write(
                &stage.path().join("metadata.json"),
                &serde_json::to_vec(&info)?,
            )?;
            sync_dir(stage.path())?;
            let destination = backups.join(id);
            // Only pre-activation refreshes this backup. Recovery never needs a
            // backup while in BackingUp; the old DB has not been migrated yet.
            if destination.exists() {
                plain(&destination, true)?;
                fs::remove_dir_all(&destination)?;
            }
            let stage_path = stage.keep();
            fs::rename(stage_path, destination)?;
            sync_dir(&backups)?;
            status.backup = Some(info.clone());
            status.backups.retain(|b| b.id != id);
            status.backups.insert(0, info);
            Ok(())
        }
        fn activate(&self, version: &str) -> anyhow::Result<()> {
            let parsed = release::stable_version(version)?;
            let target = Path::new("versions").join(parsed.to_string());
            plain(&self.config.install_dir.join(&target), true)?;
            plain(
                &self.config.install_dir.join(&target).join("rustpost-cli"),
                false,
            )?;
            #[cfg(unix)]
            {
                let tmp = self
                    .config
                    .install_dir
                    .join(format!(".current-{}", Uuid::new_v4()));
                std::os::unix::fs::symlink(target, &tmp)?;
                fs::rename(tmp, self.config.install_dir.join("current"))?;
                sync_dir(&self.config.install_dir)?;
                Ok(())
            }
            #[cfg(not(unix))]
            {
                anyhow::bail!("atomic activation requires Unix");
            }
        }
        fn restore(&self, status: &Status) -> anyhow::Result<i64> {
            let backup = status
                .backup
                .as_ref()
                .context("missing verified rollback backup")?;
            Uuid::parse_str(&backup.id)?;
            let directory = self.config.state_dir.join("backups").join(&backup.id);
            no_symlink_ancestors(&directory)?;
            let database = directory.join("database.sqlite3");
            let settings = directory.join("settings.toml");
            plain(&database, false)?;
            plain(&settings, false)?;
            anyhow::ensure!(
                backup.verified
                    && hash_file(&database)? == backup.database_sha256
                    && hash_file(&settings)? == backup.configuration_sha256
                    && verify_database(&database)? == backup.previous_schema,
                "rollback backup failed verification"
            );
            no_symlink_ancestors(&self.config.data_dir.join("db"))?;
            no_symlink_ancestors(&self.config.settings_path)?;
            for suffix in ["wal", "shm"] {
                let sidecar = self
                    .config
                    .data_dir
                    .join(format!("db/rustpost.sqlite3-{suffix}"));
                match fs::remove_file(sidecar) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
            }
            // Stream into a same-filesystem temporary file then rename/fsync.
            let live = self.config.data_dir.join("db/rustpost.sqlite3");
            let mut stage =
                tempfile::NamedTempFile::new_in(live.parent().context("missing db parent")?)?;
            std::io::copy(&mut File::open(database)?, &mut stage)?;
            stage.as_file().sync_all()?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                stage
                    .as_file()
                    .set_permissions(fs::Permissions::from_mode(0o660))?;
            }
            stage.persist(&live).map_err(|e| e.error)?;
            sync_dir(live.parent().context("missing db parent")?)?;
            atomic_write(
                &self.config.settings_path,
                &read_limited(&settings, 4 * 1024 * 1024)?,
            )?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                fs::set_permissions(
                    &self.config.settings_path,
                    fs::Permissions::from_mode(0o660),
                )?;
                File::open(&self.config.settings_path)?.sync_all()?;
            }
            Ok(backup.previous_schema)
        }
        fn rollback(
            &self,
            status: &mut Status,
            service: &impl Service,
            message: &str,
        ) -> anyhow::Result<()> {
            self.save(
                status,
                Phase::RollingBack,
                "Restoring the verified pre-upgrade database, configuration, and executable.",
            )?;
            let result = (|| {
                service.stop()?;
                let schema = self.restore(status)?;
                let previous = status
                    .previous_version
                    .clone()
                    .context("missing previous version")?;
                self.activate(&previous)?;
                self.save(
                    status,
                    Phase::RestartingPrevious,
                    "Health-checking the restored previous version.",
                )?;
                service.start()?;
                service.health(&previous, schema)?;
                status.installed = previous;
                Ok::<_, anyhow::Error>(())
            })();
            match result {
                Ok(()) => self.save(status, Phase::RolledBack, message),
                Err(error) => {
                    tracing::error!(error = %error, "automatic rollback failed");
                    self.save(status, Phase::FailedManualIntervention, "Upgrade and automatic rollback failed. RustPost is stopped or unhealthy; operator intervention required. The rollback backup is retained.")
                }
            }
        }
        pub fn recover(&self, service: &impl Service) -> anyhow::Result<()> {
            let _lock = self.lock()?;
            let mut status = self.status()?;
            self.reconcile_backups(&mut status)?;
            if !status.phase.active() {
                self.retain_terminal(&mut status);
                self.clean_staging()?;
                return Ok(());
            }
            let _coordination = crate::backup::update_coordination_lock(
                &crate::runtime::RuntimePaths::from_data_dir(self.config.data_dir.clone()),
            )?;
            if status.phase.needs_restore() {
                self.rollback(
                    &mut status,
                    service,
                    "Interrupted update recovered by restoring previous software and database.",
                )?;
                self.retain_terminal(&mut status);
                return self.clean_staging();
            }
            if matches!(
                status.phase,
                Phase::Stopping | Phase::BackingUp | Phase::Staged | Phase::ResumingPrevious
            ) {
                let previous = status
                    .previous_version
                    .clone()
                    .context("missing previous version")?;
                self.activate(&previous)?;
                self.save(
                    &mut status,
                    Phase::ResumingPrevious,
                    "Restarting previous version after interrupted preparation.",
                )?;
                if let Err(error) = service.start().and_then(|()| {
                    service.health(
                        &previous,
                        status
                            .backup
                            .as_ref()
                            .map_or(crate::db::CURRENT_SCHEMA_VERSION, |b| b.previous_schema),
                    )
                }) {
                    tracing::error!(error = %error, "preparation recovery failed");
                    return self.save(&mut status, Phase::FailedManualIntervention, "Interrupted preparation could not recover the previous service; operator intervention required.");
                }
            }
            self.save(
                &mut status,
                Phase::Failed,
                "Interrupted update stopped before activation; previous version retained.",
            )?;
            self.retain_terminal(&mut status);
            self.clean_staging()
        }
        fn reconcile_backups(&self, status: &mut Status) -> anyhow::Result<()> {
            let backups = self.config.state_dir.join("backups");
            for entry in fs::read_dir(&backups)?.take(1000) {
                let entry = entry?;
                let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                if Uuid::parse_str(&id).is_err() || status.backups.iter().any(|b| b.id == id) {
                    continue;
                }
                no_symlink_ancestors(&entry.path())?;
                let info: BackupInfo = serde_json::from_slice(&read_limited(
                    &entry.path().join("metadata.json"),
                    16 * 1024,
                )?)?;
                anyhow::ensure!(
                    info.id == id && info.reason == "pre_upgrade" && info.verified,
                    "invalid managed backup metadata"
                );
                release::stable_version(&info.previous_version)?;
                release::stable_version(&info.target_version)?;
                chrono::DateTime::parse_from_rfc3339(&info.created_at)?;
                anyhow::ensure!(
                    hash_file(&entry.path().join("database.sqlite3"))? == info.database_sha256
                        && hash_file(&entry.path().join("settings.toml"))?
                            == info.configuration_sha256,
                    "orphan backup failed verification"
                );
                status.backups.push(info);
            }
            status
                .backups
                .sort_by(|a, b| b.created_at.cmp(&a.created_at));
            Ok(())
        }
        fn retain_terminal(&self, status: &mut Status) {
            if !status.phase.active()
                && status.phase != Phase::FailedManualIntervention
                && let Err(error) = self.prune(status)
            {
                tracing::warn!(error = %error, "update retention failed; retained extra backups");
            }
        }
        fn clean_staging(&self) -> anyhow::Result<()> {
            for (parent, prefix) in [
                (self.config.install_dir.join("versions"), ".update-stage-"),
                (self.config.state_dir.join("backups"), ".update-backup-"),
            ] {
                for entry in fs::read_dir(&parent)?.take(1000) {
                    let entry = entry?;
                    if entry
                        .file_name()
                        .to_str()
                        .is_some_and(|name| name.starts_with(prefix))
                    {
                        no_symlink_ancestors(&entry.path())?;
                        fs::remove_dir_all(entry.path())?;
                    }
                }
                sync_dir(&parent)?;
            }
            Ok(())
        }

        fn prune(&self, status: &mut Status) -> anyhow::Result<()> {
            let protected = status.backup.as_ref().map(|backup| backup.id.as_str());
            let remove: Vec<_> = status
                .backups
                .iter()
                .skip(self.config.retention)
                .filter(|b| Some(b.id.as_str()) != protected)
                .map(|b| b.id.clone())
                .collect();
            for id in &remove {
                Uuid::parse_str(id)?;
                let path = self.config.state_dir.join("backups").join(id);
                no_symlink_ancestors(&path)?;
                fs::remove_dir_all(path)?;
            }
            status.backups.retain(|b| !remove.contains(&b.id));
            if status.phase == Phase::Succeeded {
                let current = self.current_version()?;
                for entry in fs::read_dir(self.config.install_dir.join("versions"))?.take(1000) {
                    let entry = entry?;
                    let name = entry.file_name();
                    let Some(name) = name.to_str() else {
                        continue;
                    };
                    if release::stable_version(name).is_ok()
                        && name != current
                        && Some(name) != status.previous_version.as_deref()
                    {
                        no_symlink_ancestors(&entry.path())?;
                        fs::remove_dir_all(entry.path())?;
                    }
                }
                sync_dir(&self.config.install_dir.join("versions"))?;
            }
            let phase = status.phase;
            let message = status.message.clone();
            self.save(status, phase, &message)
        }
    }

    fn public_failure(error: &anyhow::Error) -> &'static str {
        let detail = error.to_string();
        if detail.contains("space") {
            "Update blocked: insufficient disk space for staging and a verified backup."
        } else if detail.contains("backup")
            || detail.contains("database")
            || detail.contains("coordination")
        {
            "Update blocked: database backup/preflight failed or another backup/restore is running."
        } else if detail.contains("checksum")
            || detail.contains("archive")
            || detail.contains("architecture")
        {
            "Update blocked: release integrity, archive layout or target verification failed."
        } else if detail.contains("configuration") {
            "Update blocked: configuration could not be safely preserved."
        } else {
            "Update stopped before activation. The previous version is retained. Check updater logs for details."
        }
    }

    fn read_limited(path: &Path, limit: u64) -> anyhow::Result<Vec<u8>> {
        plain(path, false)?;
        let mut bytes = Vec::new();
        File::open(path)?.take(limit + 1).read_to_end(&mut bytes)?;
        anyhow::ensure!(
            u64::try_from(bytes.len())? <= limit,
            "managed file exceeds size limit"
        );
        Ok(bytes)
    }
    fn hash_file(path: &Path) -> anyhow::Result<String> {
        use sha2::{Digest as _, Sha256};
        plain(path, false)?;
        let mut reader = File::open(path)?;
        let mut hash = Sha256::new();
        let mut buffer = [0; 16 * 1024];
        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
        }
        Ok(release::hex(&hash.finalize()))
    }
    fn verify_database(path: &Path) -> anyhow::Result<i64> {
        plain(path, false)?;
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let integrity: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        anyhow::ensure!(integrity == "ok", "database backup integrity check failed");
        let mut foreign_keys = conn.prepare("PRAGMA foreign_key_check")?;
        anyhow::ensure!(
            foreign_keys.query([])?.next()?.is_none(),
            "database backup foreign key check failed"
        );
        crate::db::schema_version_from_connection(&conn)
    }

    fn verify_staged_install(
        directory: &Path,
        manifest: &Manifest,
        web_uid: u32,
    ) -> anyhow::Result<()> {
        no_symlink_ancestors(directory)?;
        let entries = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
        anyhow::ensure!(
            entries.len() == 1 && entries[0].file_name() == "rustpost-cli",
            "existing staged release has unexpected layout"
        );
        let binary = directory.join("rustpost-cli");
        plain(&binary, false)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            for path in [directory, binary.as_path()] {
                let metadata = fs::metadata(path)?;
                anyhow::ensure!(
                    metadata.uid() != web_uid && metadata.mode() & 0o022 == 0,
                    "staged release permissions are unsafe"
                );
            }
        }
        anyhow::ensure!(
            fs::metadata(&binary)?.len() == manifest.executable_size
                && hash_file(&binary)? == manifest.executable_sha256,
            "existing staged release checksum mismatch"
        );
        verify_executable(&binary, &manifest.target)
    }

    fn verify_executable(path: &Path, target: &str) -> anyhow::Result<()> {
        let mut header = [0u8; 64];
        File::open(path)?.read_exact(&mut header)?;
        let machine = match target {
            "x86_64-unknown-linux-gnu" => 62,
            "aarch64-unknown-linux-gnu" => 183,
            _ => anyhow::bail!("unsupported executable target"),
        };
        anyhow::ensure!(
            &header[..7] == b"\x7fELF\x02\x01\x01"
                && matches!(u16::from_le_bytes([header[16], header[17]]), 2 | 3)
                && u16::from_le_bytes([header[18], header[19]]) == machine,
            "release executable architecture or format mismatch"
        );
        Ok(())
    }

    struct ExpandedReader<R> {
        inner: R,
        remaining: u64,
    }
    impl<R: std::io::Read> std::io::Read for ExpandedReader<R> {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if buffer.is_empty() {
                return Ok(0);
            }
            if self.remaining == 0 {
                let mut extra = [0u8; 1];
                if self.inner.read(&mut extra)? != 0 {
                    return Err(std::io::Error::other(
                        "release archive exceeds expansion limit",
                    ));
                }
                return Ok(0);
            }
            let count = usize::try_from(self.remaining)
                .unwrap_or(usize::MAX)
                .min(buffer.len());
            let read = self.inner.read(&mut buffer[..count])?;
            self.remaining -= u64::try_from(read).map_err(std::io::Error::other)?;
            Ok(read)
        }
    }

    fn stage_archive(bytes: &[u8], manifest: &Manifest, destination: &Path) -> anyhow::Result<()> {
        anyhow::ensure!(
            u64::try_from(bytes.len())? == manifest.size
                && release::digest(bytes) == manifest.sha256,
            "artifact checksum or size mismatch"
        );
        let decoder = flate2::read::MultiGzDecoder::new(bytes);
        let mut archive = tar::Archive::new(ExpandedReader {
            inner: decoder,
            remaining: manifest.executable_size.saturating_add(64 * 1024),
        });
        let mut count = 0;
        for entry in archive.entries()?.raw(true) {
            let mut entry = entry?;
            anyhow::ensure!(
                entry.header().entry_type().is_file()
                    && entry.path_bytes().as_ref() == b"rustpost-cli"
                    && count == 0
                    && entry.size() == manifest.executable_size,
                "unexpected release archive layout or traversal attempt"
            );
            let mut executable = File::options()
                .write(true)
                .create_new(true)
                .open(destination.join("rustpost-cli"))?;
            std::io::copy(&mut entry, &mut executable)?;
            executable.sync_all()?;
            count += 1;
        }
        let mut remainder = archive.into_inner();
        let mut padding = [0u8; 16 * 1024];
        loop {
            let count = remainder.read(&mut padding)?;
            if count == 0 {
                break;
            }
            anyhow::ensure!(
                padding[..count].iter().all(|byte| *byte == 0),
                "unexpected trailing release archive contents"
            );
        }
        anyhow::ensure!(
            count == 1
                && hash_file(&destination.join("rustpost-cli"))? == manifest.executable_sha256,
            "release executable verification failed"
        );
        verify_executable(&destination.join("rustpost-cli"), &manifest.target)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(
                destination.join("rustpost-cli"),
                fs::Permissions::from_mode(0o755),
            )?;
            fs::set_permissions(destination, fs::Permissions::from_mode(0o755))?;
        }
        sync_dir(destination)
    }

    #[cfg(test)]
    mod tests {
        include!("transaction_tests.rs");
    }
}
