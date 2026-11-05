//! Delete for the media subsystem.

use super::*;

pub async fn delete_media(
    pool: &SqlitePool,
    paths: &RuntimePaths,
    media_id: i64,
) -> anyhow::Result<()> {
    validate_media_deletion_paths(pool, paths, media_id).await?;
    let paths_to_remove = pool
        .call(move |conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let referenced: bool = tx.query_row(
                r#"
                SELECT EXISTS(
                    SELECT 1 FROM post_media WHERE media_id = ?
                    UNION ALL SELECT 1 FROM draft_media WHERE media_id = ?
                    UNION ALL
                    SELECT 1 FROM users
                    WHERE profile_picture_media_id = ? OR banner_media_id = ?
                )
                "#,
                params![media_id, media_id, media_id, media_id],
                |row| row.get(0),
            )?;
            if referenced {
                anyhow::bail!("media is still referenced by a post or profile");
            }
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

pub(super) async fn validate_media_deletion_paths(
    pool: &SqlitePool,
    paths: &RuntimePaths,
    media_id: i64,
) -> anyhow::Result<()> {
    let media_paths: Option<(Option<String>, String, Option<String>)> = pool
        .call(move |conn| {
            conn.query_row(
                "SELECT original_path, stored_path, thumbnail_path FROM media WHERE id = ?",
                [media_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(Into::into)
        })
        .await?;
    let Some((original_path, stored_path, thumbnail_path)) = media_paths else {
        return Ok(());
    };
    for path in [original_path, Some(stored_path), thumbnail_path]
        .into_iter()
        .flatten()
        .filter(|path| !path.is_empty())
    {
        validate_media_deletion_path(paths, Path::new(&path)).await?;
    }
    Ok(())
}

pub(super) async fn validate_media_deletion_path(
    paths: &RuntimePaths,
    path: &Path,
) -> anyhow::Result<()> {
    if !path.is_absolute() || has_parent_component(path) {
        anyhow::bail!("refusing to delete unsafe media path {}", path.display());
    }
    let allowed_roots = [
        &paths.uploads_originals,
        &paths.uploads_images,
        &paths.uploads_videos,
        &paths.uploads_thumbs,
    ];
    let Some(root) = allowed_roots
        .into_iter()
        .find(|root| path.starts_with(root))
    else {
        anyhow::bail!(
            "refusing to delete media path outside upload directories: {}",
            path.display()
        );
    };
    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_file() {
        anyhow::bail!("refusing to delete non-file media path {}", path.display());
    }
    let canonical_root = tokio::fs::canonicalize(root).await?;
    let canonical_path = tokio::fs::canonicalize(path).await?;
    if !canonical_path.starts_with(canonical_root) {
        anyhow::bail!(
            "refusing to delete media path outside upload directories: {}",
            path.display()
        );
    }
    Ok(())
}

pub async fn delete_post_media(
    pool: &SqlitePool,
    paths: &RuntimePaths,
    post_id: i64,
) -> anyhow::Result<()> {
    let media_ids = post_media_ids(pool, post_id).await?;
    for media_id in &media_ids {
        validate_media_deletion_paths(pool, paths, *media_id).await?;
    }
    let paths_to_remove = pool
        .call(move |conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            tx.execute("DELETE FROM post_media WHERE post_id = ?", [post_id])?;
            let mut paths = BTreeSet::new();
            for media_id in media_ids {
                let referenced: bool = tx.query_row(
                    r#"
                    SELECT EXISTS(
                        SELECT 1 FROM post_media WHERE media_id = ?
                    UNION ALL SELECT 1 FROM draft_media WHERE media_id = ?
                        UNION ALL
                        SELECT 1 FROM users
                        WHERE profile_picture_media_id = ? OR banner_media_id = ?
                    )
                    "#,
                    params![media_id, media_id, media_id, media_id],
                    |row| row.get(0),
                )?;
                if referenced {
                    continue;
                }
                let media_paths: Option<(Option<String>, String, Option<String>)> = tx
                    .query_row(
                        "SELECT original_path, stored_path, thumbnail_path FROM media WHERE id = ?",
                        [media_id],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .optional()?;
                let Some((original_path, stored_path, thumbnail_path)) = media_paths else {
                    continue;
                };
                promote_canonical_references(&tx, media_id)?;
                tx.execute("DELETE FROM media_jobs WHERE media_id = ?", [media_id])?;
                tx.execute("DELETE FROM media WHERE id = ?", [media_id])?;
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
            }
            tx.commit()?;
            Ok(paths.into_iter().collect::<Vec<_>>())
        })
        .await?;
    remove_media_files(paths_to_remove, post_id, "post media cleanup").await;
    Ok(())
}

pub async fn validate_post_media_deletion(
    pool: &SqlitePool,
    paths: &RuntimePaths,
    post_id: i64,
) -> anyhow::Result<()> {
    for media_id in post_media_ids(pool, post_id).await? {
        validate_media_deletion_paths(pool, paths, media_id).await?;
    }
    Ok(())
}

pub(super) async fn post_media_ids(pool: &SqlitePool, post_id: i64) -> anyhow::Result<Vec<i64>> {
    pool.call(move |conn| {
        let mut stmt =
            conn.prepare("SELECT media_id FROM post_media WHERE post_id = ? ORDER BY position")?;
        let ids = stmt
            .query_map([post_id], |row| row.get(0))?
            .collect::<Result<Vec<i64>, _>>()?;
        Ok(ids)
    })
    .await
}

pub(super) async fn remove_media_files(
    paths: Vec<PathBuf>,
    subject_id: i64,
    operation: &'static str,
) {
    for path in paths {
        if let Err(error) = tokio::fs::remove_file(&path).await
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                subject_id,
                path = %path.display(),
                error = %error,
                operation,
                "failed to remove unreferenced media file"
            );
        }
    }
}

pub(super) fn promote_canonical_references(
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
    tx.execute("UPDATE media_work SET media_id=?,status=CASE WHEN status='running' THEN CASE WHEN attempts<9 THEN 'queued' ELSE 'failed' END ELSE status END,lease_token='' WHERE media_id=?",params![replacement,media_id])?;
    tx.execute(
        "UPDATE media_jobs SET media_id=? WHERE media_id=?",
        params![replacement, media_id],
    )?;
    tx.execute("UPDATE media SET conversion_state=(SELECT CASE WHEN status='queued' THEN 'queued' WHEN status='failed' THEN 'failed' ELSE conversion_state END FROM media_work WHERE media_id=?) WHERE id=? AND EXISTS(SELECT 1 FROM media_work WHERE media_id=?)",params![replacement,replacement,replacement])?;
    Ok(())
}
