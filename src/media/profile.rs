//! Profile for the media subsystem.

use super::*;

pub async fn save_banner_upload(
    pool: &SqlitePool,
    settings: &Settings,
    paths: &RuntimePaths,
    ffmpeg: &FfmpegStatus,
    owner_user_id: i64,
    field: Field<'_>,
) -> anyhow::Result<i64> {
    Ok(save_upload_inner(
        pool,
        settings,
        paths,
        ffmpeg,
        Some(owner_user_id),
        field,
        Some("profile banner"),
    )
    .await?
    .accept())
}

pub async fn save_profile_picture_upload(
    pool: &SqlitePool,
    settings: &Settings,
    paths: &RuntimePaths,
    ffmpeg: &FfmpegStatus,
    owner_user_id: i64,
    field: Field<'_>,
) -> anyhow::Result<i64> {
    let pending_media = save_upload_inner(
        pool,
        settings,
        paths,
        ffmpeg,
        Some(owner_user_id),
        field,
        Some("profile picture"),
    )
    .await?;
    let media_id = pending_media.id;
    if let Err(error) =
        generate_profile_picture_thumbnail(pool, settings, paths, ffmpeg, media_id).await
    {
        tracing::warn!(media_id, error = %error, "thumbnail unavailable; retaining uploaded profile image");
    }
    Ok(pending_media.accept())
}

pub(super) async fn generate_profile_picture_thumbnail(
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
    let mut cleanup = StagedUploadGuard::new(output.clone());
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
                let changed = conn.execute(
                    "UPDATE media SET thumbnail_path = ?, thumbnail_public_path = ? WHERE id = ?",
                    params![thumbnail_path, thumbnail_public_path, media_id],
                )?;
                anyhow::ensure!(changed == 1, "profile image no longer exists");
                cleanup.disarm();
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

#[derive(Debug, Clone, Copy)]
pub enum ProfileMediaChange {
    Keep,
    Clear,
    Set(i64),
}

/// Apply owned-image references in the caller's profile transaction.
pub fn apply_profile_media_changes(
    tx: &rusqlite::Transaction<'_>,
    user_id: i64,
    changes: &[(ProfileMediaSlot, ProfileMediaChange)],
) -> anyhow::Result<Vec<i64>> {
    let mut previous_ids = Vec::new();
    for &(slot, change) in changes {
        let target = match change {
            ProfileMediaChange::Keep => continue,
            ProfileMediaChange::Clear => None,
            ProfileMediaChange::Set(id) => {
                let valid: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM media WHERE id = ? AND owner_user_id = ? AND media_kind = 'image')", params![id, user_id], |row| row.get(0))?;
                anyhow::ensure!(valid, "profile media must be an owned image");
                Some(id)
            }
        };
        let previous: Option<i64> = tx.query_row(
            &format!(
                "SELECT {} FROM users WHERE id = ? AND is_deleted = 0",
                slot.column()
            ),
            [user_id],
            |row| row.get(0),
        )?;
        tx.execute(&format!("UPDATE users SET {} = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ? AND is_deleted = 0", slot.column()), params![target, user_id])?;
        if let Some(previous) = previous.filter(|previous| Some(*previous) != target) {
            previous_ids.push(previous);
        }
    }
    Ok(previous_ids)
}

/// Post-commit cleanup cannot turn a saved profile into a reported rejection.
pub async fn cleanup_replaced_profile_media(
    pool: &SqlitePool,
    paths: &RuntimePaths,
    mut ids: Vec<i64>,
) {
    ids.sort_unstable();
    ids.dedup();
    for id in ids {
        if let Err(error) = delete_media(pool, paths, id).await {
            tracing::debug!(media_id = id, error = %error, "retained referenced or inaccessible replaced profile media");
        }
    }
}

pub async fn set_profile_media(
    pool: &SqlitePool,
    paths: &RuntimePaths,
    user_id: i64,
    slot: ProfileMediaSlot,
    media_id: i64,
) -> anyhow::Result<()> {
    let previous = pool
        .call(move |conn| {
            let tx = conn.transaction()?;
            let previous = apply_profile_media_changes(
                &tx,
                user_id,
                &[(slot, ProfileMediaChange::Set(media_id))],
            )?;
            tx.commit()?;
            Ok(previous)
        })
        .await?;
    cleanup_replaced_profile_media(pool, paths, previous).await;
    Ok(())
}

pub async fn clear_profile_media(
    pool: &SqlitePool,
    paths: &RuntimePaths,
    user_id: i64,
    slot: ProfileMediaSlot,
) -> anyhow::Result<()> {
    let previous = pool
        .call(move |conn| {
            let tx = conn.transaction()?;
            let previous =
                apply_profile_media_changes(&tx, user_id, &[(slot, ProfileMediaChange::Clear)])?;
            tx.commit()?;
            Ok(previous)
        })
        .await?;
    cleanup_replaced_profile_media(pool, paths, previous).await;
    Ok(())
}
