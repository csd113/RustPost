//! Posting for the social subsystem.

use super::*;

pub async fn create_post(
    pool: &SqlitePool,
    settings: &Settings,
    user_id: Option<i64>,
    text: &str,
    parent_post_id: Option<i64>,
    media_ids: &[i64],
) -> anyhow::Result<i64> {
    if user_id.is_none() && !settings.accounts.anonymous_mode_enabled {
        anyhow::bail!("anonymous posting is disabled");
    }
    if media_ids.len() > settings.posts.max_media_per_post {
        anyhow::bail!("too many media attachments");
    }
    let max_images_per_post = settings.posts.max_images_per_post;
    let max_videos_per_post = settings.posts.max_videos_per_post;
    let text = clean_post_text(text, settings.posts.max_text_chars, media_ids.len())?;
    let youtube_embeds = crate::youtube::metadata_for_text(&text).await;
    let allow_mentions = settings.posts.allow_mentions;
    let media_ids = media_ids.to_vec();
    pool.call(move |conn| {
        let tx = conn.transaction()?;
        let root_post_id = if let Some(parent_id) = parent_post_id {
            if let Some(actor_id) = user_id {
                ensure_post_interaction_accessible_tx(&tx, actor_id, parent_id)?;
            }
            let root = tx
                .query_row(
                    "SELECT COALESCE(root_post_id, id) FROM posts WHERE id = ? AND is_deleted = 0",
                    [parent_id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?;
            let Some(root) = root else {
                anyhow::bail!("parent post not found");
            };
            Some(root)
        } else {
            None
        };
        let (image_count, video_count) = media_kind_counts_tx(&tx, user_id, &media_ids)?;
        if image_count > max_images_per_post {
            anyhow::bail!("too many image attachments");
        }
        if video_count > max_videos_per_post {
            anyhow::bail!("too many video attachments");
        }
        let anonymous_label = user_id.is_none().then_some("Anonymous");
        tx.execute(
            "INSERT INTO posts (user_id, anonymous_label, text, parent_post_id, root_post_id) VALUES (?, ?, ?, ?, ?)",
            params![user_id, anonymous_label, text, parent_post_id, root_post_id],
        )?;
        let post_id = tx.last_insert_rowid();
        replace_youtube_embeds_tx(&tx, post_id, &youtube_embeds)?;
        for (position, media_id) in media_ids.iter().enumerate() {
            tx.execute(
                "INSERT INTO post_media (post_id, media_id, position) VALUES (?, ?, ?)",
                params![post_id, media_id, i64::try_from(position)?],
            )?;
        }
        let reply_owner = if let (Some(parent_id), Some(actor_id)) = (parent_post_id, user_id) {
            notify_post_owner_tx(
                &tx,
                parent_id,
                actor_id,
                post_id,
                "reply",
                "replied to your post",
            )?
        } else {
            None
        };
        if allow_mentions && let Some(actor_id) = user_id {
            notify_mentioned_users_tx(&tx, &text, actor_id, post_id, reply_owner)?;
        }
        tx.commit()?;
        Ok(post_id)
    })
    .await
}

pub(super) fn media_kind_counts_tx(
    tx: &rusqlite::Transaction<'_>,
    owner_user_id: Option<i64>,
    media_ids: &[i64],
) -> anyhow::Result<(usize, usize)> {
    let mut image_count = 0usize;
    let mut video_count = 0usize;
    for media_id in media_ids {
        let media_kind = tx
            .query_row(
                "SELECT media_kind FROM media WHERE id = ? AND owner_user_id IS ?",
                params![media_id, owner_user_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        match media_kind.as_deref() {
            Some("image") => image_count += 1,
            Some("video") => video_count += 1,
            Some(_) => anyhow::bail!("media attachment has unsupported kind"),
            None => anyhow::bail!("media attachment not found"),
        }
    }
    Ok((image_count, video_count))
}

pub async fn edit_post(
    pool: &SqlitePool,
    settings: &Settings,
    user_id: i64,
    post_id: i64,
    text: &str,
) -> Result<bool, EditPostError> {
    let max_text_chars = settings.posts.max_text_chars;
    let edit_window_modifier = edit_window_modifier(settings.posts.post_edit_window_seconds);
    let raw_text = text.to_owned();
    let edit_window_for_check = edit_window_modifier.clone();
    let row = pool
        .call(move |conn| {
            let row = conn
                .query_row(
                    r#"
                SELECT p.user_id, p.text,
                  CASE
                    WHEN ? IS NULL THEN 0
                    ELSE p.created_at >= datetime('now', ?)
                  END AS within_window,
                  (SELECT COUNT(*) FROM post_media WHERE post_id = p.id) AS media_count
                FROM posts p
                WHERE p.id = ? AND p.is_deleted = 0
                "#,
                    params![
                        edit_window_for_check.clone(),
                        edit_window_for_check,
                        post_id
                    ],
                    |row| {
                        Ok((
                            row.get::<_, Option<i64>>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)? != 0,
                            row.get::<_, i64>(3)?,
                        ))
                    },
                )
                .optional()?;
            let existing_youtube_embeds = if row.is_some() {
                youtube_embeds_for_post_conn(conn, post_id)?
            } else {
                Vec::new()
            };
            Ok(row.map(|(owner, text, within_window, media_count)| {
                (
                    owner,
                    text,
                    within_window,
                    media_count,
                    existing_youtube_embeds,
                )
            }))
        })
        .await
        .map_err(|err| EditPostError::Database(err.to_string()))?;
    let Some((owner, current_text, within_window, media_count, existing_youtube_embeds)) = row
    else {
        return Err(EditPostError::NotFound);
    };
    if owner != Some(user_id) {
        return Err(EditPostError::Forbidden);
    }
    if !within_window {
        return Err(EditPostError::WindowExpired);
    }
    let media_count = usize::try_from(media_count).unwrap_or(usize::MAX);
    let text = clean_post_text(&raw_text, max_text_chars, media_count)
        .map_err(|err| EditPostError::Validation(err.to_string()))?;
    if text == current_text {
        return Ok(false);
    }
    let mut youtube_embeds = crate::youtube::embeds_for_text(&text);
    reuse_stored_youtube_metadata(&mut youtube_embeds, &existing_youtube_embeds);
    let youtube_embeds = crate::youtube::metadata_for_embeds(youtube_embeds).await;
    pool.call(move |conn| {
        let tx = conn.transaction()?;
        let Some(edit_window_modifier) = edit_window_modifier else {
            tx.commit()?;
            return Ok(Err(EditPostError::WindowExpired));
        };
        let changed = tx.execute(
            r#"
            UPDATE posts
            SET text = ?, edited_at = CURRENT_TIMESTAMP
            WHERE id = ? AND user_id = ? AND is_deleted = 0 AND created_at >= datetime('now', ?)
            "#,
            params![text, post_id, user_id, edit_window_modifier],
        )?;
        if changed == 0 {
            tx.commit()?;
            return Ok(Err(EditPostError::WindowExpired));
        }
        replace_youtube_embeds_tx(&tx, post_id, &youtube_embeds)?;
        tx.commit()?;
        Ok(Ok(true))
    })
    .await
    .map_err(|err| EditPostError::Database(err.to_string()))?
}

pub(super) fn edit_window_modifier(seconds: u64) -> Option<String> {
    if seconds == 0 {
        return None;
    }
    i64::try_from(seconds)
        .ok()
        .map(|seconds| format!("-{seconds} seconds"))
}

pub async fn timeline(
    pool: &SqlitePool,
    viewer_id: Option<i64>,
    mode: &str,
    cursor: Option<i64>,
) -> anyhow::Result<Vec<PostView>> {
    let mut posts = post_events(pool, viewer_id, mode, cursor).await?;
    if mode != "bookmarks" {
        posts.extend(repost_events(pool, viewer_id, mode, None).await?);
    }
    sort_events_newest_first(&mut posts);
    posts.truncate(40);
    Ok(posts)
}

/// Orders merged post/repost events newest first.
///
/// Timestamps have second granularity, so the tie-break uses the numeric row
/// id rather than the `p:`/`r:` prefixed event id string (which would order
/// `p:9` before `p:10`).
pub(super) fn sort_events_newest_first(posts: &mut [PostView]) {
    posts.sort_by(|left, right| {
        right
            .event_created_at
            .cmp(&left.event_created_at)
            .then_with(|| event_sort_key(right).cmp(&event_sort_key(left)))
    });
}

pub(super) fn event_sort_key(post: &PostView) -> i64 {
    post.event_id
        .rsplit(':')
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

pub async fn profile_timeline(
    pool: &SqlitePool,
    viewer_id: Option<i64>,
    user_id: i64,
) -> anyhow::Result<Vec<PostView>> {
    profile_tab_timeline(pool, viewer_id, user_id, ProfileTimelineTab::Posts).await
}

pub async fn profile_tab_timeline(
    pool: &SqlitePool,
    viewer_id: Option<i64>,
    user_id: i64,
    tab: ProfileTimelineTab,
) -> anyhow::Result<Vec<PostView>> {
    if !profile_activity_visible(pool, viewer_id, user_id).await? {
        return Ok(Vec::new());
    }
    let mut posts = match tab {
        ProfileTimelineTab::Posts => post_events_for_user(pool, viewer_id, user_id).await?,
        ProfileTimelineTab::Replies => reply_events_for_user(pool, viewer_id, user_id).await?,
        ProfileTimelineTab::Media => media_events_for_user(pool, viewer_id, user_id).await?,
        ProfileTimelineTab::Likes => liked_events_for_user(pool, viewer_id, user_id).await?,
    };
    if matches!(tab, ProfileTimelineTab::Posts | ProfileTimelineTab::Media) {
        posts.extend(repost_events(pool, viewer_id, tab.repost_mode(), Some(user_id)).await?);
    }
    sort_events_newest_first(&mut posts);
    posts.truncate(40);
    Ok(posts)
}

pub async fn profile_activity_visible(
    pool: &SqlitePool,
    viewer_id: Option<i64>,
    user_id: i64,
) -> anyhow::Result<bool> {
    let Some(viewer_id) = viewer_id else {
        return Ok(true);
    };
    if viewer_id == user_id {
        return Ok(true);
    }
    pool.call(move |conn| {
        let hidden: i64 = conn.query_row(
            r#"
            SELECT
              EXISTS(SELECT 1 FROM blocks WHERE blocker_id = ? AND blocked_id = ?)
              OR EXISTS(SELECT 1 FROM mutes WHERE muter_id = ? AND muted_id = ?)
              OR EXISTS(SELECT 1 FROM blocks WHERE blocker_id = ? AND blocked_id = ?)
            "#,
            params![viewer_id, user_id, viewer_id, user_id, user_id, viewer_id],
            |row| row.get(0),
        )?;
        Ok(hidden == 0)
    })
    .await
}

pub async fn profile_pinned_post(
    pool: &SqlitePool,
    viewer_id: Option<i64>,
    user_id: i64,
) -> anyhow::Result<Option<PostView>> {
    let mut sql = base_post_query();
    sql.push_str(
        " AND p.id = (SELECT pinned_post_id FROM users WHERE id = ? AND is_deleted = 0) AND p.user_id = ?",
    );
    append_viewer_filters(&mut sql, "p.user_id", "p.text", viewer_id);
    sql.push_str(" LIMIT 1");
    let mut bindings = vec![user_id, user_id];
    push_viewer_filter_bindings(&mut bindings, viewer_id);
    let rows = pool
        .call(move |conn| query_post_rows(conn, &sql, params_from_iter(bindings)))
        .await?;
    let mut posts = rows_to_posts(pool, rows, viewer_id).await?;
    Ok(posts.pop())
}

pub async fn post_thread(
    pool: &SqlitePool,
    viewer_id: Option<i64>,
    post_id: i64,
) -> anyhow::Result<Vec<PostView>> {
    let root = pool
        .call(move |conn| {
            conn.query_row(
                "SELECT COALESCE(root_post_id, id) FROM posts WHERE id = ? AND is_deleted = 0",
                [post_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(Into::into)
        })
        .await?;
    let Some(root) = root else {
        return Ok(Vec::new());
    };
    let root_id = root;
    let mut sql = base_post_query();
    sql.push_str(" AND (p.id = ? OR p.root_post_id = ?)");
    append_viewer_filters(&mut sql, "p.user_id", "p.text", viewer_id);
    sql.push_str(" ORDER BY p.id ASC LIMIT 200");
    let mut bindings = vec![root_id, root_id];
    push_viewer_filter_bindings(&mut bindings, viewer_id);
    let rows = pool
        .call(move |conn| query_post_rows(conn, &sql, params_from_iter(bindings)))
        .await?;
    let posts = rows_to_posts(pool, rows, viewer_id).await?;
    // The requested post can be hidden by a block, mute, suspension, or
    // deletion while visible replies remain. Treat that as "not found" rather
    // than rendering an orphaned reply branch.
    if posts.iter().any(|post| post.id == post_id) {
        Ok(posts)
    } else {
        Ok(Vec::new())
    }
}

pub async fn repost(pool: &SqlitePool, user_id: i64, post_id: i64) -> anyhow::Result<bool> {
    pool.call(move |conn| {
        let tx = conn.transaction()?;
        ensure_post_interaction_accessible_tx(&tx, user_id, post_id)?;
        let owner: Option<i64> = tx
            .query_row(
                "SELECT user_id FROM posts WHERE id = ? AND is_deleted = 0",
                [post_id],
                |row| row.get(0),
            )
            .optional()?;
        if owner.is_none() {
            anyhow::bail!("post not found");
        }
        if owner == Some(user_id) {
            anyhow::bail!("cannot repost your own post");
        }
        let changed = tx.execute(
            "INSERT OR IGNORE INTO reposts (user_id, post_id) VALUES (?, ?)",
            params![user_id, post_id],
        )?;
        if changed == 0 {
            tx.commit()?;
            return Ok(false);
        }
        notify_post_owner_tx(
            &tx,
            post_id,
            user_id,
            post_id,
            "repost",
            "reposted your post",
        )?;
        tx.commit()?;
        Ok(true)
    })
    .await
}

pub async fn create_quote_post(
    pool: &SqlitePool,
    settings: &Settings,
    user_id: i64,
    quote_post_id: i64,
    text: &str,
) -> anyhow::Result<QuotePostOutcome> {
    let text = clean_post_text(text, settings.posts.max_text_chars, 0)?;
    let duplicate_text = text.clone();
    let existing_post_id = pool
        .call(move |conn| {
            let tx = conn.transaction()?;
            ensure_quote_target_accessible_tx(&tx, user_id, quote_post_id)?;
            let existing_post_id = tx
                .query_row(
                    r#"
                    SELECT id FROM posts
                    WHERE user_id = ? AND quote_post_id = ? AND text = ? AND is_deleted = 0
                    ORDER BY id DESC LIMIT 1
                    "#,
                    params![user_id, quote_post_id, duplicate_text],
                    |row| row.get(0),
                )
                .optional()?;
            tx.commit()?;
            Ok(existing_post_id)
        })
        .await?;
    if let Some(post_id) = existing_post_id {
        return Ok(QuotePostOutcome {
            post_id,
            created: false,
        });
    }

    let youtube_embeds = crate::youtube::metadata_for_text(&text).await;
    let allow_mentions = settings.posts.allow_mentions;
    pool.call(move |conn| {
        let tx = conn.transaction()?;
        ensure_quote_target_accessible_tx(&tx, user_id, quote_post_id)?;
        let changed = tx.execute(
            "INSERT OR IGNORE INTO posts (user_id, text, quote_post_id) VALUES (?, ?, ?)",
            params![user_id, text, quote_post_id],
        )?;
        let post_id = if changed > 0 {
            tx.last_insert_rowid()
        } else {
            tx.query_row(
                r#"
                SELECT id FROM posts
                WHERE user_id = ? AND quote_post_id = ? AND text = ? AND is_deleted = 0
                ORDER BY id DESC LIMIT 1
                "#,
                params![user_id, quote_post_id, text],
                |row| row.get(0),
            )?
        };
        if changed > 0 {
            replace_youtube_embeds_tx(&tx, post_id, &youtube_embeds)?;
            let quote_owner = notify_post_owner_tx(
                &tx,
                quote_post_id,
                user_id,
                post_id,
                "quote",
                "quoted your post",
            )?;
            if allow_mentions {
                notify_mentioned_users_tx(&tx, &text, user_id, post_id, quote_owner)?;
            }
        }
        tx.commit()?;
        Ok(QuotePostOutcome {
            post_id,
            created: changed > 0,
        })
    })
    .await
}

pub async fn quote_target_preview(
    pool: &SqlitePool,
    viewer_id: Option<i64>,
    post_id: i64,
) -> anyhow::Result<QuotePreview> {
    let previews = pool
        .call(move |conn| quote_previews(conn, viewer_id, &[post_id]))
        .await?;
    previews
        .get(&post_id)
        .filter(|preview| !preview.unavailable)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("post not found"))
}

pub async fn unrepost(pool: &SqlitePool, user_id: i64, post_id: i64) -> anyhow::Result<bool> {
    pool.call(move |conn| {
        let changed = conn.execute(
            "DELETE FROM reposts WHERE user_id = ? AND post_id = ?",
            params![user_id, post_id],
        )?;
        Ok(changed > 0)
    })
    .await
}
