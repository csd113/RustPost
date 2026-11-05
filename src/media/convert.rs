//! Convert for the media subsystem.

use super::*;

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

pub(super) fn classify(settings: &Settings, mime: &str, bytes: u64) -> anyhow::Result<MediaKind> {
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

pub(super) async fn convert_or_original(
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
                let out = match unique_media_path(&paths.uploads_images, basename, "webp") {
                    Ok(path) => path,
                    Err(error) => {
                        return original_fallback(
                            original,
                            paths,
                            media_kind,
                            original_mime,
                            "fallback",
                            &error.to_string(),
                        );
                    }
                };
                let cleanup = StagedUploadGuard::new(out.clone());
                match crate::ffmpeg::convert_image_to_webp(&settings.media, original, &out).await {
                    Ok(stderr) => {
                        return StoredVariant {
                            cleanup,
                            public_path: public_upload_path(paths, &out)
                                .unwrap_or_else(|_| format!("/uploads/images/{basename}.webp")),
                            path: out,
                            mime_type: "image/webp".to_owned(),
                            state: "converted".to_owned(),
                            stderr,
                        };
                    }
                    Err(err) => {
                        remove_partial_conversion(&out).await;
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
                let out = match unique_media_path(&paths.uploads_videos, basename, "webm") {
                    Ok(path) => path,
                    Err(error) => {
                        return original_fallback(
                            original,
                            paths,
                            media_kind,
                            original_mime,
                            "fallback",
                            &error.to_string(),
                        );
                    }
                };
                let cleanup = StagedUploadGuard::new(out.clone());
                match crate::ffmpeg::convert_video_to_webm(&settings.media, original, &out).await {
                    Ok(stderr) => {
                        return StoredVariant {
                            cleanup,
                            public_path: public_upload_path(paths, &out)
                                .unwrap_or_else(|_| format!("/uploads/videos/{basename}.webm")),
                            path: out,
                            mime_type: "video/webm".to_owned(),
                            state: "converted".to_owned(),
                            stderr,
                        };
                    }
                    Err(err) => {
                        remove_partial_conversion(&out).await;
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

/// Removes a partially written conversion output after an ffmpeg failure or
/// timeout. Missing files are fine.
pub(super) async fn remove_partial_conversion(path: &Path) {
    if let Err(error) = tokio::fs::remove_file(path).await
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(
            path = %path.display(),
            error = %error,
            "failed to remove partial media conversion output"
        );
    }
}

pub(super) fn original_fallback(
    original: &Path,
    paths: &RuntimePaths,
    _media_kind: MediaKind,
    mime: &str,
    state: &str,
    stderr: &str,
) -> StoredVariant {
    StoredVariant {
        cleanup: StagedUploadGuard::new(original.to_owned()),
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

pub(super) fn stderr_summary_for_db(stderr: &str) -> String {
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
