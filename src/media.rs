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
}

#[derive(Debug, Clone)]
struct StoredVariant {
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
}

struct StagedUpload {
    owner_user_id: Option<i64>,
    original_filename: String,
    staging: PathBuf,
    bytes: u64,
}

struct PreparedUpload {
    owner_user_id: Option<i64>,
    original_filename: String,
    staging: PathBuf,
    bytes: u64,
    mime: String,
    media_kind: MediaKind,
    original_sha256: String,
}

struct NewMediaRecord {
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

pub async fn save_upload(
    pool: &SqlitePool,
    settings: &Settings,
    paths: &RuntimePaths,
    ffmpeg: &FfmpegStatus,
    owner_user_id: Option<i64>,
    mut field: Field<'_>,
) -> anyhow::Result<i64> {
    let original_filename = field.file_name().unwrap_or("upload").to_owned();
    reject_path_tricks(&original_filename)?;
    let id = Uuid::new_v4().simple().to_string();
    let staging = paths.staged_upload_path(&id);
    let bytes = write_upload_to_staging(settings, &staging, &mut field).await?;
    let context = UploadContext {
        pool,
        settings,
        paths,
        ffmpeg,
    };
    save_staged_upload(
        &context,
        StagedUpload {
            owner_user_id,
            original_filename,
            staging,
            bytes,
        },
    )
    .await
}

async fn save_staged_upload(
    context: &UploadContext<'_>,
    upload: StagedUpload,
) -> anyhow::Result<i64> {
    let prepared = prepare_staged_upload(context.settings, upload).await?;
    if let Some(media_id) = try_insert_duplicate(
        context,
        &prepared,
        MediaHashKind::Original,
        &prepared.original_sha256,
    )
    .await?
    {
        remove_staged_upload(&prepared.staging).await;
        return Ok(media_id);
    }
    store_new_upload(context, prepared).await
}

async fn prepare_staged_upload(
    settings: &Settings,
    upload: StagedUpload,
) -> anyhow::Result<PreparedUpload> {
    let data = tokio::fs::read(&upload.staging).await?;
    let Some(kind) = infer::get(&data) else {
        remove_staged_upload(&upload.staging).await;
        anyhow::bail!("unsupported media type");
    };
    let mime = kind.mime_type().to_owned();
    let media_kind = match classify(settings, &mime, upload.bytes) {
        Ok(media_kind) => media_kind,
        Err(error) => {
            remove_staged_upload(&upload.staging).await;
            return Err(error);
        }
    };
    Ok(PreparedUpload {
        owner_user_id: upload.owner_user_id,
        original_filename: upload.original_filename,
        staging: upload.staging,
        bytes: upload.bytes,
        mime,
        media_kind,
        original_sha256: sha256_hex(&data),
    })
}

async fn try_insert_duplicate(
    context: &UploadContext<'_>,
    upload: &PreparedUpload,
    hash_kind: MediaHashKind,
    hash: &str,
) -> anyhow::Result<Option<i64>> {
    if let Some(canonical) = find_duplicate_media(
        context.pool,
        context.paths,
        upload.media_kind,
        hash_kind,
        hash,
    )
    .await?
    {
        tracing::info!(
            canonical_media_id = canonical.id,
            media_kind = upload.media_kind.as_str(),
            hash_kind = ?hash_kind,
            "reusing media by content hash"
        );
        return insert_duplicate_media(
            context.pool,
            upload.owner_user_id,
            upload.original_filename.clone(),
            upload.bytes,
            upload.original_sha256.clone(),
            canonical,
        )
        .await
        .map(Some);
    }
    Ok(None)
}

async fn store_new_upload(
    context: &UploadContext<'_>,
    upload: PreparedUpload,
) -> anyhow::Result<i64> {
    let ext = safe_extension(&upload.mime, upload.media_kind);
    let mut basename = stable_media_basename(&upload.original_filename, &upload.original_sha256);
    let original_path = unique_media_path(&context.paths.uploads_originals, &basename, ext);
    if let Some(stem) = original_path.file_stem().and_then(|stem| stem.to_str()) {
        stem.clone_into(&mut basename);
    }
    let original_public_path = public_upload_path(context.paths, &original_path)?;
    tokio::fs::rename(&upload.staging, &original_path).await?;
    let stored = convert_or_original(
        context.settings,
        context.paths,
        context.ffmpeg,
        &original_path,
        &basename,
        upload.media_kind,
        &upload.mime,
    )
    .await;
    let normalized_sha256 = hash_file(&stored.path).await?;
    if stored.state == "converted" && !context.settings.media.keep_original_uploads {
        let _ = tokio::fs::remove_file(&original_path).await;
    }

    if let Some(media_id) = try_insert_duplicate(
        context,
        &upload,
        MediaHashKind::Normalized,
        &normalized_sha256,
    )
    .await?
    {
        cleanup_unreferenced_uploads([&original_path, &stored.path]).await;
        return Ok(media_id);
    }

    let original_variant =
        if stored.state == "converted" && !context.settings.media.keep_original_uploads {
            None
        } else {
            Some(OriginalVariant {
                path: original_path.clone(),
                public_path: original_public_path,
            })
        };
    let cleanup_paths = [original_path.clone(), stored.path.clone()];
    let original_path = original_variant
        .as_ref()
        .map(|variant| variant.path.to_string_lossy().to_string());
    let record = NewMediaRecord {
        owner_user_id: upload.owner_user_id,
        original_filename: upload.original_filename.clone(),
        original_path,
        original_public_path: original_variant.map(|variant| variant.public_path),
        stored_path: stored.path.to_string_lossy().to_string(),
        public_path: stored.public_path,
        mime_type: stored.mime_type,
        media_kind: upload.media_kind.as_str().to_owned(),
        byte_len: i64::try_from(upload.bytes)?,
        conversion_state: stored.state,
        ffmpeg_stderr: stored.stderr,
        original_sha256: upload.original_sha256.clone(),
        normalized_sha256: normalized_sha256.clone(),
    };
    let media_id = match insert_new_media_record(context.pool, record).await {
        Ok(media_id) => media_id,
        Err(error) => {
            if let Some(media_id) =
                try_insert_duplicate_after_canonical_conflict(context, &upload, &normalized_sha256)
                    .await?
            {
                cleanup_unreferenced_uploads([&cleanup_paths[0], &cleanup_paths[1]]).await;
                return Ok(media_id);
            }
            cleanup_unreferenced_uploads([&cleanup_paths[0], &cleanup_paths[1]]).await;
            return Err(error);
        }
    };
    Ok(media_id)
}

async fn insert_new_media_record(pool: &SqlitePool, record: NewMediaRecord) -> anyhow::Result<i64> {
    pool.call(move |conn| {
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO media (owner_user_id, original_filename, original_path, original_public_path, stored_path, public_path, mime_type, media_kind, byte_len, conversion_state, ffmpeg_stderr, original_sha256, normalized_sha256) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                record.owner_user_id,
                record.original_filename,
                record.original_path,
                record.original_public_path,
                record.stored_path,
                record.public_path,
                record.mime_type,
                record.media_kind,
                record.byte_len,
                record.conversion_state,
                record.ffmpeg_stderr,
                record.original_sha256,
                record.normalized_sha256,
            ],
        )?;
        let media_id = tx.last_insert_rowid();
        record_media_job_in_tx(
            &tx,
            media_id,
            &record.conversion_state,
            &record.ffmpeg_stderr,
        )?;
        tx.commit()?;
        Ok(media_id)
    })
    .await
}

fn record_media_job_in_tx(
    tx: &rusqlite::Transaction<'_>,
    media_id: i64,
    state: &str,
    stderr: &str,
) -> anyhow::Result<()> {
    if state != "converted" && state != "fallback" {
        return Ok(());
    }
    let stderr_summary = stderr_summary_for_db(stderr);
    tx.execute(
        "INSERT INTO media_jobs (media_id, status, stderr_summary, finished_at) VALUES (?, ?, ?, CURRENT_TIMESTAMP)",
        params![media_id, state, stderr_summary],
    )?;
    Ok(())
}

async fn try_insert_duplicate_after_canonical_conflict(
    context: &UploadContext<'_>,
    upload: &PreparedUpload,
    normalized_sha256: &str,
) -> anyhow::Result<Option<i64>> {
    if let Some(media_id) = try_insert_duplicate(
        context,
        upload,
        MediaHashKind::Original,
        &upload.original_sha256,
    )
    .await?
    {
        return Ok(Some(media_id));
    }
    try_insert_duplicate(
        context,
        upload,
        MediaHashKind::Normalized,
        normalized_sha256,
    )
    .await
}

async fn insert_duplicate_media(
    pool: &SqlitePool,
    owner_user_id: Option<i64>,
    original_filename: String,
    bytes: u64,
    original_sha256: String,
    canonical: StoredMedia,
) -> anyhow::Result<i64> {
    let byte_len = i64::try_from(bytes)?;
    let normalized_sha256 = canonical
        .normalized_sha256
        .clone()
        .filter(|hash| !hash.is_empty());
    pool.call(move |conn| {
        conn.execute(
            "INSERT INTO media (owner_user_id, original_filename, stored_path, public_path, mime_type, media_kind, byte_len, conversion_state, original_sha256, normalized_sha256, canonical_media_id) VALUES (?, ?, ?, ?, ?, ?, ?, 'duplicate', ?, ?, ?)",
            params![
                owner_user_id,
                original_filename,
                canonical.stored_path,
                canonical.public_path,
                canonical.mime_type,
                canonical.media_kind,
                byte_len,
                original_sha256,
                normalized_sha256,
                canonical.id,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    })
    .await
}

async fn write_upload_to_staging(
    settings: &Settings,
    staging: &Path,
    field: &mut Field<'_>,
) -> anyhow::Result<u64> {
    let mut file = tokio::fs::File::create(staging).await?;
    let mut bytes = 0_u64;
    while let Some(chunk) = field.chunk().await? {
        bytes += u64::try_from(chunk.len())?;
        if bytes > settings.media.max_video_size {
            remove_staged_upload(staging).await;
            anyhow::bail!("upload exceeds maximum size");
        }
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    Ok(bytes)
}

pub async fn save_profile_picture_upload(
    pool: &SqlitePool,
    settings: &Settings,
    paths: &RuntimePaths,
    ffmpeg: &FfmpegStatus,
    owner_user_id: i64,
    field: Field<'_>,
) -> anyhow::Result<i64> {
    let media_id = save_upload(pool, settings, paths, ffmpeg, Some(owner_user_id), field).await?;
    generate_profile_picture_thumbnail(pool, settings, paths, ffmpeg, media_id).await?;
    Ok(media_id)
}

async fn generate_profile_picture_thumbnail(
    pool: &SqlitePool,
    settings: &Settings,
    paths: &RuntimePaths,
    ffmpeg: &FfmpegStatus,
    media_id: i64,
) -> anyhow::Result<()> {
    let media: Option<(String, String)> = pool
        .call(move |conn| {
            conn.query_row(
                "SELECT stored_path, media_kind FROM media WHERE id = ?",
                [media_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(Into::into)
        })
        .await?;
    let Some((stored_path, media_kind)) = media else {
        return Ok(());
    };
    if media_kind != MediaKind::Image.as_str() {
        return Ok(());
    }
    if !settings.media.convert_images_to_webp || !ffmpeg.available || !ffmpeg.supports_webp {
        tracing::warn!(
            media_id,
            ffmpeg = %ffmpeg.summary(),
            "profile picture thumbnail unavailable; compact avatars will use original image"
        );
        return Ok(());
    }
    let output = paths
        .uploads_thumbs
        .join(format!("{media_id}-profile.webp"));
    match crate::ffmpeg::convert_image_to_webp_thumbnail(
        &settings.media,
        Path::new(&stored_path),
        &output,
        PROFILE_PICTURE_THUMB_SIZE,
    )
    .await
    {
        Ok(_) => {
            let thumbnail_path = output.to_string_lossy().to_string();
            let thumbnail_public_path = format!("/uploads/thumbs/{media_id}-profile.webp");
            pool.call(move |conn| {
                conn.execute(
                    "UPDATE media SET thumbnail_path = ?, thumbnail_public_path = ? WHERE id = ?",
                    params![thumbnail_path, thumbnail_public_path, media_id],
                )?;
                Ok(())
            })
            .await?;
        }
        Err(error) => {
            tracing::warn!(
                media_id,
                error = %error,
                "profile picture thumbnail generation failed; compact avatars will use original image"
            );
        }
    }
    Ok(())
}

async fn remove_staged_upload(path: &Path) {
    if let Err(error) = tokio::fs::remove_file(path).await {
        tracing::debug!(error = %error, "failed to remove rejected staged upload");
    }
}

async fn cleanup_unreferenced_uploads<const N: usize>(paths: [&Path; N]) {
    let unique_paths = paths.into_iter().collect::<BTreeSet<_>>();
    for path in unique_paths {
        if let Err(error) = tokio::fs::remove_file(path).await
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "failed to remove duplicate upload scratch file"
            );
        }
    }
}

async fn find_duplicate_media(
    pool: &SqlitePool,
    paths: &RuntimePaths,
    media_kind: MediaKind,
    hash_kind: MediaHashKind,
    hash: &str,
) -> anyhow::Result<Option<StoredMedia>> {
    let media_kind_str = media_kind.as_str().to_owned();
    let hash = hash.to_owned();
    let query_hash = hash.clone();
    let candidates = pool
        .call(move |conn| {
            let sql = match hash_kind {
                MediaHashKind::Original => {
                    "SELECT id, stored_path, public_path, mime_type, media_kind, original_sha256, normalized_sha256 FROM media WHERE canonical_media_id IS NULL AND media_kind = ? AND original_sha256 = ? ORDER BY id ASC LIMIT 8"
                }
                MediaHashKind::Normalized => {
                    "SELECT id, stored_path, public_path, mime_type, media_kind, original_sha256, normalized_sha256 FROM media WHERE canonical_media_id IS NULL AND media_kind = ? AND normalized_sha256 = ? ORDER BY id ASC LIMIT 8"
                }
            };
            let mut stmt = conn.prepare(sql)?;
            let rows = stmt
                .query_map(params![media_kind_str, query_hash], |row| {
                    Ok(StoredMedia {
                        id: row.get(0)?,
                        stored_path: row.get(1)?,
                        public_path: row.get(2)?,
                        mime_type: row.get(3)?,
                        media_kind: row.get(4)?,
                        original_sha256: row.get(5)?,
                        normalized_sha256: row.get(6)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?;

    for candidate in candidates {
        if validate_canonical_media(paths, &candidate, media_kind, hash_kind, hash.as_str()).await {
            return Ok(Some(candidate));
        }
        tracing::warn!(
            media_id = candidate.id,
            media_kind = media_kind.as_str(),
            hash_kind = ?hash_kind,
            "ignoring invalid duplicate media candidate"
        );
        quarantine_invalid_canonical_media(pool, candidate.id).await?;
    }
    Ok(None)
}

async fn quarantine_invalid_canonical_media(
    pool: &SqlitePool,
    media_id: i64,
) -> anyhow::Result<()> {
    pool.call(move |conn| {
        conn.execute(
            "UPDATE media SET original_sha256 = '', normalized_sha256 = NULL WHERE id = ? AND canonical_media_id IS NULL",
            [media_id],
        )?;
        Ok(())
    })
    .await
}

async fn validate_canonical_media(
    paths: &RuntimePaths,
    candidate: &StoredMedia,
    expected_kind: MediaKind,
    hash_kind: MediaHashKind,
    expected_hash: &str,
) -> bool {
    if candidate.media_kind != expected_kind.as_str()
        || !mime_matches_kind(&candidate.mime_type, expected_kind)
    {
        return false;
    }
    let path = PathBuf::from(&candidate.stored_path);
    if !safe_stored_media_path(paths, &path, expected_kind) {
        return false;
    }
    let Ok(public_path) = public_upload_path(paths, &path) else {
        return false;
    };
    if public_path != candidate.public_path {
        return false;
    }
    if !stored_file_is_safe(paths, &path, expected_kind, &candidate.mime_type).await {
        return false;
    }
    let hash_to_check = match hash_kind {
        MediaHashKind::Original => candidate
            .normalized_sha256
            .as_deref()
            .filter(|hash| !hash.is_empty())
            .or_else(|| {
                (!candidate.original_sha256.is_empty())
                    .then_some(candidate.original_sha256.as_str())
            }),
        MediaHashKind::Normalized => Some(expected_hash),
    };
    let Some(hash_to_check) = hash_to_check else {
        return false;
    };
    match hash_file(&path).await {
        Ok(actual_hash) => actual_hash == hash_to_check,
        Err(_) => false,
    }
}

fn safe_stored_media_path(paths: &RuntimePaths, path: &Path, media_kind: MediaKind) -> bool {
    if !path.is_absolute() || has_parent_component(path) {
        return false;
    }
    let allowed_roots = match media_kind {
        MediaKind::Image => [&paths.uploads_images, &paths.uploads_originals],
        MediaKind::Video => [&paths.uploads_videos, &paths.uploads_originals],
    };
    allowed_roots.iter().any(|root| path.starts_with(root))
}

async fn stored_file_is_safe(
    paths: &RuntimePaths,
    path: &Path,
    media_kind: MediaKind,
    mime_type: &str,
) -> bool {
    if !path_extension_matches_mime(path, media_kind, mime_type) {
        return false;
    }
    let Ok(metadata) = tokio::fs::symlink_metadata(path).await else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    let Ok(canonical_path) = tokio::fs::canonicalize(path).await else {
        return false;
    };
    let allowed_roots = match media_kind {
        MediaKind::Image => [&paths.uploads_images, &paths.uploads_originals],
        MediaKind::Video => [&paths.uploads_videos, &paths.uploads_originals],
    };
    for root in allowed_roots {
        let Ok(canonical_root) = tokio::fs::canonicalize(root).await else {
            return false;
        };
        if canonical_path.starts_with(canonical_root) {
            return true;
        }
    }
    false
}

fn path_extension_matches_mime(path: &Path, media_kind: MediaKind, mime_type: &str) -> bool {
    let Some(extension) = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
    else {
        return false;
    };
    match (media_kind, mime_type) {
        (MediaKind::Image, "image/jpeg") => extension == "jpg" || extension == "jpeg",
        (MediaKind::Image, "image/png") => extension == "png",
        (MediaKind::Image, "image/gif") => extension == "gif",
        (MediaKind::Image, "image/webp") => extension == "webp",
        (MediaKind::Video, "video/webm") => extension == "webm",
        (MediaKind::Video, "video/quicktime") => extension == "mov",
        (MediaKind::Video, _) => extension == "mp4",
        (MediaKind::Image, _) => extension == "img",
    }
}

fn mime_matches_kind(mime_type: &str, media_kind: MediaKind) -> bool {
    match media_kind {
        MediaKind::Image => mime_type.starts_with("image/"),
        MediaKind::Video => mime_type.starts_with("video/"),
    }
}

async fn hash_file(path: &Path) -> anyhow::Result<String> {
    let data = tokio::fs::read(path).await?;
    Ok(sha256_hex(&data))
}

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

fn stable_media_basename(original_filename: &str, original_sha256: &str) -> String {
    let stem = Path::new(original_filename)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("upload");
    let mut sanitized = String::with_capacity(stem.len().min(64) + 1 + HASH_PREFIX_LEN);
    let mut last_was_separator = false;
    for character in stem.chars().flat_map(char::to_lowercase) {
        let next = if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
            Some(character)
        } else if character.is_ascii_whitespace() {
            Some('-')
        } else {
            None
        };
        let Some(next) = next else {
            continue;
        };
        if matches!(next, '-' | '_' | '.') {
            if sanitized.is_empty() || last_was_separator {
                continue;
            }
            last_was_separator = true;
        } else {
            last_was_separator = false;
        }
        sanitized.push(next);
        if sanitized.len() >= 64 {
            break;
        }
    }
    while sanitized.ends_with(['-', '_', '.']) {
        sanitized.pop();
    }
    if sanitized.is_empty() {
        sanitized.push_str("upload");
    }
    let prefix_len = original_sha256.len().min(HASH_PREFIX_LEN);
    format!("{}-{}", sanitized, &original_sha256[..prefix_len])
}

fn unique_media_path(dir: &Path, basename: &str, extension: &str) -> PathBuf {
    let path = dir.join(format!("{basename}.{extension}"));
    if !path.exists() {
        return path;
    }
    dir.join(format!(
        "{}-{}.{}",
        basename,
        Uuid::new_v4().simple(),
        extension
    ))
}

fn public_upload_path(paths: &RuntimePaths, path: &Path) -> anyhow::Result<String> {
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("media path has no file name"))?;
    if path.starts_with(&paths.uploads_originals) {
        return Ok(format!("/uploads/originals/{filename}"));
    }
    if path.starts_with(&paths.uploads_images) {
        return Ok(format!("/uploads/images/{filename}"));
    }
    if path.starts_with(&paths.uploads_videos) {
        return Ok(format!("/uploads/videos/{filename}"));
    }
    if path.starts_with(&paths.uploads_thumbs) {
        return Ok(format!("/uploads/thumbs/{filename}"));
    }
    anyhow::bail!("media path is outside upload directories");
}

fn has_parent_component(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, Component::ParentDir))
}

pub async fn set_profile_media(
    pool: &SqlitePool,
    user_id: i64,
    slot: ProfileMediaSlot,
    media_id: i64,
) -> anyhow::Result<()> {
    let media_kind: Option<String> = pool
        .call(move |conn| {
            conn.query_row(
                "SELECT media_kind FROM media WHERE id = ? AND owner_user_id = ?",
                params![media_id, user_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
        })
        .await?;
    if media_kind.as_deref() != Some("image") {
        delete_media(pool, media_id).await?;
        anyhow::bail!("profile media must be an image");
    }
    let select_sql = format!("SELECT {} FROM users WHERE id = ?", slot.column());
    let previous: Option<i64> = pool
        .call(move |conn| Ok(conn.query_row(&select_sql, [user_id], |row| row.get(0))?))
        .await?;
    let update_sql = format!(
        "UPDATE users SET {} = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
        slot.column()
    );
    pool.call(move |conn| {
        conn.execute(&update_sql, params![media_id, user_id])?;
        Ok(())
    })
    .await?;
    if let Some(previous) = previous.filter(|previous| *previous != media_id) {
        delete_media(pool, previous).await?;
    }
    Ok(())
}

pub async fn clear_profile_media(
    pool: &SqlitePool,
    user_id: i64,
    slot: ProfileMediaSlot,
) -> anyhow::Result<()> {
    let select_sql = format!("SELECT {} FROM users WHERE id = ?", slot.column());
    let previous: Option<i64> = pool
        .call(move |conn| Ok(conn.query_row(&select_sql, [user_id], |row| row.get(0))?))
        .await?;
    let update_sql = format!(
        "UPDATE users SET {} = NULL, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
        slot.column()
    );
    pool.call(move |conn| {
        conn.execute(&update_sql, [user_id])?;
        Ok(())
    })
    .await?;
    if let Some(previous) = previous {
        delete_media(pool, previous).await?;
    }
    Ok(())
}

pub async fn delete_media(pool: &SqlitePool, media_id: i64) -> anyhow::Result<()> {
    let paths_to_remove = pool
        .call(move |conn| {
            let tx = conn.transaction()?;
            let media_paths: Option<(Option<String>, String, Option<String>)> = tx
                .query_row(
                    "SELECT original_path, stored_path, thumbnail_path FROM media WHERE id = ?",
                    [media_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            let Some((original_path, stored_path, thumbnail_path)) = media_paths else {
                return Ok(Vec::new());
            };
            promote_canonical_references(&tx, media_id)?;
            tx.execute("DELETE FROM media_jobs WHERE media_id = ?", [media_id])?;
            tx.execute("DELETE FROM media WHERE id = ?", [media_id])?;
            let mut paths = BTreeSet::new();
            for path in [original_path, Some(stored_path), thumbnail_path]
                .into_iter()
                .flatten()
                .filter(|path| !path.is_empty())
            {
                let remaining: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM media WHERE original_path = ? OR stored_path = ? OR thumbnail_path = ?",
                    params![path, path, path],
                    |row| row.get(0),
                )?;
                if remaining == 0 {
                    paths.insert(PathBuf::from(path));
                }
            }
            tx.commit()?;
            Ok(paths.into_iter().collect::<Vec<_>>())
        })
        .await?;
    for path in paths_to_remove {
        if let Err(error) = tokio::fs::remove_file(&path).await
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                media_id,
                path = %path.display(),
                error = %error,
                "failed to remove unreferenced media file after media deletion"
            );
        }
    }
    Ok(())
}

fn promote_canonical_references(
    tx: &rusqlite::Transaction<'_>,
    media_id: i64,
) -> anyhow::Result<()> {
    let replacement: Option<i64> = tx
        .query_row(
            "SELECT id FROM media WHERE canonical_media_id = ? ORDER BY id ASC LIMIT 1",
            [media_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(replacement) = replacement else {
        return Ok(());
    };
    tx.execute(
        "UPDATE media SET original_sha256 = '', normalized_sha256 = NULL WHERE id = ?",
        [media_id],
    )?;
    tx.execute(
        "UPDATE media SET canonical_media_id = NULL WHERE id = ?",
        [replacement],
    )?;
    tx.execute(
        "UPDATE media SET canonical_media_id = ? WHERE canonical_media_id = ?",
        params![replacement, media_id],
    )?;
    Ok(())
}

pub async fn set_media_nsfw(
    pool: &SqlitePool,
    media_ids: &[i64],
    is_nsfw: bool,
) -> anyhow::Result<()> {
    if media_ids.is_empty() {
        return Ok(());
    }
    let media_ids = media_ids.to_vec();
    pool.call(move |conn| {
        let tx = conn.transaction()?;
        for media_id in media_ids {
            let changed = tx.execute(
                "UPDATE media SET is_nsfw = ? WHERE id = ?",
                params![i64::from(is_nsfw), media_id],
            )?;
            if changed != 1 {
                anyhow::bail!("media attachment not found");
            }
        }
        tx.commit()?;
        Ok(())
    })
    .await
}

fn classify(settings: &Settings, mime: &str, bytes: u64) -> anyhow::Result<MediaKind> {
    if settings
        .media
        .allowed_image_mime_types
        .iter()
        .any(|allowed| allowed == mime)
    {
        if bytes > settings.media.max_image_size {
            anyhow::bail!("image exceeds maximum size");
        }
        return Ok(MediaKind::Image);
    }
    if settings
        .media
        .allowed_video_mime_types
        .iter()
        .any(|allowed| allowed == mime)
    {
        if bytes > settings.media.max_video_size {
            anyhow::bail!("video exceeds maximum size");
        }
        return Ok(MediaKind::Video);
    }
    anyhow::bail!("unsupported media type");
}

async fn convert_or_original(
    settings: &Settings,
    paths: &RuntimePaths,
    ffmpeg: &FfmpegStatus,
    original: &Path,
    basename: &str,
    media_kind: MediaKind,
    original_mime: &str,
) -> StoredVariant {
    if ffmpeg.available {
        match media_kind {
            MediaKind::Image if settings.media.convert_images_to_webp && ffmpeg.supports_webp => {
                let out = unique_media_path(&paths.uploads_images, basename, "webp");
                match crate::ffmpeg::convert_image_to_webp(&settings.media, original, &out).await {
                    Ok(stderr) => {
                        return StoredVariant {
                            public_path: public_upload_path(paths, &out)
                                .unwrap_or_else(|_| format!("/uploads/images/{basename}.webp")),
                            path: out,
                            mime_type: "image/webp".to_owned(),
                            state: "converted".to_owned(),
                            stderr,
                        };
                    }
                    Err(err) => {
                        return original_fallback(
                            original,
                            paths,
                            media_kind,
                            original_mime,
                            "fallback",
                            &err.to_string(),
                        );
                    }
                }
            }
            MediaKind::Video if settings.media.convert_videos_to_webm && ffmpeg.supports_vp9 => {
                let out = unique_media_path(&paths.uploads_videos, basename, "webm");
                match crate::ffmpeg::convert_video_to_webm(&settings.media, original, &out).await {
                    Ok(stderr) => {
                        return StoredVariant {
                            public_path: public_upload_path(paths, &out)
                                .unwrap_or_else(|_| format!("/uploads/videos/{basename}.webm")),
                            path: out,
                            mime_type: "video/webm".to_owned(),
                            state: "converted".to_owned(),
                            stderr,
                        };
                    }
                    Err(err) => {
                        return original_fallback(
                            original,
                            paths,
                            media_kind,
                            original_mime,
                            "fallback",
                            &err.to_string(),
                        );
                    }
                }
            }
            _ => {}
        }
    }
    original_fallback(original, paths, media_kind, original_mime, "original", "")
}

fn original_fallback(
    original: &Path,
    paths: &RuntimePaths,
    _media_kind: MediaKind,
    mime: &str,
    state: &str,
    stderr: &str,
) -> StoredVariant {
    StoredVariant {
        path: original.to_owned(),
        public_path: public_upload_path(paths, original).unwrap_or_else(|_| {
            let base = original
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("upload");
            format!("/uploads/originals/{base}")
        }),
        mime_type: mime.to_owned(),
        state: state.to_owned(),
        stderr: stderr.to_owned(),
    }
}

fn stderr_summary_for_db(stderr: &str) -> String {
    stderr
        .lines()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn reject_path_tricks(filename: &str) -> anyhow::Result<()> {
    let path = Path::new(filename);
    if path.is_absolute()
        || filename.contains('/')
        || filename.contains('\\')
        || filename.contains("..")
        || filename.contains(':')
        || filename.chars().any(char::is_control)
    {
        anyhow::bail!("unsafe upload filename");
    }
    Ok(())
}

fn safe_extension(mime: &str, kind: MediaKind) -> &'static str {
    match (kind, mime) {
        (MediaKind::Image, "image/jpeg") => "jpg",
        (MediaKind::Image, "image/png") => "png",
        (MediaKind::Image, "image/gif") => "gif",
        (MediaKind::Image, "image/webp") => "webp",
        (MediaKind::Video, "video/webm") => "webm",
        (MediaKind::Video, "video/quicktime") => "mov",
        (MediaKind::Video, _) => "mp4",
        (MediaKind::Image, _) => "img",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn filename_safety() {
        assert!(reject_path_tricks("photo.png").is_ok());
        assert!(reject_path_tricks("../photo.png").is_err());
        assert!(reject_path_tricks("a/b.png").is_err());
        assert!(reject_path_tricks("C:\\x.png").is_err());
    }

    #[test]
    fn size_limits() {
        let settings = Settings::default();
        assert!(classify(&settings, "image/png", settings.media.max_image_size).is_ok());
        assert!(classify(&settings, "image/png", settings.media.max_image_size + 1).is_err());
        assert!(classify(&settings, "video/mp4", settings.media.max_video_size).is_ok());
        assert!(classify(&settings, "video/mp4", settings.media.max_video_size + 1).is_err());
    }

    #[tokio::test]
    async fn exact_same_file_uploaded_twice_reuses_canonical_media() {
        let (_temp, paths, pool, settings, ffmpeg, user_id) = media_fixture().await;

        let first = save_test_upload(
            &pool,
            &settings,
            &paths,
            &ffmpeg,
            user_id,
            "Photo.PNG",
            &tiny_png_bytes(),
        )
        .await;
        let second = save_test_upload(
            &pool,
            &settings,
            &paths,
            &ffmpeg,
            user_id,
            "Photo.PNG",
            &tiny_png_bytes(),
        )
        .await;

        let first = media_row(&pool, first).await;
        let second = media_row(&pool, second).await;
        assert_eq!(second.canonical_media_id, Some(first.id));
        assert_eq!(second.stored_path, first.stored_path);
        assert_eq!(second.public_path, first.public_path);
        assert_eq!(second.original_sha256, first.original_sha256);
        assert!(Path::new(&first.stored_path).exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn existing_transcoded_webm_is_reused_for_matching_mp4_output() {
        let (temp, paths, pool, settings, mut ffmpeg, user_id) = media_fixture().await;
        let normalized = b"normalized-video";
        let normalized_hash = sha256_hex(normalized);
        let canonical_path = paths.uploads_videos.join("clip-canonical.webm");
        tokio::fs::write(&canonical_path, normalized)
            .await
            .expect("canonical webm");
        let canonical_id = insert_canonical_media(
            &pool,
            user_id,
            TestCanonicalMedia {
                stored_path: &canonical_path,
                public_path: "/uploads/videos/clip-canonical.webm",
                mime_type: "video/webm",
                media_kind: "video",
                original_sha256: "seed-original",
                normalized_sha256: &normalized_hash,
            },
        )
        .await;
        ffmpeg.available = true;
        ffmpeg.supports_vp9 = true;
        let mut settings = settings;
        settings.media.ffmpeg_path = fake_ffmpeg_output(&temp, normalized).display().to_string();

        let uploaded = save_test_upload(
            &pool,
            &settings,
            &paths,
            &ffmpeg,
            user_id,
            "clip.mp4",
            &tiny_mp4_bytes(),
        )
        .await;

        let row = media_row(&pool, uploaded).await;
        assert_eq!(row.canonical_media_id, Some(canonical_id));
        assert_eq!(row.stored_path, canonical_path.to_string_lossy());
        assert!(!paths.uploads_videos.join("clip.mp4").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn different_image_formats_with_same_webp_output_reuse_canonical() {
        let (temp, paths, pool, mut settings, mut ffmpeg, user_id) = media_fixture().await;
        let normalized = b"normalized-image";
        settings.media.ffmpeg_path = fake_ffmpeg_output(&temp, normalized).display().to_string();
        ffmpeg.available = true;
        ffmpeg.supports_webp = true;

        let png = save_test_upload(
            &pool,
            &settings,
            &paths,
            &ffmpeg,
            user_id,
            "image.png",
            &tiny_png_bytes(),
        )
        .await;
        let jpg = save_test_upload(
            &pool,
            &settings,
            &paths,
            &ffmpeg,
            user_id,
            "image.jpg",
            &tiny_jpeg_bytes(),
        )
        .await;

        let png = media_row(&pool, png).await;
        let jpg = media_row(&pool, jpg).await;
        assert_eq!(jpg.canonical_media_id, Some(png.id));
        assert_eq!(jpg.stored_path, png.stored_path);
        assert_eq!(jpg.normalized_sha256, png.normalized_sha256);
    }

    #[tokio::test]
    async fn missing_canonical_file_disables_duplicate_reuse_safely() {
        let (_temp, paths, pool, settings, ffmpeg, user_id) = media_fixture().await;
        let raw_hash = sha256_hex(&tiny_png_bytes());
        let missing = paths.uploads_images.join("missing.webp");
        let canonical_id = insert_canonical_media(
            &pool,
            user_id,
            TestCanonicalMedia {
                stored_path: &missing,
                public_path: "/uploads/images/missing.webp",
                mime_type: "image/webp",
                media_kind: "image",
                original_sha256: &raw_hash,
                normalized_sha256: &raw_hash,
            },
        )
        .await;

        let uploaded = save_test_upload(
            &pool,
            &settings,
            &paths,
            &ffmpeg,
            user_id,
            "photo.png",
            &tiny_png_bytes(),
        )
        .await;

        let row = media_row(&pool, uploaded).await;
        assert_ne!(row.id, canonical_id);
        assert_eq!(row.canonical_media_id, None);
        assert_ne!(row.stored_path, missing.to_string_lossy());
        assert!(Path::new(&row.stored_path).exists());
    }

    #[tokio::test]
    async fn misleading_canonical_extension_disables_duplicate_reuse_safely() {
        let (_temp, paths, pool, settings, ffmpeg, user_id) = media_fixture().await;
        let raw = tiny_png_bytes();
        let raw_hash = sha256_hex(&raw);
        let misleading = paths.uploads_originals.join("photo.mp4");
        tokio::fs::write(&misleading, &raw)
            .await
            .expect("misleading canonical file");
        let canonical_id = insert_canonical_media(
            &pool,
            user_id,
            TestCanonicalMedia {
                stored_path: &misleading,
                public_path: "/uploads/originals/photo.mp4",
                mime_type: "image/png",
                media_kind: "image",
                original_sha256: &raw_hash,
                normalized_sha256: &raw_hash,
            },
        )
        .await;

        let uploaded = save_test_upload(
            &pool,
            &settings,
            &paths,
            &ffmpeg,
            user_id,
            "photo.png",
            &raw,
        )
        .await;

        let row = media_row(&pool, uploaded).await;
        assert_ne!(row.id, canonical_id);
        assert_eq!(row.canonical_media_id, None);
        assert_ne!(row.stored_path, misleading.to_string_lossy());
        assert!(Path::new(&row.stored_path).exists());
    }

    #[tokio::test]
    async fn deleting_one_shared_media_row_keeps_canonical_file_until_last_reference() {
        let (_temp, paths, pool, _settings, _ffmpeg, user_id) = media_fixture().await;
        let shared = paths.uploads_images.join("shared.webp");
        tokio::fs::write(&shared, b"shared").await.expect("shared");
        let shared_hash = sha256_hex(b"shared");
        let first = insert_canonical_media(
            &pool,
            user_id,
            TestCanonicalMedia {
                stored_path: &shared,
                public_path: "/uploads/images/shared.webp",
                mime_type: "image/webp",
                media_kind: "image",
                original_sha256: "first",
                normalized_sha256: &shared_hash,
            },
        )
        .await;
        let second = insert_duplicate_row(&pool, user_id, first, &shared).await;

        delete_media(&pool, second).await.expect("delete duplicate");
        assert!(shared.exists());
        delete_media(&pool, first).await.expect("delete canonical");
        assert!(!shared.exists());
    }

    #[tokio::test]
    async fn deleting_canonical_media_promotes_remaining_shared_reference() {
        let (_temp, paths, pool, _settings, _ffmpeg, user_id) = media_fixture().await;
        let shared = paths.uploads_images.join("promoted.webp");
        tokio::fs::write(&shared, b"shared").await.expect("shared");
        let shared_hash = sha256_hex(b"shared");
        let first = insert_canonical_media(
            &pool,
            user_id,
            TestCanonicalMedia {
                stored_path: &shared,
                public_path: "/uploads/images/promoted.webp",
                mime_type: "image/webp",
                media_kind: "image",
                original_sha256: "first",
                normalized_sha256: &shared_hash,
            },
        )
        .await;
        let second = insert_duplicate_row(&pool, user_id, first, &shared).await;

        delete_media(&pool, first).await.expect("delete canonical");

        assert!(shared.exists());
        let promoted = media_row(&pool, second).await;
        assert_eq!(promoted.canonical_media_id, None);
        assert_eq!(promoted.stored_path, shared.to_string_lossy());
    }

    #[tokio::test]
    async fn different_files_with_same_name_do_not_collide() {
        let (_temp, paths, pool, settings, ffmpeg, user_id) = media_fixture().await;
        let mut other_png = tiny_png_bytes();
        other_png.push(0);

        let first = save_test_upload(
            &pool,
            &settings,
            &paths,
            &ffmpeg,
            user_id,
            "same.png",
            &tiny_png_bytes(),
        )
        .await;
        let second = save_test_upload(
            &pool, &settings, &paths, &ffmpeg, user_id, "same.png", &other_png,
        )
        .await;

        let first = media_row(&pool, first).await;
        let second = media_row(&pool, second).await;
        assert_ne!(first.stored_path, second.stored_path);
        assert_ne!(first.public_path, second.public_path);
        assert!(Path::new(&first.stored_path).exists());
        assert!(Path::new(&second.stored_path).exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn different_media_types_never_dedupe_into_each_other() {
        let (temp, paths, pool, mut settings, mut ffmpeg, user_id) = media_fixture().await;
        let normalized = b"same-normalized-output";
        let normalized_hash = sha256_hex(normalized);
        let image_path = paths.uploads_images.join("image.webp");
        tokio::fs::write(&image_path, normalized)
            .await
            .expect("image");
        insert_canonical_media(
            &pool,
            user_id,
            TestCanonicalMedia {
                stored_path: &image_path,
                public_path: "/uploads/images/image.webp",
                mime_type: "image/webp",
                media_kind: "image",
                original_sha256: "seed-image",
                normalized_sha256: &normalized_hash,
            },
        )
        .await;
        settings.media.ffmpeg_path = fake_ffmpeg_output(&temp, normalized).display().to_string();
        ffmpeg.available = true;
        ffmpeg.supports_vp9 = true;

        let video = save_test_upload(
            &pool,
            &settings,
            &paths,
            &ffmpeg,
            user_id,
            "clip.mp4",
            &tiny_mp4_bytes(),
        )
        .await;

        let row = media_row(&pool, video).await;
        assert_eq!(row.media_kind, "video");
        assert_eq!(row.canonical_media_id, None);
        assert_ne!(row.stored_path, image_path.to_string_lossy());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn original_and_transcoded_variants_share_stable_basename() {
        let (temp, paths, pool, mut settings, mut ffmpeg, user_id) = media_fixture().await;
        settings.media.keep_original_uploads = true;
        settings.media.ffmpeg_path = fake_ffmpeg_output(&temp, b"webp-output")
            .display()
            .to_string();
        ffmpeg.available = true;
        ffmpeg.supports_webp = true;
        ffmpeg.supports_vp9 = true;

        let media_id = save_test_upload(
            &pool,
            &settings,
            &paths,
            &ffmpeg,
            user_id,
            "My Photo.PNG",
            &tiny_png_bytes(),
        )
        .await;

        let row = media_row(&pool, media_id).await;
        let original_path = row.original_path.expect("original path");
        let original_stem = Path::new(&original_path)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .expect("original stem");
        let stored_stem = Path::new(&row.stored_path)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .expect("stored stem");
        assert_eq!(original_stem, stored_stem);
        assert!(original_stem.starts_with("my-photo-"));
        assert!(row.public_path.ends_with(&format!("{stored_stem}.webp")));
        assert!(matches!(
            row.original_public_path.as_deref(),
            Some(path) if path.ends_with(&format!("{original_stem}.png"))
        ));

        let video_id = save_test_upload(
            &pool,
            &settings,
            &paths,
            &ffmpeg,
            user_id,
            "My Clip.MP4",
            &tiny_mp4_bytes(),
        )
        .await;
        let video = media_row(&pool, video_id).await;
        let original_path = video.original_path.expect("video original path");
        let original_stem = Path::new(&original_path)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .expect("video original stem");
        let stored_stem = Path::new(&video.stored_path)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .expect("video stored stem");
        assert_eq!(original_stem, stored_stem);
        assert!(original_stem.starts_with("my-clip-"));
        assert!(video.public_path.ends_with(&format!("{stored_stem}.webm")));
        assert!(matches!(
            video.original_public_path.as_deref(),
            Some(path) if path.ends_with(&format!("{original_stem}.mp4"))
        ));
    }

    #[tokio::test]
    async fn profile_media_replacement_deletes_previous_file() {
        let temp = tempfile::tempdir().expect("temp dir");
        let pool = crate::db::connect(&temp.path().join("test.sqlite3"))
            .await
            .expect("connect");
        crate::db::migrate(&pool).await.expect("migrate");
        let settings = Settings::default();
        let user_id =
            crate::auth::register_user(&pool, &settings, "alice", "very secure password", false)
                .await
                .expect("user");
        let first = temp.path().join("first.webp");
        let first_thumb = temp.path().join("first-thumb.webp");
        let second = temp.path().join("second.webp");
        tokio::fs::write(&first, b"first").await.expect("first");
        tokio::fs::write(&first_thumb, b"first-thumb")
            .await
            .expect("first thumb");
        tokio::fs::write(&second, b"second").await.expect("second");
        let first_id = insert_test_media(&pool, user_id, &first, "image").await;
        let second_id = insert_test_media(&pool, user_id, &second, "image").await;
        set_test_thumbnail(&pool, first_id, &first_thumb).await;

        set_profile_media(&pool, user_id, ProfileMediaSlot::Picture, first_id)
            .await
            .expect("set first");
        set_profile_media(&pool, user_id, ProfileMediaSlot::Picture, second_id)
            .await
            .expect("set second");

        assert!(!first.exists());
        assert!(!first_thumb.exists());
        assert!(second.exists());
    }

    #[tokio::test]
    async fn profile_media_rejects_non_image_and_removes_upload() {
        let temp = tempfile::tempdir().expect("temp dir");
        let pool = crate::db::connect(&temp.path().join("test.sqlite3"))
            .await
            .expect("connect");
        crate::db::migrate(&pool).await.expect("migrate");
        let settings = Settings::default();
        let user_id =
            crate::auth::register_user(&pool, &settings, "alice", "very secure password", false)
                .await
                .expect("user");
        let video = temp.path().join("video.webm");
        tokio::fs::write(&video, b"video").await.expect("video");
        let media_id = insert_test_media(&pool, user_id, &video, "video").await;

        assert!(
            set_profile_media(&pool, user_id, ProfileMediaSlot::Picture, media_id)
                .await
                .is_err()
        );
        assert!(!video.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn profile_picture_thumbnail_generation_creates_webp_metadata() {
        let temp = tempfile::tempdir().expect("temp dir");
        let paths = RuntimePaths::from_data_dir(temp.path().join("data"));
        paths.ensure().expect("paths");
        let pool = crate::db::connect(&paths.database_path)
            .await
            .expect("connect");
        crate::db::migrate(&pool).await.expect("migrate");
        let mut settings = Settings::default();
        settings.media.ffmpeg_path = fake_ffmpeg(&temp).display().to_string();
        let ffmpeg = FfmpegStatus {
            available: true,
            version: "fake ffmpeg".to_owned(),
            supports_webp: true,
            supports_vp9: false,
            error: None,
        };
        let user_id =
            crate::auth::register_user(&pool, &settings, "alice", "very secure password", false)
                .await
                .expect("user");
        let source = paths.uploads_images.join("source.webp");
        tokio::fs::write(&source, b"source").await.expect("source");
        let media_id = insert_test_media(&pool, user_id, &source, "image").await;

        generate_profile_picture_thumbnail(&pool, &settings, &paths, &ffmpeg, media_id)
            .await
            .expect("thumbnail");

        let (thumbnail_path, thumbnail_public_path): (Option<String>, Option<String>) = pool
            .call(move |conn| {
                conn.query_row(
                    "SELECT thumbnail_path, thumbnail_public_path FROM media WHERE id = ?",
                    [media_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(Into::into)
            })
            .await
            .expect("thumbnail row");
        assert_eq!(
            thumbnail_public_path.as_deref(),
            Some("/uploads/thumbs/1-profile.webp")
        );
        let thumbnail_path = thumbnail_path.expect("thumbnail path");
        assert!(Path::new(&thumbnail_path).exists());
    }

    #[tokio::test]
    async fn profile_picture_thumbnail_generation_is_optional() {
        let temp = tempfile::tempdir().expect("temp dir");
        let paths = RuntimePaths::from_data_dir(temp.path().join("data"));
        paths.ensure().expect("paths");
        let pool = crate::db::connect(&paths.database_path)
            .await
            .expect("connect");
        crate::db::migrate(&pool).await.expect("migrate");
        let settings = Settings::default();
        let ffmpeg = FfmpegStatus {
            available: false,
            version: String::new(),
            supports_webp: false,
            supports_vp9: false,
            error: Some("ffmpeg command not found".to_owned()),
        };
        let user_id =
            crate::auth::register_user(&pool, &settings, "alice", "very secure password", false)
                .await
                .expect("user");
        let source = paths.uploads_originals.join("source.png");
        tokio::fs::write(&source, b"source").await.expect("source");
        let media_id = insert_test_media(&pool, user_id, &source, "image").await;

        generate_profile_picture_thumbnail(&pool, &settings, &paths, &ffmpeg, media_id)
            .await
            .expect("thumbnail fallback");

        let thumbnail_public_path: Option<String> = pool
            .call(move |conn| {
                conn.query_row(
                    "SELECT thumbnail_public_path FROM media WHERE id = ?",
                    [media_id],
                    |row| row.get(0),
                )
                .map_err(Into::into)
            })
            .await
            .expect("thumbnail row");
        assert!(thumbnail_public_path.is_none());
    }

    #[derive(Debug)]
    struct MediaRow {
        id: i64,
        original_path: Option<String>,
        original_public_path: Option<String>,
        stored_path: String,
        public_path: String,
        media_kind: String,
        original_sha256: String,
        normalized_sha256: Option<String>,
        canonical_media_id: Option<i64>,
    }

    async fn media_fixture() -> (
        tempfile::TempDir,
        RuntimePaths,
        SqlitePool,
        Settings,
        FfmpegStatus,
        i64,
    ) {
        let temp = tempfile::tempdir().expect("temp dir");
        let paths = RuntimePaths::from_data_dir(temp.path().join("data"));
        paths.ensure().expect("paths");
        let pool = crate::db::connect(&paths.database_path)
            .await
            .expect("connect");
        crate::db::migrate(&pool).await.expect("migrate");
        let settings = Settings::default();
        let ffmpeg = FfmpegStatus {
            available: false,
            version: String::new(),
            supports_webp: false,
            supports_vp9: false,
            error: Some("disabled in tests".to_owned()),
        };
        let user_id =
            crate::auth::register_user(&pool, &settings, "alice", "very secure password", false)
                .await
                .expect("user");
        (temp, paths, pool, settings, ffmpeg, user_id)
    }

    async fn save_test_upload(
        pool: &SqlitePool,
        settings: &Settings,
        paths: &RuntimePaths,
        ffmpeg: &FfmpegStatus,
        user_id: i64,
        filename: &str,
        bytes: &[u8],
    ) -> i64 {
        let staging = paths.staged_upload_path(&Uuid::new_v4().simple().to_string());
        tokio::fs::write(&staging, bytes).await.expect("staging");
        let context = UploadContext {
            pool,
            settings,
            paths,
            ffmpeg,
        };
        save_staged_upload(
            &context,
            StagedUpload {
                owner_user_id: Some(user_id),
                original_filename: filename.to_owned(),
                staging,
                bytes: u64::try_from(bytes.len()).expect("byte len"),
            },
        )
        .await
        .expect("save upload")
    }

    async fn media_row(pool: &SqlitePool, media_id: i64) -> MediaRow {
        pool.call(move |conn| {
            conn.query_row(
                "SELECT id, original_path, original_public_path, stored_path, public_path, media_kind, original_sha256, normalized_sha256, canonical_media_id FROM media WHERE id = ?",
                [media_id],
                |row| {
                    Ok(MediaRow {
                        id: row.get(0)?,
                        original_path: row.get(1)?,
                        original_public_path: row.get(2)?,
                        stored_path: row.get(3)?,
                        public_path: row.get(4)?,
                        media_kind: row.get(5)?,
                        original_sha256: row.get(6)?,
                        normalized_sha256: row.get(7)?,
                        canonical_media_id: row.get(8)?,
                    })
                },
            )
            .map_err(Into::into)
        })
        .await
        .expect("media row")
    }

    struct TestCanonicalMedia<'a> {
        stored_path: &'a Path,
        public_path: &'a str,
        mime_type: &'a str,
        media_kind: &'a str,
        original_sha256: &'a str,
        normalized_sha256: &'a str,
    }

    async fn insert_canonical_media(
        pool: &SqlitePool,
        user_id: i64,
        media: TestCanonicalMedia<'_>,
    ) -> i64 {
        let stored_path = media.stored_path.to_string_lossy().to_string();
        let public_path = media.public_path.to_owned();
        let mime_type = media.mime_type.to_owned();
        let media_kind = media.media_kind.to_owned();
        let original_sha256 = media.original_sha256.to_owned();
        let normalized_sha256 = media.normalized_sha256.to_owned();
        pool.call(move |conn| {
            conn.execute(
                "INSERT INTO media (owner_user_id, original_filename, stored_path, public_path, mime_type, media_kind, byte_len, original_sha256, normalized_sha256) VALUES (?, 'canonical', ?, ?, ?, ?, 1, ?, ?)",
                params![
                    user_id,
                    stored_path,
                    public_path,
                    mime_type,
                    media_kind,
                    original_sha256,
                    normalized_sha256,
                ],
            )?;
            Ok(conn.last_insert_rowid())
        })
        .await
        .expect("canonical media")
    }

    async fn insert_duplicate_row(
        pool: &SqlitePool,
        user_id: i64,
        canonical_id: i64,
        stored_path: &Path,
    ) -> i64 {
        let stored_path = stored_path.to_string_lossy().to_string();
        pool.call(move |conn| {
            conn.execute(
                "INSERT INTO media (owner_user_id, original_filename, stored_path, public_path, mime_type, media_kind, byte_len, canonical_media_id) VALUES (?, 'duplicate', ?, '/uploads/images/shared.webp', 'image/webp', 'image', 1, ?)",
                params![user_id, stored_path, canonical_id],
            )?;
            Ok(conn.last_insert_rowid())
        })
        .await
        .expect("duplicate media")
    }

    fn tiny_png_bytes() -> Vec<u8> {
        vec![
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9c, 0x63, 0xf8, 0xcf, 0xc0, 0x00, 0x00, 0x04, 0x00, 0x01, 0xfe, 0xa7, 0x69, 0x9d,
            0x16, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ]
    }

    fn tiny_jpeg_bytes() -> Vec<u8> {
        vec![
            0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01, 0x01, 0x00,
            0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0xff, 0xd9,
        ]
    }

    fn tiny_mp4_bytes() -> Vec<u8> {
        let mut bytes = vec![
            0x00, 0x00, 0x00, 0x18, b'f', b't', b'y', b'p', b'i', b's', b'o', b'm', 0x00, 0x00,
            0x00, 0x00, b'i', b's', b'o', b'm', b'm', b'p', b'4', b'2',
        ];
        bytes.extend_from_slice(b"rustpost-test-video");
        bytes
    }

    async fn insert_test_media(
        pool: &SqlitePool,
        user_id: i64,
        stored_path: &Path,
        media_kind: &str,
    ) -> i64 {
        let stored_path = stored_path.to_string_lossy().to_string();
        let media_kind = media_kind.to_owned();
        pool.call(move |conn| {
            conn.execute(
                "INSERT INTO media (owner_user_id, original_filename, stored_path, public_path, mime_type, media_kind, byte_len) VALUES (?, 'x', ?, '/uploads/images/x.webp', 'image/webp', ?, 1)",
                params![user_id, stored_path, media_kind],
            )?;
            Ok(conn.last_insert_rowid())
        })
        .await
        .expect("media")
    }

    async fn set_test_thumbnail(pool: &SqlitePool, media_id: i64, thumbnail_path: &Path) {
        let thumbnail_path = thumbnail_path.to_string_lossy().to_string();
        pool.call(move |conn| {
            conn.execute(
                "UPDATE media SET thumbnail_path = ?, thumbnail_public_path = '/uploads/thumbs/x.webp' WHERE id = ?",
                params![thumbnail_path, media_id],
            )?;
            Ok(())
        })
        .await
        .expect("thumbnail");
    }

    #[cfg(unix)]
    fn fake_ffmpeg(temp: &tempfile::TempDir) -> PathBuf {
        fake_ffmpeg_output(temp, b"webp")
    }

    #[cfg(unix)]
    fn fake_ffmpeg_output(temp: &tempfile::TempDir, output: &[u8]) -> PathBuf {
        let path = temp.path().join("fake-ffmpeg");
        let output = std::str::from_utf8(output).expect("fake output utf8");
        assert!(!output.contains('\''));
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nlast=\"\"\nfor arg do\n  last=\"$arg\"\ndone\nprintf '%s' '{output}' > \"$last\"\n"
            ),
        )
        .expect("fake ffmpeg");
        let mut permissions = std::fs::metadata(&path)
            .expect("fake ffmpeg metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).expect("fake ffmpeg permissions");
        path
    }
}
