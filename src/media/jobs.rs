//! Durable single-worker conversion queue; originals remain usable throughout processing.

use super::*;

pub const QUEUE_LIMIT: i64 = 128;

pub(super) fn conversion_requested(
    settings: &Settings,
    ffmpeg: &FfmpegStatus,
    kind: MediaKind,
) -> bool {
    ffmpeg.available
        && match kind {
            MediaKind::Image => settings.media.convert_images_to_webp && ffmpeg.supports_webp,
            MediaKind::Video => settings.media.convert_videos_to_webm && ffmpeg.supports_vp9,
        }
}

struct Work {
    id: i64,
    token: String,
    original: PathBuf,
    mime: String,
    kind: MediaKind,
}

/// Called after acquiring the instance's exclusive server-runtime lock.
pub async fn recover(pool: &SqlitePool) -> anyhow::Result<()> {
    pool.call(|conn| {
        let tx=conn.transaction()?;
        tx.execute("DELETE FROM storage_reservations",[])?;
        tx.execute("UPDATE media SET conversion_state=CASE WHEN (SELECT attempts FROM media_work WHERE media_id=COALESCE(media.canonical_media_id,media.id))>=9 THEN 'failed' ELSE 'queued' END WHERE COALESCE(canonical_media_id,id) IN (SELECT media_id FROM media_work WHERE status='running')",[])?;
        tx.execute("UPDATE media_work SET status=CASE WHEN attempts>=9 THEN 'failed' ELSE 'queued' END,lease_token='',run_after=0 WHERE status='running'",[])?;
        tx.commit()?; Ok(())
    }).await
}

pub async fn retry(pool: &SqlitePool, user_id: i64, id: i64) -> anyhow::Result<()> {
    pool.call(move |conn| {
        let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let count:i64=tx.query_row("SELECT COUNT(*) FROM media_work WHERE status IN ('queued','running')",[],|r|r.get(0))?;
        anyhow::ensure!(count<QUEUE_LIMIT,"media queue is full; try later");
        let changed=tx.execute("UPDATE media_work SET status='queued',run_after=0,error='',lease_token='' WHERE media_id=(SELECT COALESCE(canonical_media_id,id) FROM media WHERE id=? AND owner_user_id=?) AND status='failed' AND attempts<9",params![id,user_id])?;
        anyhow::ensure!(changed==1,"this media cannot be retried or its retry limit was reached");
        tx.execute("UPDATE media SET conversion_state='queued' WHERE COALESCE(canonical_media_id,id)=(SELECT COALESCE(canonical_media_id,id) FROM media WHERE id=?)",[id])?;
        tx.execute("UPDATE media_jobs SET status='queued',finished_at=NULL WHERE media_id=(SELECT COALESCE(canonical_media_id,id) FROM media WHERE id=?)",[id])?;
        tx.commit()?; Ok(())
    }).await
}

pub async fn process_next(
    pool: &SqlitePool,
    settings: &Settings,
    paths: &RuntimePaths,
    ffmpeg: &FfmpegStatus,
) -> anyhow::Result<bool> {
    let work=pool.call(|conn| {
        let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let row=tx.query_row("SELECT w.media_id,COALESCE(m.original_path,m.stored_path),m.mime_type,m.media_kind FROM media_work w JOIN media m ON m.id=w.media_id WHERE w.status='queued' AND w.run_after<=unixepoch() ORDER BY w.media_id LIMIT 1",[],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?))).optional()?;
        let Some((id,path,mime,kind))=row else{return Ok(None)};
        let token=Uuid::new_v4().to_string();
        tx.execute("UPDATE media_work SET status='running',attempts=attempts+1,lease_token=? WHERE media_id=?",params![token,id])?;
        tx.execute("UPDATE media SET conversion_state='processing' WHERE id=? OR canonical_media_id=?",params![id,id])?;
        tx.execute("UPDATE media_jobs SET status='processing',finished_at=NULL WHERE media_id=?",[id])?;
        tx.commit()?;
        Ok(Some(Work{id,token,original:PathBuf::from(path),mime,kind:if kind=="image"{MediaKind::Image}else{MediaKind::Video}}))
    }).await?;
    let Some(work) = work else { return Ok(false) };
    let id = work.id;
    let token = work.token.clone();
    if let Err(error) = process_claimed(pool, settings, paths, ffmpeg, work).await {
        tracing::warn!(media_id=id,error=%error,"background media conversion failed; original retained");
        let _publication_lease = publication_lease(paths).await?;
        pool.call(move |conn| {
            let tx=conn.transaction()?;
            let changed=tx.execute("UPDATE media_work SET status=CASE WHEN attempts<3 THEN 'queued' ELSE 'failed' END,run_after=unixepoch()+30*attempts,lease_token='',error='Conversion failed; original retained.' WHERE media_id=? AND lease_token=?",params![id,token])?;
            if changed==1 {
                tx.execute("UPDATE media SET conversion_state=(SELECT status FROM media_work WHERE media_id=?) WHERE id=? OR canonical_media_id=?",params![id,id,id])?;
                tx.execute("UPDATE media_jobs SET status=(SELECT status FROM media_work WHERE media_id=?),stderr_summary='Conversion failed; original retained.',finished_at=CURRENT_TIMESTAMP WHERE media_id=?",params![id,id])?;
            }
            tx.commit()?; Ok(())
        }).await?;
    }
    Ok(true)
}

async fn process_claimed(
    pool: &SqlitePool,
    settings: &Settings,
    paths: &RuntimePaths,
    ffmpeg: &FfmpegStatus,
    work: Work,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        safe_stored_media_path(paths, &work.original, work.kind),
        "unsafe conversion source"
    );
    delete::validate_media_deletion_path(paths, &work.original).await?;
    crate::storage_limits::check_available(
        pool,
        settings,
        paths,
        if work.kind == MediaKind::Image {
            settings.media.max_image_size
        } else {
            settings.media.max_video_size
        },
    )
    .await?;
    let basename = work
        .original
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid conversion source"))?;
    let mut stored = convert_or_original(
        settings,
        paths,
        ffmpeg,
        &work.original,
        basename,
        work.kind,
        &work.mime,
    )
    .await;
    // The original already belongs to a committed media row, so this worker never owns its cleanup guard.
    if stored.path == work.original {
        stored.cleanup.disarm();
    }
    anyhow::ensure!(stored.state != "fallback", "conversion failed");
    let hash = hash_file(&stored.path).await?;
    let original = work.original.clone();
    let remove_original = stored.state == "converted" && !settings.media.keep_original_uploads;
    let _publication_lease = publication_lease(paths).await?;
    let committed=pool.call(move |conn| {
        let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let owned:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM media_work WHERE media_id=? AND status='running' AND lease_token=?)",params![work.id,work.token],|r|r.get(0))?;
        if !owned {return Ok(false)}
        let existing=tx.query_row("SELECT id,stored_path,public_path,mime_type FROM media WHERE id!=? AND canonical_media_id IS NULL AND media_kind=? AND normalized_sha256=? ORDER BY id LIMIT 1",params![work.id,work.kind.as_str(),hash],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?))).optional()?;
        let (canonical,path,public,mime)=existing.as_ref().map_or_else(||(None,stored.path.to_string_lossy().into_owned(),stored.public_path.clone(),stored.mime_type.clone()),|(id,path,public,mime)|(Some(*id),path.clone(),public.clone(),mime.clone()));
        tx.execute("UPDATE media SET stored_path=?,public_path=?,mime_type=?,normalized_sha256=?,conversion_state=?,canonical_media_id=?,original_path=CASE WHEN ? THEN NULL ELSE original_path END,original_public_path=CASE WHEN ? THEN NULL ELSE original_public_path END WHERE id=?",params![path,public,mime,hash,stored.state,canonical,remove_original,remove_original,work.id])?;
        tx.execute("UPDATE media SET stored_path=?,public_path=?,mime_type=?,normalized_sha256=?,conversion_state='duplicate',canonical_media_id=? WHERE canonical_media_id=? AND id!=?",params![path,public,mime,hash,canonical.unwrap_or(work.id),work.id,work.id])?;
        tx.execute("UPDATE media_work SET status='done',lease_token='',error='' WHERE media_id=?",[work.id])?;
        tx.execute("UPDATE media_jobs SET status=?,stderr_summary='',finished_at=CURRENT_TIMESTAMP WHERE media_id=?",params![stored.state,work.id])?;
        tx.commit()?;
        if existing.is_none(){stored.cleanup.disarm();}
        Ok(true)
    }).await?;
    if committed && remove_original {
        upload::cleanup_unreferenced_uploads(pool, [original.as_path()]).await;
    }
    Ok(())
}

async fn publication_lease(paths: &RuntimePaths) -> anyhow::Result<std::fs::File> {
    loop {
        match crate::runtime::snapshot_lease(paths, false) {
            Ok(lease) => return Ok(lease),
            Err(_) if crate::backup::operation_in_progress(paths) => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await
            }
            Err(error) => return Err(error),
        }
    }
}
