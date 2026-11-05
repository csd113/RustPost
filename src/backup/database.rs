//! Database for the backup subsystem.

use super::*;

pub(super) fn snapshot_database(source: &Path, destination: &Path) -> anyhow::Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let conn = Connection::open_with_flags(source, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("failed to open SQLite database {}", source.display()))?;
    conn.busy_timeout(Duration::from_secs(5))?;
    let mut destination_conn = Connection::open(destination)?;
    let backup = rusqlite::backup::Backup::new(&conn, &mut destination_conn)?;
    let deadline = std::time::Instant::now() + Duration::from_mins(5);
    loop {
        match backup.step(1024)? {
            rusqlite::backup::StepResult::Done => break,
            rusqlite::backup::StepResult::Busy | rusqlite::backup::StepResult::Locked => {
                std::thread::sleep(Duration::from_millis(10));
            }
            rusqlite::backup::StepResult::More => {}
            _ => anyhow::bail!("unsupported SQLite backup state"),
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "SQLite snapshot timed out"
        );
    }
    drop(backup);
    drop(destination_conn);
    validate_and_normalize_sqlite_database(destination, None)?;
    Ok(())
}

pub(super) fn database_schema_version(path: &Path) -> anyhow::Result<i64> {
    let conn = Connection::open(path)?;
    db::schema_version_from_connection(&conn)
}

pub(super) fn validate_and_normalize_sqlite_database(
    path: &Path,
    expected_schema_version: Option<i64>,
) -> anyhow::Result<()> {
    let conn = Connection::open(path)
        .with_context(|| format!("failed to open restored database {}", path.display()))?;
    let integrity: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        anyhow::bail!("restored database failed integrity_check");
    }
    let foreign_key_problem: Option<String> = conn
        .query_row("PRAGMA foreign_key_check", [], |row| row.get(0))
        .optional()?;
    if foreign_key_problem.is_some() {
        anyhow::bail!("restored database failed foreign_key_check");
    }
    let schema_version = db::schema_version_from_connection(&conn)?;
    if let Some(expected) = expected_schema_version
        && expected != schema_version
    {
        anyhow::bail!("manifest schema version does not match restored database");
    }
    db::normalize_restorable_schema(&conn)?;
    Ok(())
}

/// Rewrites absolute media paths in the staged database so they point at the
/// destination data directory.
///
/// Media rows store absolute host paths. Restoring to a different data
/// directory would otherwise leave every path pointing at the source machine's
/// filesystem, which breaks media deletion, dedupe, and account deletion even
/// though the files themselves were restored correctly.
pub(super) fn rewrite_restored_media_paths(
    database_path: &Path,
    paths: &RuntimePaths,
) -> anyhow::Result<()> {
    let mut conn = Connection::open(database_path).with_context(|| {
        format!(
            "failed to open restored database {}",
            database_path.display()
        )
    })?;
    conn.busy_timeout(Duration::from_secs(5))?;
    let staging_dir = database_path
        .parent()
        .and_then(Path::parent)
        .context("restored database must be inside the staging db directory")?;
    let roots = [
        ("/uploads/originals/", &paths.uploads_originals),
        ("/uploads/images/", &paths.uploads_images),
        ("/uploads/videos/", &paths.uploads_videos),
        ("/uploads/thumbs/", &paths.uploads_thumbs),
    ];
    let tx = conn.transaction()?;
    let rows = {
        let mut stmt = tx.prepare(
            "SELECT id, original_path, stored_path, thumbnail_path FROM media ORDER BY id",
        )?;
        stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?
    };
    let mut rewritten = 0usize;
    for (id, original_path, stored_path, thumbnail_path) in rows {
        let new_original = original_path
            .as_deref()
            .map(|path| remap_media_path(path, &roots, staging_dir))
            .transpose()?;
        let new_stored = remap_media_path(&stored_path, &roots, staging_dir)?;
        let new_thumbnail = thumbnail_path
            .as_deref()
            .map(|path| remap_media_path(path, &roots, staging_dir))
            .transpose()?;
        if new_original.as_deref() == original_path.as_deref()
            && new_stored == stored_path
            && new_thumbnail.as_deref() == thumbnail_path.as_deref()
        {
            continue;
        }
        tx.execute(
            "UPDATE media SET original_path = ?, stored_path = ?, thumbnail_path = ? WHERE id = ?",
            params![new_original, new_stored, new_thumbnail, id],
        )?;
        rewritten += 1;
    }
    tx.commit()?;
    if rewritten > 0 {
        tracing::info!(
            rewritten,
            "rewrote restored media paths for the destination data directory"
        );
    }
    Ok(())
}

fn managed_media_relative_path(path: &str) -> anyhow::Result<String> {
    let normalized = path.replace('\\', "/");
    for marker in [
        "uploads/originals/",
        "uploads/images/",
        "uploads/videos/",
        "uploads/thumbs/",
    ] {
        if let Some((_, relative)) = normalized.split_once(&format!("/{marker}")) {
            anyhow::ensure!(
                !relative.is_empty()
                    && !relative
                        .split('/')
                        .any(|part| matches!(part, "" | "." | "..")),
                "restored media path contains traversal or an empty component"
            );
            anyhow::ensure!(
                !relative.contains(':') && !relative.chars().any(char::is_control),
                "restored media path contains unsafe characters"
            );
            return Ok(format!("{marker}{relative}"));
        }
    }
    anyhow::bail!("media path is outside managed upload roots")
}

fn remap_media_path(
    path: &str,
    roots: &[(&str, &PathBuf)],
    staging_dir: &Path,
) -> anyhow::Result<String> {
    let relative = managed_media_relative_path(path)?;
    anyhow::ensure!(
        staging_dir.join(&relative).is_file(),
        "restored database references a missing media file"
    );
    for (marker, root) in roots {
        if let Some(suffix) = relative.strip_prefix(marker.trim_start_matches('/')) {
            return Ok(root.join(suffix).to_string_lossy().into_owned());
        }
    }
    anyhow::bail!("media path is outside managed upload roots")
}

/// Fail backup creation before publishing when the snapshot references media
/// absent from the captured inventory. Duplicate rows may share one entry.
pub(super) fn validate_snapshot_media_entries(
    database_path: &Path,
    files: &[ArchiveFile],
) -> anyhow::Result<()> {
    let conn =
        Connection::open_with_flags(database_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let captured = files
        .iter()
        .map(|file| file.archive_path.as_str())
        .collect::<BTreeSet<_>>();
    let mut stmt = conn.prepare("SELECT original_path, stored_path, thumbnail_path FROM media")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        for index in 0..3 {
            if let Some(path) = row.get::<_, Option<String>>(index)? {
                let relative = managed_media_relative_path(&path)?;
                anyhow::ensure!(
                    captured.contains(relative.as_str()),
                    "database references media missing from the backup inventory"
                );
            }
        }
    }
    Ok(())
}
