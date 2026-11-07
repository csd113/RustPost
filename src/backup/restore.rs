//! Restore for the backup subsystem.

use super::*;
use std::fmt::Write as _;

pub fn restore_backup(
    paths: &RuntimePaths,
    archive_path: &Path,
    include_tor_keys: bool,
) -> anyhow::Result<RestoreReport> {
    let _temp_operation = crate::runtime::begin_temp_operation();
    let _lock = OperationLock::acquire(paths)?;
    let _offline = crate::runtime::acquire_server_lease(paths).context(
        "stop RustPost before restoring a backup; live restore cannot safely replace SQLite",
    )?;
    let staging = tempfile::Builder::new()
        .prefix("rustpost-restore-")
        .tempdir_in(&paths.data_dir)
        .with_context(|| "failed to create restore staging directory")?;
    let staged = extract_archive_to_stage(archive_path, staging.path(), include_tor_keys)
        .with_context(|| "backup archive failed validation")?;
    validate_staged_backup(staging.path(), &staged, include_tor_keys)?;
    rewrite_restored_media_paths(&staging.path().join("db/rustpost.sqlite3"), paths)?;
    let pre_restore_backup =
        create_backup_inner(paths, include_tor_keys, BackupKind::PreRestore, false, None)
            .with_context(|| "failed to create pre-restore safety backup")?;
    swap_staged_runtime(paths, staging.path(), include_tor_keys)
        .with_context(|| "failed to install restored runtime state")?;
    Ok(RestoreReport {
        pre_restore_backup: Some(pre_restore_backup),
        tor_keys_restored: include_tor_keys && staged.manifest.tor_keys_included,
    })
}

pub(super) fn swap_staged_runtime(
    paths: &RuntimePaths,
    staging_dir: &Path,
    include_tor_keys: bool,
) -> anyhow::Result<()> {
    let rollback = tempfile::Builder::new()
        .prefix(".rustpost-restore-rollback-")
        .tempdir_in(&paths.data_dir)?
        .keep();
    let mut moved_live = Vec::<(PathBuf, PathBuf)>::new();
    let mut installed = Vec::<PathBuf>::new();
    let result = (|| -> anyhow::Result<()> {
        // Keep a durable description for operator recovery if power is lost.
        let roots = restore_roots(paths, staging_dir, include_tor_keys);
        let mut recovery = String::new();
        for root in &roots {
            writeln!(recovery, "{} -> {}", root.relative, root.live.display())?;
        }
        fs::write(rollback.join("recovery-paths.txt"), recovery)?;
        OpenOptions::new()
            .write(true)
            .open(rollback.join("recovery-paths.txt"))?
            .sync_all()?;
        #[cfg(unix)]
        {
            sync_directory(&rollback)?;
            sync_directory(&paths.data_dir)?;
        }
        for root in roots {
            if root.live.try_exists()? {
                let rollback_path = rollback.join(root.relative);
                if let Some(parent) = rollback_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::rename(&root.live, &rollback_path).with_context(|| {
                    format!("failed to preserve live path {}", root.live.display())
                })?;
                moved_live.push((rollback_path, root.live.clone()));
            }
            if let Some(parent) = root.live.parent() {
                fs::create_dir_all(parent)?;
            }
            if root.staged.try_exists()? {
                fs::rename(&root.staged, &root.live)?;
            } else if root.directory {
                fs::create_dir_all(&root.live)?;
            } else {
                anyhow::bail!("staged restore file missing");
            }
            installed.push(root.live);
        }
        #[cfg(unix)]
        apply_restored_permissions(paths, include_tor_keys)?;
        for path in &installed {
            sync_restored_tree(path)?;
        }
        #[cfg(unix)]
        sync_directory(&paths.data_dir)?;
        Ok(())
    })();
    if let Err(error) = result {
        if let Err(rollback_error) = rollback_restore(&installed, &moved_live) {
            anyhow::bail!(
                "restore failed: {error:#}; rollback failed: {rollback_error:#}; preserved recovery files in {}",
                rollback.display()
            );
        }
        fs::remove_dir_all(&rollback)?;
        return Err(error).context("restore failed; previous runtime restored");
    }
    // If cleanup fails, keep recovery evidence and require operator attention.
    fs::remove_dir_all(&rollback)?;
    #[cfg(unix)]
    sync_directory(&paths.data_dir)?;
    Ok(())
}

pub(super) fn sync_restored_tree(path: &Path) -> anyhow::Result<()> {
    for entry in WalkDir::new(path).contents_first(true).follow_links(false) {
        let entry = entry?;
        if entry.file_type().is_file() {
            OpenOptions::new()
                .write(true)
                .open(entry.path())?
                .sync_all()?;
        }
        #[cfg(unix)]
        if entry.file_type().is_dir() {
            sync_directory(entry.path())?;
        }
    }
    Ok(())
}

pub(super) fn rollback_restore(
    installed: &[PathBuf],
    moved_live: &[(PathBuf, PathBuf)],
) -> anyhow::Result<()> {
    for path in installed.iter().rev() {
        remove_path_if_exists(path)?;
    }
    for (rollback_path, live_path) in moved_live.iter().rev() {
        if let Some(parent) = live_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(rollback_path, live_path).with_context(|| {
            format!(
                "failed to restore live path {} after restore failure",
                live_path.display()
            )
        })?;
    }
    Ok(())
}

#[cfg(unix)]
pub(super) fn apply_restored_permissions(
    paths: &RuntimePaths,
    include_tor_keys: bool,
) -> anyhow::Result<()> {
    if include_tor_keys && paths.tor_onion_service_dir.exists() {
        restrict_tor_permissions(&paths.tor_onion_service_dir)?;
    }
    Ok(())
}

#[cfg(unix)]
pub(super) fn restrict_tor_permissions(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    for entry in WalkDir::new(path).follow_links(false) {
        let entry = entry?;
        let mode = if entry.file_type().is_dir() {
            0o700
        } else {
            0o600
        };
        fs::set_permissions(entry.path(), fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

pub(super) fn restore_roots(
    paths: &RuntimePaths,
    staging_dir: &Path,
    include_tor_keys: bool,
) -> Vec<RestoreRoot> {
    let mut roots = vec![
        RestoreRoot {
            relative: "settings.toml",
            staged: staging_dir.join("settings.toml"),
            live: paths.settings_path.clone(),
            directory: false,
        },
        RestoreRoot {
            relative: "db",
            staged: staging_dir.join("db"),
            live: paths.db_dir.clone(),
            directory: true,
        },
        RestoreRoot {
            relative: "uploads/originals",
            staged: staging_dir.join("uploads/originals"),
            live: paths.uploads_originals.clone(),
            directory: true,
        },
        RestoreRoot {
            relative: "uploads/images",
            staged: staging_dir.join("uploads/images"),
            live: paths.uploads_images.clone(),
            directory: true,
        },
        RestoreRoot {
            relative: "uploads/videos",
            staged: staging_dir.join("uploads/videos"),
            live: paths.uploads_videos.clone(),
            directory: true,
        },
        RestoreRoot {
            relative: "uploads/thumbs",
            staged: staging_dir.join("uploads/thumbs"),
            live: paths.uploads_thumbs.clone(),
            directory: true,
        },
        RestoreRoot {
            relative: "assets",
            staged: staging_dir.join("assets"),
            live: paths.assets_dir.clone(),
            directory: true,
        },
    ];
    if include_tor_keys {
        roots.push(RestoreRoot {
            relative: "tor/onion-service",
            staged: staging_dir.join("tor/onion-service"),
            live: paths.tor_onion_service_dir.clone(),
            directory: true,
        });
    }
    roots
}

pub(super) fn remove_path_if_exists(path: &Path) -> anyhow::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path)?,
        Ok(_) => fs::remove_file(path)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}
