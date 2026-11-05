//! Owned in-process workers. SQLite is the durable source of work; no external daemon is required.

use crate::{config::Settings, db::SqlitePool, ffmpeg::FfmpegStatus, runtime::RuntimePaths};

pub fn spawn(
    pool: SqlitePool,
    settings: Settings,
    paths: RuntimePaths,
    ffmpeg: FfmpegStatus,
    stop: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let schedules = scheduled_loop(pool.clone(), settings.clone(), paths.clone(), stop.clone());
        let media = media_loop(pool, settings, paths, ffmpeg, stop);
        tokio::join!(schedules, media);
    })
}

async fn scheduled_loop(
    pool: SqlitePool,
    settings: Settings,
    paths: RuntimePaths,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        if *stop.borrow() {
            break;
        }
        if !crate::backup::operation_in_progress(&paths)
            && let Ok(_lease) = crate::runtime::snapshot_lease(&paths, false)
        {
            if let Err(error) = crate::social::recovery::purge_due(&pool, &paths).await {
                tracing::warn!(error=%error,"expired post cleanup failed");
            }
            match crate::instance::load(&pool).await {
                Ok(instance) if !instance.maintenance_mode => {
                    if let Err(error) = crate::social::drafts::publish_due(&pool, &settings).await {
                        tracing::warn!(error=%error,"scheduled publication worker failed");
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(error=%error,"cannot read scheduled publication policy")
                }
            }
        }
        tokio::select! {_ = stop.changed()=>{},()=tokio::time::sleep(std::time::Duration::from_secs(5))=>{}}
    }
}

async fn media_loop(
    pool: SqlitePool,
    settings: Settings,
    paths: RuntimePaths,
    ffmpeg: FfmpegStatus,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        if *stop.borrow() {
            break;
        }
        if !crate::backup::operation_in_progress(&paths) {
            match crate::media::jobs::process_next(&pool, &settings, &paths, &ffmpeg).await {
                Ok(true) => continue,
                Ok(false) => {}
                Err(error) => tracing::warn!(error=%error,"media worker failed"),
            }
        }
        tokio::select! {_ = stop.changed()=>{},()=tokio::time::sleep(std::time::Duration::from_secs(2))=>{}}
    }
}
