//! Post actions for the server subsystem.

use super::*;

pub(super) async fn ensure_parent_post_exists(pool: &SqlitePool, parent_id: i64) -> AppResult<()> {
    let exists = pool
        .call(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT 1 FROM posts WHERE id = ? AND is_deleted = 0",
                    [parent_id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some())
        })
        .await?;
    if exists {
        Ok(())
    } else {
        Err(AppError::BadRequest(
            "reply target was not found; it may have been deleted".to_owned(),
        ))
    }
}

pub(super) async fn delete_preview(
    pool: &SqlitePool,
    actor_id: i64,
    is_admin: bool,
    post_id: i64,
) -> AppResult<DeletePreview> {
    let preview = pool
        .call(move |conn| {
            conn.query_row(
                r#"
                SELECT p.user_id, p.text, u.username, u.display_name, p.parent_post_id
                FROM posts p
                LEFT JOIN users u ON u.id = p.user_id
                WHERE p.id = ? AND p.is_deleted = 0
                "#,
                [post_id],
                |row| {
                    Ok((
                        row.get::<_, Option<i64>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<i64>>(4)?,
                    ))
                },
            )
            .optional()
            .map_err(Into::into)
        })
        .await?;
    let Some((owner, text, username, display_name, parent_post_id)) = preview else {
        return Err(AppError::NotFound);
    };
    if !is_admin && owner != Some(actor_id) {
        return Err(AppError::Forbidden);
    }
    Ok(DeletePreview {
        text,
        username,
        display_name,
        parent_post_id,
    })
}

pub(super) async fn edit_preview(
    pool: &SqlitePool,
    actor_id: i64,
    post_id: i64,
    edit_window_seconds: u64,
) -> AppResult<EditPreview> {
    let edit_window_modifier = edit_window_modifier(edit_window_seconds);
    let preview = pool
        .call(move |conn| {
            conn.query_row(
                r#"
                SELECT p.user_id, p.text, p.parent_post_id,
                  CASE
                    WHEN ? IS NULL THEN 0
                    ELSE p.created_at >= datetime('now', ?)
                  END AS within_window
                FROM posts p
                WHERE p.id = ? AND p.is_deleted = 0
                "#,
                params![edit_window_modifier.clone(), edit_window_modifier, post_id],
                |row| {
                    Ok((
                        row.get::<_, Option<i64>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, i64>(3)? != 0,
                    ))
                },
            )
            .optional()
            .map_err(Into::into)
        })
        .await?;
    let Some((owner, text, parent_post_id, within_window)) = preview else {
        return Err(AppError::NotFound);
    };
    if owner != Some(actor_id) {
        return Err(AppError::Forbidden);
    }
    if !within_window {
        return Err(AppError::BadRequest(
            "the edit window for this post has expired".to_owned(),
        ));
    }
    Ok(EditPreview {
        text,
        parent_post_id,
    })
}

pub(super) fn edit_window_modifier(seconds: u64) -> Option<String> {
    if seconds == 0 {
        return None;
    }
    i64::try_from(seconds)
        .ok()
        .map(|seconds| format!("-{seconds} seconds"))
}

pub(super) async fn post_is_reply(pool: &SqlitePool, post_id: i64) -> AppResult<bool> {
    Ok(pool
        .call(move |conn| {
            conn.query_row(
                "SELECT parent_post_id IS NOT NULL FROM posts WHERE id = ? AND is_deleted = 0",
                [post_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map(|value| value.unwrap_or(0) != 0)
            .map_err(Into::into)
        })
        .await?)
}

pub(super) fn redirect_to_post_anchor(
    headers: &HeaderMap,
    post_id: i64,
    is_reply: bool,
) -> Redirect {
    let target = anchored_return(headers, post_id, is_reply, "/home");
    Redirect::to(&target)
}

pub(super) fn enhanced_request(headers: &HeaderMap) -> bool {
    headers
        .get("x-rustpost-enhance")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == "1")
}

pub(super) async fn follow_action_response(
    pool: &SqlitePool,
    viewer_id: i64,
    profile_id: i64,
) -> AppResult<FollowActionResponse> {
    let relationship = social::profile_relationship(pool, Some(viewer_id), profile_id).await?;
    let (followers, following_count) = social::follow_counts(pool, profile_id).await?;
    let action = if relationship.following {
        format!("/users/{profile_id}/unfollow")
    } else if relationship.requested {
        format!("/users/{profile_id}/follow/cancel")
    } else {
        format!("/users/{profile_id}/follow")
    };
    Ok(FollowActionResponse {
        kind: "follow",
        user_id: profile_id,
        following: relationship.following,
        requested: relationship.requested,
        followers,
        following_count,
        action,
    })
}

pub(super) async fn post_action_response(
    pool: &SqlitePool,
    viewer_id: i64,
    post_id: i64,
) -> AppResult<PostActionResponse> {
    let state = pool
        .call(move |conn| {
            conn.query_row(
                r#"
                SELECT
                  (
                    SELECT COUNT(*)
                    FROM likes l
                    JOIN users u ON u.id = l.user_id AND u.is_deleted = 0
                    WHERE l.post_id = p.id AND (u.liked_posts_public != 0 OR l.user_id = ?)
                  ),
                  ((SELECT COUNT(*) FROM reposts WHERE post_id = p.id) +
                   (SELECT COUNT(*) FROM posts qp WHERE qp.quote_post_id = p.id AND qp.is_deleted = 0)),
                  (SELECT COUNT(*) FROM posts replies WHERE replies.parent_post_id = p.id AND replies.is_deleted = 0),
                  EXISTS(SELECT 1 FROM likes WHERE user_id = ? AND post_id = p.id),
                  EXISTS(SELECT 1 FROM bookmarks WHERE user_id = ? AND post_id = p.id),
                  EXISTS(SELECT 1 FROM reposts WHERE user_id = ? AND post_id = p.id)
                FROM posts p
                LEFT JOIN users author ON author.id = p.user_id
                WHERE p.id = ? AND p.is_deleted = 0
                  AND (p.user_id IS NULL OR (author.is_deleted = 0 AND author.is_suspended = 0))
                  AND (
                    p.user_id IS NULL
                    OR p.user_id = ?
                    OR (
                      p.user_id NOT IN (SELECT blocked_id FROM blocks WHERE blocker_id = ?)
                      AND NOT EXISTS (SELECT 1 FROM blocks WHERE blocker_id = p.user_id AND blocked_id = ?)
                    )
                  )
                "#,
                params![
                    viewer_id, viewer_id, viewer_id, viewer_id, post_id, viewer_id, viewer_id,
                    viewer_id
                ],
                |row| {
                    Ok(PostActionResponse {
                        kind: "post-action",
                        post_id,
                        likes: row.get(0)?,
                        reposts: row.get(1)?,
                        replies: row.get(2)?,
                        liked: row.get::<_, i64>(3)? != 0,
                        bookmarked: row.get::<_, i64>(4)? != 0,
                        reposted: row.get::<_, i64>(5)? != 0,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
        })
        .await?;
    state.ok_or(AppError::NotFound)
}

pub(super) async fn account_action_return(
    pool: &SqlitePool,
    headers: &HeaderMap,
    profile_id: i64,
) -> AppResult<String> {
    if let Some(target) = referer_target(headers)
        .as_deref()
        .and_then(safe_return_target)
    {
        return Ok(target);
    }
    Ok(user_profile_path(pool, profile_id)
        .await?
        .unwrap_or_else(|| "/home".to_owned()))
}

pub(super) async fn user_profile_path(
    pool: &SqlitePool,
    user_id: i64,
) -> AppResult<Option<String>> {
    Ok(pool
        .call(move |conn| {
            conn.query_row(
                "SELECT username FROM users WHERE id = ? AND is_deleted = 0",
                [user_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(Into::into)
        })
        .await?
        .map(|username| format!("/users/{username}")))
}

pub(super) fn anchored_return(
    headers: &HeaderMap,
    post_id: i64,
    is_reply: bool,
    fallback: &str,
) -> String {
    let anchor = if is_reply {
        format!("reply-{post_id}")
    } else {
        format!("post-{post_id}")
    };
    let base = referer_target(headers)
        .as_deref()
        .and_then(safe_return_target)
        .unwrap_or_else(|| fallback.to_owned());
    let base = base.split('#').next().unwrap_or(fallback);
    format!("{base}#{anchor}")
}

pub(super) fn delete_return_fallback(
    headers: &HeaderMap,
    post_id: i64,
    parent_post_id: Option<i64>,
) -> String {
    let fallback = anchored_return(headers, post_id, parent_post_id.is_some(), "/home");
    if safe_delete_return_target(&fallback, post_id).is_some() {
        return fallback;
    }
    parent_post_id.map_or_else(
        || format!("/home#post-{post_id}"),
        |parent_id| format!("/posts/{parent_id}#post-{parent_id}"),
    )
}

pub(super) fn edit_return_fallback(
    headers: &HeaderMap,
    post_id: i64,
    parent_post_id: Option<i64>,
) -> String {
    let fallback = anchored_return(headers, post_id, parent_post_id.is_some(), "/home");
    if safe_edit_return_target(&fallback, post_id).is_some() {
        return fallback;
    }
    parent_post_id.map_or_else(
        || format!("/posts/{post_id}"),
        |parent_id| format!("/posts/{parent_id}#reply-{post_id}"),
    )
}

pub(super) fn safe_edit_return_target(value: &str, post_id: i64) -> Option<String> {
    let target = safe_return_target(value)?;
    let self_path = format!("/posts/{post_id}/edit");
    if path_without_query_or_fragment(&target) == self_path {
        None
    } else {
        Some(target)
    }
}

pub(super) fn safe_delete_return_target(value: &str, post_id: i64) -> Option<String> {
    let target = safe_return_target(value)?;
    let self_path = format!("/posts/{post_id}/delete");
    let thread_path = format!("/posts/{post_id}");
    let path = path_without_query_or_fragment(&target);
    if path == self_path || path == thread_path {
        None
    } else {
        Some(target)
    }
}

pub(super) fn parse_notification_ids(value: &str) -> AppResult<Vec<i64>> {
    if value.len() > 1024 {
        return Err(AppError::BadRequest(
            "notification group is too large".to_owned(),
        ));
    }
    let mut ids = Vec::new();
    for part in value.split(',') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        let id = trimmed
            .parse::<i64>()
            .map_err(|_err| AppError::BadRequest("notification group is invalid".to_owned()))?;
        if id <= 0 {
            return Err(AppError::BadRequest(
                "notification group is invalid".to_owned(),
            ));
        }
        if !ids.contains(&id) {
            ids.push(id);
        }
        if ids.len() > 80 {
            return Err(AppError::BadRequest(
                "notification group is too large".to_owned(),
            ));
        }
    }
    if ids.is_empty() {
        return Err(AppError::BadRequest(
            "notification group is invalid".to_owned(),
        ));
    }
    Ok(ids)
}

pub(super) fn parse_notification_group_target(value: Option<&str>) -> AppResult<Option<i64>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let target = trimmed
        .parse::<i64>()
        .map_err(|_err| AppError::BadRequest("notification group target is invalid".to_owned()))?;
    if target <= 0 {
        return Err(AppError::BadRequest(
            "notification group target is invalid".to_owned(),
        ));
    }
    Ok(Some(target))
}

pub(super) fn path_without_query_or_fragment(target: &str) -> &str {
    target.split(['?', '#']).next().unwrap_or_default()
}

pub(super) fn safe_return_target(value: &str) -> Option<String> {
    let target = if value.starts_with('/') && !value.starts_with("//") {
        value.to_owned()
    } else {
        value.parse::<Uri>().ok().and_then(|uri| {
            uri.path_and_query()
                .map(|path| path.as_str().to_owned())
                .filter(|path| path.starts_with('/'))
        })?
    };
    let path = target.split('#').next().unwrap_or_default();
    if matches!(
        path,
        "/home" | "/bookmarks" | "/notifications" | "/search" | "/"
    ) || path.starts_with("/posts/")
        || path.starts_with("/users/")
        || path.starts_with("/tags/")
    {
        Some(target)
    } else {
        None
    }
}

pub(super) fn referer_target(headers: &HeaderMap) -> Option<String> {
    let value = headers
        .get(header::REFERER)
        .and_then(|value| value.to_str().ok())?;
    if value.starts_with('/') && !value.starts_with("//") {
        Some(value.to_owned())
    } else {
        let uri = value.parse::<Uri>().ok()?;
        uri.path_and_query()
            .map(|path| path.as_str().to_owned())
            .filter(|path| path.starts_with('/'))
    }
}
