//! Independent collectors. Neither DB work nor recursive IO owns the renderer.
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags, OptionalExtension as _};
use tokio::sync::{Notify, watch};

use crate::runtime::RuntimePaths;

#[derive(Debug, Clone)]
pub struct Sample<T> {
    pub value: Option<T>,
    pub collected: Option<Instant>,
    pub error: Option<&'static str>,
}
impl<T> Default for Sample<T> {
    fn default() -> Self {
        Self {
            value: None,
            collected: None,
            error: None,
        }
    }
}
impl<T> Sample<T> {
    pub fn update(&mut self, value: Result<T, &'static str>) {
        match value {
            Ok(value) => {
                self.value = Some(value);
                self.collected = Some(Instant::now());
                self.error = None;
            }
            Err(error) => self.error = Some(error),
        }
    }
    pub fn freshness(&self, threshold: Duration) -> String {
        match self.collected {
            None => format!("[UNAVAILABLE] {}", self.error.unwrap_or("collecting")),
            Some(at) => {
                let age = at.elapsed();
                let label = if self.error.is_some() || age > threshold {
                    "STALE"
                } else {
                    "LIVE"
                };
                format!(
                    "[{label}] {}s old{}",
                    age.as_secs(),
                    self.error
                        .map_or(String::new(), |error| format!("; {error}"))
                )
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct Counts {
    pub users: i64,
    pub posts: i64,
    pub admins: i64,
    pub media: i64,
    pub pending: i64,
    pub running: i64,
    pub failed: i64,
    pub succeeded: i64,
    pub due_deletions: i64,
    pub maintenance: bool,
    pub recent_failures: Vec<(i64, Option<String>)>,
}
#[derive(Debug, Clone)]
pub struct Footprint {
    pub main: u64,
    pub wal: u64,
    pub shm: u64,
}
#[derive(Debug, Clone, Default)]
pub struct Metrics {
    pub counts: Sample<Counts>,
    pub db: Sample<Footprint>,
    pub uploads: Sample<u64>,
}
pub type Shared = Arc<Mutex<Metrics>>;

pub fn spawn(
    paths: &RuntimePaths,
    shutdown: watch::Receiver<bool>,
    refresh: Arc<Notify>,
) -> (Shared, Vec<tokio::task::JoinHandle<()>>) {
    let metrics = Shared::default();
    let database = paths.database_path.clone();
    let visible = metrics.clone();
    let mut stop = shutdown.clone();
    let db_task = tokio::spawn(async move {
        loop {
            if *stop.borrow() {
                break;
            }
            let path = database.clone();
            let result = tokio::task::spawn_blocking(move || collect_counts(&path))
                .await
                .unwrap_or(Err("database collector failed"));
            if let Ok(mut snapshot) = visible.lock() {
                snapshot.counts.update(result);
            }
            // A refresh flood cannot admit more than one DB sample per second.
            tokio::select! { () = tokio::time::sleep(Duration::from_secs(1)) => {}, _ = stop.changed() => {} }
            if *stop.borrow() {
                break;
            }
            tokio::select! { () = tokio::time::sleep(Duration::from_secs(2)) => {}, () = refresh.notified() => {}, _ = stop.changed() => {} }
        }
    });
    let visible = metrics.clone();
    let mut stop = shutdown.clone();
    let database = paths.database_path.clone();
    let file_task = tokio::spawn(async move {
        loop {
            if *stop.borrow() {
                break;
            }
            let path = database.clone();
            let result = tokio::task::spawn_blocking(move || footprint(&path))
                .await
                .unwrap_or(Err("file collector failed"));
            if let Ok(mut snapshot) = visible.lock() {
                snapshot.db.update(result);
            }
            tokio::select! { () = tokio::time::sleep(Duration::from_secs(3)) => {}, _ = stop.changed() => {} }
        }
    });
    let visible = metrics.clone();
    let mut stop = shutdown;
    let root = paths.data_dir.join("uploads");
    let storage_task = tokio::spawn(async move {
        loop {
            if *stop.borrow() {
                break;
            }
            let root = root.clone();
            let cancel = stop.clone();
            let result = tokio::task::spawn_blocking(move || upload_bytes(&root, &cancel))
                .await
                .unwrap_or(Err("storage collector failed"));
            if let Ok(mut snapshot) = visible.lock() {
                snapshot.uploads.update(result);
            }
            tokio::select! { () = tokio::time::sleep(Duration::from_secs(30)) => {}, _ = stop.changed() => {} }
        }
    });
    (metrics, vec![db_task, file_task, storage_task])
}

fn collect_counts(path: &Path) -> Result<Counts, &'static str> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| "database open failed")?;
    conn.busy_timeout(Duration::from_millis(100))
        .map_err(|_| "database timeout setup failed")?;
    conn.execute_batch("BEGIN DEFERRED")
        .map_err(|_| "database read snapshot unavailable")?;
    let started = Instant::now();
    conn.progress_handler(
        1000,
        Some(move || started.elapsed() > Duration::from_millis(250)),
    )
    .map_err(|_| "database query budget setup failed")?;
    let count = |sql: &str| {
        conn.query_row(sql, [], |row| row.get::<_, i64>(0))
            .map_err(|_| "database busy, query failed or 250ms budget exceeded")
    };
    // Same active-content semantics as the web instance counts.
    let users = count("SELECT COUNT(*) FROM users WHERE is_deleted = 0")?;
    let posts = count("SELECT COUNT(*) FROM posts WHERE is_deleted = 0")?;
    let admins = count("SELECT COUNT(*) FROM users WHERE is_admin = 1 AND is_deleted = 0")?;
    let media = count("SELECT COUNT(*) FROM media")?;
    let (pending, running, failed, succeeded) = conn.query_row("SELECT COALESCE(SUM(status IN ('pending','queued')),0), COALESCE(SUM(status='running'),0), COALESCE(SUM(status IN ('failed','error','fallback')),0), COALESCE(SUM(status IN ('succeeded','success','converted')),0) FROM media_jobs", [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))).map_err(|_| "media-job query unavailable")?;
    let due_deletions = count(
        "SELECT COUNT(*) FROM users WHERE deletion_scheduled_at IS NOT NULL AND deletion_scheduled_at <= CURRENT_TIMESTAMP",
    )?;
    let maintenance = conn
        .query_row(
            "SELECT value = '1' FROM instance_settings WHERE key = 'maintenance_mode'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| "maintenance query unavailable")?
        .unwrap_or(false);
    // No filenames, user content or stderr are read into the terminal buffer.
    let mut stmt = conn.prepare("SELECT id, finished_at FROM media_jobs WHERE status IN ('failed','error','fallback') ORDER BY id DESC LIMIT 5").map_err(|_| "failure query unavailable")?;
    let recent_failures = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(|_| "failure query unavailable")?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "failure rows unavailable")?;
    Ok(Counts {
        users,
        posts,
        admins,
        media,
        pending,
        running,
        failed,
        succeeded,
        due_deletions,
        maintenance,
        recent_failures,
    })
}

fn footprint(path: &Path) -> Result<Footprint, &'static str> {
    let bytes = |path: &Path, optional| match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => Ok(meta.len()),
        Err(error) if optional && error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        _ => Err("database file metadata unavailable"),
    };
    let sidecar = |suffix: &str| {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        std::path::PathBuf::from(name)
    };
    Ok(Footprint {
        main: bytes(path, false)?,
        wal: bytes(&sidecar("-wal"), true)?,
        shm: bytes(&sidecar("-shm"), true)?,
    })
}

fn upload_bytes(root: &Path, shutdown: &watch::Receiver<bool>) -> Result<u64, &'static str> {
    let root_metadata = std::fs::symlink_metadata(root).map_err(|_| "upload root unavailable")?;
    if !root_metadata.is_dir() {
        return Err("upload root is not a directory (symlinks are not followed)");
    }
    let start = Instant::now();
    let mut total = 0u64;
    for (index, entry) in walkdir::WalkDir::new(root)
        .follow_root_links(false)
        .follow_links(false)
        .into_iter()
        .enumerate()
    {
        if *shutdown.borrow() {
            return Err("scan cancelled");
        }
        if index >= 10_000 || start.elapsed() > Duration::from_millis(200) {
            return Err("scan incomplete: 10,000-entry / 200ms budget");
        }
        let entry = entry.map_err(|_| "upload directory unavailable")?;
        if entry.file_type().is_file() {
            total = total.saturating_add(
                entry
                    .metadata()
                    .map_err(|_| "upload metadata unavailable")?
                    .len(),
            );
        }
    }
    Ok(total)
}

pub fn bytes(value: u64) -> String {
    format!("{value} B")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn a_symlinked_upload_root_is_unknown_instead_of_a_fabricated_zero() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let target = directory.path().join("target");
        std::fs::create_dir(&target)?;
        std::fs::write(target.join("fixture"), b"fixture")?;
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&target, &link)?;
        let (_stop, shutdown) = watch::channel(false);
        assert!(upload_bytes(&link, &shutdown).is_err());
        assert_eq!(upload_bytes(&target, &shutdown), Ok(7));
        Ok(())
    }

    #[test]
    fn failures_preserve_last_good_data_with_independent_freshness() {
        let mut sample = Sample::default();
        sample.update(Ok(37));
        sample.update(Err("database unavailable"));
        assert_eq!(sample.value, Some(37));
        let status = sample.freshness(Duration::from_secs(10));
        assert!(status.contains("[STALE]"));
        assert!(status.contains("database unavailable"));
        assert!(
            Sample::<u64>::default()
                .freshness(Duration::from_secs(10))
                .contains("[UNAVAILABLE]")
        );
    }
}
