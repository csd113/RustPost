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
    pub event_created_at: String,
    pub like_count: i64,
    pub repost_count: i64,
    pub reply_count: i64,
    pub viewer_liked: bool,
    pub viewer_bookmarked: bool,
    pub viewer_reposted: bool,
    pub viewer_can_repost: bool,
    pub original_unavailable: bool,
    pub reposted_by_user_id: Option<i64>,
    pub reposted_by_username: Option<String>,
    pub reposted_by_display_name: Option<String>,
    pub reposted_at: Option<String>,
    pub media: Vec<MediaView>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimelineEventKind {
    Post,
    Repost,
}

#[derive(Debug, Clone)]
pub struct MediaView {
    pub public_path: String,
    pub mime_type: String,
    pub media_kind: String,
    pub alt_text: String,
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
    like_count: i64,
    repost_count: i64,
    reply_count: i64,
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
        if let (Some(parent_id), Some(actor_id)) = (parent_post_id, user_id) {
            notify_post_owner_tx(
                &tx,
                parent_id,
                actor_id,
                post_id,
                "reply",
                "replied to your post",
            )?;
        }
        tx.commit()?;
        Ok(post_id)
    })
    .await
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
    let mut posts = post_events_for_user(pool, viewer_id, user_id).await?;
    posts.extend(repost_events(pool, viewer_id, "profile", Some(user_id)).await?);
    posts.sort_by(|left, right| {
        right
            .event_created_at
            .cmp(&left.event_created_at)
            .then_with(|| right.event_id.cmp(&left.event_id))
    });
    posts.truncate(40);
    Ok(posts)
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
        let changed = conn.execute(
            "INSERT OR IGNORE INTO follows (follower_id, followed_id) VALUES (?, ?)",
            params![follower_id, followed_id],
        )?;
        if changed > 0 {
            create_notification_conn(
                conn,
                followed_id,
                Some(follower_id),
                None,
                "follow",
                "followed you",
            )?;
        }
        Ok(changed > 0)
    })
    .await
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
            SELECT u.id, u.username, u.display_name, u.bio, pic.public_path
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

pub async fn like(pool: &SqlitePool, user_id: i64, post_id: i64) -> anyhow::Result<()> {
    pool.call(move |conn| {
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO likes (user_id, post_id) VALUES (?, ?)",
            params![user_id, post_id],
        )?;
        notify_post_owner_tx(&tx, post_id, user_id, post_id, "like", "liked your post")?;
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
        Ok(())
    })
    .await
}

pub async fn search(
    pool: &SqlitePool,
    viewer_id: Option<i64>,
    query: &str,
) -> anyhow::Result<(Vec<(i64, String, String)>, Vec<PostView>)> {
    let username_query = format!("%{}%", query.to_ascii_lowercase());
    let display_query = format!("%{query}%");
    let users = pool
        .call(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, username, display_name FROM users WHERE is_deleted = 0 AND (normalized_username LIKE ? OR display_name LIKE ?) LIMIT 20",
            )?;
            let rows = stmt
                .query_map(params![username_query, display_query], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?;
    let mut post_sql = base_post_query();
    post_sql
        .push_str(" AND p.id IN (SELECT rowid FROM posts_fts WHERE posts_fts MATCH ?) LIMIT 40");
    let query = query.to_owned();
    let rows = pool
        .call(move |conn| query_post_rows(conn, &post_sql, params![query]))
        .await?;
    Ok((users, rows_to_posts(pool, rows, viewer_id).await?))
}

pub async fn notifications(
    pool: &SqlitePool,
    user_id: i64,
) -> anyhow::Result<Vec<(i64, String, String, Option<i64>, String)>> {
    pool.call(move |conn| {
        let mut stmt = conn.prepare(
            "SELECT id, kind, message, post_id, created_at FROM notifications WHERE user_id = ? ORDER BY id DESC LIMIT 80",
        )?;
        let rows = stmt
            .query_map([user_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
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

fn base_post_query() -> String {
    r#"
    SELECT 'post' AS event_kind, 'p:' || p.id AS event_id, p.created_at AS event_created_at,
      NULL AS repost_user_id, NULL AS repost_username, NULL AS repost_display_name, NULL AS repost_created_at,
      0 AS original_unavailable,
      p.id, p.user_id, u.username, u.display_name, pic.public_path AS profile_picture_path,
      p.anonymous_label, p.text, p.parent_post_id, p.created_at,
      (SELECT COUNT(*) FROM likes WHERE post_id = p.id) AS like_count,
      (SELECT COUNT(*) FROM reposts WHERE post_id = p.id) AS repost_count,
      (SELECT COUNT(*) FROM posts r WHERE r.parent_post_id = p.id AND r.is_deleted = 0) AS reply_count
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
      pic.public_path AS profile_picture_path, p.anonymous_label,
      COALESCE(p.text, '') AS text, p.parent_post_id, COALESCE(p.created_at, r.created_at) AS created_at,
      CASE WHEN p.id IS NULL OR p.is_deleted != 0 THEN 0 ELSE (SELECT COUNT(*) FROM likes WHERE post_id = p.id) END AS like_count,
      CASE WHEN p.id IS NULL OR p.is_deleted != 0 THEN 0 ELSE (SELECT COUNT(*) FROM reposts WHERE post_id = p.id) END AS repost_count,
      CASE WHEN p.id IS NULL OR p.is_deleted != 0 THEN 0 ELSE (SELECT COUNT(*) FROM posts replies WHERE replies.parent_post_id = p.id AND replies.is_deleted = 0) END AS reply_count
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

fn append_viewer_filters(sql: &mut String, user_column: &str, viewer_id: Option<i64>) {
    if viewer_id.is_some() {
        sql.push_str(&format!(
            " AND ({user_column} IS NULL OR {user_column} NOT IN (SELECT blocked_id FROM blocks WHERE blocker_id = ?))"
        ));
        sql.push_str(&format!(
            " AND ({user_column} IS NULL OR {user_column} NOT IN (SELECT muted_id FROM mutes WHERE muter_id = ?))"
        ));
    }
}

fn push_viewer_filter_bindings(bindings: &mut Vec<i64>, viewer_id: Option<i64>) {
    if let Some(id) = viewer_id {
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
        like_count: row.get(17)?,
        repost_count: row.get(18)?,
        reply_count: row.get(19)?,
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
            event_created_at: row.event_created_at,
            like_count: row.like_count,
            repost_count: row.repost_count,
            reply_count: row.reply_count,
            viewer_liked,
            viewer_bookmarked,
            viewer_reposted,
            viewer_can_repost,
            original_unavailable,
            reposted_by_user_id: row.repost_user_id,
            reposted_by_username: row.repost_username,
            reposted_by_display_name: row.repost_display_name,
            reposted_at: row.repost_created_at,
            media,
        });
    }
    Ok(posts)
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
        SELECT m.public_path, m.mime_type, m.media_kind, m.alt_text
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
) -> anyhow::Result<()> {
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
    Ok(())
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

fn create_notification_conn(
    conn: &Connection,
    user_id: i64,
    actor_id: Option<i64>,
    post_id: Option<i64>,
    kind: &str,
    message: &str,
) -> anyhow::Result<()> {
    if actor_id == Some(user_id) {
        return Ok(());
    }
    conn.execute(
        "INSERT INTO notifications (user_id, actor_user_id, post_id, kind, message) VALUES (?, ?, ?, ?, ?)",
        params![user_id, actor_id, post_id, kind, message],
    )?;
    Ok(())
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
