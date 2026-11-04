//! Media for the portability subsystem.

use super::*;

/// Rejects deleted and deletion-pending destination accounts.
pub(super) async fn ensure_import_destination(
    pool: &SqlitePool,
    user_id: i64,
) -> anyhow::Result<()> {
    let state = pool
        .call(move |conn| {
            conn.query_row(
                "SELECT is_deleted, deletion_requested_at, deletion_scheduled_at FROM users WHERE id = ?",
                [user_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(Into::into)
        })
        .await?;
    match state {
        None => anyhow::bail!("import destination account does not exist"),
        Some((is_deleted, _, _)) if is_deleted != 0 => {
            anyhow::bail!("import destination account is deleted")
        }
        Some((_, requested_at, scheduled_at))
            if requested_at.is_some() || scheduled_at.is_some() =>
        {
            anyhow::bail!("import destination account is pending deletion")
        }
        Some(_) => Ok(()),
    }
}

pub(super) async fn archive_already_imported(
    pool: &SqlitePool,
    user_id: i64,
    archive_id: &str,
) -> anyhow::Result<bool> {
    let archive_id = archive_id.to_owned();
    let existing = pool
        .call(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT 1 FROM account_imports WHERE user_id = ? AND archive_id = ?",
                    params![user_id, archive_id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some())
        })
        .await?;
    Ok(existing)
}

/// Copies every non-duplicate media entry into the destination upload
/// directories before the database transaction runs. On failure all files
/// copied so far are removed.
pub(super) async fn install_media_files(
    pool: &SqlitePool,
    paths: &RuntimePaths,
    settings: &Settings,
    media: &[ValidatedMedia],
) -> anyhow::Result<InstalledMedia> {
    let mut decisions = Vec::with_capacity(media.len());
    let mut cleanup = Vec::new();
    for entry in media {
        match install_one_media(pool, paths, settings, entry).await {
            Ok((decision, copied_path)) => {
                if let Some(copied_path) = copied_path {
                    cleanup.push(copied_path);
                }
                decisions.push(decision);
            }
            Err(error) => {
                return Err(error);
            }
        }
    }
    Ok(InstalledMedia { decisions, cleanup })
}

pub(super) async fn install_one_media(
    pool: &SqlitePool,
    paths: &RuntimePaths,
    settings: &Settings,
    entry: &ValidatedMedia,
) -> anyhow::Result<(MediaInstallDecision, Option<TempOutputGuard>)> {
    if let Some(canonical) = find_canonical_media(pool, &entry.media_kind, &entry.sha256).await? {
        return Ok((
            MediaInstallDecision::Canonical {
                canonical_id: canonical.id,
                stored_path: canonical.stored_path,
                public_path: canonical.public_path,
            },
            None,
        ));
    }
    let directory = media_install_dir(paths, settings, &entry.media_kind, &entry.mime_type)?;
    fs::create_dir_all(directory).with_context(|| {
        format!(
            "failed to create media upload directory {}",
            directory.display()
        )
    })?;
    let extension = import_media_extension(&entry.mime_type, &entry.media_kind)?;
    let basename = import_media_basename(&entry.original_filename, &entry.sha256);
    let target = claim_media_path(directory, &basename, extension)?;
    let source = entry.staged_path.clone();
    let bytes = entry.byte_len;
    let archive_path = entry.archive_path.clone();
    let guard = TempOutputGuard::new(target);
    let cleanup = tokio::task::spawn_blocking(move || -> anyhow::Result<TempOutputGuard> {
        let _operation = crate::runtime::begin_temp_operation();
        copy_media_bytes(&source, &guard.path, bytes)
            .with_context(|| format!("failed to install imported media {archive_path}"))?;
        Ok(guard)
    })
    .await
    .context("media copy worker failed")??;
    let target = &cleanup.path;
    let public_path = public_media_path(paths, target)?;
    Ok((
        MediaInstallDecision::Copied {
            stored_path: target.clone(),
            public_path,
        },
        Some(cleanup),
    ))
}

pub(super) async fn find_canonical_media(
    pool: &SqlitePool,
    media_kind: &str,
    sha256: &str,
) -> anyhow::Result<Option<CanonicalMedia>> {
    let media_kind = media_kind.to_owned();
    let sha256 = sha256.to_owned();
    pool.call(move |conn| {
        conn.query_row(
            r#"
            SELECT id, stored_path, public_path FROM media
            WHERE canonical_media_id IS NULL AND media_kind = ? AND original_sha256 = ?
            ORDER BY id ASC LIMIT 1
            "#,
            params![media_kind, sha256],
            |row| {
                Ok(CanonicalMedia {
                    id: row.get(0)?,
                    stored_path: row.get(1)?,
                    public_path: row.get(2)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    })
    .await
}

/// Chooses the upload directory for imported bytes. Types outside the
/// instance's convertible allow-list are stored in `originals` because the
/// server layer would never convert or serve them from images/videos.
pub(super) fn media_install_dir<'a>(
    paths: &'a RuntimePaths,
    settings: &Settings,
    media_kind: &str,
    mime_type: &str,
) -> anyhow::Result<&'a Path> {
    match media_kind {
        "image" => {
            if settings
                .media
                .allowed_image_mime_types
                .iter()
                .any(|allowed| allowed == mime_type)
            {
                Ok(&paths.uploads_images)
            } else {
                Ok(&paths.uploads_originals)
            }
        }
        "video" => {
            if settings
                .media
                .allowed_video_mime_types
                .iter()
                .any(|allowed| allowed == mime_type)
            {
                Ok(&paths.uploads_videos)
            } else {
                Ok(&paths.uploads_originals)
            }
        }
        _ => anyhow::bail!("media entry has an unsupported media kind"),
    }
}

pub(super) fn public_media_path(paths: &RuntimePaths, path: &Path) -> anyhow::Result<String> {
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("installed media path has no file name"))?;
    if path.starts_with(&paths.uploads_images) {
        return Ok(format!("/uploads/images/{filename}"));
    }
    if path.starts_with(&paths.uploads_videos) {
        return Ok(format!("/uploads/videos/{filename}"));
    }
    if path.starts_with(&paths.uploads_originals) {
        return Ok(format!("/uploads/originals/{filename}"));
    }
    anyhow::bail!("installed media path is outside the upload directories")
}

/// Claims an unused destination file name with `create_new` so concurrent
/// imports can never overwrite each other's files.
pub(super) fn claim_media_path(
    directory: &Path,
    basename: &str,
    extension: &str,
) -> anyhow::Result<PathBuf> {
    let fallback = directory.join(format!(
        "{basename}-{}.{extension}",
        Uuid::new_v4().simple()
    ));
    let candidates = [
        directory.join(format!("{basename}.{extension}")),
        fallback,
        directory.join(format!(
            "{basename}-{}.{extension}",
            Uuid::new_v4().simple()
        )),
    ];
    for candidate in candidates {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(_claimed) => return Ok(candidate),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to create media file {}", candidate.display())
                });
            }
        }
    }
    anyhow::bail!("failed to claim a unique file name for imported media")
}

pub(super) fn copy_media_bytes(source: &Path, target: &Path, expected: u64) -> anyhow::Result<()> {
    let mut source_file = File::open(source)
        .with_context(|| format!("failed to open staged media file {}", source.display()))?;
    let mut target_file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(target)
        .with_context(|| format!("failed to open media destination {}", target.display()))?;
    let copied = io::copy(&mut source_file, &mut target_file)?;
    target_file
        .sync_all()
        .with_context(|| format!("failed to flush media destination {}", target.display()))?;
    if copied != expected {
        anyhow::bail!("copied media size does not match the archive entry");
    }
    Ok(())
}

/// Mirrors `media::stable_media_basename` (private): sanitized original stem
/// plus a short content-hash prefix.
pub(super) fn import_media_basename(original_filename: &str, sha256: &str) -> String {
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
    let prefix_len = sha256.len().min(HASH_PREFIX_LEN);
    format!("{}-{}", sanitized, &sha256[..prefix_len])
}

/// Mirrors `media::safe_extension` (private).
pub(super) fn import_media_extension(
    mime_type: &str,
    media_kind: &str,
) -> anyhow::Result<&'static str> {
    match (media_kind, mime_type) {
        ("image", "image/jpeg") => Ok("jpg"),
        ("image", "image/png") => Ok("png"),
        ("image", "image/gif") => Ok("gif"),
        ("image", "image/webp") => Ok("webp"),
        ("image", _) => Ok("img"),
        ("video", "video/webm") => Ok("webm"),
        ("video", "video/quicktime") => Ok("mov"),
        ("video", _) => Ok("mp4"),
        _ => anyhow::bail!("media entry has an unsupported media kind"),
    }
}

pub(super) fn remove_files_best_effort(paths: &[PathBuf]) {
    for path in paths {
        if let Err(error) = fs::remove_file(path)
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "failed to remove imported media file during cleanup"
            );
        }
    }
}
