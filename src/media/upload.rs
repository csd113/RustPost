//! Upload for the media subsystem.

use super::*;

static UPLOADS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(16);

pub async fn save_upload(
    pool: &SqlitePool,
    settings: &Settings,
    paths: &RuntimePaths,
    ffmpeg: &FfmpegStatus,
    owner_user_id: Option<i64>,
    field: Field<'_>,
) -> anyhow::Result<i64> {
    Ok(
        save_upload_inner(pool, settings, paths, ffmpeg, owner_user_id, field, None)
            .await?
            .accept(),
    )
}

pub(super) async fn save_upload_inner(
    pool: &SqlitePool,
    settings: &Settings,
    paths: &RuntimePaths,
    ffmpeg: &FfmpegStatus,
    owner_user_id: Option<i64>,
    mut field: Field<'_>,
    required_image_label: Option<&'static str>,
) -> anyhow::Result<PendingMedia> {
    let _temp_operation = crate::runtime::begin_temp_operation();
    let _permit = tokio::time::timeout(std::time::Duration::from_secs(30), UPLOADS.acquire())
        .await
        .map_err(|_| anyhow::anyhow!("uploads are busy; try again shortly"))??;
    let original_filename = field.file_name().unwrap_or("upload").to_owned();
    reject_path_tricks(&original_filename)?;
    let id = Uuid::new_v4().simple().to_string();
    let staging_path = paths.staged_upload_path(&id);
    // Reserve synchronously before arming cleanup: cancellation cannot race a
    // detached create operation or remove a file that this request never owned.
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staging_path)?;
    let staging = StagedUploadGuard::new(staging_path);
    let reservation =
        crate::storage_limits::Reservation::new(pool, settings, paths, owner_user_id).await?;
    let bytes =
        write_upload_to_staging(settings, staging.path(), &mut field, file, &reservation).await?;
    let context = UploadContext {
        pool,
        settings,
        paths,
        ffmpeg,
        background: required_image_label.is_none(),
    };
    save_staged_upload(
        &context,
        StagedUpload {
            reservation,
            owner_user_id,
            original_filename,
            staging,
            bytes,
        },
        required_image_label,
    )
    .await
}

pub(super) async fn save_staged_upload(
    context: &UploadContext<'_>,
    upload: StagedUpload,
    required_image_label: Option<&'static str>,
) -> anyhow::Result<PendingMedia> {
    save_staged_upload_inner(context, upload, required_image_label).await
}

pub(super) async fn save_staged_upload_inner(
    context: &UploadContext<'_>,
    upload: StagedUpload,
    required_image_label: Option<&'static str>,
) -> anyhow::Result<PendingMedia> {
    let mut prepared = prepare_staged_upload(context.settings, upload).await?;
    if let Some(label) = required_image_label
        && prepared.media_kind != MediaKind::Image
    {
        anyhow::bail!("{label} must be an image");
    }
    if let Some(media_id) = try_insert_duplicate(
        context,
        &prepared,
        MediaHashKind::Original,
        &prepared.original_sha256,
    )
    .await?
    {
        prepared.staging.remove().await;
        return Ok(media_id);
    }
    store_new_upload(context, prepared).await
}

pub(super) async fn prepare_staged_upload(
    settings: &Settings,
    upload: StagedUpload,
) -> anyhow::Result<PreparedUpload> {
    let (kind, original_sha256, prefix) = sniff_and_hash(upload.staging.path()).await?;
    let Some(kind) = kind else {
        anyhow::bail!("unsupported media type");
    };
    let mime = kind.mime_type().to_owned();
    let media_kind = classify(settings, &mime, upload.bytes)?;
    if media_kind == MediaKind::Image
        && let Some((width, height)) = image_dimensions(&prefix)
    {
        let pixels = u64::from(width).saturating_mul(u64::from(height));
        if pixels > MAX_IMAGE_PIXELS {
            anyhow::bail!("image dimensions exceed the maximum supported size");
        }
    }
    Ok(PreparedUpload {
        reservation: upload.reservation,
        owner_user_id: upload.owner_user_id,
        original_filename: upload.original_filename,
        staging: upload.staging,
        bytes: upload.bytes,
        mime,
        media_kind,
        original_sha256,
    })
}

pub(super) async fn store_new_upload(
    context: &UploadContext<'_>,
    mut upload: PreparedUpload,
) -> anyhow::Result<PendingMedia> {
    let ext = safe_extension(&upload.mime, upload.media_kind);
    let mut basename = stable_media_basename(&upload.original_filename, &upload.original_sha256);
    let original_path = unique_media_path(&context.paths.uploads_originals, &basename, ext)?;
    let original_cleanup = StagedUploadGuard::new(original_path.clone());
    if let Some(stem) = original_path.file_stem().and_then(|stem| stem.to_str()) {
        stem.clone_into(&mut basename);
    }
    let original_public_path = public_upload_path(context.paths, &original_path)?;
    // Rename is a short metadata operation. Keep it synchronous so cancellation
    // cannot race a detached Tokio filesystem operation with guard cleanup.
    std::fs::rename(upload.staging.path(), &original_path)?;
    upload.staging.disarm();
    let stored = initial_variant(context, &upload, &original_path, &basename).await;
    let normalized_sha256 = match hash_file(&stored.path).await {
        Ok(hash) => hash,
        Err(error) => {
            cleanup_unreferenced_uploads(context.pool, [&original_path, &stored.path]).await;
            return Err(error);
        }
    };
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
        cleanup_unreferenced_uploads(context.pool, [&original_path, &stored.path]).await;
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
        reservation_id: upload.reservation.id().to_owned(),
        cleanup: vec![original_cleanup, stored.cleanup],
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
    let media_id = match insert_new_media_record(context.pool, context.paths, record).await {
        Ok(media_id) => media_id,
        Err(error) => {
            if let Some(media_id) =
                try_insert_duplicate_after_canonical_conflict(context, &upload, &normalized_sha256)
                    .await?
            {
                cleanup_unreferenced_uploads(context.pool, [&cleanup_paths[0], &cleanup_paths[1]])
                    .await;
                return Ok(media_id);
            }
            cleanup_unreferenced_uploads(context.pool, [&cleanup_paths[0], &cleanup_paths[1]])
                .await;
            return Err(error);
        }
    };
    Ok(media_id)
}

pub(super) async fn insert_new_media_record(
    pool: &SqlitePool,
    paths: &RuntimePaths,
    mut record: NewMediaRecord,
) -> anyhow::Result<PendingMedia> {
    let mut cleanup = PendingUploads::new(pool, paths);
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
        tx.execute("DELETE FROM storage_reservations WHERE id=?",[&record.reservation_id])?;
        record_media_job_in_tx(
            &tx,
            media_id,
            &record.conversion_state,
            &record.ffmpeg_stderr,
        )?;
        tx.commit()?;
        // The DB worker owns cleanup through commit, even if the HTTP future
        // is cancelled while waiting for this result.
        for guard in &mut record.cleanup { guard.disarm(); }
        cleanup.track(media_id);
        Ok(PendingMedia { id: media_id, cleanup })
    })
    .await
}

pub(super) fn record_media_job_in_tx(
    tx: &rusqlite::Transaction<'_>,
    media_id: i64,
    state: &str,
    stderr: &str,
) -> anyhow::Result<()> {
    if state == "queued" {
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM media_work WHERE status IN ('queued','running')",
            [],
            |r| r.get(0),
        )?;
        anyhow::ensure!(
            count < jobs::QUEUE_LIMIT,
            "media processing queue is full; try again later"
        );
        tx.execute(
            "INSERT INTO media_work(media_id,status) VALUES(?,'queued')",
            [media_id],
        )?;
        tx.execute(
            "INSERT INTO media_jobs(media_id,status) VALUES(?,'queued')",
            [media_id],
        )?;
        return Ok(());
    }
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

pub(super) async fn write_upload_to_staging(
    settings: &Settings,
    staging: &Path,
    field: &mut Field<'_>,
    file: std::fs::File,
    reservation: &crate::storage_limits::Reservation,
) -> anyhow::Result<u64> {
    let result = tokio::time::timeout(
        std::time::Duration::from_hours(1),
        write_upload_to_staging_inner(settings, file, field, reservation),
    )
    .await
    .map_err(|_| anyhow::anyhow!("upload timed out"))?;
    if result.is_err() {
        remove_staged_upload(staging).await;
    }
    result
}

pub(super) async fn write_upload_to_staging_inner(
    settings: &Settings,
    file: std::fs::File,
    field: &mut Field<'_>,
    reservation: &crate::storage_limits::Reservation,
) -> anyhow::Result<u64> {
    let mut file = tokio::fs::File::from_std(file);
    let mut bytes = 0_u64;
    while let Some(chunk) = tokio::time::timeout(std::time::Duration::from_secs(120), field.chunk())
        .await
        .map_err(|_| anyhow::anyhow!("upload stalled; try again"))??
    {
        let previous_bytes = bytes;
        bytes = bytes
            .checked_add(u64::try_from(chunk.len())?)
            .ok_or_else(|| anyhow::anyhow!("upload size overflow"))?;
        if bytes
            > settings
                .media
                .max_image_size
                .max(settings.media.max_video_size)
        {
            anyhow::bail!("upload exceeds maximum size");
        }
        reservation
            .grow(
                bytes,
                previous_bytes == 0
                    || bytes / (8 * 1024 * 1024) != previous_bytes / (8 * 1024 * 1024),
            )
            .await?;
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    Ok(bytes)
}

pub(super) async fn remove_staged_upload(path: &Path) -> bool {
    match tokio::fs::remove_file(path).await {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(error) => {
            tracing::debug!(error = %error, "failed to remove rejected staged upload");
            false
        }
    }
}

/// Removes upload scratch files that are not referenced by any `media` row.
///
/// The reference check protects a concurrently committed canonical row from
/// having its file removed by a duplicate-upload cleanup path.
pub(super) async fn cleanup_unreferenced_uploads<const N: usize>(
    pool: &SqlitePool,
    paths: [&Path; N],
) {
    let unique_paths = paths.into_iter().collect::<BTreeSet<_>>();
    for path in unique_paths {
        let path_string = path.to_string_lossy().to_string();
        let referenced = pool
            .call(move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT 1 FROM media WHERE original_path = ? OR stored_path = ? OR thumbnail_path = ? LIMIT 1",
                        params![path_string, path_string, path_string],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some())
            })
            .await
            .unwrap_or(true);
        if referenced {
            continue;
        }
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

async fn initial_variant(
    context: &UploadContext<'_>,
    upload: &PreparedUpload,
    original: &Path,
    basename: &str,
) -> StoredVariant {
    if context.background
        && jobs::conversion_requested(context.settings, context.ffmpeg, upload.media_kind)
    {
        convert::original_fallback(
            original,
            context.paths,
            upload.media_kind,
            &upload.mime,
            "queued",
            "",
        )
    } else {
        convert_or_original(
            context.settings,
            context.paths,
            context.ffmpeg,
            original,
            basename,
            upload.media_kind,
            &upload.mime,
        )
        .await
    }
}
