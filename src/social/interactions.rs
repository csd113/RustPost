//! Interactions for the social subsystem.

use super::*;

pub(super) fn clean_muted_word(term: &str) -> anyhow::Result<String> {
    let trimmed = term.trim();
    if trimmed.is_empty() {
        anyhow::bail!("muted word cannot be empty");
    }
    if trimmed.chars().count() > 100 {
        anyhow::bail!("muted word is too long");
    }
    if trimmed
        .chars()
        .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
    {
        anyhow::bail!("muted word contains unsupported control characters");
    }
    Ok(trimmed.to_owned())
}

pub async fn like(pool: &SqlitePool, user_id: i64, post_id: i64) -> anyhow::Result<()> {
    pool.call(move |conn| {
        let tx = conn.transaction()?;
        ensure_post_interaction_accessible_tx(&tx, user_id, post_id)?;
        let owner_exists = tx
            .query_row(
                "SELECT 1 FROM posts WHERE id = ? AND is_deleted = 0",
                [post_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !owner_exists {
            anyhow::bail!("post not found");
        }
        let changed = tx.execute(
            "INSERT OR IGNORE INTO likes (user_id, post_id) VALUES (?, ?)",
            params![user_id, post_id],
        )?;
        if changed > 0 {
            notify_post_owner_tx(&tx, post_id, user_id, post_id, "like", "liked your post")?;
        }
        tx.commit()?;
        Ok(())
    })
    .await
}

pub async fn unlike(pool: &SqlitePool, user_id: i64, post_id: i64) -> anyhow::Result<()> {
    pool.call(move |conn| {
        conn.execute(
            "DELETE FROM likes WHERE user_id = ? AND post_id = ?",
            params![user_id, post_id],
        )?;
        Ok(())
    })
    .await
}

pub async fn bookmark(pool: &SqlitePool, user_id: i64, post_id: i64) -> anyhow::Result<()> {
    pool.call(move |conn| {
        let tx = conn.transaction()?;
        ensure_post_interaction_accessible_tx(&tx, user_id, post_id)?;
        tx.execute(
            "INSERT OR IGNORE INTO bookmarks (user_id, post_id) VALUES (?, ?)",
            params![user_id, post_id],
        )?;
        tx.commit()?;
        Ok(())
    })
    .await
}

pub(super) fn ensure_post_interaction_accessible_tx(
    tx: &rusqlite::Transaction<'_>,
    user_id: i64,
    post_id: i64,
) -> anyhow::Result<()> {
    let owner = tx
        .query_row(
            r#"
            SELECT p.user_id
            FROM posts p
            LEFT JOIN users u ON u.id = p.user_id
            WHERE p.id = ? AND p.is_deleted = 0
              AND (p.user_id IS NULL OR (u.is_deleted = 0 AND u.is_suspended = 0))
            "#,
            [post_id],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()?;
    let Some(owner) = owner else {
        anyhow::bail!("post not found");
    };
    if let Some(owner) = owner
        && owner != user_id
        && blocks_exist_tx(tx, user_id, owner)?
    {
        anyhow::bail!("post not found");
    }
    Ok(())
}

pub(super) fn blocks_exist_tx(
    tx: &rusqlite::Transaction<'_>,
    left_id: i64,
    right_id: i64,
) -> anyhow::Result<bool> {
    Ok(tx
        .query_row(
            r#"
            SELECT 1 FROM blocks
            WHERE (blocker_id = ? AND blocked_id = ?)
               OR (blocker_id = ? AND blocked_id = ?)
            LIMIT 1
            "#,
            params![left_id, right_id, right_id, left_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

pub async fn unbookmark(pool: &SqlitePool, user_id: i64, post_id: i64) -> anyhow::Result<()> {
    pool.call(move |conn| {
        conn.execute(
            "DELETE FROM bookmarks WHERE user_id = ? AND post_id = ?",
            params![user_id, post_id],
        )?;
        Ok(())
    })
    .await
}

pub async fn pinned_post_id(pool: &SqlitePool, user_id: i64) -> anyhow::Result<Option<i64>> {
    pool.call(move |conn| {
        conn.query_row(
            "SELECT pinned_post_id FROM users WHERE id = ? AND is_deleted = 0",
            [user_id],
            |row| row.get(0),
        )
        .optional()
        .map(std::option::Option::flatten)
        .map_err(Into::into)
    })
    .await
}

pub async fn pin_post(pool: &SqlitePool, user_id: i64, post_id: i64) -> Result<(), PinPostError> {
    pool.call(move |conn| {
        let result: Result<(), PinPostError> = (|| {
            let tx = conn.transaction()?;
            let owner = tx
                .query_row(
                    "SELECT user_id FROM posts WHERE id = ? AND is_deleted = 0",
                    [post_id],
                    |row| row.get::<_, Option<i64>>(0),
                )
                .optional()?;
            let Some(owner) = owner else {
                return Err(PinPostError::NotFound);
            };
            if owner != Some(user_id) {
                return Err(PinPostError::Forbidden);
            }
            tx.execute(
                "UPDATE users SET pinned_post_id = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ? AND is_deleted = 0",
                params![post_id, user_id],
            )?;
            tx.commit()?;
            Ok(())
        })();
        Ok(result)
    })
    .await
    .map_err(|err| PinPostError::Database(err.to_string()))?
}

pub async fn unpin_post(pool: &SqlitePool, user_id: i64, post_id: i64) -> anyhow::Result<bool> {
    pool.call(move |conn| {
        let changed = conn.execute(
            "UPDATE users SET pinned_post_id = NULL, updated_at = CURRENT_TIMESTAMP WHERE id = ? AND pinned_post_id = ?",
            params![user_id, post_id],
        )?;
        Ok(changed > 0)
    })
    .await
}

pub async fn delete_post(
    pool: &SqlitePool,
    actor_id: i64,
    post_id: i64,
    is_admin: bool,
) -> anyhow::Result<()> {
    recovery::delete(pool, actor_id, post_id, is_admin, 0)
        .await
        .map(|_| ())
}

pub async fn set_post_media_nsfw(
    pool: &SqlitePool,
    post_id: i64,
    is_nsfw: bool,
) -> anyhow::Result<usize> {
    pool.call(move |conn| {
        let exists = conn
            .query_row(
                "SELECT 1 FROM posts WHERE id = ? AND is_deleted = 0",
                [post_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !exists {
            anyhow::bail!("post not found");
        }
        Ok(conn.execute(
            "UPDATE media SET is_nsfw = ? WHERE id IN (SELECT media_id FROM post_media WHERE post_id = ?)",
            params![i64::from(is_nsfw), post_id],
        )?)
    })
    .await
}
