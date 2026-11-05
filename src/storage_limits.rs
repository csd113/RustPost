//! Upload accounting reservations serialize quotas across concurrent requests.

use crate::{
    config::{Settings, StorageSettings},
    db::SqlitePool,
    runtime::RuntimePaths,
};
use rusqlite::params;
use std::path::{Path, PathBuf};

pub fn available_space(path: &Path) -> anyhow::Result<u64> {
    let path = path.canonicalize()?;
    #[cfg(windows)]
    let path = {
        let text = path.to_string_lossy();
        PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(&text))
    };
    let disks = sysinfo::Disks::new_with_refreshed_list();
    disks.iter().filter(|d|path.starts_with(d.mount_point())).max_by_key(|d|d.mount_point().components().count()).map(sysinfo::Disk::available_space).ok_or_else(||anyhow::anyhow!("cannot inspect storage headroom; check the data filesystem or storage.min_free_bytes"))
}

/// Best-effort admission probe only: it does not reserve physical disk capacity.
/// Concurrent uploads, conversion, backup and verification can pass against the same free bytes.
pub fn ensure_headroom(path: &Path, minimum: u64, additional: u64) -> anyhow::Result<()> {
    if minimum == 0 {
        return Ok(());
    }
    anyhow::ensure!(
        available_space(path)? >= minimum.saturating_add(additional),
        "storage headroom is too low; free space or ask the administrator to review storage limits"
    );
    Ok(())
}

pub async fn check_available(
    pool: &SqlitePool,
    settings: &Settings,
    paths: &RuntimePaths,
    additional: u64,
) -> anyhow::Result<()> {
    let reserved = u64::try_from(
        pool.call(|conn| {
            Ok(conn.query_row(
                "SELECT COALESCE(SUM(bytes),0) FROM storage_reservations",
                [],
                |r| r.get::<_, i64>(0),
            )?)
        })
        .await?,
    )?;
    let path = paths.data_dir.clone();
    let min = settings.storage.min_free_bytes;
    tokio::task::spawn_blocking(move || {
        ensure_headroom(&path, min, reserved.saturating_add(additional))
    })
    .await?
}

pub(crate) struct Reservation {
    id: String,
    pool: SqlitePool,
    settings: StorageSettings,
    directory: PathBuf,
    owner: Option<i64>,
}

impl Reservation {
    pub(crate) async fn new(
        pool: &SqlitePool,
        settings: &Settings,
        paths: &RuntimePaths,
        owner: Option<i64>,
    ) -> anyhow::Result<Self> {
        let id = uuid::Uuid::new_v4().to_string();
        let key = id.clone();
        pool.call(move |conn| {
            conn.execute(
                "INSERT INTO storage_reservations(id,owner_user_id,bytes) VALUES(?,?,0)",
                params![key, owner],
            )?;
            Ok(())
        })
        .await?;
        Ok(Self {
            id,
            pool: pool.clone(),
            settings: settings.storage.clone(),
            directory: paths.data_dir.clone(),
            owner,
        })
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) async fn grow(&self, bytes: u64, check_disk: bool) -> anyhow::Result<()> {
        let id = self.id.clone();
        let owner = self.owner;
        let settings = self.settings.clone();
        let directory = self.directory.clone();
        self.pool.call(move |conn| {
            let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let changed=tx.execute("UPDATE storage_reservations SET bytes=? WHERE id=?",params![i64::try_from(bytes)?,id])?;
            anyhow::ensure!(changed==1,"upload reservation expired; retry the upload");
            let reserved=u64::try_from(tx.query_row("SELECT COALESCE(SUM(bytes),0) FROM storage_reservations",[],|r|r.get::<_,i64>(0))?)?;
            let stored=u64::try_from(tx.query_row("SELECT COALESCE(SUM(byte_len),0) FROM media",[],|r|r.get::<_,i64>(0))?)?;
            anyhow::ensure!(settings.max_upload_bytes==0 || stored.saturating_add(reserved)<=settings.max_upload_bytes,"instance upload quota reached; ask the administrator to review storage");
            if let Some(owner)=owner {
                let used=u64::try_from(tx.query_row("SELECT (SELECT COALESCE(SUM(byte_len),0) FROM media WHERE owner_user_id=?)+(SELECT COALESCE(SUM(bytes),0) FROM storage_reservations WHERE owner_user_id=?)",params![owner,owner],|r|r.get::<_,i64>(0))?)?;
                anyhow::ensure!(settings.max_user_upload_bytes==0 || used<=settings.max_user_upload_bytes,"your upload quota is reached; remove unused media or ask the administrator");
            }
            if check_disk{ensure_headroom(&directory,settings.min_free_bytes,reserved)?;}
            tx.commit()?;Ok(())
        }).await
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let id = self.id.clone();
        let pool = self.pool.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = pool
                    .call(move |conn| {
                        conn.execute("DELETE FROM storage_reservations WHERE id=?", [id])?;
                        Ok(())
                    })
                    .await;
            });
        }
    }
}
