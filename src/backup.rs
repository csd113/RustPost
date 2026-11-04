use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read as _, Write as _};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Context as _;
use chrono::{SecondsFormat, Utc};
use ring::digest::{Context as Sha256Context, SHA256};
use rusqlite::{Connection, OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use tar::{Archive, Builder, EntryType, Header};
use tokio::sync::watch;
use tracing::{info, warn};
use walkdir::WalkDir;

use crate::config::{BackupSettings, Settings};
use crate::db;
use crate::runtime::RuntimePaths;

const FORMAT_VERSION: u16 = 1;
const MANIFEST_PATH: &str = "manifest.toml";
const MANIFEST_MAX_BYTES: u64 = 16 * 1024 * 1024;
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExtractedEntry {
    kind: ManifestEntryKind,
    size: u64,
    sha256: Option<String>,
}

/// Hashes a bounded stream without retaining file contents in memory.
struct HashingReader<R> {
    inner: R,
    hasher: Sha256Context,
    size: u64,
}

impl<R> HashingReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: Sha256Context::new(&SHA256),
            size: 0,
        }
    }
}

impl<R: io::Read> io::Read for HashingReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.hasher.update(&buffer[..read]);
        self.size = self
            .size
            .saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
        Ok(read)
    }
}

#[derive(Debug)]
struct StagedBackup {
    manifest: BackupManifest,
    entries: BTreeMap<String, ExtractedEntry>,
}

#[derive(Debug)]
struct RestoreRoot {
    relative: &'static str,
    staged: PathBuf,
    live: PathBuf,
    directory: bool,
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

/// Larger bounded chunks keep hashing and archive IO efficient on big media.
fn copy_stream<R: io::Read, W: io::Write>(reader: &mut R, writer: &mut W) -> io::Result<u64> {
    let mut buffer = vec![0u8; 256 * 1024];
    let mut total = 0u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        writer.write_all(&buffer[..read])?;
        total = total.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
    }
    Ok(total)
}

struct OperationLock {
    path: PathBuf,
    _coordination: File,
    snapshot: Option<File>,
}

impl OperationLock {
    fn acquire(paths: &RuntimePaths) -> anyhow::Result<Self> {
        fs::create_dir_all(&paths.tmp_dir)?;
        let coordination = backup_coordination_lock(paths)?;
        let path = paths.tmp_dir.join(LOCK_DIR);
        match fs::create_dir(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                // Exclusive OS coordination proves the marker is stale.
                fs::remove_dir(&path).context("failed to remove stale backup marker")?;
                fs::create_dir(&path)?;
            }
            Err(error) => return Err(error).context("failed to create backup operation lock"),
        }
        let mut lock = Self {
            path,
            _coordination: coordination,
            snapshot: None,
        };
        // Announce intent before draining admitted requests, preventing a busy
        // site from starving automatic backups through new shared leases.
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            match crate::runtime::snapshot_lease(paths, true) {
                Ok(snapshot) => {
                    lock.snapshot = Some(snapshot);
                    return Ok(lock);
                }
                Err(error) if std::time::Instant::now() >= deadline => {
                    return Err(error)
                        .context("requests did not finish within 30 seconds; retry the backup");
                }
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }
}

impl Drop for OperationLock {
    fn drop(&mut self) {
        let _remove_result = fs::remove_dir(&self.path);
    }
}

mod create;
pub use create::create_backup;
use create::create_backup_inner;
mod archive;
pub use archive::read_manifest_from_archive;
use archive::{extract_archive_to_stage, validate_staged_backup};
mod database;
use database::{
    database_schema_version, rewrite_restored_media_paths, snapshot_database,
    validate_and_normalize_sqlite_database, validate_snapshot_media_entries,
};
mod restore;
pub use restore::restore_backup;
mod paths;
pub use paths::validate_archive_path;
use paths::{
    component_for_archive_path, ensure_under_root, excluded_runtime_artifact, format_archive_path,
    required_archive_dirs, validate_allowed_archive_path, validate_archive_path_text,
    validate_entry_path,
};
mod scheduler;
pub use scheduler::{
    apply_retention, run_automatic_backup_if_due, spawn_automatic_scheduler,
    spawn_automatic_scheduler_observed,
};
mod storage;
pub(crate) use storage::backup_coordination_lock;
pub use storage::{backup_path_for_download, list_backups, operation_in_progress};
use storage::{sync_directory, unique_archive_path};
