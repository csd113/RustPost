//! Search for the social subsystem.

use super::{
    AccountView, MENTION_QUERY_MAX_CHARS, MENTION_SUGGESTION_LIMIT, MentionSuggestion, PostView,
    SqlitePool, append_viewer_filters, base_post_query, params, query_post_rows, rows_to_posts,
};

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
                ORDER BY lower(u.username)
                LIMIT 20
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
                        display_query
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
        .await?;
    let rows = if let Some(fts_query) = fts_query_from_user_input(query) {
        let mut post_sql = base_post_query();
        post_sql.push_str(" AND p.id IN (SELECT rowid FROM posts_fts WHERE posts_fts MATCH ?)");
        append_viewer_filters(&mut post_sql, "p.user_id", "p.text", viewer_id);
        post_sql.push_str(" LIMIT 40");
        pool.call(move |conn| {
            if let Some(viewer_id) = viewer_id {
                query_post_rows(
                    conn,
                    &post_sql,
                    params![
                        fts_query, viewer_id, viewer_id, viewer_id, viewer_id, viewer_id
                    ],
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
