use std::collections::BTreeSet;

use rusqlite::{Connection, OptionalExtension as _, Row, params, params_from_iter};

use crate::config::Settings;
use crate::db::SqlitePool;
use crate::validation::clean_post_text;

#[derive(Debug, Clone)]
pub struct AccountView {
    pub id: i64,
    pub username: String,
    pub display_name: String,
    pub bio: String,
    pub profile_picture_path: Option<String>,
    pub viewer_following: bool,
}

#[derive(Debug, Clone)]
pub struct PostView {
    pub event_id: String,
    pub event_kind: TimelineEventKind,
    pub id: i64,
    pub user_id: Option<i64>,
    pub username: Option<String>,
    pub display_name: Option<String>,
    pub profile_picture_path: Option<String>,
    pub anonymous_label: Option<String>,
    pub text: String,
    pub parent_post_id: Option<i64>,
    pub created_at: String,
    pub edited_at: Option<String>,
    pub event_created_at: String,
    pub like_count: i64,
    pub repost_count: i64,
    pub reply_count: i64,
    pub viewer_liked: bool,
    pub viewer_bookmarked: bool,
    pub viewer_reposted: bool,
    pub viewer_can_repost: bool,
    pub pinned_by_author: bool,
    pub original_unavailable: bool,
    pub reposted_by_user_id: Option<i64>,
    pub reposted_by_username: Option<String>,
    pub reposted_by_display_name: Option<String>,
    pub reposted_at: Option<String>,
    pub quote: Option<QuotePreview>,
    pub media: Vec<MediaView>,
}

#[derive(Debug, Clone)]
pub struct QuotePreview {
    pub id: i64,
    pub username: Option<String>,
    pub display_name: Option<String>,
    pub anonymous_label: Option<String>,
    pub text: String,
    pub created_at: String,
    pub unavailable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimelineEventKind {
    Post,
    Repost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileTimelineTab {
    Posts,
    Replies,
    Media,
    Likes,
}

impl ProfileTimelineTab {
    const fn repost_mode(self) -> &'static str {
        match self {
            Self::Media => "profile_media",
            Self::Posts | Self::Replies | Self::Likes => "profile",
        }
    }
}

#[derive(Debug, Clone)]
pub struct MediaView {
    pub public_path: String,
    pub mime_type: String,
    pub media_kind: String,
    pub alt_text: String,
    pub is_nsfw: bool,
}

#[derive(Debug, Clone)]
pub struct MutedWord {
    pub id: i64,
    pub term: String,
}

#[derive(Debug, Clone)]
pub struct NotificationView {
    pub id: i64,
    pub kind: String,
    pub actor_user_id: Option<i64>,
    pub actor_username: Option<String>,
    pub actor_display_name: Option<String>,
    pub post_id: Option<i64>,
    pub group_target_post_id: Option<i64>,
    pub post_text: Option<String>,
    pub post_available: bool,
    pub read_at: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionSuggestion {
    pub username: String,
    pub display_name: String,
}

#[derive(Debug, Clone)]
pub struct NotificationActorView {
    pub user_id: Option<i64>,
    pub username: Option<String>,
    pub display_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NotificationGroupView {
    pub id: i64,
    pub kind: String,
    pub group_target_post_id: Option<i64>,
    pub post_id: Option<i64>,
    pub post_text: Option<String>,
    pub post_available: bool,
    pub unread_count: i64,
    pub total_count: usize,
    pub notification_ids: Vec<i64>,
    pub actors: Vec<NotificationActorView>,
    pub created_at: String,
}

#[derive(Debug)]
struct PostRow {
    event_kind: String,
    event_id: String,
    event_created_at: String,
    repost_user_id: Option<i64>,
    repost_username: Option<String>,
    repost_display_name: Option<String>,
    repost_created_at: Option<String>,
    original_unavailable: bool,
    id: i64,
    user_id: Option<i64>,
    username: Option<String>,
    display_name: Option<String>,
    profile_picture_path: Option<String>,
    anonymous_label: Option<String>,
    text: String,
    parent_post_id: Option<i64>,
    created_at: String,
    edited_at: Option<String>,
    like_count: i64,
    repost_count: i64,
    reply_count: i64,
    quote_post_id: Option<i64>,
    pinned_by_author: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotePostOutcome {
    pub post_id: i64,
    pub created: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditPostError {
    NotFound,
    Forbidden,
    WindowExpired,
    Validation(String),
    Database(String),
}

impl std::fmt::Display for EditPostError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => formatter.write_str("post not found"),
            Self::Forbidden => formatter.write_str("cannot edit this post"),
            Self::WindowExpired => formatter.write_str("the edit window for this post has expired"),
            Self::Validation(message) | Self::Database(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for EditPostError {}

impl From<rusqlite::Error> for EditPostError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinPostError {
    NotFound,
    Forbidden,
    Database(String),
}

impl std::fmt::Display for PinPostError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => formatter.write_str("post not found"),
            Self::Forbidden => formatter.write_str("cannot pin this post"),
            Self::Database(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for PinPostError {}

impl From<rusqlite::Error> for PinPostError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error.to_string())
    }
}

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
    let text = clean_post_text(text, settings.posts.max_text_chars, media_ids.len())?;
    let allow_mentions = settings.posts.allow_mentions;
    let media_ids = media_ids.to_vec();
    pool.call(move |conn| {
        let tx = conn.transaction()?;
        let root_post_id = if let Some(parent_id) = parent_post_id {
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
        let anonymous_label = user_id.is_none().then_some("Anonymous");
        tx.execute(
            "INSERT INTO posts (user_id, anonymous_label, text, parent_post_id, root_post_id) VALUES (?, ?, ?, ?, ?)",
            params![user_id, anonymous_label, text, parent_post_id, root_post_id],
        )?;
        let post_id = tx.last_insert_rowid();
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
    pool.call(move |conn| {
        let result: Result<bool, EditPostError> = (|| {
            let tx = conn.transaction()?;
            let row = tx
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
                        edit_window_modifier.clone(),
                        edit_window_modifier,
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
            let Some((owner, current_text, within_window, media_count)) = row else {
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
                tx.commit()?;
                return Ok(false);
            }
            tx.execute(
                "UPDATE posts SET text = ?, edited_at = CURRENT_TIMESTAMP WHERE id = ? AND user_id = ? AND is_deleted = 0",
                params![text, post_id, user_id],
            )?;
            tx.commit()?;
            Ok(true)
        })();
        Ok(result)
    })
    .await
    .map_err(|err| EditPostError::Database(err.to_string()))?
}

fn edit_window_modifier(seconds: u64) -> Option<String> {
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
    posts.sort_by(|left, right| {
        right
            .event_created_at
            .cmp(&left.event_created_at)
            .then_with(|| right.event_id.cmp(&left.event_id))
    });
    posts.truncate(40);
    Ok(posts)
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
    let mut posts = match tab {
        ProfileTimelineTab::Posts => post_events_for_user(pool, viewer_id, user_id).await?,
        ProfileTimelineTab::Replies => reply_events_for_user(pool, viewer_id, user_id).await?,
        ProfileTimelineTab::Media => media_events_for_user(pool, viewer_id, user_id).await?,
        ProfileTimelineTab::Likes => liked_events_for_user(pool, viewer_id, user_id).await?,
    };
    if matches!(tab, ProfileTimelineTab::Posts | ProfileTimelineTab::Media) {
        posts.extend(repost_events(pool, viewer_id, tab.repost_mode(), Some(user_id)).await?);
    }
    posts.sort_by(|left, right| {
        right
            .event_created_at
            .cmp(&left.event_created_at)
            .then_with(|| right.event_id.cmp(&left.event_id))
    });
    posts.truncate(40);
    Ok(posts)
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
    append_viewer_filters(&mut sql, "p.user_id", viewer_id);
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
    sql.push_str(" AND (p.id = ? OR p.root_post_id = ?) ORDER BY p.id ASC LIMIT 200");
    let rows = pool
        .call(move |conn| query_post_rows(conn, &sql, params![root_id, root_id]))
        .await?;
    rows_to_posts(pool, rows, viewer_id).await
}

pub async fn repost(pool: &SqlitePool, user_id: i64, post_id: i64) -> anyhow::Result<bool> {
    pool.call(move |conn| {
        let tx = conn.transaction()?;
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
    let preview = quote_preview_for_post(pool, Some(post_id), viewer_id)
        .await?
        .filter(|preview| !preview.unavailable);
    preview.ok_or_else(|| anyhow::anyhow!("post not found"))
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

pub async fn follow(pool: &SqlitePool, follower_id: i64, followed_id: i64) -> anyhow::Result<bool> {
    if follower_id == followed_id {
        anyhow::bail!("cannot follow yourself");
    }
    pool.call(move |conn| {
        let tx = conn.transaction()?;
        let target_available = tx
            .query_row(
                "SELECT is_deleted = 0 AND is_suspended = 0 FROM users WHERE id = ?",
                [followed_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0)
            != 0;
        if !target_available {
            anyhow::bail!("account cannot be followed");
        }
        let changed = tx.execute(
            "INSERT OR IGNORE INTO follows (follower_id, followed_id) VALUES (?, ?)",
            params![follower_id, followed_id],
        )?;
        if changed > 0 {
            create_notification_tx(
                &tx,
                followed_id,
                Some(follower_id),
                None,
                "follow",
                "followed you",
            )?;
        }
        tx.commit()?;
        Ok(changed > 0)
    })
    .await
}

pub async fn active_follow_targets(pool: &SqlitePool, ids: &[i64]) -> anyhow::Result<Vec<i64>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = vec!["?"; ids.len()].join(", ");
    let sql = format!(
        "SELECT id FROM users WHERE id IN ({placeholders}) AND is_deleted = 0 AND is_suspended = 0"
    );
    let candidate_ids = ids.to_vec();
    let query_ids = candidate_ids.clone();
    let available = pool
        .call(move |conn| {
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map(params_from_iter(query_ids.iter()), |row| {
                    row.get::<_, i64>(0)
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?
        .into_iter()
        .collect::<BTreeSet<_>>();
    Ok(candidate_ids
        .iter()
        .copied()
        .filter(|id| available.contains(id))
        .collect())
}

pub async fn unfollow(
    pool: &SqlitePool,
    follower_id: i64,
    followed_id: i64,
) -> anyhow::Result<bool> {
    pool.call(move |conn| {
        let changed = conn.execute(
            "DELETE FROM follows WHERE follower_id = ? AND followed_id = ?",
            params![follower_id, followed_id],
        )?;
        Ok(changed > 0)
    })
    .await
}

pub async fn is_following(
    pool: &SqlitePool,
    follower_id: i64,
    followed_id: i64,
) -> anyhow::Result<bool> {
    pool.call(move |conn| {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM follows WHERE follower_id = ? AND followed_id = ?",
            params![follower_id, followed_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    })
    .await
}

pub async fn follow_counts(pool: &SqlitePool, user_id: i64) -> anyhow::Result<(i64, i64)> {
    pool.call(move |conn| {
        let followers = conn.query_row(
            "SELECT COUNT(*) FROM follows f JOIN users u ON u.id = f.follower_id WHERE f.followed_id = ? AND u.is_deleted = 0",
            [user_id],
            |row| row.get(0),
        )?;
        let following = conn.query_row(
            "SELECT COUNT(*) FROM follows f JOIN users u ON u.id = f.followed_id WHERE f.follower_id = ? AND u.is_deleted = 0",
            [user_id],
            |row| row.get(0),
        )?;
        Ok((followers, following))
    })
    .await
}

pub async fn instance_counts(pool: &SqlitePool) -> anyhow::Result<(i64, i64)> {
    pool.call(|conn| {
        let users = conn.query_row(
            "SELECT COUNT(*) FROM users WHERE is_deleted = 0",
            [],
            |row| row.get(0),
        )?;
        let posts = conn.query_row(
            "SELECT COUNT(*) FROM posts WHERE is_deleted = 0",
            [],
            |row| row.get(0),
        )?;
        Ok((users, posts))
    })
    .await
}

pub async fn following_accounts(
    pool: &SqlitePool,
    viewer_id: i64,
) -> anyhow::Result<Vec<AccountView>> {
    pool.call(move |conn| {
        let mut stmt = conn.prepare(
            r#"
            SELECT u.id, u.username, u.display_name, u.bio,
              COALESCE(pic.thumbnail_public_path, pic.public_path)
            FROM follows f
            JOIN users u ON u.id = f.followed_id
            LEFT JOIN media pic ON pic.id = u.profile_picture_media_id
            WHERE f.follower_id = ? AND u.is_deleted = 0
            ORDER BY lower(u.username)
            "#,
        )?;
        let rows = stmt
            .query_map([viewer_id], |row| {
                Ok(AccountView {
                    id: row.get(0)?,
                    username: row.get(1)?,
                    display_name: row.get(2)?,
                    bio: row.get(3)?,
                    profile_picture_path: row.get(4)?,
                    viewer_following: true,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}

pub async fn onboarding_suggestions(
    pool: &SqlitePool,
    viewer_id: i64,
    limit: usize,
) -> anyhow::Result<Vec<AccountView>> {
    let limit = i64::try_from(limit)?;
    pool.call(move |conn| {
        let mut stmt = conn.prepare(
            r#"
            SELECT u.id, u.username, u.display_name, u.bio,
              COALESCE(pic.thumbnail_public_path, pic.public_path),
              EXISTS(
                SELECT 1 FROM follows vf
                WHERE vf.follower_id = ? AND vf.followed_id = u.id
              )
            FROM users u
            LEFT JOIN media pic ON pic.id = u.profile_picture_media_id
            WHERE u.id != ?
              AND u.is_deleted = 0
              AND u.is_suspended = 0
            ORDER BY lower(u.username), u.id
            LIMIT ?
            "#,
        )?;
        let rows = stmt
            .query_map(params![viewer_id, viewer_id, limit], |row| {
                Ok(AccountView {
                    id: row.get(0)?,
                    username: row.get(1)?,
                    display_name: row.get(2)?,
                    bio: row.get(3)?,
                    profile_picture_path: row.get(4)?,
                    viewer_following: row.get::<_, i64>(5)? != 0,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}

pub async fn followers_accounts(
    pool: &SqlitePool,
    account_id: i64,
    viewer_id: Option<i64>,
) -> anyhow::Result<Vec<AccountView>> {
    let viewer_id = viewer_id.unwrap_or(0);
    pool.call(move |conn| {
        let mut stmt = conn.prepare(
            r#"
            SELECT u.id, u.username, u.display_name, u.bio,
              COALESCE(pic.thumbnail_public_path, pic.public_path),
              EXISTS(
                SELECT 1 FROM follows vf
                WHERE vf.follower_id = ? AND vf.followed_id = u.id
              )
            FROM follows f
            JOIN users u ON u.id = f.follower_id
            LEFT JOIN media pic ON pic.id = u.profile_picture_media_id
            WHERE f.followed_id = ? AND u.is_deleted = 0
            ORDER BY lower(u.username)
            "#,
        )?;
        let rows = stmt
            .query_map(params![viewer_id, account_id], |row| {
                Ok(AccountView {
                    id: row.get(0)?,
                    username: row.get(1)?,
                    display_name: row.get(2)?,
                    bio: row.get(3)?,
                    profile_picture_path: row.get(4)?,
                    viewer_following: row.get::<_, i64>(5)? != 0,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}

pub async fn following_accounts_for_profile(
    pool: &SqlitePool,
    account_id: i64,
    viewer_id: Option<i64>,
) -> anyhow::Result<Vec<AccountView>> {
    let viewer_id = viewer_id.unwrap_or(0);
    pool.call(move |conn| {
        let mut stmt = conn.prepare(
            r#"
            SELECT u.id, u.username, u.display_name, u.bio,
              COALESCE(pic.thumbnail_public_path, pic.public_path),
              EXISTS(
                SELECT 1 FROM follows vf
                WHERE vf.follower_id = ? AND vf.followed_id = u.id
              )
            FROM follows f
            JOIN users u ON u.id = f.followed_id
            LEFT JOIN media pic ON pic.id = u.profile_picture_media_id
            WHERE f.follower_id = ? AND u.is_deleted = 0
            ORDER BY lower(u.username)
            "#,
        )?;
        let rows = stmt
            .query_map(params![viewer_id, account_id], |row| {
                Ok(AccountView {
                    id: row.get(0)?,
                    username: row.get(1)?,
                    display_name: row.get(2)?,
                    bio: row.get(3)?,
                    profile_picture_path: row.get(4)?,
                    viewer_following: row.get::<_, i64>(5)? != 0,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}

pub async fn block(pool: &SqlitePool, blocker_id: i64, blocked_id: i64) -> anyhow::Result<()> {
    if blocker_id == blocked_id {
        anyhow::bail!("cannot block yourself");
    }
    pool.call(move |conn| {
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO blocks (blocker_id, blocked_id) VALUES (?, ?)",
            params![blocker_id, blocked_id],
        )?;
        tx.execute(
            "DELETE FROM follows WHERE (follower_id = ? AND followed_id = ?) OR (follower_id = ? AND followed_id = ?)",
            params![blocker_id, blocked_id, blocked_id, blocker_id],
        )?;
        tx.commit()?;
        Ok(())
    })
    .await
}

pub async fn unblock(pool: &SqlitePool, blocker_id: i64, blocked_id: i64) -> anyhow::Result<()> {
    pool.call(move |conn| {
        conn.execute(
            "DELETE FROM blocks WHERE blocker_id = ? AND blocked_id = ?",
            params![blocker_id, blocked_id],
        )?;
        Ok(())
    })
    .await
}

pub async fn blocked_users(
    pool: &SqlitePool,
    blocker_id: i64,
) -> anyhow::Result<Vec<(i64, String, String)>> {
    pool.call(move |conn| {
        let mut stmt = conn.prepare(
            r#"
            SELECT u.id, u.username, u.display_name
            FROM blocks b
            JOIN users u ON u.id = b.blocked_id
            WHERE b.blocker_id = ? AND u.is_deleted = 0
            ORDER BY lower(u.username)
            "#,
        )?;
        let rows = stmt
            .query_map([blocker_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}

pub async fn mute(pool: &SqlitePool, muter_id: i64, muted_id: i64) -> anyhow::Result<()> {
    if muter_id == muted_id {
        anyhow::bail!("cannot mute yourself");
    }
    pool.call(move |conn| {
        conn.execute(
            "INSERT OR IGNORE INTO mutes (muter_id, muted_id) VALUES (?, ?)",
            params![muter_id, muted_id],
        )?;
        Ok(())
    })
    .await
}

pub async fn unmute(pool: &SqlitePool, muter_id: i64, muted_id: i64) -> anyhow::Result<()> {
    pool.call(move |conn| {
        conn.execute(
            "DELETE FROM mutes WHERE muter_id = ? AND muted_id = ?",
            params![muter_id, muted_id],
        )?;
        Ok(())
    })
    .await
}

pub async fn muted_users(
    pool: &SqlitePool,
    muter_id: i64,
) -> anyhow::Result<Vec<(i64, String, String)>> {
    pool.call(move |conn| {
        let mut stmt = conn.prepare(
            r#"
            SELECT u.id, u.username, u.display_name
            FROM mutes m
            JOIN users u ON u.id = m.muted_id
            WHERE m.muter_id = ? AND u.is_deleted = 0
            ORDER BY lower(u.username)
            "#,
        )?;
        let rows = stmt
            .query_map([muter_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}

pub async fn add_muted_word(pool: &SqlitePool, user_id: i64, term: &str) -> anyhow::Result<()> {
    let term = clean_muted_word(term)?;
    let normalized = term.to_ascii_lowercase();
    pool.call(move |conn| {
        conn.execute(
            "INSERT OR IGNORE INTO muted_words (user_id, term, normalized_term) VALUES (?, ?, ?)",
            params![user_id, term, normalized],
        )?;
        Ok(())
    })
    .await
}

pub async fn remove_muted_word(
    pool: &SqlitePool,
    user_id: i64,
    muted_word_id: i64,
) -> anyhow::Result<()> {
    pool.call(move |conn| {
        conn.execute(
            "DELETE FROM muted_words WHERE user_id = ? AND id = ?",
            params![user_id, muted_word_id],
        )?;
        Ok(())
    })
    .await
}

pub async fn muted_words(pool: &SqlitePool, user_id: i64) -> anyhow::Result<Vec<MutedWord>> {
    pool.call(move |conn| {
        let mut stmt = conn
            .prepare("SELECT id, term FROM muted_words WHERE user_id = ? ORDER BY lower(term)")?;
        let rows = stmt
            .query_map([user_id], |row| {
                Ok(MutedWord {
                    id: row.get(0)?,
                    term: row.get(1)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}

fn clean_muted_word(term: &str) -> anyhow::Result<String> {
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
        conn.execute(
            "INSERT OR IGNORE INTO bookmarks (user_id, post_id) VALUES (?, ?)",
            params![user_id, post_id],
        )?;
        Ok(())
    })
    .await
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
    let owner: Option<i64> = pool
        .call(move |conn| {
            conn.query_row(
                "SELECT user_id FROM posts WHERE id = ? AND is_deleted = 0",
                [post_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
        })
        .await?;
    if !is_admin && owner != Some(actor_id) {
        anyhow::bail!("cannot delete this post");
    }
    pool.call(move |conn| {
        conn.execute(
            "UPDATE posts SET is_deleted = 1, deleted_at = CURRENT_TIMESTAMP WHERE id = ?",
            [post_id],
        )?;
        conn.execute(
            "UPDATE users SET pinned_post_id = NULL, updated_at = CURRENT_TIMESTAMP WHERE pinned_post_id = ?",
            [post_id],
        )?;
        Ok(())
    })
    .await
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

pub async fn search(
    pool: &SqlitePool,
    viewer_id: Option<i64>,
    query: &str,
) -> anyhow::Result<(Vec<AccountView>, Vec<PostView>)> {
    let user_query = user_query_from_input(query);
    let users = pool
        .call(move |conn| {
            let Some(user_query) = user_query else {
                return Ok(Vec::new());
            };
            let username_query = format!("%{}%", user_query.to_ascii_lowercase());
            let display_query = format!("%{user_query}%");
            let viewer_id = viewer_id.unwrap_or(-1);
            let mut stmt = conn.prepare(
                r#"
                SELECT u.id, u.username, u.display_name, u.bio,
                  COALESCE(pic.thumbnail_public_path, pic.public_path),
                  EXISTS(SELECT 1 FROM follows WHERE follower_id = ? AND followed_id = u.id)
                FROM users u
                LEFT JOIN media pic ON pic.id = u.profile_picture_media_id
                WHERE u.is_deleted = 0
                  AND (u.normalized_username LIKE ? OR u.display_name LIKE ?)
                ORDER BY lower(u.username)
                LIMIT 20
                "#,
            )?;
            let rows = stmt
                .query_map(params![viewer_id, username_query, display_query], |row| {
                    Ok(AccountView {
                        id: row.get(0)?,
                        username: row.get(1)?,
                        display_name: row.get(2)?,
                        bio: row.get(3)?,
                        profile_picture_path: row.get(4)?,
                        viewer_following: row.get::<_, i64>(5)? != 0,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?;
    let rows = if let Some(fts_query) = fts_query_from_user_input(query) {
        let mut post_sql = base_post_query();
        post_sql.push_str(" AND p.id IN (SELECT rowid FROM posts_fts WHERE posts_fts MATCH ?)");
        append_viewer_filters(&mut post_sql, "p.user_id", viewer_id);
        post_sql.push_str(" LIMIT 40");
        pool.call(move |conn| {
            if let Some(viewer_id) = viewer_id {
                query_post_rows(
                    conn,
                    &post_sql,
                    params![fts_query, viewer_id, viewer_id, viewer_id],
                )
            } else {
                query_post_rows(conn, &post_sql, params![fts_query])
            }
        })
        .await?
    } else {
        Vec::new()
    };
    Ok((users, rows_to_posts(pool, rows, viewer_id).await?))
}

const MENTION_SUGGESTION_LIMIT: usize = 8;
const MENTION_QUERY_MAX_CHARS: usize = 32;

pub async fn mention_suggestions(
    pool: &SqlitePool,
    viewer_id: Option<i64>,
    query: &str,
) -> anyhow::Result<Vec<MentionSuggestion>> {
    let Some(fragment) = mention_query_fragment(query) else {
        return Ok(Vec::new());
    };
    let username_query = format!("{}%", escape_like_prefix(&fragment));
    let limit = i64::try_from(MENTION_SUGGESTION_LIMIT)?;
    pool.call(move |conn| {
        if let Some(viewer_id) = viewer_id {
            let mut stmt = conn.prepare(
                r#"
                SELECT username, display_name
                FROM users u
                WHERE u.is_deleted = 0
                  AND u.is_suspended = 0
                  AND u.normalized_username LIKE ? ESCAPE '\'
                  AND u.id NOT IN (SELECT blocked_id FROM blocks WHERE blocker_id = ?)
                  AND u.id NOT IN (SELECT muted_id FROM mutes WHERE muter_id = ?)
                  AND NOT EXISTS (
                    SELECT 1 FROM blocks
                    WHERE blocker_id = u.id AND blocked_id = ?
                  )
                ORDER BY u.normalized_username, u.id
                LIMIT ?
                "#,
            )?;
            let rows = stmt
                .query_map(
                    params![username_query, viewer_id, viewer_id, viewer_id, limit],
                    |row| {
                        Ok(MentionSuggestion {
                            username: row.get(0)?,
                            display_name: row.get(1)?,
                        })
                    },
                )?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        } else {
            let mut stmt = conn.prepare(
                r#"
                SELECT username, display_name
                FROM users u
                WHERE u.is_deleted = 0
                  AND u.is_suspended = 0
                  AND u.normalized_username LIKE ? ESCAPE '\'
                ORDER BY u.normalized_username, u.id
                LIMIT ?
                "#,
            )?;
            let rows = stmt
                .query_map(params![username_query, limit], |row| {
                    Ok(MentionSuggestion {
                        username: row.get(0)?,
                        display_name: row.get(1)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        }
    })
    .await
}

fn mention_query_fragment(query: &str) -> Option<String> {
    let fragment = query.trim().trim_start_matches('@');
    if fragment.chars().any(|character| {
        !(character.is_ascii_alphanumeric() || character == '_' || character == '-')
    }) {
        return None;
    }
    Some(
        fragment
            .chars()
            .take(MENTION_QUERY_MAX_CHARS)
            .collect::<String>()
            .to_ascii_lowercase(),
    )
}

fn escape_like_prefix(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(character, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn user_query_from_input(query: &str) -> Option<String> {
    let trimmed = query.trim().trim_start_matches('@').trim();
    let value = trimmed
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_matches(|character: char| {
            !(character.is_alphanumeric() || character == '_' || character == '-')
        });
    (!value.is_empty()).then(|| value.to_owned())
}

fn fts_query_from_user_input(query: &str) -> Option<String> {
    let terms = query
        .split_whitespace()
        .flat_map(|word| {
            word.trim_start_matches(['#', '@'])
                .split(|character: char| !(character.is_alphanumeric() || character == '_'))
        })
        .filter(|term| !term.is_empty())
        .collect::<Vec<_>>();
    if terms.is_empty() {
        None
    } else {
        Some(
            terms
                .into_iter()
                .map(|term| format!(r#""{term}""#))
                .collect::<Vec<_>>()
                .join(" "),
        )
    }
}

pub async fn notifications(
    pool: &SqlitePool,
    user_id: i64,
) -> anyhow::Result<Vec<NotificationView>> {
    pool.call(move |conn| {
        let mut stmt = conn.prepare(
            r#"
            SELECT n.id, n.kind, n.actor_user_id, u.username, u.display_name,
              n.post_id, p.text, p.is_deleted, p.parent_post_id, p.quote_post_id,
              n.read_at, n.created_at
            FROM notifications n
            LEFT JOIN users u ON u.id = n.actor_user_id AND u.is_deleted = 0
            LEFT JOIN posts p ON p.id = n.post_id
            WHERE n.user_id = ?
            ORDER BY n.id DESC
            LIMIT 80
            "#,
        )?;
        let rows = stmt
            .query_map([user_id], |row| {
                let kind = row.get::<_, String>(1)?;
                let post_id = row.get::<_, Option<i64>>(5)?;
                let post_is_deleted = row
                    .get::<_, Option<i64>>(7)?
                    .is_some_and(|value| value != 0);
                let parent_post_id = row.get::<_, Option<i64>>(8)?;
                let quote_post_id = row.get::<_, Option<i64>>(9)?;
                let group_target_post_id = match kind.as_str() {
                    "reply" => parent_post_id.or(post_id),
                    "quote" => quote_post_id.or(post_id),
                    _ => post_id,
                };
                Ok(NotificationView {
                    id: row.get(0)?,
                    kind,
                    actor_user_id: row.get(2)?,
                    actor_username: row.get(3)?,
                    actor_display_name: row.get(4)?,
                    post_id,
                    group_target_post_id,
                    post_text: row.get(6)?,
                    post_available: row
                        .get::<_, Option<i64>>(7)?
                        .is_some_and(|_| !post_is_deleted),
                    read_at: row.get(10)?,
                    created_at: row.get(11)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}

pub async fn notification_groups(
    pool: &SqlitePool,
    user_id: i64,
) -> anyhow::Result<Vec<NotificationGroupView>> {
    let notifications = notifications(pool, user_id).await?;
    let mut groups = group_notifications(&notifications);
    refresh_notification_group_counts(pool, user_id, &mut groups).await?;
    Ok(groups)
}

fn group_notifications(notifications: &[NotificationView]) -> Vec<NotificationGroupView> {
    let mut groups: Vec<(NotificationGroupKey, NotificationGroupView)> = Vec::new();
    for notification in notifications {
        let key = notification_group_key(notification);
        if let Some((_key, group)) = groups
            .iter_mut()
            .find(|(existing_key, _group)| *existing_key == key)
        {
            push_notification_group_item(group, notification);
        } else {
            groups.push((key, notification_group_from_item(notification)));
        }
    }
    groups.into_iter().map(|(_key, group)| group).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NotificationGroupKey {
    kind: String,
    target: NotificationGroupTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum NotificationGroupTarget {
    Follow,
    Post(i64),
    Notification(i64),
}

fn notification_group_key(notification: &NotificationView) -> NotificationGroupKey {
    let target = match notification.kind.as_str() {
        "follow" => NotificationGroupTarget::Follow,
        "like" | "repost" | "reply" | "mention" | "quote" => {
            notification.group_target_post_id.map_or(
                NotificationGroupTarget::Notification(notification.id),
                NotificationGroupTarget::Post,
            )
        }
        _ => NotificationGroupTarget::Notification(notification.id),
    };
    NotificationGroupKey {
        kind: notification.kind.clone(),
        target,
    }
}

fn notification_group_from_item(notification: &NotificationView) -> NotificationGroupView {
    let mut group = NotificationGroupView {
        id: notification.id,
        kind: notification.kind.clone(),
        group_target_post_id: notification.group_target_post_id,
        post_id: notification.post_id,
        post_text: notification.post_text.clone(),
        post_available: notification.post_available,
        unread_count: 0,
        total_count: 0,
        notification_ids: Vec::new(),
        actors: Vec::new(),
        created_at: notification.created_at.clone(),
    };
    push_notification_group_item(&mut group, notification);
    group
}

fn push_notification_group_item(
    group: &mut NotificationGroupView,
    notification: &NotificationView,
) {
    group.total_count += 1;
    group.notification_ids.push(notification.id);
    if notification.read_at.is_none() {
        group.unread_count += 1;
    }
    group.actors.push(NotificationActorView {
        user_id: notification.actor_user_id,
        username: notification.actor_username.clone(),
        display_name: notification.actor_display_name.clone(),
    });
}

pub async fn unread_notification_count(pool: &SqlitePool, user_id: i64) -> anyhow::Result<i64> {
    pool.call(move |conn| {
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM notifications WHERE user_id = ? AND read_at IS NULL",
            [user_id],
            |row| row.get(0),
        )?)
    })
    .await
}

pub async fn mark_notification_ids_read(
    pool: &SqlitePool,
    user_id: i64,
    notification_ids: &[i64],
) -> anyhow::Result<()> {
    let notification_ids = notification_ids.to_vec();
    pool.call(move |conn| {
        let tx = conn.transaction()?;
        for notification_id in notification_ids {
            tx.execute(
                "UPDATE notifications SET read_at = CURRENT_TIMESTAMP WHERE user_id = ? AND id = ? AND read_at IS NULL",
                params![user_id, notification_id],
            )?;
        }
        tx.commit()?;
        Ok(())
    })
    .await
}

async fn refresh_notification_group_counts(
    pool: &SqlitePool,
    user_id: i64,
    groups: &mut [NotificationGroupView],
) -> anyhow::Result<()> {
    let descriptors = groups
        .iter()
        .enumerate()
        .filter(|(_index, group)| {
            group.kind == "follow"
                || (notification_group_target_expr(&group.kind).is_some()
                    && group.group_target_post_id.is_some())
        })
        .map(|(index, group)| (index, group.kind.clone(), group.group_target_post_id))
        .collect::<Vec<_>>();
    let counts = pool
        .call(move |conn| {
            let mut counts = Vec::with_capacity(descriptors.len());
            for (index, kind, target_post_id) in descriptors {
                let (total_count, unread_count) =
                    notification_group_counts_tx(conn, user_id, &kind, target_post_id)?;
                counts.push((index, total_count, unread_count));
            }
            Ok(counts)
        })
        .await?;
    for (index, total_count, unread_count) in counts {
        groups[index].total_count = usize::try_from(total_count)?;
        groups[index].unread_count = unread_count;
    }
    Ok(())
}

fn notification_group_counts_tx(
    conn: &Connection,
    user_id: i64,
    kind: &str,
    target_post_id: Option<i64>,
) -> anyhow::Result<(i64, i64)> {
    if kind == "follow" {
        return Ok(conn.query_row(
            r#"
            SELECT COUNT(*), COALESCE(SUM(CASE WHEN read_at IS NULL THEN 1 ELSE 0 END), 0)
            FROM notifications
            WHERE user_id = ? AND kind = 'follow'
            "#,
            [user_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?);
    }
    let Some(target_expr) = notification_group_target_expr(kind) else {
        return Ok((0, 0));
    };
    let Some(target_post_id) = target_post_id else {
        return Ok((0, 0));
    };
    let sql = format!(
        r#"
        SELECT COUNT(*), COALESCE(SUM(CASE WHEN n.read_at IS NULL THEN 1 ELSE 0 END), 0)
        FROM notifications n
        LEFT JOIN posts p ON p.id = n.post_id
        WHERE n.user_id = ? AND n.kind = ? AND {target_expr} = ?
        "#
    );
    Ok(
        conn.query_row(&sql, params![user_id, kind, target_post_id], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?,
    )
}

fn notification_group_target_expr(kind: &str) -> Option<&'static str> {
    match kind {
        "like" | "repost" | "mention" => Some("n.post_id"),
        "reply" => Some("COALESCE(p.parent_post_id, n.post_id)"),
        "quote" => Some("COALESCE(p.quote_post_id, n.post_id)"),
        _ => None,
    }
}

pub async fn mark_notifications_read(pool: &SqlitePool, user_id: i64) -> anyhow::Result<()> {
    pool.call(move |conn| {
        conn.execute(
            "UPDATE notifications SET read_at = CURRENT_TIMESTAMP WHERE user_id = ? AND read_at IS NULL",
            [user_id],
        )?;
        Ok(())
    })
    .await
}

pub async fn mark_notification_group_read(
    pool: &SqlitePool,
    user_id: i64,
    kind: &str,
    target_post_id: Option<i64>,
) -> anyhow::Result<()> {
    let kind = kind.to_owned();
    pool.call(move |conn| {
        if kind == "follow" {
            conn.execute(
                "UPDATE notifications SET read_at = CURRENT_TIMESTAMP WHERE user_id = ? AND kind = 'follow' AND read_at IS NULL",
                [user_id],
            )?;
            return Ok(());
        }
        let Some(target_expr) = notification_group_target_expr(&kind) else {
            anyhow::bail!("notification group kind is invalid");
        };
        let Some(target_post_id) = target_post_id else {
            anyhow::bail!("notification group target is invalid");
        };
        let sql = format!(
            r#"
            UPDATE notifications
            SET read_at = CURRENT_TIMESTAMP
            WHERE user_id = ? AND read_at IS NULL AND id IN (
              SELECT n.id
              FROM notifications n
              LEFT JOIN posts p ON p.id = n.post_id
              WHERE n.user_id = ? AND n.kind = ? AND {target_expr} = ?
            )
            "#
        );
        conn.execute(&sql, params![user_id, user_id, kind, target_post_id])?;
        Ok(())
    })
    .await
}

fn base_post_query() -> String {
    r#"
    SELECT 'post' AS event_kind, 'p:' || p.id AS event_id, p.created_at AS event_created_at,
      NULL AS repost_user_id, NULL AS repost_username, NULL AS repost_display_name, NULL AS repost_created_at,
      0 AS original_unavailable,
      p.id, p.user_id, u.username, u.display_name,
      COALESCE(pic.thumbnail_public_path, pic.public_path) AS profile_picture_path,
      p.anonymous_label, p.text, p.parent_post_id, p.created_at,
      p.edited_at,
      (SELECT COUNT(*) FROM likes WHERE post_id = p.id) AS like_count,
      ((SELECT COUNT(*) FROM reposts WHERE post_id = p.id) +
       (SELECT COUNT(*) FROM posts qp WHERE qp.quote_post_id = p.id AND qp.is_deleted = 0)) AS repost_count,
      (SELECT COUNT(*) FROM posts r WHERE r.parent_post_id = p.id AND r.is_deleted = 0) AS reply_count,
      p.quote_post_id,
      COALESCE(u.pinned_post_id = p.id, 0) AS pinned_by_author
    FROM posts p
    LEFT JOIN users u ON u.id = p.user_id
    LEFT JOIN media pic ON pic.id = u.profile_picture_media_id
    WHERE p.is_deleted = 0
    "#.to_owned()
}

fn base_repost_query() -> String {
    r#"
    SELECT 'repost' AS event_kind, 'r:' || r.id AS event_id, r.created_at AS event_created_at,
      ru.id AS repost_user_id, ru.username AS repost_username, ru.display_name AS repost_display_name, r.created_at AS repost_created_at,
      CASE WHEN p.id IS NULL OR p.is_deleted != 0 THEN 1 ELSE 0 END AS original_unavailable,
      COALESCE(p.id, r.post_id) AS id, p.user_id, u.username, u.display_name,
      COALESCE(pic.thumbnail_public_path, pic.public_path) AS profile_picture_path, p.anonymous_label,
      COALESCE(p.text, '') AS text, p.parent_post_id, COALESCE(p.created_at, r.created_at) AS created_at,
      p.edited_at,
      CASE WHEN p.id IS NULL OR p.is_deleted != 0 THEN 0 ELSE (SELECT COUNT(*) FROM likes WHERE post_id = p.id) END AS like_count,
      CASE WHEN p.id IS NULL OR p.is_deleted != 0 THEN 0 ELSE
        ((SELECT COUNT(*) FROM reposts WHERE post_id = p.id) +
         (SELECT COUNT(*) FROM posts qp WHERE qp.quote_post_id = p.id AND qp.is_deleted = 0))
      END AS repost_count,
      CASE WHEN p.id IS NULL OR p.is_deleted != 0 THEN 0 ELSE (SELECT COUNT(*) FROM posts replies WHERE replies.parent_post_id = p.id AND replies.is_deleted = 0) END AS reply_count,
      p.quote_post_id,
      CASE WHEN p.id IS NULL OR p.is_deleted != 0 THEN 0 ELSE COALESCE(u.pinned_post_id = p.id, 0) END AS pinned_by_author
    FROM reposts r
    JOIN users ru ON ru.id = r.user_id
    LEFT JOIN posts p ON p.id = r.post_id
    LEFT JOIN users u ON u.id = p.user_id
    LEFT JOIN media pic ON pic.id = u.profile_picture_media_id
    WHERE ru.is_deleted = 0
    "#.to_owned()
}

async fn post_events(
    pool: &SqlitePool,
    viewer_id: Option<i64>,
    mode: &str,
    cursor: Option<i64>,
) -> anyhow::Result<Vec<PostView>> {
    let mut sql = base_post_query();
    match mode {
        "home" => sql.push_str(
            " AND (p.user_id = ? OR p.user_id IN (SELECT followed_id FROM follows WHERE follower_id = ?))",
        ),
        "bookmarks" => sql.push_str(" AND p.id IN (SELECT post_id FROM bookmarks WHERE user_id = ?)"),
        _ => sql.push_str(" AND p.parent_post_id IS NULL"),
    }
    if matches!(mode, "home" | "bookmarks") {
        sql.push_str(" AND p.parent_post_id IS NULL");
    }
    append_viewer_filters(&mut sql, "p.user_id", viewer_id);
    if let Some(cursor) = cursor {
        sql.push_str(" AND p.id < ");
        sql.push_str(&cursor.to_string());
    }
    sql.push_str(" ORDER BY p.id DESC LIMIT 40");
    let mut bindings = Vec::new();
    if mode == "home" {
        let id = viewer_id.unwrap_or(-1);
        bindings.push(id);
        bindings.push(id);
    } else if mode == "bookmarks" {
        bindings.push(viewer_id.unwrap_or(-1));
    }
    push_viewer_filter_bindings(&mut bindings, viewer_id);
    let rows = pool
        .call(move |conn| query_post_rows(conn, &sql, params_from_iter(bindings)))
        .await?;
    rows_to_posts(pool, rows, viewer_id).await
}

async fn post_events_for_user(
    pool: &SqlitePool,
    viewer_id: Option<i64>,
    user_id: i64,
) -> anyhow::Result<Vec<PostView>> {
    let mut sql = base_post_query();
    sql.push_str(" AND p.user_id = ? AND p.parent_post_id IS NULL");
    append_viewer_filters(&mut sql, "p.user_id", viewer_id);
    sql.push_str(" ORDER BY p.id DESC LIMIT 40");
    let mut bindings = vec![user_id];
    push_viewer_filter_bindings(&mut bindings, viewer_id);
    let rows = pool
        .call(move |conn| query_post_rows(conn, &sql, params_from_iter(bindings)))
        .await?;
    rows_to_posts(pool, rows, viewer_id).await
}

async fn reply_events_for_user(
    pool: &SqlitePool,
    viewer_id: Option<i64>,
    user_id: i64,
) -> anyhow::Result<Vec<PostView>> {
    let mut sql = base_post_query();
    sql.push_str(" AND p.user_id = ? AND p.parent_post_id IS NOT NULL");
    append_viewer_filters(&mut sql, "p.user_id", viewer_id);
    sql.push_str(" ORDER BY p.id DESC LIMIT 40");
    let mut bindings = vec![user_id];
    push_viewer_filter_bindings(&mut bindings, viewer_id);
    let rows = pool
        .call(move |conn| query_post_rows(conn, &sql, params_from_iter(bindings)))
        .await?;
    rows_to_posts(pool, rows, viewer_id).await
}

async fn media_events_for_user(
    pool: &SqlitePool,
    viewer_id: Option<i64>,
    user_id: i64,
) -> anyhow::Result<Vec<PostView>> {
    let mut sql = base_post_query();
    sql.push_str(" AND p.user_id = ? AND ");
    sql.push_str(&media_surface_condition("p.id", "p.text"));
    append_viewer_filters(&mut sql, "p.user_id", viewer_id);
    sql.push_str(" ORDER BY p.id DESC LIMIT 40");
    let mut bindings = vec![user_id];
    push_viewer_filter_bindings(&mut bindings, viewer_id);
    let rows = pool
        .call(move |conn| query_post_rows(conn, &sql, params_from_iter(bindings)))
        .await?;
    rows_to_posts(pool, rows, viewer_id).await
}

async fn liked_events_for_user(
    pool: &SqlitePool,
    viewer_id: Option<i64>,
    user_id: i64,
) -> anyhow::Result<Vec<PostView>> {
    let mut sql = base_post_query();
    sql.push_str(" AND p.id IN (SELECT post_id FROM likes WHERE user_id = ?)");
    append_viewer_filters(&mut sql, "p.user_id", viewer_id);
    sql.push_str(
        " ORDER BY (SELECT created_at FROM likes WHERE user_id = ? AND post_id = p.id) DESC, p.id DESC LIMIT 40",
    );
    let mut bindings = vec![user_id];
    push_viewer_filter_bindings(&mut bindings, viewer_id);
    bindings.push(user_id);
    let rows = pool
        .call(move |conn| query_post_rows(conn, &sql, params_from_iter(bindings)))
        .await?;
    rows_to_posts(pool, rows, viewer_id).await
}

async fn repost_events(
    pool: &SqlitePool,
    viewer_id: Option<i64>,
    mode: &str,
    profile_user_id: Option<i64>,
) -> anyhow::Result<Vec<PostView>> {
    let mut sql = base_repost_query();
    match mode {
        "home" => sql.push_str(
            " AND (r.user_id = ? OR r.user_id IN (SELECT followed_id FROM follows WHERE follower_id = ?))",
        ),
        "profile" => sql.push_str(" AND r.user_id = ?"),
        "profile_media" => {
            sql.push_str(" AND r.user_id = ? AND ");
            sql.push_str(&media_surface_condition("p.id", "COALESCE(p.text, '')"));
        }
        _ => {}
    }
    if viewer_id.is_some() {
        sql.push_str(" AND r.user_id NOT IN (SELECT blocked_id FROM blocks WHERE blocker_id = ?)");
        sql.push_str(" AND r.user_id NOT IN (SELECT muted_id FROM mutes WHERE muter_id = ?)");
        append_viewer_filters(&mut sql, "p.user_id", viewer_id);
    }
    sql.push_str(" ORDER BY r.id DESC LIMIT 40");
    let mut bindings = Vec::new();
    if mode == "home" {
        let id = viewer_id.unwrap_or(-1);
        bindings.push(id);
        bindings.push(id);
    } else if let Some(user_id) = profile_user_id {
        bindings.push(user_id);
    }
    if let Some(id) = viewer_id {
        bindings.push(id);
        bindings.push(id);
    }
    push_viewer_filter_bindings(&mut bindings, viewer_id);
    let rows = pool
        .call(move |conn| query_post_rows(conn, &sql, params_from_iter(bindings)))
        .await?;
    rows_to_posts(pool, rows, viewer_id).await
}

fn media_surface_condition(post_id_column: &str, text_column: &str) -> String {
    format!(
        "(EXISTS (SELECT 1 FROM post_media pm WHERE pm.post_id = {post_id_column}) OR instr(lower({text_column}), 'youtube.com/') > 0 OR instr(lower({text_column}), 'youtu.be/') > 0)"
    )
}

fn append_viewer_filters(sql: &mut String, user_column: &str, viewer_id: Option<i64>) {
    if viewer_id.is_some() {
        sql.push_str(&format!(
            " AND ({user_column} IS NULL OR {user_column} NOT IN (SELECT blocked_id FROM blocks WHERE blocker_id = ?))"
        ));
        sql.push_str(&format!(
            " AND ({user_column} IS NULL OR {user_column} NOT IN (SELECT muted_id FROM mutes WHERE muter_id = ?))"
        ));
        sql.push_str(
            " AND NOT EXISTS (SELECT 1 FROM muted_words mw WHERE mw.user_id = ? AND instr(lower(p.text), mw.normalized_term) > 0)",
        );
    }
}

fn push_viewer_filter_bindings(bindings: &mut Vec<i64>, viewer_id: Option<i64>) {
    if let Some(id) = viewer_id {
        bindings.push(id);
        bindings.push(id);
        bindings.push(id);
    }
}

fn query_post_rows<P>(conn: &Connection, sql: &str, params: P) -> anyhow::Result<Vec<PostRow>>
where
    P: rusqlite::Params,
{
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map(params, map_post_row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn map_post_row(row: &Row<'_>) -> rusqlite::Result<PostRow> {
    Ok(PostRow {
        event_kind: row.get(0)?,
        event_id: row.get(1)?,
        event_created_at: row.get(2)?,
        repost_user_id: row.get(3)?,
        repost_username: row.get(4)?,
        repost_display_name: row.get(5)?,
        repost_created_at: row.get(6)?,
        original_unavailable: row.get::<_, i64>(7)? != 0,
        id: row.get(8)?,
        user_id: row.get(9)?,
        username: row.get(10)?,
        display_name: row.get(11)?,
        profile_picture_path: row.get(12)?,
        anonymous_label: row.get(13)?,
        text: row.get(14)?,
        parent_post_id: row.get(15)?,
        created_at: row.get(16)?,
        edited_at: row.get(17)?,
        like_count: row.get(18)?,
        repost_count: row.get(19)?,
        reply_count: row.get(20)?,
        quote_post_id: row.get(21)?,
        pinned_by_author: row.get::<_, i64>(22)? != 0,
    })
}

async fn rows_to_posts(
    pool: &SqlitePool,
    rows: Vec<PostRow>,
    viewer_id: Option<i64>,
) -> anyhow::Result<Vec<PostView>> {
    let mut posts = Vec::with_capacity(rows.len());
    for row in rows {
        let id = row.id;
        let original_unavailable = row.original_unavailable;
        let media = if original_unavailable {
            Vec::new()
        } else {
            media_for_post(pool, id).await?
        };
        let viewer_liked = if let Some(user_id) = viewer_id {
            !original_unavailable && relation_exists(pool, "likes", user_id, id).await?
        } else {
            false
        };
        let viewer_bookmarked = if let Some(user_id) = viewer_id {
            !original_unavailable && relation_exists(pool, "bookmarks", user_id, id).await?
        } else {
            false
        };
        let viewer_reposted = if let Some(user_id) = viewer_id {
            !original_unavailable && relation_exists(pool, "reposts", user_id, id).await?
        } else {
            false
        };
        let viewer_can_repost =
            viewer_id.is_some_and(|user_id| !original_unavailable && row.user_id != Some(user_id));
        let quote = quote_preview_for_post(pool, row.quote_post_id, viewer_id).await?;
        posts.push(PostView {
            event_id: row.event_id,
            event_kind: if row.event_kind == "repost" {
                TimelineEventKind::Repost
            } else {
                TimelineEventKind::Post
            },
            id,
            user_id: row.user_id,
            username: row.username,
            display_name: row.display_name,
            profile_picture_path: row.profile_picture_path,
            anonymous_label: row.anonymous_label,
            text: row.text,
            parent_post_id: row.parent_post_id,
            created_at: row.created_at,
            edited_at: row.edited_at,
            event_created_at: row.event_created_at,
            like_count: row.like_count,
            repost_count: row.repost_count,
            reply_count: row.reply_count,
            viewer_liked,
            viewer_bookmarked,
            viewer_reposted,
            viewer_can_repost,
            pinned_by_author: row.pinned_by_author,
            original_unavailable,
            reposted_by_user_id: row.repost_user_id,
            reposted_by_username: row.repost_username,
            reposted_by_display_name: row.repost_display_name,
            reposted_at: row.repost_created_at,
            quote,
            media,
        });
    }
    Ok(posts)
}

async fn quote_preview_for_post(
    pool: &SqlitePool,
    quote_post_id: Option<i64>,
    viewer_id: Option<i64>,
) -> anyhow::Result<Option<QuotePreview>> {
    let Some(quote_post_id) = quote_post_id else {
        return Ok(None);
    };
    pool.call(move |conn| {
        let mut sql = r#"
            SELECT q.id, q.user_id, u.username, u.display_name, q.anonymous_label, q.text, q.created_at
            FROM posts q
            LEFT JOIN users u ON u.id = q.user_id
            WHERE q.id = ? AND q.is_deleted = 0
        "#
        .to_owned();
        if viewer_id.is_some() {
            sql.push_str(
                " AND (q.user_id IS NULL OR q.user_id NOT IN (SELECT blocked_id FROM blocks WHERE blocker_id = ?))",
            );
            sql.push_str(
                " AND (q.user_id IS NULL OR q.user_id NOT IN (SELECT muted_id FROM mutes WHERE muter_id = ?))",
            );
            sql.push_str(
                " AND (q.user_id IS NULL OR NOT EXISTS (SELECT 1 FROM blocks WHERE blocker_id = q.user_id AND blocked_id = ?))",
            );
            sql.push_str(
                " AND NOT EXISTS (SELECT 1 FROM muted_words mw WHERE mw.user_id = ? AND instr(lower(q.text), mw.normalized_term) > 0)",
            );
        }
        let preview = if let Some(viewer_id) = viewer_id {
            conn.query_row(
                &sql,
                params![quote_post_id, viewer_id, viewer_id, viewer_id, viewer_id],
                map_quote_preview_row,
            )
            .optional()?
        } else {
            conn.query_row(&sql, params![quote_post_id], map_quote_preview_row)
                .optional()?
        };
        Ok(Some(preview.unwrap_or_else(|| QuotePreview {
            id: quote_post_id,
            username: None,
            display_name: None,
            anonymous_label: None,
            text: String::new(),
            created_at: String::new(),
            unavailable: true,
        })))
    })
    .await
}

fn map_quote_preview_row(row: &Row<'_>) -> rusqlite::Result<QuotePreview> {
    Ok(QuotePreview {
        id: row.get(0)?,
        username: row.get(2)?,
        display_name: row.get(3)?,
        anonymous_label: row.get(4)?,
        text: row.get(5)?,
        created_at: row.get(6)?,
        unavailable: false,
    })
}

fn ensure_quote_target_accessible_tx(
    tx: &rusqlite::Transaction<'_>,
    user_id: i64,
    post_id: i64,
) -> anyhow::Result<()> {
    let owner: Option<i64> = tx
        .query_row(
            "SELECT user_id FROM posts WHERE id = ? AND is_deleted = 0",
            [post_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(owner) = owner else {
        anyhow::bail!("post not found");
    };
    if owner == user_id {
        anyhow::bail!("cannot quote repost your own post");
    }
    if user_relation_exists_tx(tx, "blocks", user_id, owner)?
        || user_relation_exists_tx(tx, "mutes", user_id, owner)?
        || user_relation_exists_tx(tx, "blocks", owner, user_id)?
    {
        anyhow::bail!("post not found");
    }
    Ok(())
}

fn user_relation_exists_tx(
    tx: &rusqlite::Transaction<'_>,
    table: &str,
    left_id: i64,
    right_id: i64,
) -> anyhow::Result<bool> {
    let (left_column, right_column) = match table {
        "blocks" => ("blocker_id", "blocked_id"),
        "mutes" => ("muter_id", "muted_id"),
        _ => anyhow::bail!("unsupported relation table"),
    };
    let sql = format!("SELECT 1 FROM {table} WHERE {left_column} = ? AND {right_column} = ?");
    Ok(tx
        .query_row(&sql, params![left_id, right_id], |_| Ok(()))
        .optional()?
        .is_some())
}

async fn relation_exists(
    pool: &SqlitePool,
    table: &str,
    user_id: i64,
    post_id: i64,
) -> anyhow::Result<bool> {
    let sql = format!("SELECT 1 FROM {table} WHERE user_id = ? AND post_id = ?");
    pool.call(move |conn| {
        Ok(conn
            .query_row(&sql, params![user_id, post_id], |_| Ok(()))
            .optional()?
            .is_some())
    })
    .await
}

async fn media_for_post(pool: &SqlitePool, post_id: i64) -> anyhow::Result<Vec<MediaView>> {
    pool.call(move |conn| {
        let mut stmt = conn.prepare(
            r#"
        SELECT m.public_path, m.mime_type, m.media_kind, m.alt_text, m.is_nsfw
        FROM post_media pm JOIN media m ON m.id = pm.media_id
        WHERE pm.post_id = ? ORDER BY pm.position ASC
        "#,
        )?;
        let rows = stmt
            .query_map([post_id], |row| {
                Ok(MediaView {
                    public_path: row.get(0)?,
                    mime_type: row.get(1)?,
                    media_kind: row.get(2)?,
                    alt_text: row.get(3)?,
                    is_nsfw: row.get::<_, i64>(4)? != 0,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}

fn notify_post_owner_tx(
    tx: &rusqlite::Transaction<'_>,
    owner_post_id: i64,
    actor_id: i64,
    link_post_id: i64,
    kind: &str,
    message: &str,
) -> anyhow::Result<Option<i64>> {
    let owner: Option<i64> = tx
        .query_row(
            "SELECT user_id FROM posts WHERE id = ?",
            [owner_post_id],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(owner) = owner
        && owner != actor_id
        && !is_blocked_tx(tx, owner, actor_id)?
    {
        create_notification_tx(tx, owner, Some(actor_id), Some(link_post_id), kind, message)?;
    }
    Ok(owner)
}

fn notify_mentioned_users_tx(
    tx: &rusqlite::Transaction<'_>,
    text: &str,
    actor_id: i64,
    link_post_id: i64,
    excluded_user_id: Option<i64>,
) -> anyhow::Result<()> {
    for normalized_username in mentioned_usernames(text) {
        let mentioned_user_id = tx
            .query_row(
                "SELECT id FROM users WHERE normalized_username = ? AND is_deleted = 0",
                [&normalized_username],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        let Some(mentioned_user_id) = mentioned_user_id else {
            continue;
        };
        if mentioned_user_id == actor_id
            || Some(mentioned_user_id) == excluded_user_id
            || is_blocked_tx(tx, mentioned_user_id, actor_id)?
        {
            continue;
        }
        create_notification_tx(
            tx,
            mentioned_user_id,
            Some(actor_id),
            Some(link_post_id),
            "mention",
            "mentioned you in a post",
        )?;
    }
    Ok(())
}

fn mentioned_usernames(text: &str) -> Vec<String> {
    let mut usernames = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some((_, character)) = chars.next() {
        if character != '@' {
            continue;
        }
        let mut username = String::new();
        while let Some((_, next)) = chars.peek() {
            if next.is_ascii_alphanumeric() || *next == '_' || *next == '-' {
                username.push(*next);
                chars.next();
            } else {
                break;
            }
        }
        if !username.is_empty() {
            let normalized = username.to_ascii_lowercase();
            if !usernames.contains(&normalized) {
                usernames.push(normalized);
            }
        }
    }
    usernames
}

fn is_blocked_tx(
    tx: &rusqlite::Transaction<'_>,
    user_id: i64,
    actor_id: i64,
) -> anyhow::Result<bool> {
    Ok(tx
        .query_row(
            "SELECT 1 FROM blocks WHERE blocker_id = ? AND blocked_id = ?",
            params![user_id, actor_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

#[cfg(test)]
mod reply_tests {
    use super::*;

    async fn test_pool() -> (tempfile::TempDir, SqlitePool, Settings) {
        let temp = tempfile::tempdir().expect("temp dir");
        let pool = crate::db::connect(&temp.path().join("test.sqlite3"))
            .await
            .expect("connect");
        crate::db::migrate(&pool).await.expect("migrate");
        (temp, pool, Settings::default())
    }

    #[test]
    fn search_fts_query_uses_safe_terms() {
        assert_eq!(
            fts_query_from_user_input("#self-hosted @ada"),
            Some(r#""self" "hosted" "ada""#.to_owned())
        );
        assert_eq!(
            fts_query_from_user_input("NEAR OR rust"),
            Some(r#""NEAR" "OR" "rust""#.to_owned())
        );
        assert_eq!(fts_query_from_user_input("#"), None);
    }

    #[test]
    fn search_user_query_supports_mentions() {
        assert_eq!(user_query_from_input("@alice"), Some("alice".to_owned()));
        assert_eq!(user_query_from_input(" @alice, "), Some("alice".to_owned()));
        assert_eq!(user_query_from_input("@"), None);
    }

    #[tokio::test]
    async fn reply_is_linked_to_parent_and_rendered_in_thread() {
        let (_temp, pool, settings) = test_pool().await;
        let user_id =
            crate::auth::register_user(&pool, &settings, "alice", "very secure password", false)
                .await
                .expect("user");
        let parent_id = create_post(&pool, &settings, Some(user_id), "parent post", None, &[])
            .await
            .expect("parent");

        let reply_id = create_post(
            &pool,
            &settings,
            Some(user_id),
            "child reply",
            Some(parent_id),
            &[],
        )
        .await
        .expect("reply");

        let parent_row = pool
            .call(move |conn| {
                conn.query_row(
                    "SELECT parent_post_id, root_post_id FROM posts WHERE id = ?",
                    [reply_id],
                    |row| Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, Option<i64>>(1)?)),
                )
                .map_err(Into::into)
            })
            .await
            .expect("reply row");
        assert_eq!(parent_row, (Some(parent_id), Some(parent_id)));

        let thread = post_thread(&pool, Some(user_id), parent_id)
            .await
            .expect("thread");
        assert_eq!(thread.len(), 2);
        assert_eq!(thread[1].id, reply_id);
        assert_eq!(thread[1].parent_post_id, Some(parent_id));
    }

    #[tokio::test]
    async fn reply_does_not_appear_as_top_level_timeline_post() {
        let (_temp, pool, settings) = test_pool().await;
        let user_id =
            crate::auth::register_user(&pool, &settings, "alice", "very secure password", false)
                .await
                .expect("user");
        let parent_id = create_post(&pool, &settings, Some(user_id), "parent post", None, &[])
            .await
            .expect("parent");
        let reply_id = create_post(
            &pool,
            &settings,
            Some(user_id),
            "child reply",
            Some(parent_id),
            &[],
        )
        .await
        .expect("reply");

        let timeline_posts = timeline(&pool, Some(user_id), "local", None)
            .await
            .expect("timeline");

        assert!(timeline_posts.iter().any(|post| post.id == parent_id));
        assert!(!timeline_posts.iter().any(|post| post.id == reply_id));
    }

    #[tokio::test]
    async fn invalid_parent_post_is_rejected() {
        let (_temp, pool, settings) = test_pool().await;
        let user_id =
            crate::auth::register_user(&pool, &settings, "alice", "very secure password", false)
                .await
                .expect("user");

        let result = create_post(
            &pool,
            &settings,
            Some(user_id),
            "orphan reply",
            Some(9_999),
            &[],
        )
        .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn deleted_parent_does_not_crash_thread_rendering() {
        let (_temp, pool, settings) = test_pool().await;
        let user_id =
            crate::auth::register_user(&pool, &settings, "alice", "very secure password", false)
                .await
                .expect("user");
        let parent_id = create_post(&pool, &settings, Some(user_id), "parent post", None, &[])
            .await
            .expect("parent");
        create_post(
            &pool,
            &settings,
            Some(user_id),
            "child reply",
            Some(parent_id),
            &[],
        )
        .await
        .expect("reply");
        delete_post(&pool, user_id, parent_id, false)
            .await
            .expect("delete parent");

        let thread = post_thread(&pool, Some(user_id), parent_id)
            .await
            .expect("thread lookup");

        assert!(thread.is_empty());
    }
}

fn create_notification_tx(
    tx: &rusqlite::Transaction<'_>,
    user_id: i64,
    actor_id: Option<i64>,
    post_id: Option<i64>,
    kind: &str,
    message: &str,
) -> anyhow::Result<()> {
    if actor_id == Some(user_id) {
        return Ok(());
    }
    let exists = tx
        .query_row(
            r#"
            SELECT 1 FROM notifications
            WHERE user_id = ? AND kind = ?
              AND ((actor_user_id IS NULL AND ? IS NULL) OR actor_user_id = ?)
              AND ((post_id IS NULL AND ? IS NULL) OR post_id = ?)
            LIMIT 1
            "#,
            params![user_id, kind, actor_id, actor_id, post_id, post_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if exists {
        return Ok(());
    }
    tx.execute(
        "INSERT INTO notifications (user_id, actor_user_id, post_id, kind, message) VALUES (?, ?, ?, ?, ?)",
        params![user_id, actor_id, post_id, kind, message],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{auth, config::Settings, db};

    async fn fixture() -> (SqlitePool, Settings, i64, i64) {
        let temp = tempfile::tempdir().expect("temp dir");
        let pool = db::connect(&temp.path().join("test.sqlite3"))
            .await
            .expect("connect");
        db::migrate(&pool).await.expect("migrate");
        let settings = Settings::default();
        let alice = auth::register_user(&pool, &settings, "alice", "very secure password", false)
            .await
            .expect("alice");
        let bob = auth::register_user(&pool, &settings, "bob", "very secure password", false)
            .await
            .expect("bob");
        (pool, settings, alice, bob)
    }

    #[tokio::test]
    async fn social_core_behaviors() {
        let (pool, settings, alice, bob) = fixture().await;
        let post = create_post(&pool, &settings, Some(alice), &"a".repeat(280), None, &[])
            .await
            .expect("post");
        assert!(
            create_post(&pool, &settings, Some(alice), &"a".repeat(281), None, &[])
                .await
                .is_err()
        );
        assert!(
            create_post(&pool, &settings, Some(alice), " ", None, &[])
                .await
                .is_err()
        );
        follow(&pool, bob, alice).await.expect("follow");
        assert!(follow(&pool, alice, alice).await.is_err());
        like(&pool, bob, post).await.expect("like");
        unlike(&pool, bob, post).await.expect("unlike");
        bookmark(&pool, bob, post).await.expect("bookmark");
        unbookmark(&pool, bob, post).await.expect("unbookmark");
        let reply = create_post(&pool, &settings, Some(bob), "reply", Some(post), &[])
            .await
            .expect("reply");
        assert_ne!(post, reply);
        assert!(repost(&pool, bob, post).await.expect("repost"));
        assert!(!repost(&pool, bob, post).await.expect("duplicate repost"));
        block(&pool, alice, bob).await.expect("block");
        mute(&pool, alice, bob).await.expect("mute");
    }

    #[tokio::test]
    async fn muted_users_can_be_listed_and_removed() {
        let (pool, _settings, alice, bob) = fixture().await;

        mute(&pool, alice, bob).await.expect("mute");
        let muted = muted_users(&pool, alice).await.expect("muted users");

        assert_eq!(muted, vec![(bob, "bob".to_owned(), "bob".to_owned())]);
        unmute(&pool, alice, bob).await.expect("unmute");
        assert!(
            muted_users(&pool, alice)
                .await
                .expect("muted users after unmute")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn muted_words_persist_and_filter_viewer_timelines_and_search() {
        let (pool, settings, alice, bob) = fixture().await;
        let hidden = create_post(
            &pool,
            &settings,
            Some(bob),
            "This contains Spoilers",
            None,
            &[],
        )
        .await
        .expect("hidden post");
        let visible = create_post(&pool, &settings, Some(bob), "plain update", None, &[])
            .await
            .expect("visible post");

        add_muted_word(&pool, alice, "spoilers")
            .await
            .expect("add muted word");
        let words = muted_words(&pool, alice).await.expect("muted words");
        assert_eq!(words.len(), 1);
        assert_eq!(words[0].term, "spoilers");

        let alice_timeline = timeline(&pool, Some(alice), "local", None)
            .await
            .expect("alice timeline");
        assert!(!alice_timeline.iter().any(|post| post.id == hidden));
        assert!(alice_timeline.iter().any(|post| post.id == visible));

        let bob_timeline = timeline(&pool, Some(bob), "local", None)
            .await
            .expect("bob timeline");
        assert!(bob_timeline.iter().any(|post| post.id == hidden));

        let (_users, alice_search) = search(&pool, Some(alice), "Spoilers")
            .await
            .expect("alice search");
        assert!(alice_search.is_empty());

        remove_muted_word(&pool, alice, words[0].id)
            .await
            .expect("remove muted word");
        let alice_timeline = timeline(&pool, Some(alice), "local", None)
            .await
            .expect("alice timeline after remove");
        assert!(alice_timeline.iter().any(|post| post.id == hidden));
    }

    #[tokio::test]
    async fn anonymous_mode_gated() {
        let (pool, mut settings, _, _) = fixture().await;
        assert!(
            create_post(&pool, &settings, None, "anon", None, &[])
                .await
                .is_err()
        );
        settings.accounts.anonymous_mode_enabled = true;
        create_post(&pool, &settings, None, "anon", None, &[])
            .await
            .expect("anon post");
    }

    #[tokio::test]
    async fn repost_appears_as_timeline_event_and_counts_once() {
        let (pool, settings, alice, bob) = fixture().await;
        let post = create_post(&pool, &settings, Some(alice), "hello", None, &[])
            .await
            .expect("post");
        assert!(repost(&pool, bob, post).await.expect("repost"));
        assert!(!repost(&pool, bob, post).await.expect("duplicate repost"));

        let local = timeline(&pool, Some(alice), "local", None)
            .await
            .expect("timeline");
        assert!(
            local
                .iter()
                .any(|event| event.event_kind == TimelineEventKind::Repost)
        );
        let original = local
            .iter()
            .find(|event| event.event_kind == TimelineEventKind::Post && event.id == post)
            .expect("original post");
        assert_eq!(original.repost_count, 1);
    }

    #[tokio::test]
    async fn quote_repost_appears_as_post_with_original_preview_and_counts_once() {
        let (pool, settings, alice, bob) = fixture().await;
        let original = create_post(&pool, &settings, Some(alice), "original", None, &[])
            .await
            .expect("original");

        let quote = create_quote_post(&pool, &settings, bob, original, "my comment")
            .await
            .expect("quote");
        let duplicate = create_quote_post(&pool, &settings, bob, original, "my comment")
            .await
            .expect("duplicate quote");

        assert!(quote.created);
        assert!(!duplicate.created);
        assert_eq!(duplicate.post_id, quote.post_id);

        let local = timeline(&pool, Some(alice), "local", None)
            .await
            .expect("timeline");
        let quote_post = local
            .iter()
            .find(|event| event.id == quote.post_id)
            .expect("quote post");
        assert_eq!(quote_post.text, "my comment");
        assert_eq!(
            quote_post
                .quote
                .as_ref()
                .map(|preview| preview.text.as_str()),
            Some("original")
        );
        let original_post = local
            .iter()
            .find(|event| event.event_kind == TimelineEventKind::Post && event.id == original)
            .expect("original post");
        assert_eq!(original_post.repost_count, 1);
    }

    #[tokio::test]
    async fn quote_repost_original_becomes_unavailable_after_delete() {
        let (pool, settings, alice, bob) = fixture().await;
        let original = create_post(&pool, &settings, Some(alice), "original", None, &[])
            .await
            .expect("original");
        let quote = create_quote_post(&pool, &settings, bob, original, "my comment")
            .await
            .expect("quote");
        delete_post(&pool, alice, original, false)
            .await
            .expect("delete original");

        let local = timeline(&pool, Some(bob), "local", None)
            .await
            .expect("timeline");
        let quote_post = local
            .iter()
            .find(|event| event.id == quote.post_id)
            .expect("quote post");

        assert!(
            quote_post
                .quote
                .as_ref()
                .is_some_and(|preview| preview.unavailable)
        );
    }

    #[tokio::test]
    async fn quote_repost_respects_block_and_mute_rules() {
        let (pool, settings, alice, bob) = fixture().await;
        let original = create_post(&pool, &settings, Some(alice), "original", None, &[])
            .await
            .expect("original");

        block(&pool, alice, bob).await.expect("block");
        assert!(
            create_quote_post(&pool, &settings, bob, original, "blocked quote")
                .await
                .is_err()
        );
        unblock(&pool, alice, bob).await.expect("unblock");
        mute(&pool, bob, alice).await.expect("mute");
        assert!(
            create_quote_post(&pool, &settings, bob, original, "muted quote")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn compact_post_avatar_prefers_profile_thumbnail() {
        let (pool, settings, alice, _) = fixture().await;
        set_profile_picture(
            &pool,
            alice,
            "/uploads/images/alice.webp",
            Some("/uploads/thumbs/alice-profile.webp"),
        )
        .await;
        let post = create_post(&pool, &settings, Some(alice), "hello", None, &[])
            .await
            .expect("post");

        let local = timeline(&pool, Some(alice), "local", None)
            .await
            .expect("timeline");
        let original = local
            .iter()
            .find(|event| event.event_kind == TimelineEventKind::Post && event.id == post)
            .expect("original post");

        assert_eq!(
            original.profile_picture_path.as_deref(),
            Some("/uploads/thumbs/alice-profile.webp")
        );
    }

    #[tokio::test]
    async fn compact_post_avatar_falls_back_to_original_without_thumbnail() {
        let (pool, settings, alice, _) = fixture().await;
        set_profile_picture(&pool, alice, "/uploads/images/alice.webp", None).await;
        let post = create_post(&pool, &settings, Some(alice), "hello", None, &[])
            .await
            .expect("post");

        let local = timeline(&pool, Some(alice), "local", None)
            .await
            .expect("timeline");
        let original = local
            .iter()
            .find(|event| event.event_kind == TimelineEventKind::Post && event.id == post)
            .expect("original post");

        assert_eq!(
            original.profile_picture_path.as_deref(),
            Some("/uploads/images/alice.webp")
        );
    }

    #[tokio::test]
    async fn following_accounts_keep_default_avatar_when_no_profile_picture() {
        let (pool, _settings, alice, bob) = fixture().await;
        follow(&pool, bob, alice).await.expect("follow");

        let accounts = following_accounts(&pool, bob).await.expect("accounts");

        assert_eq!(accounts.len(), 1);
        assert!(accounts[0].profile_picture_path.is_none());
    }

    #[tokio::test]
    async fn profile_timeline_includes_user_reposts() {
        let (pool, settings, alice, bob) = fixture().await;
        let post = create_post(&pool, &settings, Some(alice), "hello", None, &[])
            .await
            .expect("post");
        repost(&pool, bob, post).await.expect("repost");

        let profile = profile_timeline(&pool, Some(alice), bob)
            .await
            .expect("profile");
        assert_eq!(profile.len(), 1);
        assert_eq!(profile[0].event_kind, TimelineEventKind::Repost);
        assert_eq!(profile[0].id, post);
        assert_eq!(profile[0].reposted_by_user_id, Some(bob));
        assert!(!profile[0].viewer_can_repost);

        let profile_for_bob = profile_timeline(&pool, Some(bob), bob)
            .await
            .expect("profile for bob");
        assert!(profile_for_bob[0].viewer_can_repost);
    }

    #[tokio::test]
    async fn profile_posts_tab_includes_originals_and_reposts_but_excludes_plain_replies() {
        let (pool, settings, alice, bob) = fixture().await;
        let original = create_post(&pool, &settings, Some(bob), "bob original", None, &[])
            .await
            .expect("original");
        let parent = create_post(&pool, &settings, Some(alice), "alice parent", None, &[])
            .await
            .expect("parent");
        let reply = create_post(&pool, &settings, Some(bob), "bob reply", Some(parent), &[])
            .await
            .expect("reply");
        repost(&pool, bob, parent).await.expect("repost");

        let posts = profile_tab_timeline(&pool, Some(alice), bob, ProfileTimelineTab::Posts)
            .await
            .expect("posts tab");

        assert!(posts.iter().any(|post| post.id == original));
        assert!(
            posts
                .iter()
                .any(|post| { post.id == parent && post.event_kind == TimelineEventKind::Repost })
        );
        assert!(!posts.iter().any(|post| post.id == reply));
    }

    #[tokio::test]
    async fn profile_replies_tab_includes_plain_replies() {
        let (pool, settings, alice, bob) = fixture().await;
        let parent = create_post(&pool, &settings, Some(alice), "alice parent", None, &[])
            .await
            .expect("parent");
        let reply = create_post(&pool, &settings, Some(bob), "bob reply", Some(parent), &[])
            .await
            .expect("reply");

        let replies = profile_tab_timeline(&pool, Some(alice), bob, ProfileTimelineTab::Replies)
            .await
            .expect("replies tab");

        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].id, reply);
        assert_eq!(replies[0].parent_post_id, Some(parent));
    }

    #[tokio::test]
    async fn profile_media_tab_includes_only_profile_user_media_posts_and_replies() {
        let (pool, settings, alice, bob) = fixture().await;
        let plain = create_post(&pool, &settings, Some(bob), "plain", None, &[])
            .await
            .expect("plain");
        let media_post = create_post(&pool, &settings, Some(bob), "with media", None, &[])
            .await
            .expect("media post");
        attach_test_media(&pool, bob, media_post).await;
        let embedded = create_post(
            &pool,
            &settings,
            Some(bob),
            "https://youtu.be/dQw4w9WgXcQ",
            None,
            &[],
        )
        .await
        .expect("embedded");
        let parent = create_post(&pool, &settings, Some(alice), "alice parent", None, &[])
            .await
            .expect("parent");
        let media_reply = create_post(
            &pool,
            &settings,
            Some(bob),
            "reply media",
            Some(parent),
            &[],
        )
        .await
        .expect("media reply");
        attach_test_media(&pool, bob, media_reply).await;
        let liked_media = create_post(&pool, &settings, Some(alice), "liked media", None, &[])
            .await
            .expect("liked media");
        attach_test_media(&pool, alice, liked_media).await;
        like(&pool, bob, liked_media).await.expect("like");

        let media = profile_tab_timeline(&pool, Some(alice), bob, ProfileTimelineTab::Media)
            .await
            .expect("media tab");

        assert!(media.iter().any(|post| post.id == media_post));
        assert!(media.iter().any(|post| post.id == embedded));
        assert!(media.iter().any(|post| post.id == media_reply));
        assert!(!media.iter().any(|post| post.id == plain));
        assert!(!media.iter().any(|post| post.id == liked_media));
    }

    #[tokio::test]
    async fn profile_likes_tab_shows_liked_posts_themselves() {
        let (pool, settings, alice, bob) = fixture().await;
        let liked = create_post(&pool, &settings, Some(alice), "liked by bob", None, &[])
            .await
            .expect("liked");
        let not_liked = create_post(&pool, &settings, Some(bob), "not liked by bob", None, &[])
            .await
            .expect("not liked");
        like(&pool, bob, liked).await.expect("like");

        let likes = profile_tab_timeline(&pool, Some(alice), bob, ProfileTimelineTab::Likes)
            .await
            .expect("likes tab");

        assert_eq!(likes.len(), 1);
        assert_eq!(likes[0].id, liked);
        assert_eq!(likes[0].user_id, Some(alice));
        assert!(!likes.iter().any(|post| post.id == not_liked));
    }

    #[tokio::test]
    async fn deleted_original_repost_renders_as_unavailable_event() {
        let (pool, settings, alice, bob) = fixture().await;
        let post = create_post(&pool, &settings, Some(alice), "hello", None, &[])
            .await
            .expect("post");
        repost(&pool, bob, post).await.expect("repost");
        delete_post(&pool, alice, post, false)
            .await
            .expect("delete");

        let profile = profile_timeline(&pool, Some(alice), bob)
            .await
            .expect("profile");
        let repost_event = profile
            .iter()
            .find(|event| event.event_kind == TimelineEventKind::Repost)
            .expect("repost event");
        assert!(repost_event.original_unavailable);
        assert!(repost_event.media.is_empty());
    }

    async fn set_profile_picture(
        pool: &SqlitePool,
        user_id: i64,
        public_path: &str,
        thumbnail_public_path: Option<&str>,
    ) {
        let public_path = public_path.to_owned();
        let thumbnail_public_path = thumbnail_public_path.map(ToOwned::to_owned);
        pool.call(move |conn| {
            conn.execute(
                "INSERT INTO media (owner_user_id, original_filename, stored_path, public_path, mime_type, media_kind, byte_len, thumbnail_public_path) VALUES (?, 'avatar.webp', '/tmp/avatar.webp', ?, 'image/webp', 'image', 1, ?)",
                params![user_id, public_path, thumbnail_public_path],
            )?;
            let media_id = conn.last_insert_rowid();
            conn.execute(
                "UPDATE users SET profile_picture_media_id = ? WHERE id = ?",
                params![media_id, user_id],
            )?;
            Ok(())
        })
        .await
        .expect("profile picture");
    }

    async fn attach_test_media(pool: &SqlitePool, owner_user_id: i64, post_id: i64) {
        pool.call(move |conn| {
            conn.execute(
                "INSERT INTO media (owner_user_id, original_filename, stored_path, public_path, mime_type, media_kind, byte_len) VALUES (?, 'media.webp', '/tmp/media.webp', '/uploads/images/media.webp', 'image/webp', 'image', 1)",
                params![owner_user_id],
            )?;
            let media_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO post_media (post_id, media_id, position) VALUES (?, ?, 0)",
                params![post_id, media_id],
            )?;
            Ok(())
        })
        .await
        .expect("attach media");
    }

    async fn notification_count(
        pool: &SqlitePool,
        user_id: i64,
        kind: &str,
    ) -> anyhow::Result<i64> {
        let kind = kind.to_owned();
        pool.call(move |conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM notifications WHERE user_id = ? AND kind = ?",
                params![user_id, kind],
                |row| row.get(0),
            )?)
        })
        .await
    }

    #[tokio::test]
    async fn repost_notification_created_only_once() {
        let (pool, settings, alice, bob) = fixture().await;
        let post = create_post(&pool, &settings, Some(alice), "hello", None, &[])
            .await
            .expect("post");
        assert!(repost(&pool, bob, post).await.expect("repost"));
        assert!(!repost(&pool, bob, post).await.expect("duplicate repost"));
        let count: i64 = pool
            .call(move |conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM notifications WHERE user_id = ? AND kind = 'repost'",
                    [alice],
                    |row| row.get(0),
                )?)
            })
            .await
            .expect("count");
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn duplicate_likes_and_bookmarks_are_ignored() {
        let (pool, settings, alice, bob) = fixture().await;
        let post = create_post(&pool, &settings, Some(alice), "hello", None, &[])
            .await
            .expect("post");

        like(&pool, bob, post).await.expect("first like");
        like(&pool, bob, post).await.expect("duplicate like");
        bookmark(&pool, bob, post).await.expect("first bookmark");
        bookmark(&pool, bob, post)
            .await
            .expect("duplicate bookmark");

        let (likes, bookmarks): (i64, i64) = pool
            .call(move |conn| {
                let likes = conn.query_row(
                    "SELECT COUNT(*) FROM likes WHERE user_id = ? AND post_id = ?",
                    params![bob, post],
                    |row| row.get(0),
                )?;
                let bookmarks = conn.query_row(
                    "SELECT COUNT(*) FROM bookmarks WHERE user_id = ? AND post_id = ?",
                    params![bob, post],
                    |row| row.get(0),
                )?;
                Ok((likes, bookmarks))
            })
            .await
            .expect("counts");

        assert_eq!(likes, 1);
        assert_eq!(bookmarks, 1);

        let notifications = notification_count(&pool, alice, "like")
            .await
            .expect("notification count");
        assert_eq!(notifications, 1);
    }

    #[tokio::test]
    async fn own_actions_do_not_create_notifications() {
        let (pool, settings, alice, _) = fixture().await;
        let post = create_post(&pool, &settings, Some(alice), "hello @alice", None, &[])
            .await
            .expect("post");

        like(&pool, alice, post).await.expect("like own post");

        assert_eq!(
            unread_notification_count(&pool, alice)
                .await
                .expect("unread count"),
            0
        );
    }

    #[tokio::test]
    async fn mentions_notify_existing_users_once() {
        let (pool, settings, alice, bob) = fixture().await;
        create_post(
            &pool,
            &settings,
            Some(alice),
            "hello @bob and @Bob and @missing",
            None,
            &[],
        )
        .await
        .expect("post");

        let notifications = notifications(&pool, bob).await.expect("notifications");

        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0].kind, "mention");
        assert_eq!(notifications[0].actor_user_id, Some(alice));
        assert_eq!(notifications[0].actor_username.as_deref(), Some("alice"));
        assert!(notifications[0].post_available);
        assert_eq!(
            unread_notification_count(&pool, bob)
                .await
                .expect("unread count"),
            1
        );
    }

    #[tokio::test]
    async fn mention_suggestions_match_visible_usernames_safely() {
        let (pool, settings, alice, bob) = fixture().await;
        let carol = auth::register_user(&pool, &settings, "carol", "very secure password", false)
            .await
            .expect("carol");
        let _alex = auth::register_user(&pool, &settings, "al_ex", "very secure password", false)
            .await
            .expect("alex");
        let alx = auth::register_user(&pool, &settings, "alxex", "very secure password", false)
            .await
            .expect("alx");
        let deleted = auth::register_user(
            &pool,
            &settings,
            "al-deleted",
            "very secure password",
            false,
        )
        .await
        .expect("deleted");
        let muted =
            auth::register_user(&pool, &settings, "al-muted", "very secure password", false)
                .await
                .expect("muted");
        let blocker = auth::register_user(
            &pool,
            &settings,
            "al-blocker",
            "very secure password",
            false,
        )
        .await
        .expect("blocker");
        pool.call(move |conn| {
            conn.execute("UPDATE users SET is_suspended = 1 WHERE id = ?", [carol])?;
            conn.execute("UPDATE users SET is_deleted = 1 WHERE id = ?", [deleted])?;
            conn.execute(
                "UPDATE users SET display_name = '<b>Alice</b>' WHERE id = ?",
                [alice],
            )?;
            conn.execute(
                "INSERT INTO blocks (blocker_id, blocked_id) VALUES (?, ?)",
                params![bob, alx],
            )?;
            conn.execute(
                "INSERT INTO mutes (muter_id, muted_id) VALUES (?, ?)",
                params![bob, muted],
            )?;
            conn.execute(
                "INSERT INTO blocks (blocker_id, blocked_id) VALUES (?, ?)",
                params![blocker, bob],
            )?;
            Ok(())
        })
        .await
        .expect("mark users");

        let suggestions = mention_suggestions(&pool, Some(bob), "AL")
            .await
            .expect("suggestions");
        assert_eq!(
            suggestions,
            vec![
                MentionSuggestion {
                    username: "al_ex".to_owned(),
                    display_name: "al_ex".to_owned(),
                },
                MentionSuggestion {
                    username: "alice".to_owned(),
                    display_name: "<b>Alice</b>".to_owned(),
                },
            ]
        );

        let escaped = mention_suggestions(&pool, Some(bob), "al_")
            .await
            .expect("escaped suggestions");
        assert_eq!(
            escaped
                .iter()
                .map(|suggestion| suggestion.username.as_str())
                .collect::<Vec<_>>(),
            vec!["al_ex"]
        );
        assert!(
            !escaped
                .iter()
                .any(|suggestion| suggestion.username == "alxex")
        );

        for wildcard_query in ["%", "_", "al%"] {
            let wildcard = mention_suggestions(&pool, Some(bob), wildcard_query)
                .await
                .expect("wildcard query");
            assert!(
                wildcard.is_empty(),
                "{wildcard_query:?} should not broaden mention suggestions"
            );
        }
    }

    #[tokio::test]
    async fn notification_groups_count_unread_and_mark_group_read() {
        let (pool, settings, alice, bob) = fixture().await;
        let carol = auth::register_user(&pool, &settings, "carol", "very secure password", false)
            .await
            .expect("carol");
        let dave = auth::register_user(&pool, &settings, "dave", "very secure password", false)
            .await
            .expect("dave");
        let first_post = create_post(&pool, &settings, Some(alice), "first post", None, &[])
            .await
            .expect("first post");
        let second_post = create_post(&pool, &settings, Some(alice), "second post", None, &[])
            .await
            .expect("second post");

        like(&pool, bob, first_post).await.expect("bob like");
        like(&pool, carol, second_post).await.expect("carol like");
        like(&pool, dave, first_post).await.expect("dave like");

        let groups = notification_groups(&pool, alice).await.expect("groups");

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].kind, "like");
        assert_eq!(groups[0].post_id, Some(first_post));
        assert_eq!(groups[0].total_count, 2);
        assert_eq!(groups[0].unread_count, 2);
        assert_eq!(
            groups[0]
                .actors
                .iter()
                .filter_map(|actor| actor.username.as_deref())
                .collect::<Vec<_>>(),
            vec!["dave", "bob"]
        );
        assert_eq!(groups[1].post_id, Some(second_post));
        assert_eq!(groups[1].unread_count, 1);

        mark_notification_ids_read(&pool, alice, &groups[0].notification_ids)
            .await
            .expect("mark group read");

        assert_eq!(
            unread_notification_count(&pool, alice)
                .await
                .expect("unread count"),
            1
        );
        let groups = notification_groups(&pool, alice).await.expect("groups");
        let first_group = groups
            .iter()
            .find(|group| group.post_id == Some(first_post))
            .expect("first post group");
        let second_group = groups
            .iter()
            .find(|group| group.post_id == Some(second_post))
            .expect("second post group");
        assert_eq!(first_group.unread_count, 0);
        assert_eq!(second_group.unread_count, 1);
    }

    #[tokio::test]
    async fn notification_group_counts_and_read_transition_include_older_rows() {
        let (pool, settings, alice, _bob) = fixture().await;
        let post = create_post(&pool, &settings, Some(alice), "busy post", None, &[])
            .await
            .expect("post");
        pool.call(move |conn| {
            let tx = conn.transaction()?;
            for _ in 0..85 {
                tx.execute(
                    "INSERT INTO notifications (user_id, actor_user_id, post_id, kind, message) VALUES (?, NULL, ?, 'like', 'liked your post')",
                    params![alice, post],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
        .expect("notifications");

        let groups = notification_groups(&pool, alice).await.expect("groups");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].total_count, 85);
        assert_eq!(groups[0].unread_count, 85);
        assert_eq!(groups[0].notification_ids.len(), 80);

        mark_notification_group_read(&pool, alice, "like", Some(post))
            .await
            .expect("mark group read");

        assert_eq!(
            unread_notification_count(&pool, alice)
                .await
                .expect("unread count"),
            0
        );
        let groups = notification_groups(&pool, alice).await.expect("groups");
        assert_eq!(groups[0].total_count, 85);
        assert_eq!(groups[0].unread_count, 0);
    }

    #[tokio::test]
    async fn notification_groups_keep_types_and_targets_separate() {
        let (pool, settings, alice, bob) = fixture().await;
        let carol = auth::register_user(&pool, &settings, "carol", "very secure password", false)
            .await
            .expect("carol");
        let post = create_post(&pool, &settings, Some(alice), "group target", None, &[])
            .await
            .expect("post");

        like(&pool, bob, post).await.expect("bob like");
        like(&pool, carol, post).await.expect("carol like");
        repost(&pool, bob, post).await.expect("bob repost");
        repost(&pool, carol, post).await.expect("carol repost");
        create_post(&pool, &settings, Some(bob), "reply one", Some(post), &[])
            .await
            .expect("reply one");
        create_post(&pool, &settings, Some(carol), "reply two", Some(post), &[])
            .await
            .expect("reply two");
        follow(&pool, bob, alice).await.expect("bob follow");
        follow(&pool, carol, alice).await.expect("carol follow");

        let groups = notification_groups(&pool, alice).await.expect("groups");

        let like_group = groups
            .iter()
            .find(|group| group.kind == "like")
            .expect("like group");
        let repost_group = groups
            .iter()
            .find(|group| group.kind == "repost")
            .expect("repost group");
        let reply_group = groups
            .iter()
            .find(|group| group.kind == "reply")
            .expect("reply group");
        let follow_group = groups
            .iter()
            .find(|group| group.kind == "follow")
            .expect("follow group");

        assert_eq!(like_group.total_count, 2);
        assert_eq!(repost_group.total_count, 2);
        assert_eq!(reply_group.total_count, 2);
        assert_eq!(follow_group.total_count, 2);
        assert_ne!(like_group.notification_ids, repost_group.notification_ids);
        assert_ne!(like_group.notification_ids, reply_group.notification_ids);
    }

    #[test]
    fn notification_grouping_keeps_mentions_on_distinct_targets() {
        let rows = vec![
            NotificationView {
                id: 3,
                kind: "mention".to_owned(),
                actor_user_id: Some(3),
                actor_username: Some("carol".to_owned()),
                actor_display_name: Some("Carol".to_owned()),
                post_id: Some(10),
                group_target_post_id: Some(10),
                post_text: Some("same target".to_owned()),
                post_available: true,
                read_at: None,
                created_at: "2026-05-24 10:03:00".to_owned(),
            },
            NotificationView {
                id: 2,
                kind: "mention".to_owned(),
                actor_user_id: Some(2),
                actor_username: Some("bob".to_owned()),
                actor_display_name: Some("Bob".to_owned()),
                post_id: Some(10),
                group_target_post_id: Some(10),
                post_text: Some("same target".to_owned()),
                post_available: true,
                read_at: Some("2026-05-24 10:04:00".to_owned()),
                created_at: "2026-05-24 10:02:00".to_owned(),
            },
            NotificationView {
                id: 1,
                kind: "mention".to_owned(),
                actor_user_id: Some(4),
                actor_username: Some("dave".to_owned()),
                actor_display_name: Some("Dave".to_owned()),
                post_id: Some(11),
                group_target_post_id: Some(11),
                post_text: Some("other target".to_owned()),
                post_available: true,
                read_at: None,
                created_at: "2026-05-24 10:01:00".to_owned(),
            },
        ];

        let groups = group_notifications(&rows);

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].post_id, Some(10));
        assert_eq!(groups[0].total_count, 2);
        assert_eq!(groups[0].unread_count, 1);
        assert_eq!(groups[1].post_id, Some(11));
        assert_eq!(groups[1].total_count, 1);
    }

    #[tokio::test]
    async fn mark_notifications_read_persists_read_state() {
        let (pool, settings, alice, bob) = fixture().await;
        let post = create_post(&pool, &settings, Some(alice), "hello", None, &[])
            .await
            .expect("post");
        like(&pool, bob, post).await.expect("like");

        assert_eq!(
            unread_notification_count(&pool, alice)
                .await
                .expect("unread before"),
            1
        );
        mark_notifications_read(&pool, alice)
            .await
            .expect("mark read");

        let notifications = notifications(&pool, alice).await.expect("notifications");
        assert_eq!(
            unread_notification_count(&pool, alice)
                .await
                .expect("unread after"),
            0
        );
        assert!(notifications[0].read_at.is_some());
    }

    #[tokio::test]
    async fn follow_unfollow_is_idempotent_and_counts_once() {
        let (pool, _settings, alice, bob) = fixture().await;

        assert!(follow(&pool, bob, alice).await.expect("first follow"));
        assert!(!follow(&pool, bob, alice).await.expect("duplicate follow"));
        assert!(is_following(&pool, bob, alice).await.expect("is following"));
        assert_eq!(
            follow_counts(&pool, alice).await.expect("alice counts"),
            (1, 0)
        );
        assert_eq!(follow_counts(&pool, bob).await.expect("bob counts"), (0, 1));

        let notifications: i64 = pool
            .call(move |conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM notifications WHERE user_id = ? AND actor_user_id = ? AND kind = 'follow'",
                    params![alice, bob],
                    |row| row.get(0),
                )?)
            })
            .await
            .expect("notification count");
        assert_eq!(notifications, 1);

        assert!(unfollow(&pool, bob, alice).await.expect("unfollow"));
        assert!(
            !unfollow(&pool, bob, alice)
                .await
                .expect("duplicate unfollow")
        );
        assert!(
            !is_following(&pool, bob, alice)
                .await
                .expect("not following")
        );
    }

    #[tokio::test]
    async fn follow_rejects_deleted_or_suspended_targets() {
        let (pool, settings, alice, bob) = fixture().await;
        let carol = auth::register_user(&pool, &settings, "carol", "very secure password", false)
            .await
            .expect("carol");
        pool.call(move |conn| {
            conn.execute("UPDATE users SET is_suspended = 1 WHERE id = ?", [bob])?;
            conn.execute("UPDATE users SET is_deleted = 1 WHERE id = ?", [carol])?;
            Ok(())
        })
        .await
        .expect("mark unavailable");

        let suspended = follow(&pool, alice, bob).await.expect_err("suspended");
        let deleted = follow(&pool, alice, carol).await.expect_err("deleted");

        assert_eq!(suspended.to_string(), "account cannot be followed");
        assert_eq!(deleted.to_string(), "account cannot be followed");
        assert!(
            !is_following(&pool, alice, bob)
                .await
                .expect("not following")
        );
        assert!(
            !is_following(&pool, alice, carol)
                .await
                .expect("not following")
        );
    }

    #[tokio::test]
    async fn following_accounts_returns_only_viewer_follows() {
        let (pool, settings, alice, bob) = fixture().await;
        let carol = auth::register_user(&pool, &settings, "carol", "very secure password", false)
            .await
            .expect("carol");

        follow(&pool, alice, bob).await.expect("alice follows bob");
        follow(&pool, carol, alice)
            .await
            .expect("carol follows alice");

        let accounts = following_accounts(&pool, alice)
            .await
            .expect("following accounts");
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].id, bob);
        assert_eq!(accounts[0].username, "bob");
        assert!(accounts[0].viewer_following);
    }
}
