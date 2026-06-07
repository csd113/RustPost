use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Context as _;
use chrono::{SecondsFormat, Utc};
use rusqlite::{Connection, OptionalExtension as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tar::{Archive, Builder, EntryType, Header};
use tokio::sync::watch;
use tracing::{info, warn};
use walkdir::WalkDir;

use crate::config::{BackupSettings, Settings};
use crate::db;
use crate::runtime::RuntimePaths;

const FORMAT_VERSION: u16 = 1;
const MANIFEST_PATH: &str = "manifest.toml";
const MANIFEST_MAX_BYTES: u64 = 256 * 1024;
const LOCK_DIR: &str = "backup-restore.lock";
const AUTOMATIC_CHECK_INTERVAL_SECS: u64 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackupKind {
    Manual,
    Automatic,
    PreRestore,
}

#[derive(Debug, Clone)]
struct ArchiveFile {
    source: PathBuf,
    archive_path: String,
    component: &'static str,
    size: u64,
    sha256: String,
    mode: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupManifest {
    pub format_version: u16,
    pub rustpost_version: String,
    pub db_schema_version: Option<i64>,
    pub created_at: String,
    pub tor_keys_included: bool,
    pub components: Vec<ManifestEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub kind: ManifestEntryKind,
    pub component: String,
    pub archive_path: String,
    pub runtime_path: String,
    pub size: u64,
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManifestEntryKind {
    Directory,
    File,
}

#[derive(Debug, Clone)]
pub struct BackupArchiveInfo {
    pub filename: String,
    pub path: PathBuf,
    pub size: u64,
    pub automatic: bool,
    pub created_at: Option<String>,
    pub tor_keys_included: Option<bool>,
    pub manifest_valid: bool,
}

#[derive(Debug, Clone)]
pub struct RestoreReport {
    pub pre_restore_backup: Option<PathBuf>,
    pub tor_keys_restored: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExtractedEntry {
    kind: ManifestEntryKind,
    size: u64,
}

pub fn create_backup(paths: &RuntimePaths, include_tor_keys: bool) -> anyhow::Result<PathBuf> {
    let _lock = OperationLock::acquire(paths)?;
    create_backup_inner(paths, include_tor_keys, BackupKind::Manual, true)
}

pub fn restore_backup(
    paths: &RuntimePaths,
    archive_path: &Path,
    include_tor_keys: bool,
) -> anyhow::Result<RestoreReport> {
    let _lock = OperationLock::acquire(paths)?;
    let staging = tempfile::Builder::new()
        .prefix("rustpost-restore-")
        .tempdir_in(&paths.tmp_dir)
        .with_context(|| "failed to create restore staging directory")?;
    let staged = extract_archive_to_stage(archive_path, staging.path(), include_tor_keys)
        .with_context(|| "backup archive failed validation")?;
    validate_staged_backup(staging.path(), &staged, include_tor_keys)?;
    let pre_restore_backup =
        create_backup_inner(paths, include_tor_keys, BackupKind::PreRestore, false)
            .with_context(|| "failed to create pre-restore safety backup")?;
    swap_staged_runtime(paths, staging.path(), include_tor_keys)
        .with_context(|| "failed to install restored runtime state")?;
    apply_restored_permissions(paths, include_tor_keys)?;
    Ok(RestoreReport {
        pre_restore_backup: Some(pre_restore_backup),
        tor_keys_restored: include_tor_keys && staged.manifest.tor_keys_included,
    })
}

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

pub fn operation_in_progress(paths: &RuntimePaths) -> bool {
    paths.tmp_dir.join(LOCK_DIR).is_dir()
}

pub fn run_automatic_backup_if_due(
    base_paths: &RuntimePaths,
    settings: &Settings,
) -> anyhow::Result<Option<PathBuf>> {
    if !settings.backup.enabled || !settings.backup.automatic_enabled {
        return Ok(None);
    }
    let paths = RuntimePaths::from_data_dir(base_paths.data_dir.clone())
        .with_tor_data_dir(&settings.tor.data_dir)
        .with_backup_dir(&settings.backup.backup_dir);
    paths.ensure()?;
    if !automatic_backup_due(&paths, &settings.backup)? {
        apply_retention(&paths, &settings.backup)?;
        return Ok(None);
    }
    let _lock = OperationLock::acquire(&paths)?;
    let archive = create_backup_inner(
        &paths,
        settings.backup.automatic_include_tor_keys,
        BackupKind::Automatic,
        true,
    )?;
    apply_retention(&paths, &settings.backup)?;
    Ok(Some(archive))
}

pub fn spawn_automatic_scheduler(
    base_paths: RuntimePaths,
    settings_path: PathBuf,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        loop {
            match Settings::load(&settings_path).and_then(|settings| {
                settings.validate()?;
                run_automatic_backup_if_due(&base_paths, &settings)
            }) {
                Ok(Some(path)) => info!(archive = %path.display(), "automatic backup created"),
                Ok(None) => {}
                Err(error) => warn!(error = %error, "automatic backup check failed"),
            }
            tokio::select! {
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
                () = tokio::time::sleep(Duration::from_secs(AUTOMATIC_CHECK_INTERVAL_SECS)) => {}
            }
        }
    });
}

pub fn apply_retention(paths: &RuntimePaths, settings: &BackupSettings) -> anyhow::Result<usize> {
    let mut automatic = list_backups(paths)?
        .into_iter()
        .filter(|archive| archive.automatic)
        .collect::<Vec<_>>();
    automatic.sort_by(|left, right| right.filename.cmp(&left.filename));
    let keep_last = settings.retention_keep_last.max(1);
    let cutoff = retention_cutoff(settings.retention_max_age_days);
    let mut deleted = 0usize;
    for (index, archive) in automatic.iter().enumerate() {
        let over_count = index >= keep_last;
        let over_age = cutoff.is_some_and(|cutoff| {
            fs::metadata(&archive.path)
                .and_then(|metadata| metadata.modified())
                .is_ok_and(|modified| modified < cutoff)
        });
        if over_count || (over_age && automatic.len().saturating_sub(deleted) > keep_last) {
            fs::remove_file(&archive.path)
                .with_context(|| format!("failed to remove old backup {}", archive.filename))?;
            deleted += 1;
        }
    }
    Ok(deleted)
}

pub fn read_manifest_from_archive(path: &Path) -> anyhow::Result<BackupManifest> {
    let file = File::open(path)?;
    let mut archive = Archive::new(file);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let archive_path = validate_entry_path(&entry, true)?;
        if archive_path == MANIFEST_PATH {
            return read_manifest_entry(&mut entry);
        }
    }
    anyhow::bail!("backup manifest is missing")
}

#[expect(
    clippy::too_many_lines,
    reason = "backup creation is a linear archive assembly pipeline"
)]
fn create_backup_inner(
    paths: &RuntimePaths,
    include_tor_keys: bool,
    kind: BackupKind,
    require_database: bool,
) -> anyhow::Result<PathBuf> {
    fs::create_dir_all(&paths.backups_dir)?;
    fs::create_dir_all(&paths.tmp_dir)?;
    let staging = tempfile::Builder::new()
        .prefix("rustpost-backup-")
        .tempdir_in(&paths.tmp_dir)
        .with_context(|| "failed to create backup staging directory")?;

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

    files.sort_by(|left, right| left.archive_path.cmp(&right.archive_path));
    let manifest = build_manifest(db_schema_version, include_tor_keys, &directories, &files);
    let manifest_toml = toml::to_string(&manifest)?;
    let archive_path = unique_archive_path(&paths.backups_dir, kind);
    let file = create_private_file(&archive_path)
        .with_context(|| format!("failed to create backup archive {}", archive_path.display()))?;
    let mut builder = Builder::new(file);
    append_bytes(
        &mut builder,
        MANIFEST_PATH,
        manifest_toml.as_bytes(),
        0o600,
        EntryType::Regular,
    )?;
    for directory in &directories {
        append_bytes(&mut builder, directory, &[], 0o700, EntryType::Directory)?;
    }
    for file in &files {
        append_file(&mut builder, file)?;
    }
    builder.finish()?;
    Ok(archive_path)
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private_file(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

fn append_runtime_dir(
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

fn archive_file(
    source: PathBuf,
    archive_path: &str,
    component: &'static str,
    mode: u32,
) -> anyhow::Result<ArchiveFile> {
    validate_archive_path_text(archive_path, false, true)?;
    let metadata = fs::metadata(&source)?;
    if !metadata.is_file() {
        anyhow::bail!("backup source {} is not a regular file", source.display());
    }
    let (size, sha256) = hash_file(&source)?;
    Ok(ArchiveFile {
        source,
        archive_path: archive_path.to_owned(),
        component,
        size,
        sha256,
        mode,
    })
}

fn build_manifest(
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

fn append_file(builder: &mut Builder<File>, file: &ArchiveFile) -> anyhow::Result<()> {
    let mut source = File::open(&file.source)?;
    let mut header =
        deterministic_header(&file.archive_path, file.size, file.mode, EntryType::Regular)?;
    builder.append(&header, &mut source)?;
    header.set_cksum();
    Ok(())
}

fn append_bytes(
    builder: &mut Builder<File>,
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

fn deterministic_header(
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

fn snapshot_database(source: &Path, destination: &Path) -> anyhow::Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(source)
        .with_context(|| format!("failed to open SQLite database {}", source.display()))?;
    conn.busy_timeout(Duration::from_secs(5))?;
    let _checkpoint_result = conn.pragma_update(None, "wal_checkpoint", "FULL");
    conn.execute(
        &format!("VACUUM INTO {}", sqlite_path_literal(destination)),
        [],
    )
    .with_context(|| "failed to snapshot SQLite database")?;
    validate_and_normalize_sqlite_database(destination, None)?;
    Ok(())
}

fn sqlite_path_literal(path: &Path) -> String {
    let value = path.to_string_lossy().replace('\'', "''");
    format!("'{value}'")
}

fn database_schema_version(path: &Path) -> anyhow::Result<i64> {
    let conn = Connection::open(path)?;
    db::schema_version_from_connection(&conn)
}

fn validate_and_normalize_sqlite_database(
    path: &Path,
    expected_schema_version: Option<i64>,
) -> anyhow::Result<()> {
    let conn = Connection::open(path)
        .with_context(|| format!("failed to open restored database {}", path.display()))?;
    let integrity: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        anyhow::bail!("restored database failed integrity_check");
    }
    let foreign_key_problem: Option<String> = conn
        .query_row("PRAGMA foreign_key_check", [], |row| row.get(0))
        .optional()?;
    if foreign_key_problem.is_some() {
        anyhow::bail!("restored database failed foreign_key_check");
    }
    let schema_version = db::schema_version_from_connection(&conn)?;
    if let Some(expected) = expected_schema_version
        && expected != schema_version
    {
        anyhow::bail!("manifest schema version does not match restored database");
    }
    db::normalize_restorable_schema(&conn)?;
    Ok(())
}

fn extract_archive_to_stage(
    archive_path: &Path,
    staging_dir: &Path,
    include_tor_keys: bool,
) -> anyhow::Result<StagedBackup> {
    let file = File::open(archive_path)
        .with_context(|| format!("failed to open backup archive {}", archive_path.display()))?;
    let mut archive = Archive::new(file);
    let mut manifest = None;
    let mut seen = BTreeSet::new();
    let mut entries = BTreeMap::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
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
                },
            );
        } else if entry_type.is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)
                .with_context(|| format!("archive contains duplicate file {archive_path}"))?;
            let size = io::copy(&mut entry, &mut output)?;
            entries.insert(
                archive_path,
                ExtractedEntry {
                    kind: ManifestEntryKind::File,
                    size,
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

fn read_manifest_entry(entry: &mut tar::Entry<'_, File>) -> anyhow::Result<BackupManifest> {
    let mut limited = entry.take(MANIFEST_MAX_BYTES + 1);
    let mut raw = Vec::new();
    limited.read_to_end(&mut raw)?;
    if u64::try_from(raw.len()).unwrap_or(u64::MAX) > MANIFEST_MAX_BYTES {
        anyhow::bail!("backup manifest is too large");
    }
    toml::from_slice(&raw).with_context(|| "backup manifest is malformed")
}

#[derive(Debug)]
struct StagedBackup {
    manifest: BackupManifest,
    entries: BTreeMap<String, ExtractedEntry>,
}

fn validate_staged_backup(
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
                let (size, hash) = hash_file(&staged_path)?;
                if size != manifest_entry.size
                    || Some(hash.as_str()) != manifest_entry.sha256.as_deref()
                {
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

fn validate_manifest_entry(entry: &ManifestEntry, include_tor_keys: bool) -> anyhow::Result<()> {
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

fn require_manifest_file(
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

fn swap_staged_runtime(
    paths: &RuntimePaths,
    staging_dir: &Path,
    include_tor_keys: bool,
) -> anyhow::Result<()> {
    let rollback = tempfile::Builder::new()
        .prefix(".rustpost-restore-rollback-")
        .tempdir_in(&paths.data_dir)?;
    let mut moved_live = Vec::<(PathBuf, PathBuf)>::new();
    let mut installed = Vec::<PathBuf>::new();
    for root in restore_roots(paths, staging_dir, include_tor_keys) {
        if root.live.exists() {
            let rollback_path = rollback.path().join(root.relative);
            if let Some(parent) = rollback_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::rename(&root.live, &rollback_path).with_context(|| {
                format!(
                    "failed to move live path {} into restore rollback area",
                    root.live.display()
                )
            })?;
            moved_live.push((rollback_path, root.live.clone()));
        }
        let result = (|| {
            if let Some(parent) = root.live.parent() {
                fs::create_dir_all(parent)?;
            }
            if root.staged.exists() {
                fs::rename(&root.staged, &root.live)
            } else if root.directory {
                fs::create_dir_all(&root.live)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "staged restore file missing",
                ))
            }
        })();
        if let Err(error) = result {
            rollback_restore(&installed, &moved_live)?;
            anyhow::bail!(
                "failed to install restored path {}: {error}",
                root.live.display()
            );
        }
        installed.push(root.live);
    }
    Ok(())
}

fn rollback_restore(
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

fn apply_restored_permissions(paths: &RuntimePaths, include_tor_keys: bool) -> anyhow::Result<()> {
    if include_tor_keys && paths.tor_onion_service_dir.exists() {
        restrict_tor_permissions(&paths.tor_onion_service_dir)?;
    }
    Ok(())
}

#[cfg(unix)]
fn restrict_tor_permissions(path: &Path) -> anyhow::Result<()> {
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

#[cfg(not(unix))]
fn restrict_tor_permissions(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[derive(Debug)]
struct RestoreRoot {
    relative: &'static str,
    staged: PathBuf,
    live: PathBuf,
    directory: bool,
}

fn restore_roots(
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

fn validate_entry_path(
    entry: &tar::Entry<'_, File>,
    include_tor_keys: bool,
) -> anyhow::Result<String> {
    let entry_type = entry.header().entry_type();
    if !entry_type.is_file() && !entry_type.is_dir() {
        anyhow::bail!("archive contains unsupported entry type");
    }
    let raw = entry.path_bytes();
    let raw = std::str::from_utf8(raw.as_ref())
        .map_err(|_utf8| anyhow::anyhow!("archive path is not valid UTF-8"))?;
    validate_archive_path_text(raw, entry_type.is_dir(), include_tor_keys)
}

pub fn validate_archive_path(path: &Path, include_tor_keys: bool) -> anyhow::Result<()> {
    let text = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("archive path is not valid UTF-8"))?;
    validate_archive_path_text(text, false, include_tor_keys).map(|_| ())
}

fn validate_archive_path_text(
    raw: &str,
    is_directory: bool,
    include_tor_keys: bool,
) -> anyhow::Result<String> {
    if raw.is_empty() || raw.starts_with('/') {
        anyhow::bail!("archive path is absolute or empty");
    }
    if raw.contains('\\')
        || raw.contains(':')
        || raw.contains("//")
        || raw.contains('\u{2215}')
        || raw.contains('\u{2044}')
        || raw.contains('\u{29f8}')
        || raw.contains('\u{ff0f}')
    {
        anyhow::bail!("archive path contains unsafe characters");
    }
    let lower = raw.to_ascii_lowercase();
    if lower.contains("%2e") || lower.contains("%2f") || lower.contains("%5c") {
        anyhow::bail!("archive path contains unsafe characters");
    }
    let normalized = if is_directory {
        raw.strip_suffix('/').unwrap_or(raw)
    } else {
        raw
    };
    if normalized.is_empty() || normalized.contains("//") || normalized.ends_with('/') {
        anyhow::bail!("archive path contains unsafe separators");
    }
    let path = Path::new(normalized);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        anyhow::bail!("archive path contains traversal");
    }
    if normalized.starts_with("tor/") && !include_tor_keys {
        anyhow::bail!("archive contains Tor keys but restore did not opt in");
    }
    Ok(normalized.to_owned())
}

fn validate_allowed_archive_path(path: &str, include_tor_keys: bool) -> anyhow::Result<()> {
    if !include_tor_keys && (path == "tor" || path.starts_with("tor/")) {
        anyhow::bail!("archive contains Tor keys but restore did not opt in");
    }
    if path == MANIFEST_PATH
        || path == "settings.toml"
        || path == "db"
        || path == "db/rustpost.sqlite3"
        || path == "uploads"
        || path == "uploads/originals"
        || path.starts_with("uploads/originals/")
        || path == "uploads/images"
        || path.starts_with("uploads/images/")
        || path == "uploads/videos"
        || path.starts_with("uploads/videos/")
        || path == "uploads/thumbs"
        || path.starts_with("uploads/thumbs/")
        || path == "assets"
        || path.starts_with("assets/")
        || (include_tor_keys
            && (path == "tor"
                || path == "tor/onion-service"
                || path.starts_with("tor/onion-service/")))
    {
        return Ok(());
    }
    anyhow::bail!("archive path is outside approved runtime roots");
}

fn format_archive_path(root: &str, rel: &Path) -> anyhow::Result<String> {
    let rel = rel
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("runtime path is not valid UTF-8"))?
        .replace(std::path::MAIN_SEPARATOR, "/");
    let archive_path = format!("{root}/{rel}");
    validate_archive_path_text(&archive_path, false, true)
}

fn ensure_under_root(root: &Path, target: &Path) -> anyhow::Result<()> {
    let mut normalized = PathBuf::from(root);
    for component in target
        .strip_prefix(root)
        .with_context(|| "restore target is outside staging root")?
        .components()
    {
        match component {
            Component::Normal(part) => normalized.push(part),
            _ => anyhow::bail!("restore target contains traversal"),
        }
    }
    if normalized != target {
        anyhow::bail!("restore target normalization failed");
    }
    Ok(())
}

fn required_archive_dirs(include_tor_keys: bool) -> Vec<&'static str> {
    let mut dirs = vec![
        "db",
        "uploads",
        "uploads/originals",
        "uploads/images",
        "uploads/videos",
        "uploads/thumbs",
        "assets",
    ];
    if include_tor_keys {
        dirs.push("tor");
        dirs.push("tor/onion-service");
    }
    dirs
}

fn component_for_archive_path(path: &str) -> &str {
    if path == "db" || path.starts_with("db/") {
        "database"
    } else if path == "settings.toml" {
        "settings"
    } else if path == "assets" || path.starts_with("assets/") {
        "assets"
    } else if path == "tor" || path.starts_with("tor/") {
        "tor_keys"
    } else {
        "media"
    }
}

fn excluded_runtime_artifact(path: &Path) -> bool {
    path.components().any(|component| {
        let Component::Normal(value) = component else {
            return true;
        };
        let name = value.to_string_lossy();
        matches!(
            name.as_ref(),
            ".DS_Store"
                | "node_modules"
                | "playwright-report"
                | "test-results"
                | ".cache"
                | "trace.zip"
        ) || name.ends_with(".tmp")
            || name.ends_with(".upload")
            || name.ends_with(".log")
            || name.ends_with(".trace")
            || name.ends_with(".har")
    })
}

fn automatic_backup_due(paths: &RuntimePaths, settings: &BackupSettings) -> anyhow::Result<bool> {
    let Some(latest) = list_backups(paths)?
        .into_iter()
        .filter(|archive| archive.automatic)
        .max_by(|left, right| left.filename.cmp(&right.filename))
    else {
        return Ok(true);
    };
    let modified = fs::metadata(latest.path)?.modified()?;
    let interval = Duration::from_secs(settings.automatic_interval_minutes.saturating_mul(60));
    Ok(modified.elapsed().unwrap_or(Duration::ZERO) >= interval)
}

fn retention_cutoff(days: u64) -> Option<SystemTime> {
    if days == 0 {
        return None;
    }
    SystemTime::now().checked_sub(Duration::from_secs(days.saturating_mul(86_400)))
}

fn unique_archive_path(backups_dir: &Path, kind: BackupKind) -> PathBuf {
    let prefix = match kind {
        BackupKind::Manual => "rustpost",
        BackupKind::Automatic => "rustpost-auto",
        BackupKind::PreRestore => "rustpost-pre-restore",
    };
    let timestamp = Utc::now().format("%Y%m%dT%H%M%S%6fZ");
    backups_dir.join(format!("{prefix}-{timestamp}.tar"))
}

fn is_rustpost_backup_name(filename: &str) -> bool {
    std::path::Path::new(filename)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("tar"))
        && (filename.starts_with("rustpost-")
            || filename.starts_with("rustpost-auto-")
            || filename.starts_with("rustpost-pre-restore-"))
}

fn validate_backup_filename(filename: &str) -> anyhow::Result<()> {
    validate_archive_path_text(filename, false, false)?;
    if filename.contains('/') || !is_rustpost_backup_name(filename) {
        anyhow::bail!("invalid backup filename");
    }
    Ok(())
}

fn hash_file(path: &Path) -> anyhow::Result<(u64, String)> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut buffer = vec![0u8; 64 * 1024].into_boxed_slice();
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
        hasher.update(&buffer[..read]);
    }
    Ok((total, hex_lower(hasher.finalize().as_ref())))
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn remove_path_if_exists(path: &Path) -> anyhow::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path)?,
        Ok(_) => fs::remove_file(path)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

struct OperationLock {
    path: PathBuf,
}

impl OperationLock {
    fn acquire(paths: &RuntimePaths) -> anyhow::Result<Self> {
        fs::create_dir_all(&paths.tmp_dir)?;
        let path = paths.tmp_dir.join(LOCK_DIR);
        match fs::create_dir(&path) {
            Ok(()) => Ok(Self { path }),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                anyhow::bail!("another backup or restore is already running");
            }
            Err(error) => Err(error).with_context(|| "failed to create backup operation lock"),
        }
    }
}

impl Drop for OperationLock {
    fn drop(&mut self) {
        let _remove_result = fs::remove_dir(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::CURRENT_SCHEMA_VERSION;
    use std::thread;

    fn test_db(path: &Path, version: i64) {
        let conn = Connection::open(path).expect("open db");
        db::install_release_baseline_for_test(&conn).expect("schema");
        if version != CURRENT_SCHEMA_VERSION {
            conn.execute("DELETE FROM schema_migrations", [])
                .expect("clear version");
            conn.execute(
                "INSERT INTO schema_migrations (version) VALUES (?)",
                [version],
            )
            .expect("version");
        }
    }

    fn test_settings(path: &Path) {
        fs::write(
            path,
            toml::to_string(&Settings::default()).expect("settings"),
        )
        .expect("write settings");
    }

    fn test_paths() -> (tempfile::TempDir, RuntimePaths) {
        let temp = tempfile::tempdir().expect("temp dir");
        let paths = RuntimePaths::from_data_dir(temp.path().join("data"));
        paths.ensure().expect("ensure");
        test_db(&paths.database_path, CURRENT_SCHEMA_VERSION);
        test_settings(&paths.settings_path);
        (temp, paths)
    }

    #[test]
    fn backup_path_safety() {
        assert!(validate_archive_path(Path::new("uploads/images/a.webp"), false).is_ok());
        assert!(validate_archive_path(Path::new("../settings.toml"), false).is_err());
        assert!(validate_archive_path(Path::new("/tmp/settings.toml"), false).is_err());
        assert!(validate_archive_path(Path::new("C:/settings.toml"), false).is_err());
        assert!(validate_archive_path(Path::new("uploads\\x"), false).is_err());
        assert!(validate_archive_path(Path::new("uploads//x"), false).is_err());
        assert!(validate_archive_path(Path::new("uploads/%2e%2e/settings.toml"), false).is_err());
        assert!(validate_archive_path(Path::new("uploads/∕/x"), false).is_err());
        assert!(validate_archive_path(Path::new("tor/onion-service/key"), false).is_err());
        assert!(validate_archive_path(Path::new("tor/onion-service/key"), true).is_ok());
        let tor_error =
            validate_allowed_archive_path("tor/onion-service", false).expect_err("Tor opt-in");
        assert!(tor_error.to_string().contains("restore did not opt in"));
    }

    #[test]
    fn backup_manifest_records_hashes_and_excludes_tor_by_default() {
        let (_temp, paths) = test_paths();
        fs::write(paths.uploads_images.join("photo.webp"), b"image").expect("image");
        fs::write(paths.tor_onion_service_dir.join("secret"), b"key").expect("key");

        let archive = create_backup(&paths, false).expect("backup");
        let manifest = read_manifest_from_archive(&archive).expect("manifest");
        let names = archive_names(&archive);

        assert_eq!(manifest.format_version, FORMAT_VERSION);
        assert_eq!(manifest.db_schema_version, Some(CURRENT_SCHEMA_VERSION));
        assert!(!manifest.tor_keys_included);
        assert!(names.iter().any(|name| name == MANIFEST_PATH));
        assert!(names.iter().any(|name| name == "db/rustpost.sqlite3"));
        assert!(names.iter().any(|name| name == "settings.toml"));
        assert!(names.iter().any(|name| name == "uploads/images/photo.webp"));
        assert!(!names.iter().any(|name| name.contains("tor/onion-service")));
        assert!(manifest.components.iter().any(|entry| entry.archive_path
            == "uploads/images/photo.webp"
            && entry.sha256.is_some()));

        let with_tor = create_backup(&paths, true).expect("backup");
        let names = archive_names(&with_tor);
        assert!(names.iter().any(|name| name == "tor/onion-service/secret"));
    }

    #[cfg(unix)]
    #[test]
    fn backup_archive_is_private() {
        use std::os::unix::fs::PermissionsExt as _;

        let (_temp, paths) = test_paths();

        let archive = create_backup(&paths, true).expect("backup");
        let mode = fs::metadata(archive)
            .expect("archive metadata")
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(mode, 0o600);
    }

    #[test]
    fn backup_normalizes_valid_pre_release_schema_without_touching_live_database() {
        let (_temp, paths) = test_paths();
        let conn = Connection::open(&paths.database_path).expect("live db");
        conn.execute("DELETE FROM schema_migrations", [])
            .expect("clear version");
        conn.execute("INSERT INTO schema_migrations (version) VALUES (13)", [])
            .expect("alpha version");
        drop(conn);

        let archive = create_backup(&paths, false).expect("backup");
        let manifest = read_manifest_from_archive(&archive).expect("manifest");
        let live = Connection::open(&paths.database_path).expect("live db");
        let live_version = db::schema_version_from_connection(&live).expect("live version");

        assert_eq!(manifest.db_schema_version, Some(CURRENT_SCHEMA_VERSION));
        assert_eq!(live_version, 13);
    }

    #[test]
    fn restore_validation_normalizes_valid_pre_release_schema_before_acceptance() {
        let temp = tempfile::tempdir().expect("temp dir");
        let database = temp.path().join("alpha.sqlite3");
        test_db(&database, 13);

        validate_and_normalize_sqlite_database(&database, Some(13)).expect("valid alpha schema");
        let conn = Connection::open(&database).expect("open");
        let version = db::schema_version_from_connection(&conn).expect("version");

        assert_eq!(version, CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn restore_validation_rejects_unsafe_pre_release_schema_without_normalizing() {
        let temp = tempfile::tempdir().expect("temp dir");
        let database = temp.path().join("unsafe-alpha.sqlite3");
        test_db(&database, 13);
        let conn = Connection::open(&database).expect("open");
        conn.execute("DROP INDEX idx_posts_created", [])
            .expect("drop index");
        drop(conn);

        let error =
            validate_and_normalize_sqlite_database(&database, Some(13)).expect_err("unsafe schema");
        let conn = Connection::open(&database).expect("open");
        let version = db::schema_version_from_connection(&conn).expect("version");

        assert!(error.to_string().contains("not structurally compatible"));
        assert!(
            error
                .to_string()
                .contains("missing index idx_posts_created")
        );
        assert_eq!(version, 13);
    }

    #[test]
    fn restore_validates_hashes_before_touching_live_runtime() {
        let source_temp = tempfile::tempdir().expect("source");
        let source = RuntimePaths::from_data_dir(source_temp.path().join("source"));
        source.ensure().expect("source ensure");
        test_db(&source.database_path, CURRENT_SCHEMA_VERSION);
        test_settings(&source.settings_path);
        fs::write(source.uploads_images.join("restored.webp"), b"restored").expect("media");
        let archive = create_backup(&source, false).expect("backup");

        let target_temp = tempfile::tempdir().expect("target");
        let target = RuntimePaths::from_data_dir(target_temp.path().join("target"));
        target.ensure().expect("target ensure");
        test_db(&target.database_path, CURRENT_SCHEMA_VERSION);
        test_settings(&target.settings_path);
        fs::write(target.uploads_images.join("old.webp"), b"old").expect("old");
        let corrupted = corrupt_first_file_hash(&archive, target_temp.path());

        let error = restore_backup(&target, &corrupted, false).expect_err("restore must fail");

        assert!(error.to_string().contains("validation"));
        assert!(target.uploads_images.join("old.webp").is_file());
        assert!(!target.uploads_images.join("restored.webp").exists());
    }

    #[test]
    fn restore_rejects_malicious_archive_entries() {
        let (_temp, paths) = test_paths();
        for (name, entry_type) in [
            ("../settings.toml", EntryType::Regular),
            ("/tmp/settings.toml", EntryType::Regular),
            ("uploads//images/x", EntryType::Regular),
            ("uploads/images/x", EntryType::Symlink),
            ("uploads/images/y", EntryType::Link),
            ("C:/settings.toml", EntryType::Regular),
        ] {
            let temp = tempfile::tempdir().expect("malicious");
            let archive = temp.path().join("bad.tar");
            write_minimal_archive(&archive, name, entry_type);
            assert!(
                restore_backup(&paths, &archive, false).is_err(),
                "{name} should be rejected"
            );
        }
    }

    #[test]
    fn restore_rejects_duplicate_archive_entries() {
        let (_temp, paths) = test_paths();
        let archive = paths.tmp_dir.join("duplicate.tar");
        let file = File::create(&archive).expect("archive");
        let mut builder = Builder::new(file);
        append_bytes(
            &mut builder,
            MANIFEST_PATH,
            b"not toml",
            0o600,
            EntryType::Regular,
        )
        .expect("manifest");
        append_bytes(
            &mut builder,
            "settings.toml",
            b"one",
            0o600,
            EntryType::Regular,
        )
        .expect("one");
        append_bytes(
            &mut builder,
            "settings.toml",
            b"two",
            0o600,
            EntryType::Regular,
        )
        .expect("two");
        builder.finish().expect("finish");

        let error = restore_backup(&paths, &archive, false).expect_err("duplicate");

        assert!(error.to_string().contains("validation"));
    }

    #[test]
    fn restore_rolls_back_when_install_fails() {
        let source_temp = tempfile::tempdir().expect("source");
        let source = RuntimePaths::from_data_dir(source_temp.path().join("source"));
        source.ensure().expect("source ensure");
        test_db(&source.database_path, CURRENT_SCHEMA_VERSION);
        test_settings(&source.settings_path);
        fs::write(source.uploads_images.join("restored.webp"), b"restored").expect("media");
        let archive = create_backup(&source, false).expect("backup");

        let target_temp = tempfile::tempdir().expect("target");
        let target = RuntimePaths::from_data_dir(target_temp.path().join("target"));
        target.ensure().expect("target ensure");
        test_db(&target.database_path, CURRENT_SCHEMA_VERSION);
        fs::write(&target.settings_path, "old-settings").expect("old settings");
        let old_db = fs::read(&target.database_path).expect("old db");
        fs::remove_dir_all(target.data_dir.join("uploads")).expect("remove uploads");
        fs::write(target.data_dir.join("uploads"), b"not a directory").expect("block uploads");

        let error = restore_backup(&target, &archive, false).expect_err("install failure");

        assert!(error.to_string().contains("install"));
        assert_eq!(
            fs::read_to_string(&target.settings_path).expect("settings"),
            "old-settings"
        );
        assert_eq!(fs::read(&target.database_path).expect("db"), old_db);
        assert_eq!(
            fs::read(target.data_dir.join("uploads")).expect("uploads blocker"),
            b"not a directory"
        );
    }

    #[test]
    fn concurrent_operations_are_rejected() {
        let (_temp, paths) = test_paths();
        fs::create_dir(paths.tmp_dir.join(LOCK_DIR)).expect("lock");

        let error = create_backup(&paths, false).expect_err("locked");

        assert!(error.to_string().contains("already running"));
    }

    #[test]
    fn automatic_retention_only_deletes_automatic_backups() {
        let (_temp, paths) = test_paths();
        for name in [
            "rustpost-auto-20260526T000000000000Z.tar",
            "rustpost-auto-20260526T000001000000Z.tar",
            "rustpost-auto-20260526T000002000000Z.tar",
            "rustpost-20260526T000003000000Z.tar",
        ] {
            fs::write(paths.backups_dir.join(name), b"x").expect("backup");
            thread::sleep(Duration::from_millis(2));
        }
        let settings = BackupSettings {
            retention_keep_last: 2,
            retention_max_age_days: 0,
            ..BackupSettings::default()
        };

        let deleted = apply_retention(&paths, &settings).expect("retention");

        assert_eq!(deleted, 1);
        assert!(
            !paths
                .backups_dir
                .join("rustpost-auto-20260526T000000000000Z.tar")
                .exists()
        );
        assert!(
            paths
                .backups_dir
                .join("rustpost-auto-20260526T000001000000Z.tar")
                .exists()
        );
        assert!(
            paths
                .backups_dir
                .join("rustpost-auto-20260526T000002000000Z.tar")
                .exists()
        );
        assert!(
            paths
                .backups_dir
                .join("rustpost-20260526T000003000000Z.tar")
                .exists()
        );
    }

    fn archive_names(path: &Path) -> Vec<String> {
        let file = File::open(path).expect("archive");
        Archive::new(file)
            .entries()
            .expect("entries")
            .map(|entry| {
                let entry = entry.expect("entry");
                std::str::from_utf8(entry.path_bytes().as_ref())
                    .expect("utf8")
                    .trim_end_matches('/')
                    .to_owned()
            })
            .collect()
    }

    fn corrupt_first_file_hash(archive: &Path, dir: &Path) -> PathBuf {
        let manifest = read_manifest_from_archive(archive).expect("manifest");
        let mut manifest = manifest;
        let file = manifest
            .components
            .iter_mut()
            .find(|entry| {
                entry.kind == ManifestEntryKind::File && entry.archive_path == "settings.toml"
            })
            .expect("file");
        file.sha256 = Some("0".repeat(64));
        let manifest_toml = toml::to_string(&manifest).expect("manifest toml");
        let corrupted = dir.join("corrupted.tar");
        let output = File::create(&corrupted).expect("corrupted");
        let mut builder = Builder::new(output);
        append_bytes(
            &mut builder,
            MANIFEST_PATH,
            manifest_toml.as_bytes(),
            0o600,
            EntryType::Regular,
        )
        .expect("manifest");
        let input = File::open(archive).expect("archive");
        for entry in Archive::new(input).entries().expect("entries") {
            let mut entry = entry.expect("entry");
            let path = std::str::from_utf8(entry.path_bytes().as_ref())
                .expect("utf8")
                .trim_end_matches('/')
                .to_owned();
            if path == MANIFEST_PATH {
                continue;
            }
            let entry_type = entry.header().entry_type();
            if entry_type.is_dir() {
                append_bytes(&mut builder, &path, &[], 0o700, EntryType::Directory).expect("dir");
            } else {
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes).expect("read");
                append_bytes(&mut builder, &path, &bytes, 0o600, EntryType::Regular).expect("file");
            }
        }
        builder.finish().expect("finish");
        corrupted
    }

    fn write_minimal_archive(path: &Path, entry_name: &str, entry_type: EntryType) {
        let file = File::create(path).expect("archive");
        let mut builder = Builder::new(file);
        append_bytes(
            &mut builder,
            MANIFEST_PATH,
            b"format_version = 1",
            0o600,
            EntryType::Regular,
        )
        .expect("manifest");
        let mut header = deterministic_header("placeholder", 0, 0o600, entry_type).expect("header");
        header.as_old_mut().name.fill(0);
        header.as_old_mut().name[..entry_name.len()].copy_from_slice(entry_name.as_bytes());
        if entry_type == EntryType::Symlink || entry_type == EntryType::Link {
            header.set_link_name("settings.toml").expect("link");
        }
        header.set_cksum();
        builder.append(&header, io::empty()).expect("entry");
        builder.finish().expect("finish");
    }
}
