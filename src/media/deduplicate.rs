//! Deduplicate for the media subsystem.

use super::*;

pub(super) async fn try_insert_duplicate(
    context: &UploadContext<'_>,
    upload: &PreparedUpload,
    hash_kind: MediaHashKind,
    hash: &str,
) -> anyhow::Result<Option<PendingMedia>> {
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
        return insert_duplicate_media(context, upload, canonical)
            .await
            .map(Some);
    }
    Ok(None)
}

pub(super) async fn try_insert_duplicate_after_canonical_conflict(
    context: &UploadContext<'_>,
    upload: &PreparedUpload,
    normalized_sha256: &str,
) -> anyhow::Result<Option<PendingMedia>> {
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

pub(super) async fn insert_duplicate_media(
    context: &UploadContext<'_>,
    upload: &PreparedUpload,
    canonical: StoredMedia,
) -> anyhow::Result<PendingMedia> {
    let pool = context.pool;
    let paths = context.paths;
    let owner_user_id = upload.owner_user_id;
    let original_filename = upload.original_filename.clone();
    let bytes = upload.bytes;
    let original_sha256 = upload.original_sha256.clone();
    let reservation_id = upload.reservation.id().to_owned();
    let mut cleanup = PendingUploads::new(pool, paths);
    let byte_len = i64::try_from(bytes)?;
    let normalized_sha256 = canonical
        .normalized_sha256
        .clone()
        .filter(|hash| !hash.is_empty());
    pool.call(move |conn| {
        let tx=conn.transaction()?;
        tx.execute(
            "INSERT INTO media (owner_user_id, original_filename, stored_path, public_path, mime_type, media_kind, byte_len, conversion_state, original_sha256, normalized_sha256, canonical_media_id, is_nsfw) VALUES (?, ?, ?, ?, ?, ?, ?, COALESCE((SELECT CASE status WHEN 'running' THEN 'processing' WHEN 'queued' THEN 'queued' WHEN 'failed' THEN 'failed' ELSE 'duplicate' END FROM media_work WHERE media_id=?),'duplicate'), ?, ?, ?, ?)",
            params![
                owner_user_id,
                original_filename,
                canonical.stored_path,
                canonical.public_path,
                canonical.mime_type,
                canonical.media_kind,
                byte_len,
                canonical.id,
                original_sha256,
                normalized_sha256,
                canonical.id,
                i64::from(canonical.is_nsfw),
            ],
        )?;
        let id = tx.last_insert_rowid();
        tx.execute("DELETE FROM storage_reservations WHERE id=?",[reservation_id])?;
        tx.commit()?;
        cleanup.track(id);
        Ok(PendingMedia { id, cleanup })
    })
    .await
}

pub(super) async fn find_duplicate_media(
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
                    "SELECT id, stored_path, public_path, mime_type, media_kind, original_sha256, normalized_sha256, is_nsfw FROM media WHERE canonical_media_id IS NULL AND media_kind = ? AND original_sha256 = ? ORDER BY id ASC LIMIT 8"
                }
                MediaHashKind::Normalized => {
                    "SELECT id, stored_path, public_path, mime_type, media_kind, original_sha256, normalized_sha256, is_nsfw FROM media WHERE canonical_media_id IS NULL AND media_kind = ? AND normalized_sha256 = ? ORDER BY id ASC LIMIT 8"
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
                        is_nsfw: row.get::<_, i64>(7)? != 0,
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

pub(super) async fn quarantine_invalid_canonical_media(
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

pub(super) async fn validate_canonical_media(
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

pub(super) async fn stored_file_is_safe(
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

pub(super) fn path_extension_matches_mime(
    path: &Path,
    media_kind: MediaKind,
    mime_type: &str,
) -> bool {
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

pub(super) fn mime_matches_kind(mime_type: &str, media_kind: MediaKind) -> bool {
    match media_kind {
        MediaKind::Image => mime_type.starts_with("image/"),
        MediaKind::Video => mime_type.starts_with("video/"),
    }
}
