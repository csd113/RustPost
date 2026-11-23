//! Short, owner-only deletion recovery. Administrative removals never acquire a recovery marker.

use super::*;

pub async fn delete(
    pool: &SqlitePool,
    actor: i64,
    post: i64,
    admin: bool,
    seconds: u64,
) -> anyhow::Result<bool> {
    let seconds = i64::try_from(seconds.min(300))?;
    pool.call(move |conn| {
        let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let row=tx.query_row("SELECT user_id,is_deleted FROM posts WHERE id=?",[post],|r|Ok((r.get::<_,Option<i64>>(0)?,r.get::<_,bool>(1)?))).optional()?;
        let Some((owner,deleted))=row else{return Ok(false)};
        anyhow::ensure!(admin || owner==Some(actor),"cannot delete this post");
        if deleted {
            if admin && seconds==0 {tx.execute("DELETE FROM recoverable_deletions WHERE post_id=?",[post])?;tx.commit()?;}
            return Ok(false);
        }
        let recoverable=owner==Some(actor) && seconds>0;
        tx.execute("UPDATE posts SET is_deleted=1,deleted_at=CURRENT_TIMESTAMP WHERE id=?",[post])?;
        tx.execute("UPDATE users SET pinned_post_id=NULL,updated_at=CURRENT_TIMESTAMP WHERE pinned_post_id=?",[post])?;
        tx.execute("DELETE FROM recoverable_deletions WHERE post_id=?",[post])?;
        if recoverable {tx.execute("INSERT INTO recoverable_deletions(post_id,user_id,expires_at) VALUES(?,?,unixepoch()+?)",params![post,actor,seconds])?;}
        tx.commit()?;Ok(recoverable)
    }).await
}

pub async fn restore(pool: &SqlitePool, user: i64, post: i64) -> anyhow::Result<()> {
    pool.call(move |conn| {
        let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let changed=tx.execute("UPDATE posts SET is_deleted=0,deleted_at=NULL WHERE id=? AND user_id=? AND EXISTS(SELECT 1 FROM recoverable_deletions WHERE post_id=? AND user_id=? AND expires_at>unixepoch()) AND EXISTS(SELECT 1 FROM users WHERE id=? AND is_deleted=0 AND is_suspended=0 AND deletion_requested_at IS NULL)",params![post,user,post,user,user])?;
        anyhow::ensure!(changed==1,"this deletion cannot be undone; the recovery window may have expired");
        tx.execute("DELETE FROM recoverable_deletions WHERE post_id=?",[post])?;
        tx.commit()?;Ok(())
    }).await
}

pub async fn purge_due(
    pool: &SqlitePool,
    paths: &crate::runtime::RuntimePaths,
) -> anyhow::Result<()> {
    let ids=pool.call(|conn| {
        let mut statement=conn.prepare("SELECT post_id FROM recoverable_deletions WHERE expires_at<=unixepoch() ORDER BY expires_at LIMIT 32")?;
        Ok(statement.query_map([],|r|r.get::<_,i64>(0))?.collect::<Result<Vec<_>,_>>()?)
    }).await?;
    for id in ids {
        match crate::media::delete_post_media(pool, paths, id).await {
            Ok(()) => {
                pool.call(move |conn| {conn.execute("DELETE FROM recoverable_deletions WHERE post_id=? AND expires_at<=unixepoch()",[id])?;Ok(())}).await?;
            }
            Err(error) => {
                tracing::warn!(post_id=id,error=%error,"expired deletion media cleanup will be retried")
            }
        }
    }
    Ok(())
}
