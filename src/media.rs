use std::path::{Path, PathBuf};

use axum::extract::multipart::Field;
use rusqlite::{OptionalExtension as _, params};
use tokio::io::AsyncWriteExt as _;
use uuid::Uuid;

use crate::config::Settings;
use crate::db::SqlitePool;
use crate::ffmpeg::FfmpegStatus;
use crate::runtime::RuntimePaths;

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
    let mut file = tokio::fs::File::create(&staging).await?;
    let mut bytes = 0_u64;
    while let Some(chunk) = field.chunk().await? {
        bytes += u64::try_from(chunk.len())?;
        if bytes > settings.media.max_video_size {
            let _ = tokio::fs::remove_file(&staging).await;
            anyhow::bail!("upload exceeds maximum size");
        }
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    let data = tokio::fs::read(&staging).await?;
    let Some(kind) = infer::get(&data) else {
        remove_staged_upload(&staging).await;
        anyhow::bail!("unsupported media type");
    };
    let mime = kind.mime_type().to_owned();
    let media_kind = match classify(settings, &mime, bytes) {
        Ok(media_kind) => media_kind,
        Err(error) => {
            remove_staged_upload(&staging).await;
            return Err(error);
        }
    };
    let ext = safe_extension(&mime, media_kind);
    let original_path = paths.uploads_originals.join(format!("{id}.{ext}"));
    tokio::fs::rename(&staging, &original_path).await?;
    let (stored_path, public_path, served_mime, state, stderr) = convert_or_original(
        settings,
        paths,
        ffmpeg,
        &original_path,
        &id,
        media_kind,
        &mime,
    )
    .await;
    if state == "converted" && !settings.media.keep_original_uploads {
        let _ = tokio::fs::remove_file(&original_path).await;
    }
    let stored_path = stored_path.to_string_lossy().to_string();
    let media_kind = media_kind.as_str().to_owned();
    let byte_len = i64::try_from(bytes)?;
    let db_state = state.clone();
    let db_stderr = stderr.clone();
    let media_id = pool
        .call(move |conn| {
            conn.execute(
                "INSERT INTO media (owner_user_id, original_filename, stored_path, public_path, mime_type, media_kind, byte_len, conversion_state, ffmpeg_stderr) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    owner_user_id,
                    original_filename,
                    stored_path,
                    public_path,
                    served_mime,
                    media_kind,
                    byte_len,
                    db_state,
                    db_stderr
                ],
            )?;
            Ok(conn.last_insert_rowid())
        })
        .await?;
    if state == "converted" || state == "fallback" {
        let status = state;
        let stderr_summary = stderr_summary_for_db(&stderr);
        pool.call(move |conn| {
            conn.execute(
                "INSERT INTO media_jobs (media_id, status, stderr_summary, finished_at) VALUES (?, ?, ?, CURRENT_TIMESTAMP)",
                params![media_id, status, stderr_summary],
            )?;
            Ok(())
        })
        .await?;
    }
    Ok(media_id)
}

async fn remove_staged_upload(path: &Path) {
    if let Err(error) = tokio::fs::remove_file(path).await {
        tracing::debug!(error = %error, "failed to remove rejected staged upload");
    }
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
    let stored_path: Option<String> = pool
        .call(move |conn| {
            conn.query_row(
                "SELECT stored_path FROM media WHERE id = ?",
                [media_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
        })
        .await?;
    if let Some(stored_path) = stored_path {
        if !stored_path.is_empty() {
            let _ = tokio::fs::remove_file(stored_path).await;
        }
        pool.call(move |conn| {
            conn.execute("DELETE FROM media WHERE id = ?", [media_id])?;
            Ok(())
        })
        .await?;
    }
    Ok(())
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
    id: &str,
    media_kind: MediaKind,
    original_mime: &str,
) -> (PathBuf, String, String, String, String) {
    if ffmpeg.available {
        match media_kind {
            MediaKind::Image if settings.media.convert_images_to_webp && ffmpeg.supports_webp => {
                let out = paths.uploads_images.join(format!("{id}.webp"));
                match crate::ffmpeg::convert_image_to_webp(&settings.media, original, &out).await {
                    Ok(stderr) => {
                        return (
                            out,
                            format!("/uploads/images/{id}.webp"),
                            "image/webp".to_owned(),
                            "converted".to_owned(),
                            stderr,
                        );
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
                let out = paths.uploads_videos.join(format!("{id}.webm"));
                match crate::ffmpeg::convert_video_to_webm(&settings.media, original, &out).await {
                    Ok(stderr) => {
                        return (
                            out,
                            format!("/uploads/videos/{id}.webm"),
                            "video/webm".to_owned(),
                            "converted".to_owned(),
                            stderr,
                        );
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
    media_kind: MediaKind,
    mime: &str,
    state: &str,
    stderr: &str,
) -> (PathBuf, String, String, String, String) {
    let base = original
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("upload");
    let public = match media_kind {
        MediaKind::Image | MediaKind::Video => format!("/uploads/originals/{base}"),
    };
    let path = paths.uploads_originals.join(base);
    (
        path,
        public,
        mime.to_owned(),
        state.to_owned(),
        stderr.to_owned(),
    )
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
        let second = temp.path().join("second.webp");
        tokio::fs::write(&first, b"first").await.expect("first");
        tokio::fs::write(&second, b"second").await.expect("second");
        let first_id = insert_test_media(&pool, user_id, &first, "image").await;
        let second_id = insert_test_media(&pool, user_id, &second, "image").await;

        set_profile_media(&pool, user_id, ProfileMediaSlot::Picture, first_id)
            .await
            .expect("set first");
        set_profile_media(&pool, user_id, ProfileMediaSlot::Picture, second_id)
            .await
            .expect("set second");

        assert!(!first.exists());
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
}
