//! Full restore rehearsal in a disposable directory, without replacing live state.

use super::*;

#[derive(Debug)]
pub struct VerificationReport {
    pub files: usize,
    pub bytes: u64,
    pub schema_version: i64,
    pub tor_keys_checked: bool,
}

pub fn verify_backup(
    paths: &RuntimePaths,
    archive: &Path,
    include_tor_keys: bool,
) -> anyhow::Result<VerificationReport> {
    let _temporary = crate::runtime::begin_temp_operation();
    // Serialize against archive creation, retention and restore, without taking the live snapshot lock.
    let _operation = backup_coordination_lock(paths)?;
    let settings = Settings::load(&paths.settings_path)?;
    let size = fs::metadata(archive)?.len();
    crate::storage_limits::ensure_headroom(
        &paths.data_dir,
        settings.storage.min_free_bytes,
        size.saturating_mul(2),
    )?;
    let staging = tempfile::Builder::new()
        .prefix("rustpost-verify-")
        .tempdir_in(&paths.tmp_dir)?;
    let staged = extract_archive_to_stage(archive, staging.path(), include_tor_keys)?;
    validate_staged_backup(staging.path(), &staged, include_tor_keys)?;
    let rehearsal = RuntimePaths::from_data_dir(staging.path().to_owned());
    // This checks every media reference and rehearses path rewriting for a different host/data directory.
    rewrite_restored_media_paths(&staging.path().join("db/rustpost.sqlite3"), &rehearsal)?;
    let files = staged
        .manifest
        .components
        .iter()
        .filter(|e| e.kind == ManifestEntryKind::File)
        .count();
    let bytes = staged
        .manifest
        .components
        .iter()
        .map(|e| e.size)
        .fold(0_u64, u64::saturating_add);
    Ok(VerificationReport {
        files,
        bytes,
        schema_version: database_schema_version(&staging.path().join("db/rustpost.sqlite3"))?,
        tor_keys_checked: include_tor_keys && staged.manifest.tor_keys_included,
    })
}
