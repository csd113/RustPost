//! Create for the backup subsystem.

use super::*;

pub fn create_backup(paths: &RuntimePaths, include_tor_keys: bool) -> anyhow::Result<PathBuf> {
    let _temp_operation = crate::runtime::begin_temp_operation();
    let mut lock = OperationLock::acquire(paths)?;
    create_backup_inner(
        paths,
        include_tor_keys,
        BackupKind::Manual,
        true,
        Some(&mut lock),
    )
}

#[expect(
    clippy::too_many_lines,
    reason = "backup creation is a linear archive assembly pipeline"
)]
pub(super) fn create_backup_inner(
    paths: &RuntimePaths,
    include_tor_keys: bool,
    kind: BackupKind,
    require_database: bool,
    capture: Option<&mut OperationLock>,
) -> anyhow::Result<PathBuf> {
    fs::create_dir_all(&paths.backups_dir)?;
    fs::create_dir_all(&paths.tmp_dir)?;
    let staging = tempfile::Builder::new()
        .prefix("rustpost-backup-")
        .tempdir_in(&paths.tmp_dir)
        .with_context(|| "failed to create backup staging directory")?;

    let settings = if paths.settings_path.is_file() {
        Settings::load(&paths.settings_path)?
    } else {
        Settings::default()
    };
    crate::storage_limits::ensure_headroom(
        &paths.tmp_dir,
        settings.storage.min_free_bytes,
        paths
            .database_path
            .metadata()
            .map_or(0, |m| m.len().saturating_mul(2)),
    )?;
    let staged_db = staging.path().join("rustpost.sqlite3");
    let db_schema_version = if paths.database_path.is_file() {
        snapshot_database(&paths.database_path, &staged_db)?;
        Some(database_schema_version(&staged_db)?)
    } else if require_database {
        anyhow::bail!(
            "database {} is required for backup",
            paths.database_path.display()
        );
    } else {
        None
    };

    let mut directories = BTreeSet::new();
    for path in required_archive_dirs(include_tor_keys) {
        directories.insert(path.to_owned());
    }

    let mut files = Vec::new();
    if staged_db.is_file() {
        files.push(archive_file(
            staged_db,
            "db/rustpost.sqlite3",
            "database",
            0o600,
        )?);
    }
    if paths.settings_path.is_file() {
        files.push(archive_file(
            paths.settings_path.clone(),
            "settings.toml",
            "settings",
            0o600,
        )?);
    } else if require_database {
        anyhow::bail!(
            "settings file {} is required for backup",
            paths.settings_path.display()
        );
    }
    append_runtime_dir(
        &mut files,
        &mut directories,
        &paths.uploads_originals,
        "uploads/originals",
        "media",
    )?;
    append_runtime_dir(
        &mut files,
        &mut directories,
        &paths.uploads_images,
        "uploads/images",
        "media",
    )?;
    append_runtime_dir(
        &mut files,
        &mut directories,
        &paths.uploads_videos,
        "uploads/videos",
        "media",
    )?;
    append_runtime_dir(
        &mut files,
        &mut directories,
        &paths.uploads_thumbs,
        "uploads/thumbs",
        "media",
    )?;
    append_runtime_dir(
        &mut files,
        &mut directories,
        &paths.assets_dir,
        "assets",
        "assets",
    )?;
    if include_tor_keys {
        append_runtime_dir(
            &mut files,
            &mut directories,
            &paths.tor_onion_service_dir,
            "tor/onion-service",
            "tor_keys",
        )?;
    }

    if paths.database_path.is_file() {
        validate_snapshot_media_entries(&staging.path().join("rustpost.sqlite3"), &files)?;
    }
    anyhow::ensure!(
        directories
            .len()
            .saturating_add(files.len())
            .saturating_add(1)
            <= 100_000,
        "backup contains too many entries for restore"
    );
    if staging.path().join("rustpost.sqlite3").is_file() {
        let referenced =
            database::referenced_media_paths(&staging.path().join("rustpost.sqlite3"))?;
        // Ignore uncommitted conversion output; only database-referenced media is durable snapshot content.
        files.retain(|file| file.component != "media" || referenced.contains(&file.archive_path));
    }
    files.sort_by(|left, right| left.archive_path.cmp(&right.archive_path));
    freeze_sources(staging.path(), &mut files, settings.storage.min_free_bytes)?;
    let needed = files
        .iter()
        .map(|file| file.size)
        .fold(MANIFEST_MAX_BYTES, u64::saturating_add);
    crate::storage_limits::ensure_headroom(
        &paths.backups_dir,
        settings.storage.min_free_bytes,
        needed,
    )?;
    // The DB and each referenced file now have stable private sources. Requests may resume while tar/hash IO continues.
    if let Some(capture) = capture {
        capture.release_capture()?;
    }

    // Publish only a complete, flushed archive. Scratch files have no .tar
    // suffix, so listing, retention and interrupted-backup recovery ignore them.
    let mut pending = tempfile::Builder::new()
        .prefix(".rustpost-backup-")
        .tempfile_in(&paths.backups_dir)?;
    let mut builder = Builder::new(BufWriter::with_capacity(256 * 1024, pending.as_file_mut()));
    for directory in &directories {
        append_bytes(&mut builder, directory, &[], 0o700, EntryType::Directory)?;
    }
    // Hash the exact bytes written, once, rather than hashing files before
    // reopening them. This also detects source size changes during capture.
    for file in &mut files {
        append_file(&mut builder, file)?;
    }
    let manifest = build_manifest(db_schema_version, include_tor_keys, &directories, &files);
    let manifest_toml = toml::to_string(&manifest)?;
    anyhow::ensure!(
        u64::try_from(manifest_toml.len())? <= MANIFEST_MAX_BYTES,
        "backup manifest exceeds the supported size"
    );
    append_bytes(
        &mut builder,
        MANIFEST_PATH,
        manifest_toml.as_bytes(),
        0o600,
        EntryType::Regular,
    )?;
    let mut writer = builder.into_inner()?;
    writer.flush()?;
    drop(writer);
    pending.as_file().sync_all()?;
    let archive_path = unique_archive_path(&paths.backups_dir, kind);
    pending.persist_noclobber(&archive_path)?;
    #[cfg(unix)]
    sync_directory(&paths.backups_dir)?;
    Ok(archive_path)
}

pub(super) fn append_runtime_dir(
    files: &mut Vec<ArchiveFile>,
    directories: &mut BTreeSet<String>,
    dir: &Path,
    archive_root: &'static str,
    component: &'static str,
) -> anyhow::Result<()> {
    directories.insert(archive_root.to_owned());
    if !dir.exists() {
        return Ok(());
    }
    for entry in WalkDir::new(dir).follow_links(false).sort_by_file_name() {
        let entry = entry?;
        if entry.file_type().is_symlink() {
            anyhow::bail!("cannot back up symlink {}", entry.path().display());
        }
        let rel = entry.path().strip_prefix(dir)?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        if excluded_runtime_artifact(rel) {
            continue;
        }
        let archive_path = format_archive_path(archive_root, rel)?;
        if entry.file_type().is_dir() {
            directories.insert(archive_path);
        } else if entry.file_type().is_file() {
            files.push(archive_file(
                entry.path().to_path_buf(),
                &archive_path,
                component,
                0o600,
            )?);
        }
    }
    Ok(())
}

pub(super) fn archive_file(
    source: PathBuf,
    archive_path: &str,
    component: &'static str,
    mode: u32,
) -> anyhow::Result<ArchiveFile> {
    validate_archive_path_text(archive_path, false, true)?;
    let metadata = fs::symlink_metadata(&source)?;
    if !metadata.is_file() {
        anyhow::bail!("backup source {} is not a regular file", source.display());
    }
    let size = metadata.len();
    let sha256 = String::new();
    Ok(ArchiveFile {
        source,
        archive_path: archive_path.to_owned(),
        component,
        size,
        sha256,
        mode,
    })
}

pub(super) fn build_manifest(
    db_schema_version: Option<i64>,
    include_tor_keys: bool,
    directories: &BTreeSet<String>,
    files: &[ArchiveFile],
) -> BackupManifest {
    let mut components = Vec::with_capacity(directories.len() + files.len());
    components.extend(directories.iter().map(|path| ManifestEntry {
        kind: ManifestEntryKind::Directory,
        component: component_for_archive_path(path).to_owned(),
        archive_path: path.clone(),
        runtime_path: path.clone(),
        size: 0,
        sha256: None,
    }));
    components.extend(files.iter().map(|file| ManifestEntry {
        kind: ManifestEntryKind::File,
        component: file.component.to_owned(),
        archive_path: file.archive_path.clone(),
        runtime_path: file.archive_path.clone(),
        size: file.size,
        sha256: Some(file.sha256.clone()),
    }));
    components.sort_by(|left, right| left.archive_path.cmp(&right.archive_path));
    BackupManifest {
        format_version: FORMAT_VERSION,
        rustpost_version: env!("CARGO_PKG_VERSION").to_owned(),
        db_schema_version,
        created_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        tor_keys_included: include_tor_keys,
        components,
    }
}

pub(super) fn append_file<W: io::Write + io::Seek>(
    builder: &mut Builder<W>,
    file: &mut ArchiveFile,
) -> anyhow::Result<()> {
    let source = File::open(&file.source)?;
    anyhow::ensure!(
        source.metadata()?.len() == file.size,
        "backup source changed before capture"
    );
    let mut reader = HashingReader::new(source);
    let mut header =
        deterministic_header(&file.archive_path, file.size, file.mode, EntryType::Regular)?;
    if file.size >= 256 * 1024 {
        let mut output = builder.append_writer(&mut header, &file.archive_path)?;
        copy_stream(&mut reader, &mut output)?;
        output.finish()?;
    } else {
        builder.append(&header, &mut reader)?;
    }
    anyhow::ensure!(
        reader.size == file.size,
        "backup source changed during capture"
    );
    file.sha256 = hex_lower(reader.hasher.finish().as_ref());
    Ok(())
}

pub(super) fn append_bytes<W: io::Write>(
    builder: &mut Builder<W>,
    archive_path: &str,
    bytes: &[u8],
    mode: u32,
    entry_type: EntryType,
) -> anyhow::Result<()> {
    let mut header = deterministic_header(
        archive_path,
        u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        mode,
        entry_type,
    )?;
    builder.append(&header, bytes)?;
    header.set_cksum();
    Ok(())
}

pub(super) fn deterministic_header(
    archive_path: &str,
    size: u64,
    mode: u32,
    entry_type: EntryType,
) -> anyhow::Result<Header> {
    let mut header = Header::new_ustar();
    header.set_path(archive_path)?;
    header.set_entry_type(entry_type);
    header.set_size(size);
    header.set_mode(mode);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_cksum();
    Ok(header)
}

fn freeze_sources(staging: &Path, files: &mut [ArchiveFile], minimum: u64) -> anyhow::Result<()> {
    for file in files {
        if file.component == "database" {
            continue;
        }
        let frozen = staging.join("frozen").join(&file.archive_path);
        if let Some(parent) = frozen.parent() {
            fs::create_dir_all(parent)?;
        }
        let linked = file.component == "media" && fs::hard_link(&file.source, &frozen).is_ok();
        if !linked {
            crate::storage_limits::ensure_headroom(staging, minimum, file.size)?;
            fs::copy(&file.source, &frozen)?;
        }
        anyhow::ensure!(
            fs::metadata(&frozen)?.len() == file.size,
            "backup source changed during capture"
        );
        file.source = frozen;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{archive_file, freeze_sources};
    use crate::{
        backup::{OperationLock, backup_coordination_lock, operation_in_progress},
        runtime::{RuntimePaths, snapshot_lease},
    };
    use std::fs;

    #[test]
    fn frozen_media_survives_live_deletion_and_settings_are_independent() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("source.png");
        fs::write(&source, b"stable media")?;
        let settings = directory.path().join("settings.toml");
        fs::write(&settings, b"original settings")?;
        let staging = directory.path().join("stage");
        fs::create_dir(&staging)?;
        let mut files = vec![
            archive_file(source.clone(), "uploads/images/source.png", "media", 0o600)?,
            archive_file(settings.clone(), "settings.toml", "settings", 0o600)?,
        ];
        freeze_sources(&staging, &mut files, 0)?;
        fs::remove_file(source)?;
        fs::write(settings, b"new settings")?;
        assert_eq!(fs::read(&files[0].source)?, b"stable media");
        assert_eq!(fs::read(&files[1].source)?, b"original settings");
        Ok(())
    }

    #[test]
    fn archive_io_releases_request_pause_but_keeps_backup_serialization() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let paths = RuntimePaths::from_data_dir(directory.path().join("data"));
        paths.ensure()?;
        let mut lock = OperationLock::acquire(&paths)?;
        assert!(operation_in_progress(&paths));
        assert!(snapshot_lease(&paths, false).is_err());
        lock.release_capture()?;
        assert!(!operation_in_progress(&paths));
        let shared = snapshot_lease(&paths, false)?;
        assert!(backup_coordination_lock(&paths).is_err());
        drop(shared);
        drop(lock);
        assert!(!operation_in_progress(&paths));
        Ok(())
    }
}
