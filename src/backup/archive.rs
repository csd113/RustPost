//! Archive for the backup subsystem.

use super::*;

pub fn read_manifest_from_archive(path: &Path) -> anyhow::Result<BackupManifest> {
    let file = File::open(path)?;
    let mut archive = Archive::new(file);
    for entry in archive.entries_with_seek()? {
        let mut entry = entry?;
        let archive_path = validate_entry_path(&entry, true)?;
        if archive_path == MANIFEST_PATH {
            let manifest = read_manifest_entry(&mut entry)?;
            anyhow::ensure!(
                manifest.format_version == FORMAT_VERSION,
                "unsupported backup format"
            );
            let mut seen = BTreeSet::new();
            for component in &manifest.components {
                validate_manifest_entry(component, true)?;
                anyhow::ensure!(
                    seen.insert(&component.archive_path),
                    "duplicate manifest component"
                );
            }
            return Ok(manifest);
        }
    }
    anyhow::bail!("backup manifest is missing")
}

pub(super) fn extract_archive_to_stage(
    archive_path: &Path,
    staging_dir: &Path,
    include_tor_keys: bool,
) -> anyhow::Result<StagedBackup> {
    let file = File::open(archive_path)
        .with_context(|| format!("failed to open backup archive {}", archive_path.display()))?;
    let archive_bytes = file.metadata()?.len();
    let mut archive = Archive::new(file);
    let mut manifest = None;
    let mut seen = BTreeSet::new();
    let mut entries = BTreeMap::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        anyhow::ensure!(
            seen.len() < 100_000,
            "backup archive contains too many entries"
        );
        anyhow::ensure!(
            entry.header().size()? <= archive_bytes,
            "backup entry exceeds archive size"
        );
        let archive_path = validate_entry_path(&entry, include_tor_keys)?;
        if !seen.insert(archive_path.clone()) {
            anyhow::bail!("archive contains duplicate entry {archive_path}");
        }
        let entry_type = entry.header().entry_type();
        if archive_path == MANIFEST_PATH {
            if !entry_type.is_file() {
                anyhow::bail!("backup manifest must be a regular file");
            }
            manifest = Some(read_manifest_entry(&mut entry)?);
            continue;
        }
        validate_allowed_archive_path(&archive_path, include_tor_keys)?;
        let target = staging_dir.join(&archive_path);
        ensure_under_root(staging_dir, &target)?;
        if entry_type.is_dir() {
            fs::create_dir_all(&target)?;
            entries.insert(
                archive_path,
                ExtractedEntry {
                    kind: ManifestEntryKind::Directory,
                    size: 0,
                    sha256: None,
                },
            );
        } else if entry_type.is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut output = BufWriter::new(
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&target)
                    .with_context(|| format!("archive contains duplicate file {archive_path}"))?,
            );
            let mut reader = HashingReader::new(&mut entry);
            let size = copy_stream(&mut reader, &mut output)?;
            output.flush()?;
            let sha256 = Some(hex_lower(reader.hasher.finish().as_ref()));
            entries.insert(
                archive_path,
                ExtractedEntry {
                    kind: ManifestEntryKind::File,
                    size,
                    sha256,
                },
            );
        } else {
            anyhow::bail!("archive contains unsupported entry type");
        }
    }
    Ok(StagedBackup {
        manifest: manifest.ok_or_else(|| anyhow::anyhow!("backup manifest is missing"))?,
        entries,
    })
}

pub(super) fn read_manifest_entry(
    entry: &mut tar::Entry<'_, File>,
) -> anyhow::Result<BackupManifest> {
    let mut limited = entry.take(MANIFEST_MAX_BYTES + 1);
    let mut raw = Vec::new();
    limited.read_to_end(&mut raw)?;
    if u64::try_from(raw.len()).unwrap_or(u64::MAX) > MANIFEST_MAX_BYTES {
        anyhow::bail!("backup manifest is too large");
    }
    toml::from_slice(&raw).with_context(|| "backup manifest is malformed")
}

pub(super) fn validate_staged_backup(
    staging_dir: &Path,
    staged: &StagedBackup,
    include_tor_keys: bool,
) -> anyhow::Result<()> {
    let manifest = &staged.manifest;
    if manifest.format_version != FORMAT_VERSION {
        anyhow::bail!("unsupported backup manifest format version");
    }
    if manifest.tor_keys_included && !include_tor_keys {
        anyhow::bail!("backup contains Tor keys but restore did not opt in");
    }
    let mut manifest_entries = BTreeMap::new();
    for entry in &manifest.components {
        validate_manifest_entry(entry, include_tor_keys)?;
        if manifest_entries
            .insert(entry.archive_path.clone(), entry)
            .is_some()
        {
            anyhow::bail!("backup manifest contains duplicate component");
        }
    }
    let manifest_paths = manifest_entries.keys().collect::<BTreeSet<_>>();
    let extracted_paths = staged.entries.keys().collect::<BTreeSet<_>>();
    if manifest_paths != extracted_paths {
        anyhow::bail!("archive entries do not match backup manifest");
    }
    require_manifest_file(&manifest_entries, "db/rustpost.sqlite3")?;
    require_manifest_file(&manifest_entries, "settings.toml")?;
    // Require the upload directories even when they are empty. Restore replaces
    // the live upload roots, so an archive that silently omits them would wipe
    // every media file referenced by the restored database.
    for dir in [
        "uploads/originals",
        "uploads/images",
        "uploads/videos",
        "uploads/thumbs",
    ] {
        require_manifest_directory(&manifest_entries, dir)?;
    }

    for (path, manifest_entry) in manifest_entries {
        let extracted = staged
            .entries
            .get(&path)
            .ok_or_else(|| anyhow::anyhow!("archive entry missing after extraction"))?;
        if manifest_entry.kind != extracted.kind {
            anyhow::bail!("archive entry type does not match manifest");
        }
        let staged_path = staging_dir.join(path);
        ensure_under_root(staging_dir, &staged_path)?;
        match manifest_entry.kind {
            ManifestEntryKind::Directory => {
                if !staged_path.is_dir() {
                    anyhow::bail!("manifest directory is missing");
                }
            }
            ManifestEntryKind::File => {
                if !staged_path.is_file() {
                    anyhow::bail!("manifest file is missing");
                }
                let size = extracted.size;
                if size != manifest_entry.size || extracted.sha256 != manifest_entry.sha256 {
                    anyhow::bail!("backup file hash validation failed");
                }
                if size != extracted.size {
                    anyhow::bail!("archive file size does not match extracted size");
                }
            }
        }
    }
    validate_and_normalize_sqlite_database(
        &staging_dir.join("db/rustpost.sqlite3"),
        manifest.db_schema_version,
    )?;
    let restored_settings = fs::read_to_string(staging_dir.join("settings.toml"))?;
    let settings: Settings = toml::from_str(&restored_settings)
        .with_context(|| "restored settings.toml is malformed")?;
    settings.validate()?;
    Ok(())
}

pub(super) fn validate_manifest_entry(
    entry: &ManifestEntry,
    include_tor_keys: bool,
) -> anyhow::Result<()> {
    if entry.runtime_path != entry.archive_path {
        anyhow::bail!("manifest runtime path must match archive path");
    }
    validate_archive_path_text(
        &entry.archive_path,
        entry.kind == ManifestEntryKind::Directory,
        include_tor_keys,
    )?;
    validate_allowed_archive_path(&entry.archive_path, include_tor_keys)?;
    match entry.kind {
        ManifestEntryKind::Directory => {
            if entry.size != 0 || entry.sha256.is_some() {
                anyhow::bail!("manifest directory entries must not include hashes");
            }
        }
        ManifestEntryKind::File => {
            if entry.sha256.as_deref().is_none_or(|hash| hash.len() != 64) {
                anyhow::bail!("manifest file entries require sha256 hashes");
            }
        }
    }
    Ok(())
}

pub(super) fn require_manifest_file(
    manifest_entries: &BTreeMap<String, &ManifestEntry>,
    path: &str,
) -> anyhow::Result<()> {
    if manifest_entries
        .get(path)
        .is_none_or(|entry| entry.kind != ManifestEntryKind::File)
    {
        anyhow::bail!("backup manifest is missing required file {path}");
    }
    Ok(())
}

pub(super) fn require_manifest_directory(
    manifest_entries: &BTreeMap<String, &ManifestEntry>,
    path: &str,
) -> anyhow::Result<()> {
    if manifest_entries
        .get(path)
        .is_none_or(|entry| entry.kind != ManifestEntryKind::Directory)
    {
        anyhow::bail!("backup manifest is missing required directory {path}");
    }
    Ok(())
}
