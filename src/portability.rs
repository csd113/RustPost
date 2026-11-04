//! Account export and import archives.
//!
//! `RustPost` user archives are distinct from full-instance backups: they carry
//! one account's posts, profile data, media, outgoing follows, muted words and
//! media bytes, and are imported into an authenticated destination account.
//! Password hashes, sessions, tokens, administrative flags, instance
//! configuration, notifications, interactions (likes/bookmarks/reposts), and
//! other accounts' data are never included.
//!
//! The on-disk format is a gzip-compressed tar archive:
//!
//! ```text
//! manifest.json   { format_version, app, created_at, archive_id, username, entry_count }
//! profile.json    { display_name, bio, location, website, theme, nsfw_blur_enabled,
//!                   liked_posts_public, follow_approval_required, created_at }
//! posts.json      [ { id, text, created_at, edited_at?, parent_id?, root_id?,
//!                     quote_id?, media_ids, embeds } ]
//! media.json      [ { id, original_filename, mime_type, media_kind, byte_len,
//!                     alt_text, is_nsfw, conversion_state, sha256, archive_path } ]
//! follows.json    [ { username, created_at } ]
//! settings.json   { muted_words: [ { term, created_at } ] }
//! media/<n>.<ext> one entry per listed media document, bytes included
//! ```
//!
//! Every entry is validated before any database or filesystem mutation. Media
//! bytes are streamed and hashed, never buffered whole; a bounded prefix is
//! sniffed with `infer` and must match the declared mime type. The compressed
//! size, entry count, decompressed media total, and cumulative document bytes
//! are all enforced incrementally while reading, so a compressed bomb cannot
//! consume unbounded disk or memory. Imports apply in one database transaction
//! plus a staged-file install: media files are copied to the destination
//! upload directories first, the transaction commits the database rows, and
//! copied files are removed again if anything fails.
//!
//! Behaviour decisions worth knowing:
//!
//! * Posts are exported only when authored by the account and not deleted.
//!   Reply/root/quote references are kept only when the referenced post is
//!   also part of the export, otherwise the reference is dropped (export) or
//!   set to `NULL` (import).
//! * Media is exported for media rows owned by the account that are attached
//!   to an exported post. `sha256` and `byte_len` are computed from the bytes
//!   actually written to the archive, not from the source row.
//! * Import fills empty destination profile text fields (`display_name`,
//!   `bio`, `location`, `website`) from the archive and always applies the
//!   archive's preference fields (theme, NSFW blur, public likes, follow
//!   approval). Conflicts are counted in [`ImportReport`].
//! * Imported follows resolve against the destination instance by normalized
//!   handle. Missing, deleted, suspended, self, and blocked (either direction)
//!   targets are skipped; protected targets always produce a pending
//!   `follow_requests` row and can never be followed directly.
//! * Imported media keeps the existing content-dedupe invariants: a matching
//!   canonical row is reused (no second copy) via `canonical_media_id`.
//! * Usernames, password hashes, administrative flags, suspension, and
//!   deletion state are never written from an archive.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::Context as _;
use chrono::{SecondsFormat, Utc};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use rusqlite::{OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tar::{Archive, Builder, EntryType, Header};
use uuid::Uuid;

use crate::config::Settings;
use crate::db::SqlitePool;
use crate::runtime::RuntimePaths;
use crate::validation::{clean_post_text, normalize_username, validate_profile_text};

/// Current archive format version. Importers reject other versions.
pub const FORMAT_VERSION: u32 = 1;
/// Manifest path inside the archive.
pub const MANIFEST_PATH: &str = "manifest.json";
/// Hard ceiling for one archive, matching the instance upload body limit.
pub const MAX_ARCHIVE_BYTES: u64 = 300 * 1024 * 1024;
/// Hard ceiling for the number of archive entries.
pub const MAX_ENTRIES: usize = 10_000;
/// Prefix used for staged export files under `RuntimePaths::tmp_dir`.
pub const EXPORT_TMP_PREFIX: &str = "account-export-";
/// Prefix used for staged import uploads under `RuntimePaths::tmp_dir`.
pub const IMPORT_TMP_PREFIX: &str = "account-import-";

const APP_NAME: &str = "rustpost";
const MEDIA_PREFIX: &str = "media/";
const MEDIA_DIR: &str = "media";
const PROFILE_PATH: &str = "profile.json";
const POSTS_PATH: &str = "posts.json";
const MEDIA_PATH: &str = "media.json";
const FOLLOWS_PATH: &str = "follows.json";
const SETTINGS_PATH: &str = "settings.json";
const DOCUMENT_PATHS: [&str; 6] = [
    MANIFEST_PATH,
    PROFILE_PATH,
    POSTS_PATH,
    MEDIA_PATH,
    FOLLOWS_PATH,
    SETTINGS_PATH,
];

/// Independent limits applied to account archive export and import.
///
/// `compressed_bytes` bounds the archive file itself (upload and export),
/// while `expanded_bytes` bounds the total decompressed media content. The two
/// limits are deliberately independent: raising the upload ceiling never
/// raises the decompressed ceiling, and a tiny compressed archive can never
/// expand past `expanded_bytes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveLimits {
    pub compressed_bytes: u64,
    pub expanded_bytes: u64,
    pub entries: usize,
}

impl ArchiveLimits {
    #[must_use]
    pub fn from_settings(settings: &Settings) -> Self {
        Self {
            compressed_bytes: settings.accounts.max_archive_upload_bytes,
            expanded_bytes: settings.accounts.max_archive_expanded_bytes,
            entries: settings.accounts.max_archive_entries,
        }
    }
}

/// Records that must be duplicated rather than silently truncated.
const MAX_POSTS: usize = 5_000;
const MAX_MEDIA: usize = 1_000;
const MAX_FOLLOWS: usize = 5_000;
const MAX_MUTED_WORDS: usize = 500;
const MAX_EMBEDS_PER_POST: usize = 32;
const MAX_ALT_TEXT_CHARS: usize = 4_096;
const MAX_ORIGINAL_FILENAME_CHARS: usize = 255;
const MAX_CONVERSION_STATE_CHARS: usize = 64;
const MAX_EMBED_TITLE_CHARS: usize = 512;
const MAX_EMBED_URL_CHARS: usize = 2_048;
const MAX_EMBED_VIDEO_ID_CHARS: usize = 128;
const MAX_LOCATION_CHARS: usize = 100;
const MAX_WEBSITE_CHARS: usize = 2_048;
const MAX_TIMESTAMP_CHARS: usize = 64;
const MANIFEST_MAX_BYTES: u64 = 256 * 1024;
const DOCUMENT_MAX_BYTES: u64 = 16 * 1024 * 1024;
/// Cumulative ceiling for every JSON document in one archive, in addition to
/// the per-document caps. Six maximum-size documents would otherwise hold up
/// to 96 MiB in memory at once.
const MAX_TOTAL_DOCUMENT_BYTES: u64 = 64 * 1024 * 1024;
/// Number of leading media bytes collected while streaming extraction so the
/// `infer` crate can check the content type without buffering whole entries.
const MEDIA_SNIFF_PREFIX_BYTES: usize = 512;
/// Longest accepted media entry file name, in bytes. Longer names are rejected
/// because they cannot be represented safely on common filesystems.
const MAX_MEDIA_FILE_NAME_BYTES: usize = 255;
const HASH_PREFIX_LEN: usize = 16;
const COPY_BUFFER_BYTES: usize = 64 * 1024;
const ALREADY_IMPORTED_MESSAGE: &str = "this archive has already been imported into this account";

/// `SQLite` extended result codes for the `UNIQUE(user_id, archive_id)`
/// constraint on `account_imports`.
const SQLITE_CONSTRAINT_UNIQUE: i32 = 2_067;
const SQLITE_CONSTRAINT_PRIMARYKEY: i32 = 1_555;

const FOLLOW_NOTIFICATION_MESSAGE: &str = "followed you";
const FOLLOW_REQUEST_NOTIFICATION_MESSAGE: &str = "requested to follow you";

/// Result of writing one account archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportReport {
    pub archive_path: PathBuf,
    pub archive_id: String,
    pub bytes: u64,
    pub posts: usize,
    pub media: usize,
    pub follows: usize,
    pub muted_words: usize,
}

/// Result of importing one account archive into a destination account.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportReport {
    pub archive_id: String,
    pub archive_username: String,
    pub posts_imported: usize,
    pub media_imported: usize,
    pub follows_imported: usize,
    pub follows_pending: usize,
    pub follows_skipped: usize,
    /// Reply/root/quote references in the archive that pointed at posts which
    /// are not part of the archive and were therefore dropped.
    pub post_references_dropped: usize,
    pub muted_words_imported: usize,
    pub profile_fields_applied: usize,
    pub profile_fields_skipped: usize,
}

// ---------------------------------------------------------------------------
// Archive documents
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManifestDocument {
    format_version: u32,
    app: String,
    created_at: String,
    archive_id: String,
    username: String,
    entry_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProfileDocument {
    display_name: String,
    bio: String,
    location: String,
    website: String,
    theme: String,
    nsfw_blur_enabled: bool,
    liked_posts_public: bool,
    follow_approval_required: bool,
    created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PostDocument {
    id: i64,
    text: String,
    created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    edited_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    root_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    quote_id: Option<i64>,
    #[serde(default)]
    media_ids: Vec<i64>,
    #[serde(default)]
    embeds: Vec<EmbedDocument>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EmbedDocument {
    provider: String,
    video_id: String,
    original_url: String,
    canonical_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    thumbnail_url: String,
    embed_url: String,
    position: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MediaDocument {
    id: i64,
    original_filename: String,
    mime_type: String,
    media_kind: String,
    byte_len: u64,
    alt_text: String,
    is_nsfw: bool,
    conversion_state: String,
    sha256: String,
    archive_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FollowDocument {
    username: String,
    created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SettingsDocument {
    muted_words: Vec<MutedWordDocument>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MutedWordDocument {
    term: String,
    created_at: String,
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

struct ExportSnapshot {
    username: String,
    profile: ProfileDocument,
    posts: Vec<ExportPostRow>,
    media: Vec<ExportMediaRow>,
    follows: Vec<FollowDocument>,
    muted_words: Vec<MutedWordDocument>,
}

impl ExportSnapshot {
    fn muted_words_document(&self) -> SettingsDocument {
        SettingsDocument {
            muted_words: self.muted_words.clone(),
        }
    }
}

struct ExportPostRow {
    id: i64,
    text: String,
    created_at: String,
    edited_at: Option<String>,
    parent_post_id: Option<i64>,
    root_post_id: Option<i64>,
    quote_post_id: Option<i64>,
    media_ids: Vec<i64>,
    embeds: Vec<EmbedDocument>,
}

struct ExportMediaRow {
    id: i64,
    original_filename: String,
    mime_type: String,
    media_kind: String,
    alt_text: String,
    is_nsfw: bool,
    conversion_state: String,
    stored_path: String,
}

struct ExportMediaPlan {
    archive_id: i64,
    archive_path: String,
    source: PathBuf,
    byte_len: u64,
    sha256: String,
    original_filename: String,
    mime_type: String,
    media_kind: String,
    alt_text: String,
    is_nsfw: bool,
    conversion_state: String,
}

impl ExportMediaPlan {
    fn document(&self) -> MediaDocument {
        MediaDocument {
            id: self.archive_id,
            original_filename: self.original_filename.clone(),
            mime_type: self.mime_type.clone(),
            media_kind: self.media_kind.clone(),
            byte_len: self.byte_len,
            alt_text: self.alt_text.clone(),
            is_nsfw: self.is_nsfw,
            conversion_state: self.conversion_state.clone(),
            sha256: self.sha256.clone(),
            archive_path: self.archive_path.clone(),
        }
    }
}

struct ExportDocuments<'a> {
    manifest: &'a ManifestDocument,
    profile: &'a ProfileDocument,
    posts: &'a [PostDocument],
    follows: &'a [FollowDocument],
    settings: &'a SettingsDocument,
    media_plans: &'a [ExportMediaPlan],
}

type ExportBuilder = Builder<GzEncoder<CappedWriter<File>>>;

/// Removes the staged archive if it was not renamed into place.
struct TempOutputGuard {
    path: PathBuf,
    armed: bool,
}

impl TempOutputGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempOutputGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Err(error) = fs::remove_file(&self.path)
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::debug!(
                path = %self.path.display(),
                error = %error,
                "failed to remove staged account archive"
            );
        }
    }
}

/// Counts written bytes and fails once the archive would exceed `limit`.
/// Wrapping the gzip writer means the cap applies to the compressed archive
/// size, which is what the upload limit constrains.
struct CappedWriter<W> {
    inner: W,
    written: u64,
    limit: u64,
}

impl<W> CappedWriter<W> {
    fn new(inner: W, limit: u64) -> Self {
        Self {
            inner,
            written: 0,
            limit,
        }
    }

    fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: Write> Write for CappedWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let next = self
            .written
            .saturating_add(u64::try_from(buffer.len()).unwrap_or(u64::MAX));
        if next > self.limit {
            return Err(io::Error::other(format!(
                "account archive exceeds the {}-byte limit",
                self.limit
            )));
        }
        let written = self.inner.write(buffer)?;
        self.written = self
            .written
            .saturating_add(u64::try_from(written).unwrap_or(u64::MAX));
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

// ---------------------------------------------------------------------------
// Import: archive validation and extraction
// ---------------------------------------------------------------------------

/// Everything read from a validated archive, plus the staging location of the
/// media bytes. Documents are small JSON buffers; media stays on disk.
struct ExtractedArchive {
    documents: BTreeMap<String, Vec<u8>>,
    media_files: BTreeMap<String, ExtractedMediaFile>,
    entry_count: usize,
}

struct ExtractedMediaFile {
    staged_path: PathBuf,
    size: u64,
    sha256: String,
    /// Bounded copy of the entry's first bytes, used for content sniffing.
    prefix: Vec<u8>,
}

// ---------------------------------------------------------------------------
// Import: document validation
// ---------------------------------------------------------------------------

struct StagedArchive {
    manifest: ManifestDocument,
    profile: ProfileDocument,
    posts: Vec<ValidatedPost>,
    media: Vec<ValidatedMedia>,
    follows: Vec<FollowDocument>,
    muted_words: Vec<MutedWordDocument>,
}

struct ValidatedPost {
    archive_id: i64,
    text: String,
    created_at: String,
    edited_at: Option<String>,
    parent_id: Option<i64>,
    root_id: Option<i64>,
    quote_id: Option<i64>,
    media_ids: Vec<i64>,
    embeds: Vec<EmbedDocument>,
}

struct ValidatedMedia {
    id: i64,
    original_filename: String,
    mime_type: String,
    media_kind: String,
    byte_len: u64,
    alt_text: String,
    is_nsfw: bool,
    sha256: String,
    archive_path: String,
    staged_path: PathBuf,
}

// ---------------------------------------------------------------------------
// Import: destination checks and media installation
// ---------------------------------------------------------------------------

/// One archived media entry's installation decision.
enum MediaInstallDecision {
    /// The bytes were copied by us to these destination paths.
    Copied {
        stored_path: PathBuf,
        public_path: String,
    },
    /// An existing canonical row already stores these bytes.
    Canonical {
        canonical_id: i64,
        stored_path: String,
        public_path: String,
    },
}

struct InstalledMedia {
    decisions: Vec<MediaInstallDecision>,
    cleanup: Vec<TempOutputGuard>,
}

struct CanonicalMedia {
    id: i64,
    stored_path: String,
    public_path: String,
}

// ---------------------------------------------------------------------------
// Import: database transaction
// ---------------------------------------------------------------------------

struct ImportPlan {
    cleanup: Vec<TempOutputGuard>,
    archive_id: String,
    archive_username: String,
    profile: ProfileDocument,
    posts: Vec<ValidatedPost>,
    media: Vec<ValidatedMedia>,
    media_install: Vec<MediaInstallDecision>,
    follows: Vec<FollowDocument>,
    muted_words: Vec<MutedWordDocument>,
}

struct DestinationRow {
    display_name: String,
    bio: String,
    location: String,
    website: String,
    theme: String,
    nsfw_blur_enabled: bool,
    liked_posts_public: bool,
    follow_approval_required: bool,
}

mod export;
pub use export::export_account;
mod extract;
pub use extract::import_account;
mod media;
use media::{
    archive_already_imported, ensure_import_destination, install_media_files,
    remove_files_best_effort,
};
mod apply;
use apply::{apply_import, hex_lower};
