use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use axum::extract::multipart::Field;
use rusqlite::{OptionalExtension as _, params};
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt as _;
use uuid::Uuid;

use crate::config::Settings;
use crate::db::SqlitePool;
use crate::ffmpeg::FfmpegStatus;
use crate::runtime::RuntimePaths;

const PROFILE_PICTURE_THUMB_SIZE: u16 = 96;
const HASH_PREFIX_LEN: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Video,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileMediaSlot {
    Picture,
    Banner,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MediaHashKind {
    Original,
    Normalized,
}

#[derive(Debug, Clone)]
struct StoredMedia {
    id: i64,
    stored_path: String,
    public_path: String,
    mime_type: String,
    media_kind: String,
    original_sha256: String,
    normalized_sha256: Option<String>,
    is_nsfw: bool,
}

#[derive(Debug)]
struct StoredVariant {
    cleanup: StagedUploadGuard,
    path: PathBuf,
    public_path: String,
    mime_type: String,
    state: String,
    stderr: String,
}

#[derive(Debug, Clone)]
struct OriginalVariant {
    path: PathBuf,
    public_path: String,
}

struct UploadContext<'a> {
    pool: &'a SqlitePool,
    settings: &'a Settings,
    paths: &'a RuntimePaths,
    ffmpeg: &'a FfmpegStatus,
    background: bool,
}

struct StagedUpload {
    reservation: crate::storage_limits::Reservation,
    owner_user_id: Option<i64>,
    original_filename: String,
    staging: StagedUploadGuard,
    bytes: u64,
}

struct PreparedUpload {
    reservation: crate::storage_limits::Reservation,
    owner_user_id: Option<i64>,
    original_filename: String,
    staging: StagedUploadGuard,
    bytes: u64,
    mime: String,
    media_kind: MediaKind,
    original_sha256: String,
}

#[derive(Debug)]
struct StagedUploadGuard {
    path: PathBuf,
    armed: bool,
}

impl StagedUploadGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn disarm(&mut self) {
        self.armed = false;
    }

    async fn remove(&mut self) {
        if remove_staged_upload(&self.path).await {
            self.disarm();
        }
    }
}

impl Drop for StagedUploadGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Err(error) = std::fs::remove_file(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::debug!(
                path = %self.path.display(),
                error = %error,
                "failed to remove staged upload during guard drop"
            );
        }
    }
}

struct NewMediaRecord {
    reservation_id: String,
    cleanup: Vec<StagedUploadGuard>,
    owner_user_id: Option<i64>,
    original_filename: String,
    original_path: Option<String>,
    original_public_path: Option<String>,
    stored_path: String,
    public_path: String,
    mime_type: String,
    media_kind: String,
    byte_len: i64,
    conversion_state: String,
    ffmpeg_stderr: String,
    original_sha256: String,
    normalized_sha256: String,
}

impl ProfileMediaSlot {
    fn column(self) -> &'static str {
        match self {
            Self::Picture => "profile_picture_media_id",
            Self::Banner => "banner_media_id",
        }
    }
}

impl MediaKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Video => "video",
        }
    }
}

/// Upper bound used to reject decompression-bomb style images before ffmpeg
/// decodes them. 100 megapixels is far above any realistic profile or post
/// image while keeping a full RGBA decode under ~400 MB.
const MAX_IMAGE_PIXELS: u64 = 100_000_000;

/// Number of leading bytes inspected for content-based media type detection.
///
/// The `infer` signatures are all at the start of a file, so a bounded prefix
/// is enough and avoids loading an entire upload (potentially hundreds of
/// megabytes of video) into memory.
const MEDIA_SNIFF_PREFIX_BYTES: usize = 64 * 1024;

mod upload;
pub use upload::save_upload;
use upload::{remove_staged_upload, save_upload_inner};
mod probe;
use probe::{image_dimensions, sniff_and_hash};
mod deduplicate;
use deduplicate::{try_insert_duplicate, try_insert_duplicate_after_canonical_conflict};
mod storage;
pub use storage::reject_path_tricks;
use storage::{
    has_parent_component, hash_file, hex_lower, public_upload_path, safe_extension,
    safe_stored_media_path, stable_media_basename, unique_media_path,
};
mod profile;
pub(crate) use profile::{
    ProfileMediaChange, apply_profile_media_changes, cleanup_replaced_profile_media,
};
pub use profile::{
    clear_profile_media, save_banner_upload, save_profile_picture_upload, set_profile_media,
};
mod delete;
pub use delete::{delete_media, delete_post_media, validate_post_media_deletion};
mod convert;
pub use convert::set_media_nsfw;
use convert::{classify, convert_or_original, stderr_summary_for_db};

/// Own media rows created for an unfinished multipart form. Cancellation or
/// rejection schedules conservative cleanup; referenced rows are never deleted.
pub(crate) struct PendingUploads {
    pool: SqlitePool,
    paths: RuntimePaths,
    ids: Vec<i64>,
    runtime: Option<tokio::runtime::Handle>,
}

impl PendingUploads {
    pub(crate) fn new(pool: &SqlitePool, paths: &RuntimePaths) -> Self {
        Self {
            pool: pool.clone(),
            paths: paths.clone(),
            ids: Vec::new(),
            runtime: tokio::runtime::Handle::try_current().ok(),
        }
    }
    pub(crate) fn track(&mut self, id: i64) {
        self.ids.push(id);
    }
    pub(crate) fn commit(&mut self) {
        self.ids.clear();
    }
}

impl Drop for PendingUploads {
    fn drop(&mut self) {
        if self.ids.is_empty() {
            return;
        }
        let ids = std::mem::take(&mut self.ids);
        let pool = self.pool.clone();
        let paths = self.paths.clone();
        if let Some(runtime) = &self.runtime {
            runtime.spawn(async move {
                for id in ids {
                    for attempt in 0..3 {
                        match delete_media(&pool, &paths, id).await {
                            Ok(()) => break,
                            Err(error) => {
                                let locked = error.chain().any(|cause| matches!(cause.downcast_ref::<rusqlite::Error>(), Some(rusqlite::Error::SqliteFailure(code, _)) if matches!(code.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)));
                                if locked && attempt < 2 {
                                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                                    continue;
                                }
                                tracing::debug!(media_id = id, error = %error, "unfinished upload cleanup retained referenced or inaccessible media");
                                break;
                            }
                        }
                    }
                }
            });
        }
    }
}

/// A committed media row whose ownership has not reached its upload handler.
/// Keeping this in the DB response also cleans rows when its receiver vanishes.
struct PendingMedia {
    id: i64,
    cleanup: PendingUploads,
}

impl PendingMedia {
    fn accept(mut self) -> i64 {
        self.cleanup.commit();
        self.id
    }
}

pub mod jobs;
