//! Search for the social subsystem.

use super::{
    AccountView, MENTION_QUERY_MAX_CHARS, MENTION_SUGGESTION_LIMIT, MentionSuggestion, PostView,
    SqlitePool, append_viewer_filters, base_post_query, params, query_post_rows, rows_to_posts,
};

pub async fn search_history(
    pool: &SqlitePool,
    viewer_id: Option<i64>,
    query: &str,
    cursor: Option<i64>,
    user_after: Option<String>,
    newer: Option<i64>,
) -> anyhow::Result<SearchPage> {
    anyhow::ensure!(
        cursor.is_none() || newer.is_none(),
        "choose one search direction"
    );
    let filters = SearchFilters::parse(query)?;
    anyhow::ensure!(
        !filters.bookmarks || viewer_id.is_some(),
        "log in to search bookmarks"
    );
    let user_query = if filters.filtered {
        None
    } else {
        user_query_from_input(&filters.text)
    };
    let user_after = user_after.unwrap_or_default();
    anyhow::ensure!(user_after.len() <= 200, "invalid account cursor");
    let mut users = search_users(pool, viewer_id, user_query, user_after).await?;
    let has_more_users = users.len() > 20;
    users.truncate(20);
    let mut post_sql = base_post_query();
    let mut bindings: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(fts) = fts_query_from_user_input(&filters.text) {
        post_sql.push_str(" AND p.id IN (SELECT rowid FROM posts_fts WHERE posts_fts MATCH ?)");
        bindings.push(fts.into());
    } else if !filters.filtered {
        return Ok(SearchPage {
            users,
            posts: Vec::new(),
            has_more_users,
            has_more_posts: false,
            has_newer_posts: false,
        });
    }
    for (column, value) in [
        ("u.normalized_username = ?", filters.author),
        ("p.created_at >= ?", filters.after),
        ("p.created_at < ?", filters.before),
    ] {
        if let Some(value) = value {
            post_sql.push_str(" AND ");
            post_sql.push_str(column);
            bindings.push(value.into());
        }
    }
    if let Some(kind) = filters.media {
        if kind == "media" {
            post_sql.push_str(" AND ");
            post_sql.push_str(&super::query::media_surface_condition("p.id", "p.text"));
        } else {
            post_sql.push_str(" AND EXISTS(SELECT 1 FROM post_media pm JOIN media m ON m.id=pm.media_id WHERE pm.post_id=p.id AND m.media_kind=?)");
            bindings.push(kind.into());
        }
    }

    if filters.bookmarks {
        post_sql.push_str(" AND EXISTS(SELECT 1 FROM bookmarks WHERE post_id=p.id AND user_id=?)");
        bindings.push(viewer_id.unwrap_or(-1).into());
    }
    if let Some(cursor) = cursor.or(newer) {
        post_sql.push_str(if newer.is_some() {
            " AND p.id > ?"
        } else {
            " AND p.id < ?"
        });
        bindings.push(cursor.into());
    }
    append_viewer_filters(&mut post_sql, "p.user_id", "p.text", viewer_id);
    if let Some(id) = viewer_id {
        bindings.extend([id.into(), id.into(), id.into(), id.into(), id.into()]);
    }
    post_sql.push_str(if newer.is_some() {
        " ORDER BY p.id ASC LIMIT 41"
    } else {
        " ORDER BY p.id DESC LIMIT 41"
    });
    let mut rows = pool
        .call(move |conn| query_post_rows(conn, &post_sql, rusqlite::params_from_iter(bindings)))
        .await?;
    let has_more_posts = rows.len() > 40;
    rows.truncate(40);
    let mut posts = rows_to_posts(pool, rows, viewer_id).await?;
    if newer.is_some() {
        posts.reverse();
    }
    let has_newer_posts =
        (cursor.is_some() || (newer.is_some() && has_more_posts)) && !posts.is_empty();
    let has_more_posts = if newer.is_some() {
        !posts.is_empty()
    } else {
        has_more_posts
    };
    Ok(SearchPage {
        users,
        posts,
        has_more_users,
        has_more_posts,
        has_newer_posts,
    })
}

pub async fn search_page(
    pool: &SqlitePool,
    viewer: Option<i64>,
    query: &str,
    cursor: Option<i64>,
    user_after: Option<String>,
) -> anyhow::Result<SearchPage> {
    search_history(pool, viewer, query, cursor, user_after, None).await
}

pub struct SearchPage {
    pub users: Vec<AccountView>,
    pub posts: Vec<PostView>,
    pub has_more_users: bool,
    pub has_more_posts: bool,
    pub has_newer_posts: bool,
}

pub async fn search(
    pool: &SqlitePool,
    viewer: Option<i64>,
    query: &str,
) -> anyhow::Result<(Vec<AccountView>, Vec<PostView>)> {
    let page = search_page(pool, viewer, query, None, None).await?;
    Ok((page.users, page.posts))
}

#[derive(Default, Debug)]
struct SearchFilters {
    text: String,
    author: Option<String>,
    after: Option<String>,
    before: Option<String>,
    media: Option<String>,
    bookmarks: bool,
    filtered: bool,
}

impl SearchFilters {
    fn parse(query: &str) -> anyhow::Result<Self> {
        anyhow::ensure!(query.chars().count() <= 500, "search is too long");
        let mut filters = Self::default();
        let mut terms = Vec::new();
        for word in query.split_whitespace() {
            if let Some(author) = word.strip_prefix("from:") {
                let author = author.trim_start_matches('@');
                anyhow::ensure!(
                    !author.is_empty()
                        && author.len() <= 64
                        && author
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
                    "invalid author filter"
                );
                filters.author = Some(author.to_ascii_lowercase());
            } else if let Some(date) = word.strip_prefix("after:") {
                filters.after = Some(search_date(date)?);
            } else if let Some(date) = word.strip_prefix("before:") {
                filters.before = Some(search_date(date)?);
            } else if let Some(kind) = word.strip_prefix("has:") {
                anyhow::ensure!(
                    matches!(kind, "media" | "image" | "video"),
                    "use has:media, has:image or has:video"
                );
                filters.media = Some(kind.to_owned());
            } else if word == "is:bookmarked" {
                filters.bookmarks = true;
            } else {
                terms.push(word);
                continue;
            }
            filters.filtered = true;
        }
        if let (Some(after), Some(before)) = (&filters.after, &filters.before) {
            anyhow::ensure!(after < before, "after must be earlier than before");
        }
        filters.text = terms.join(" ");
        Ok(filters)
    }
}

fn search_date(value: &str) -> anyhow::Result<String> {
    anyhow::ensure!(value.len() == 10, "search dates use YYYY-MM-DD");
    let date = chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")?;
    Ok(format!("{} 00:00:00", date.format("%Y-%m-%d")))
}

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

pub(super) fn mention_query_fragment(query: &str) -> Option<String> {
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

pub(super) fn escape_like_prefix(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(character, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

pub(super) fn user_query_from_input(query: &str) -> Option<String> {
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

pub(super) fn fts_query_from_user_input(query: &str) -> Option<String> {
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

async fn search_users(
    pool: &SqlitePool,
    viewer_id: Option<i64>,
    user_query: Option<String>,
    user_after: String,
) -> anyhow::Result<Vec<AccountView>> {
    pool.call(move |conn| {
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
                  AND u.is_suspended = 0
                  AND (
                    ? < 0 OR (
                      u.id NOT IN (SELECT blocked_id FROM blocks WHERE blocker_id = ?)
                      AND u.id NOT IN (SELECT muted_id FROM mutes WHERE muter_id = ?)
                      AND NOT EXISTS (
                        SELECT 1 FROM blocks
                        WHERE blocker_id = u.id AND blocked_id = ?
                      )
                    )
                  )
                  AND (u.normalized_username LIKE ? OR u.display_name LIKE ?)
                AND u.normalized_username > ?
                ORDER BY u.normalized_username
                LIMIT 21
                "#,
        )?;
        let rows = stmt
            .query_map(
                params![
                    viewer_id,
                    viewer_id,
                    viewer_id,
                    viewer_id,
                    viewer_id,
                    username_query,
                    display_query,
                    user_after
                ],
                |row| {
                    Ok(AccountView {
                        id: row.get(0)?,
                        username: row.get(1)?,
                        display_name: row.get(2)?,
                        bio: row.get(3)?,
                        profile_picture_path: row.get(4)?,
                        viewer_following: row.get::<_, i64>(5)? != 0,
                    })
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}
