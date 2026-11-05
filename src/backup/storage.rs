//! Storage for the backup subsystem.

use super::*;

pub fn list_backups(paths: &RuntimePaths) -> anyhow::Result<Vec<BackupArchiveInfo>> {
    if !paths.backups_dir.exists() {
        return Ok(Vec::new());
    }
    let mut archives = Vec::new();
    for entry in fs::read_dir(&paths.backups_dir).with_context(|| {
        format!(
            "failed to read backup directory {}",
            paths.backups_dir.display()
        )
    })? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("tar") {
            continue;
        }
        let Some(filename) = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(ToOwned::to_owned)
        else {
            continue;
        };
        if !is_rustpost_backup_name(&filename) {
            continue;
        }
        let metadata = entry.metadata()?;
        let manifest = read_manifest_from_archive(&path).ok();
        let automatic = filename.starts_with("rustpost-auto-");
        archives.push(BackupArchiveInfo {
            filename,
            path,
            size: metadata.len(),
            automatic,
            created_at: manifest
                .as_ref()
                .map(|manifest| manifest.created_at.clone()),
            tor_keys_included: manifest.as_ref().map(|manifest| manifest.tor_keys_included),
            manifest_valid: manifest.is_some(),
        });
    }
    archives.sort_by(|left, right| right.filename.cmp(&left.filename));
    Ok(archives)
}

pub fn backup_path_for_download(paths: &RuntimePaths, filename: &str) -> anyhow::Result<PathBuf> {
    validate_backup_filename(filename)?;
    let path = paths.backups_dir.join(filename);
    let canonical_dir = paths
        .backups_dir
        .canonicalize()
        .with_context(|| "backup directory does not exist")?;
    let canonical_path = path
        .canonicalize()
        .with_context(|| "backup archive does not exist")?;
    if !canonical_path.starts_with(canonical_dir) || !canonical_path.is_file() {
        anyhow::bail!("backup archive does not exist");
    }
    Ok(canonical_path)
}

/// Recover an interrupted capture only while holding exclusive native backup ownership.
/// Markers are intent signals; OS locks, which release on process death, establish ownership.
pub fn operation_in_progress(paths: &RuntimePaths) -> bool {
    let capture = paths.tmp_dir.join(CAPTURE_DIR);
    if !capture.is_dir() {
        return false;
    }
    let Ok(_ownership) = backup_coordination_lock(paths) else {
        // A live owner or an IO failure must never allow an unsafe capture bypass.
        return true;
    };
    // Keep ownership through removal so another backup cannot replace the marker beneath us.
    match fs::remove_dir(&capture) {
        Ok(()) => false,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => {
            warn!(error=%error,"stale backup capture marker could not be removed");
            true
        }
    }
}

pub(super) fn sync_directory(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

pub(super) fn unique_archive_path(backups_dir: &Path, kind: BackupKind) -> PathBuf {
    let prefix = match kind {
        BackupKind::Manual => "rustpost",
        BackupKind::Automatic => "rustpost-auto",
        BackupKind::PreRestore => "rustpost-pre-restore",
    };
    let timestamp = Utc::now().format("%Y%m%dT%H%M%S%6fZ");
    backups_dir.join(format!("{prefix}-{timestamp}.tar"))
}

pub(super) fn is_rustpost_backup_name(filename: &str) -> bool {
    std::path::Path::new(filename)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("tar"))
        && (filename.starts_with("rustpost-")
            || filename.starts_with("rustpost-auto-")
            || filename.starts_with("rustpost-pre-restore-"))
}

pub(super) fn validate_backup_filename(filename: &str) -> anyhow::Result<()> {
    validate_archive_path_text(filename, false, false)?;
    if filename.contains('/') || !is_rustpost_backup_name(filename) {
        anyhow::bail!("invalid backup filename");
    }
    Ok(())
}

/// Cross-process coordination between backup and restore operations.
/// OS locking releases on interruption; the file itself is never removed.
pub fn backup_coordination_lock(paths: &RuntimePaths) -> anyhow::Result<File> {
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(paths.tmp_dir.join("backup-coordination.lock"))?;
    file.try_lock()
        .with_context(|| "another backup or restore is running")?;
    Ok(file)
}
