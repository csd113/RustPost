//! Stable keyset navigation across post and repost events.

use super::{
    PostView, ProfileTimelineTab, SqlitePool, TimelineEventKind, append_viewer_filters,
    base_post_query, params_from_iter, posting, profile_activity_visible, query, query_post_rows,
    rows_to_posts,
};
use base64::Engine as _;
use std::fmt::Write as _;

#[derive(Clone, Copy)]
pub enum Surface {
    Everyone,
    Following,
    Bookmarks,
    Profile { i: i64, tab: ProfileTimelineTab },
}

pub struct Page {
    pub posts: Vec<PostView>,
    pub older: Option<String>,
    pub newer: Option<String>,
}

fn token(post: &PostView) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!(
        "{}|{}|{}",
        post.event_created_at,
        posting::event_sort_key(post),
        post.event_kind == TimelineEventKind::Repost
    ))
}

fn decode(value: &str) -> anyhow::Result<(String, i64, String)> {
    anyhow::ensure!(value.len() <= 128, "invalid history cursor");
    let decoded =
        String::from_utf8(base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(value)?)?;
    let parts = decoded.split('|').collect::<Vec<_>>();
    anyhow::ensure!(parts.len() == 3, "invalid history cursor");
    chrono::NaiveDateTime::parse_from_str(parts[0], "%Y-%m-%d %H:%M:%S")?;
    let id = parts[1].parse::<i64>()?;
    let kind = match parts[2] {
        "true" => "repost",
        "false" => "post",
        _ => anyhow::bail!("invalid history cursor"),
    };
    Ok((parts[0].to_owned(), id, kind.to_owned()))
}

fn viewer_filters(
    sql: &mut String,
    bindings: &mut Vec<rusqlite::types::Value>,
    viewer: Option<i64>,
) {
    append_viewer_filters(sql, "p.user_id", "p.text", viewer);
    if let Some(id) = viewer {
        bindings.extend([id.into(), id.into(), id.into(), id.into(), id.into()]);
    }
}

pub async fn page(
    pool: &SqlitePool,
    viewer: Option<i64>,
    surface: Surface,
    older: Option<&str>,
    newer: Option<&str>,
) -> anyhow::Result<Page> {
    anyhow::ensure!(
        older.is_none() || newer.is_none(),
        "choose one history direction"
    );
    if matches!(surface, Surface::Following | Surface::Bookmarks) {
        anyhow::ensure!(viewer.is_some(), "log in to view this feed");
    }
    if let Surface::Profile { i, tab } = surface {
        if !profile_activity_visible(pool, viewer, i).await? {
            return Ok(Page {
                posts: Vec::new(),
                older: None,
                newer: None,
            });
        }
        if tab == ProfileTimelineTab::Likes && viewer != Some(i) {
            let public=pool.call(move |conn|Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM users WHERE id=? AND liked_posts_public=1 AND is_deleted=0)",[i],|r|r.get::<_,bool>(0))?)).await?;
            if !public {
                return Ok(Page {
                    posts: Vec::new(),
                    older: None,
                    newer: None,
                });
            }
        }
    }
    let (posts, mut bindings) = surface_queries(viewer, surface);
    let mut sql = format!("SELECT * FROM ({posts}) events WHERE 1=1");
    if let Some(cursor) = older.or(newer) {
        let (date, id, kind) = decode(cursor)?;
        sql.push_str(if newer.is_some() {
            " AND (event_created_at,CAST(substr(event_id,3) AS INTEGER),event_kind) > (?,?,?)"
        } else {
            " AND (event_created_at,CAST(substr(event_id,3) AS INTEGER),event_kind) < (?,?,?)"
        });
        bindings.extend([date.into(), id.into(), kind.into()]);
    }
    let direction = if newer.is_some() { "ASC" } else { "DESC" };
    let _ = write!(
        sql,
        " ORDER BY event_created_at {direction},CAST(substr(event_id,3) AS INTEGER) {direction},event_kind {direction} LIMIT 41"
    );
    let mut rows = pool
        .call(move |conn| query_post_rows(conn, &sql, params_from_iter(bindings)))
        .await?;
    let more = rows.len() > 40;
    rows.truncate(40);
    let mut posts = rows_to_posts(pool, rows, viewer).await?;
    if newer.is_some() {
        posts.reverse();
    }
    let older_token = if (newer.is_some() || more)
        && let Some(last) = posts.last()
    {
        Some(token(last))
    } else {
        None
    };
    let newer_token = if (older.is_some() || (newer.is_some() && more))
        && let Some(first) = posts.first()
    {
        Some(token(first))
    } else {
        None
    };
    Ok(Page {
        posts,
        older: older_token,
        newer: newer_token,
    })
}

pub struct ThreadPage {
    pub posts: Vec<PostView>,
    pub requested: PostView,
    pub root: i64,
    pub before: Option<i64>,
    pub after: Option<i64>,
}

pub async fn thread_page(
    pool: &SqlitePool,
    viewer: Option<i64>,
    id: i64,
    before: Option<i64>,
    after: Option<i64>,
) -> anyhow::Result<Option<ThreadPage>> {
    anyhow::ensure!(
        before.is_none() || after.is_none(),
        "choose one conversation direction"
    );
    let mut target = base_post_query();
    target.push_str(" AND p.id=?");
    let mut bindings = vec![id.into()];
    viewer_filters(&mut target, &mut bindings, viewer);
    let rows = pool
        .call(move |conn| query_post_rows(conn, &target, params_from_iter(bindings)))
        .await?;
    let Some(requested) = rows_to_posts(pool, rows, viewer).await?.pop() else {
        return Ok(None);
    };
    let root = pool
        .call(move |conn| {
            Ok(conn.query_row(
                "SELECT COALESCE(root_post_id,id) FROM posts WHERE id=?",
                [id],
                |r| r.get::<_, i64>(0),
            )?)
        })
        .await?;
    let mut sql = base_post_query();
    sql.push_str(" AND (p.id=? OR p.root_post_id=?)");
    let mut bindings = vec![root.into(), root.into()];
    viewer_filters(&mut sql, &mut bindings, viewer);
    let base = sql.clone();
    let base_bindings = bindings.clone();
    if let Some(cursor) = before {
        sql.push_str(" AND p.id<?");
        bindings.push(cursor.into());
    } else if let Some(cursor) = after {
        sql.push_str(" AND p.id>?");
        bindings.push(cursor.into());
    } else {
        sql.push_str(" AND p.id>=?");
        bindings.push(id.into());
    }
    sql.push_str(if before.is_some() {
        " ORDER BY p.id DESC LIMIT 100"
    } else {
        " ORDER BY p.id ASC LIMIT 100"
    });
    let rows = pool
        .call(move |conn| query_post_rows(conn, &sql, params_from_iter(bindings)))
        .await?;
    let mut posts = rows_to_posts(pool, rows, viewer).await?;
    if before.is_some() {
        posts.reverse();
    }
    let (previous, next) = if let (Some(first), Some(last)) = (posts.first(), posts.last()) {
        let first = first.id;
        let last = last.id;
        pool.call(move |conn| {
            let mut earlier = base_bindings.clone();
            earlier.push(first.into());
            let mut later = base_bindings;
            later.push(last.into());
            let previous = !query_post_rows(
                conn,
                &format!("{base} AND p.id<? LIMIT 1"),
                params_from_iter(earlier),
            )?
            .is_empty();
            let next = !query_post_rows(
                conn,
                &format!("{base} AND p.id>? LIMIT 1"),
                params_from_iter(later),
            )?
            .is_empty();
            Ok((previous.then_some(first), next.then_some(last)))
        })
        .await?
    } else {
        (None, None)
    };
    Ok(Some(ThreadPage {
        posts,
        requested,
        root,
        before: previous,
        after: next,
    }))
}

fn surface_queries(viewer: Option<i64>, surface: Surface) -> (String, Vec<rusqlite::types::Value>) {
    let mut posts = base_post_query();
    let mut bindings: Vec<rusqlite::types::Value> = Vec::new();
    match surface {
        Surface::Everyone => posts.push_str(" AND p.parent_post_id IS NULL"),
        Surface::Following => {
            posts.push_str(" AND p.parent_post_id IS NULL AND (p.user_id=? OR p.user_id IN (SELECT followed_id FROM follows WHERE follower_id=?))");
            bindings.extend([viewer.unwrap_or(-1).into(), viewer.unwrap_or(-1).into()]);
        }
        Surface::Bookmarks => {
            posts.push_str(" AND EXISTS(SELECT 1 FROM bookmarks WHERE post_id=p.id AND user_id=?)");
            bindings.push(viewer.unwrap_or(-1).into());
        }
        Surface::Profile { i, tab } => {
            if tab == ProfileTimelineTab::Likes {
                posts=posts.replace("p.created_at AS event_created_at",&format!("(SELECT created_at FROM likes WHERE user_id={i} AND post_id=p.id) AS event_created_at"));
                posts.push_str(" AND EXISTS(SELECT 1 FROM likes WHERE post_id=p.id AND user_id=?)");
                bindings.push(i.into());
            } else {
                posts.push_str(" AND p.user_id=?");
                bindings.push(i.into());
                match tab {
                    ProfileTimelineTab::Posts => posts.push_str(" AND p.parent_post_id IS NULL"),
                    ProfileTimelineTab::Replies => {
                        posts.push_str(" AND p.parent_post_id IS NOT NULL")
                    }
                    ProfileTimelineTab::Media => {
                        posts.push_str(" AND ");
                        posts.push_str(&query::media_surface_condition("p.id", "p.text"));
                    }
                    ProfileTimelineTab::Likes => {}
                }
            }
        }
    }
    viewer_filters(&mut posts, &mut bindings, viewer);
    if matches!(
        surface,
        Surface::Everyone
            | Surface::Following
            | Surface::Profile {
                tab: ProfileTimelineTab::Posts | ProfileTimelineTab::Media,
                ..
            }
    ) {
        let mut reposts = query::base_repost_query();
        match surface {
            Surface::Following => {
                reposts.push_str(" AND (r.user_id=? OR r.user_id IN (SELECT followed_id FROM follows WHERE follower_id=?))");
                bindings.extend([viewer.unwrap_or(-1).into(), viewer.unwrap_or(-1).into()]);
            }
            Surface::Profile { i, tab } => {
                reposts.push_str(" AND r.user_id=?");
                bindings.push(i.into());
                if tab == ProfileTimelineTab::Media {
                    reposts.push_str(" AND ");
                    reposts.push_str(&query::media_surface_condition(
                        "p.id",
                        "COALESCE(p.text,'')",
                    ));
                }
            }
            _ => {}
        }
        if let Some(id) = viewer {
            reposts.push_str(" AND r.user_id NOT IN (SELECT blocked_id FROM blocks WHERE blocker_id=?) AND r.user_id NOT IN (SELECT muted_id FROM mutes WHERE muter_id=?) AND NOT EXISTS(SELECT 1 FROM blocks WHERE blocker_id=r.user_id AND blocked_id=?)");
            bindings.extend([id.into(), id.into(), id.into()]);
        }
        viewer_filters(&mut reposts, &mut bindings, viewer);
        posts.push_str(" UNION ALL ");
        posts.push_str(&reposts);
    }

    (posts, bindings)
}
