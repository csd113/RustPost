//! Query for the social subsystem.

use super::*;

pub(super) fn base_post_query() -> String {
    r#"
    SELECT 'post' AS event_kind, 'p:' || p.id AS event_id, p.created_at AS event_created_at,
      NULL AS repost_user_id, NULL AS repost_username, NULL AS repost_display_name, NULL AS repost_created_at,
      0 AS original_unavailable,
      p.id, p.user_id, u.username, u.display_name,
      COALESCE(pic.thumbnail_public_path, pic.public_path) AS profile_picture_path,
      p.anonymous_label, p.text, p.parent_post_id, p.created_at,
      p.edited_at,
      ((SELECT COUNT(*) FROM reposts WHERE post_id = p.id) +
       (SELECT COUNT(*) FROM posts qp WHERE qp.quote_post_id = p.id AND qp.is_deleted = 0)) AS repost_count,
      (SELECT COUNT(*) FROM posts r WHERE r.parent_post_id = p.id AND r.is_deleted = 0) AS reply_count,
      p.quote_post_id,
      COALESCE(u.pinned_post_id = p.id, 0) AS pinned_by_author
    FROM posts p
    LEFT JOIN users u ON u.id = p.user_id
    LEFT JOIN media pic ON pic.id = u.profile_picture_media_id
    WHERE p.is_deleted = 0
      AND (p.user_id IS NULL OR (u.is_deleted = 0 AND u.is_suspended = 0))
    "#.to_owned()
}

pub(super) fn base_repost_query() -> String {
    r#"
    SELECT 'repost' AS event_kind, 'r:' || r.id AS event_id, r.created_at AS event_created_at,
      ru.id AS repost_user_id, ru.username AS repost_username, ru.display_name AS repost_display_name, r.created_at AS repost_created_at,
      CASE WHEN p.id IS NULL OR p.is_deleted != 0 THEN 1 ELSE 0 END AS original_unavailable,
      COALESCE(p.id, r.post_id) AS id, p.user_id, u.username, u.display_name,
      COALESCE(pic.thumbnail_public_path, pic.public_path) AS profile_picture_path, p.anonymous_label,
      COALESCE(p.text, '') AS text, p.parent_post_id, COALESCE(p.created_at, r.created_at) AS created_at,
      p.edited_at,
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
    WHERE ru.is_deleted = 0 AND ru.is_suspended = 0
      AND (p.user_id IS NULL OR (u.is_deleted = 0 AND u.is_suspended = 0))
    "#.to_owned()
}

pub(super) async fn post_events(
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
    append_viewer_filters(&mut sql, "p.user_id", "p.text", viewer_id);
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

pub(super) async fn post_events_for_user(
    pool: &SqlitePool,
    viewer_id: Option<i64>,
    user_id: i64,
) -> anyhow::Result<Vec<PostView>> {
    let mut sql = base_post_query();
    sql.push_str(" AND p.user_id = ? AND p.parent_post_id IS NULL");
    append_viewer_filters(&mut sql, "p.user_id", "p.text", viewer_id);
    sql.push_str(" ORDER BY p.id DESC LIMIT 40");
    let mut bindings = vec![user_id];
    push_viewer_filter_bindings(&mut bindings, viewer_id);
    let rows = pool
        .call(move |conn| query_post_rows(conn, &sql, params_from_iter(bindings)))
        .await?;
    rows_to_posts(pool, rows, viewer_id).await
}

pub(super) async fn reply_events_for_user(
    pool: &SqlitePool,
    viewer_id: Option<i64>,
    user_id: i64,
) -> anyhow::Result<Vec<PostView>> {
    let mut sql = base_post_query();
    sql.push_str(" AND p.user_id = ? AND p.parent_post_id IS NOT NULL");
    append_viewer_filters(&mut sql, "p.user_id", "p.text", viewer_id);
    sql.push_str(" ORDER BY p.id DESC LIMIT 40");
    let mut bindings = vec![user_id];
    push_viewer_filter_bindings(&mut bindings, viewer_id);
    let rows = pool
        .call(move |conn| query_post_rows(conn, &sql, params_from_iter(bindings)))
        .await?;
    rows_to_posts(pool, rows, viewer_id).await
}

pub(super) async fn media_events_for_user(
    pool: &SqlitePool,
    viewer_id: Option<i64>,
    user_id: i64,
) -> anyhow::Result<Vec<PostView>> {
    let mut sql = base_post_query();
    sql.push_str(" AND p.user_id = ? AND ");
    sql.push_str(&media_surface_condition("p.id", "p.text"));
    append_viewer_filters(&mut sql, "p.user_id", "p.text", viewer_id);
    sql.push_str(" ORDER BY p.id DESC LIMIT 40");
    let mut bindings = vec![user_id];
    push_viewer_filter_bindings(&mut bindings, viewer_id);
    let rows = pool
        .call(move |conn| query_post_rows(conn, &sql, params_from_iter(bindings)))
        .await?;
    rows_to_posts(pool, rows, viewer_id).await
}

pub(super) async fn liked_events_for_user(
    pool: &SqlitePool,
    viewer_id: Option<i64>,
    user_id: i64,
) -> anyhow::Result<Vec<PostView>> {
    let mut sql = base_post_query();
    sql.push_str(" AND p.id IN (SELECT post_id FROM likes WHERE user_id = ?)");
    append_viewer_filters(&mut sql, "p.user_id", "p.text", viewer_id);
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

pub(super) async fn repost_events(
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
        append_viewer_filters(&mut sql, "p.user_id", "p.text", viewer_id);
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

pub(super) fn media_surface_condition(post_id_column: &str, text_column: &str) -> String {
    format!(
        "(EXISTS (SELECT 1 FROM post_media pm WHERE pm.post_id = {post_id_column}) OR EXISTS (SELECT 1 FROM post_embeds pe WHERE pe.post_id = {post_id_column} AND pe.provider = 'youtube') OR instr(lower({text_column}), 'youtube.com/') > 0 OR instr(lower({text_column}), 'youtu.be/') > 0 OR instr(lower({text_column}), 'youtube-nocookie.com/') > 0)"
    )
}

/// Appends the standard viewer visibility predicates: accounts the viewer
/// blocked, accounts that blocked the viewer, accounts the viewer muted, and
/// the viewer's muted words. The viewer's own posts are never hidden by their
/// own muted words.
pub(super) fn append_viewer_filters(
    sql: &mut String,
    user_column: &str,
    text_column: &str,
    viewer_id: Option<i64>,
) {
    if viewer_id.is_some() {
        sql.push_str(&format!(
            " AND ({user_column} IS NULL OR {user_column} NOT IN (SELECT blocked_id FROM blocks WHERE blocker_id = ?))"
        ));
        sql.push_str(&format!(
            " AND ({user_column} IS NULL OR {user_column} NOT IN (SELECT muted_id FROM mutes WHERE muter_id = ?))"
        ));
        sql.push_str(&format!(
            " AND ({user_column} IS NULL OR NOT EXISTS (SELECT 1 FROM blocks WHERE blocker_id = {user_column} AND blocked_id = ?))"
        ));
        sql.push_str(&format!(
            " AND ({user_column} IS NULL OR {user_column} = ? OR NOT EXISTS (SELECT 1 FROM muted_words mw WHERE mw.user_id = ? AND rustpost_muted_word_match({text_column}, mw.normalized_term)))"
        ));
    }
}

pub(super) fn push_viewer_filter_bindings(bindings: &mut Vec<i64>, viewer_id: Option<i64>) {
    if let Some(id) = viewer_id {
        bindings.extend(std::iter::repeat_n(id, VIEWER_FILTER_BINDINGS));
    }
}

pub(super) fn query_post_rows<P>(
    conn: &Connection,
    sql: &str,
    params: P,
) -> anyhow::Result<Vec<PostRow>>
where
    P: rusqlite::Params,
{
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map(params, map_post_row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub(super) fn map_post_row(row: &Row<'_>) -> rusqlite::Result<PostRow> {
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
        repost_count: row.get(18)?,
        reply_count: row.get(19)?,
        quote_post_id: row.get(20)?,
        pinned_by_author: row.get::<_, i64>(21)? != 0,
    })
}

/// Enriches pre-fetched post rows with media, embeds, viewer state, counts,
/// and quote previews using a fixed number of batched queries.
///
/// This replaced a per-row enrichment loop that issued up to seven queries per
/// post, which meant hundreds of round trips for a single 40-post timeline.
pub(super) async fn rows_to_posts(
    pool: &SqlitePool,
    rows: Vec<PostRow>,
    viewer_id: Option<i64>,
) -> anyhow::Result<Vec<PostView>> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    pool.call(move |conn| enrich_posts(conn, rows, viewer_id))
        .await
}

pub(super) fn enrich_posts(
    conn: &Connection,
    rows: Vec<PostRow>,
    viewer_id: Option<i64>,
) -> anyhow::Result<Vec<PostView>> {
    let visible_ids = rows
        .iter()
        .filter(|row| !row.original_unavailable)
        .map(|row| row.id)
        .collect::<Vec<_>>();
    let polls = polls::for_posts(conn, &visible_ids, viewer_id)?;
    let media = media_for_posts(conn, &visible_ids)?;
    let embeds = youtube_embeds_for_posts(conn, &visible_ids)?;
    let viewer_likes = viewer_relation_ids(conn, viewer_id, ViewerRelation::Likes, &visible_ids)?;
    let viewer_bookmarks =
        viewer_relation_ids(conn, viewer_id, ViewerRelation::Bookmarks, &visible_ids)?;
    let viewer_reposts =
        viewer_relation_ids(conn, viewer_id, ViewerRelation::Reposts, &visible_ids)?;
    let like_counts = visible_like_counts(conn, viewer_id, &visible_ids)?;
    let quote_ids = rows
        .iter()
        .filter_map(|row| row.quote_post_id)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let quotes = quote_previews(conn, viewer_id, &quote_ids)?;

    let mut posts = Vec::with_capacity(rows.len());
    for row in rows {
        let unavailable = row.original_unavailable;
        let id = row.id;
        let quote = row.quote_post_id.map(|quote_id| {
            quotes
                .get(&quote_id)
                .cloned()
                .unwrap_or_else(|| unavailable_quote_preview(quote_id))
        });
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
            like_count: if unavailable {
                0
            } else {
                like_counts.get(&id).copied().unwrap_or(0)
            },
            repost_count: row.repost_count,
            reply_count: row.reply_count,
            viewer_liked: viewer_likes.contains(&id),
            viewer_bookmarked: viewer_bookmarks.contains(&id),
            viewer_reposted: viewer_reposts.contains(&id),
            viewer_can_repost: viewer_id
                .is_some_and(|user_id| !unavailable && row.user_id != Some(user_id)),
            pinned_by_author: row.pinned_by_author,
            original_unavailable: unavailable,
            reposted_by_user_id: row.repost_user_id,
            reposted_by_username: row.repost_username,
            reposted_by_display_name: row.repost_display_name,
            reposted_at: row.repost_created_at,
            quote,
            media: media.get(&id).cloned().unwrap_or_default(),
            youtube_embeds: embeds.get(&id).cloned().unwrap_or_default(),
            poll: polls.get(&id).cloned(),
        });
    }
    Ok(posts)
}

pub(super) fn unavailable_quote_preview(quote_post_id: i64) -> QuotePreview {
    QuotePreview {
        id: quote_post_id,
        username: None,
        display_name: None,
        anonymous_label: None,
        text: String::new(),
        created_at: String::new(),
        unavailable: true,
    }
}

pub(super) fn sql_placeholders(count: usize) -> String {
    std::iter::repeat_n("?", count)
        .collect::<Vec<_>>()
        .join(",")
}

pub(super) fn media_for_posts(
    conn: &Connection,
    post_ids: &[i64],
) -> anyhow::Result<HashMap<i64, Vec<MediaView>>> {
    if post_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        r#"
        SELECT pm.post_id, m.public_path, m.mime_type, m.media_kind, m.alt_text, m.is_nsfw, m.id, m.conversion_state
        FROM post_media pm
        JOIN media m ON m.id = pm.media_id
        WHERE pm.post_id IN ({})
        ORDER BY pm.post_id ASC, pm.position ASC
        "#,
        sql_placeholders(post_ids.len())
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(post_ids.iter()), |row| {
        Ok((
            row.get::<_, i64>(0)?,
            MediaView {
                public_path: row.get(1)?,
                mime_type: row.get(2)?,
                media_kind: row.get(3)?,
                alt_text: row.get(4)?,
                id: row.get(6)?,
                conversion_state: row.get(7)?,
                is_nsfw: row.get::<_, i64>(5)? != 0,
            },
        ))
    })?;
    let mut media: HashMap<i64, Vec<MediaView>> = HashMap::new();
    for row in rows {
        let (post_id, view) = row?;
        media.entry(post_id).or_default().push(view);
    }
    Ok(media)
}

pub(super) fn youtube_embeds_for_posts(
    conn: &Connection,
    post_ids: &[i64],
) -> anyhow::Result<HashMap<i64, Vec<YoutubeEmbed>>> {
    if post_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        r#"
        SELECT post_id, video_id, title
        FROM post_embeds
        WHERE provider = 'youtube' AND post_id IN ({})
        ORDER BY post_id ASC, position ASC, id ASC
        "#,
        sql_placeholders(post_ids.len())
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(post_ids.iter()), |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    })?;
    let mut embeds: HashMap<i64, Vec<YoutubeEmbed>> = HashMap::new();
    for row in rows {
        let (post_id, video_id, title) = row?;
        if let Some(embed) = crate::youtube::embed_from_stored(&video_id, title) {
            embeds.entry(post_id).or_default().push(embed);
        }
    }
    Ok(embeds)
}

pub(super) fn viewer_relation_ids(
    conn: &Connection,
    viewer_id: Option<i64>,
    relation: ViewerRelation,
    post_ids: &[i64],
) -> anyhow::Result<BTreeSet<i64>> {
    let Some(viewer_id) = viewer_id else {
        return Ok(BTreeSet::new());
    };
    if post_ids.is_empty() {
        return Ok(BTreeSet::new());
    }
    let sql = format!(
        "SELECT post_id FROM {} WHERE user_id = ? AND post_id IN ({})",
        relation.table(),
        sql_placeholders(post_ids.len())
    );
    let mut bindings = Vec::with_capacity(post_ids.len() + 1);
    bindings.push(viewer_id);
    bindings.extend_from_slice(post_ids);
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(bindings), |row| row.get::<_, i64>(0))?;
    Ok(rows.collect::<Result<BTreeSet<_>, _>>()?)
}

pub(super) fn visible_like_counts(
    conn: &Connection,
    viewer_id: Option<i64>,
    post_ids: &[i64],
) -> anyhow::Result<HashMap<i64, i64>> {
    if post_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        r#"
        SELECT l.post_id, COUNT(*)
        FROM likes l
        JOIN users u ON u.id = l.user_id AND u.is_deleted = 0
        WHERE l.post_id IN ({}) AND (u.liked_posts_public != 0 OR l.user_id = ?)
        GROUP BY l.post_id
        "#,
        sql_placeholders(post_ids.len())
    );
    let mut bindings = Vec::with_capacity(post_ids.len() + 1);
    bindings.extend_from_slice(post_ids);
    bindings.push(viewer_id.unwrap_or(-1));
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(bindings), |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
    })?;
    Ok(rows.collect::<Result<HashMap<_, _>, _>>()?)
}

pub(super) fn quote_previews(
    conn: &Connection,
    viewer_id: Option<i64>,
    quote_ids: &[i64],
) -> anyhow::Result<HashMap<i64, QuotePreview>> {
    if quote_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let mut sql = format!(
        r#"
        SELECT q.id, q.user_id, u.username, u.display_name, q.anonymous_label, q.text, q.created_at
        FROM posts q
        LEFT JOIN users u ON u.id = q.user_id
        WHERE q.id IN ({}) AND q.is_deleted = 0
          AND (q.user_id IS NULL OR (u.is_deleted = 0 AND u.is_suspended = 0))
        "#,
        sql_placeholders(quote_ids.len())
    );
    append_viewer_filters(&mut sql, "q.user_id", "q.text", viewer_id);
    let mut bindings = quote_ids.to_vec();
    push_viewer_filter_bindings(&mut bindings, viewer_id);
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(bindings), |row| {
        let preview = map_quote_preview_row(row)?;
        Ok((preview.id, preview))
    })?;
    Ok(rows.collect::<Result<HashMap<_, _>, _>>()?)
}

pub(super) fn map_quote_preview_row(row: &Row<'_>) -> rusqlite::Result<QuotePreview> {
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

pub(super) fn ensure_quote_target_accessible_tx(
    tx: &rusqlite::Transaction<'_>,
    user_id: i64,
    post_id: i64,
) -> anyhow::Result<()> {
    let owner: Option<i64> = tx
        .query_row(
            r#"
            SELECT p.user_id
            FROM posts p
            JOIN users u ON u.id = p.user_id
            WHERE p.id = ? AND p.is_deleted = 0
              AND u.is_deleted = 0 AND u.is_suspended = 0
            "#,
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

pub(super) fn user_relation_exists_tx(
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

pub(super) fn youtube_embeds_for_post_conn(
    conn: &Connection,
    post_id: i64,
) -> rusqlite::Result<Vec<YoutubeEmbed>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT video_id, title
        FROM post_embeds
        WHERE post_id = ? AND provider = 'youtube'
        ORDER BY position ASC, id ASC
        "#,
    )?;
    let rows = stmt
        .query_map([post_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows
        .into_iter()
        .filter_map(|(video_id, title)| crate::youtube::embed_from_stored(&video_id, title))
        .collect())
}

pub(super) fn reuse_stored_youtube_metadata(embeds: &mut [YoutubeEmbed], stored: &[YoutubeEmbed]) {
    let stored_titles = stored
        .iter()
        .filter_map(|embed| {
            embed
                .title
                .as_deref()
                .map(|title| (embed.video_id.as_str(), title))
        })
        .collect::<HashMap<_, _>>();
    for embed in embeds {
        if let Some(title) = stored_titles.get(embed.video_id.as_str()) {
            embed.title = Some((*title).to_owned());
        }
    }
}

pub(super) fn replace_youtube_embeds_tx(
    tx: &rusqlite::Transaction<'_>,
    post_id: i64,
    embeds: &[YoutubeEmbed],
) -> anyhow::Result<()> {
    tx.execute("DELETE FROM post_embeds WHERE post_id = ?", [post_id])?;
    for (position, embed) in embeds.iter().enumerate() {
        let position = i64::try_from(position)?;
        let title = embed.title.as_deref();
        tx.execute(
            r#"
            INSERT INTO post_embeds (
                post_id,
                provider,
                video_id,
                original_url,
                canonical_url,
                title,
                thumbnail_url,
                embed_url,
                position,
                fetched_at
            )
            VALUES (?, 'youtube', ?, ?, ?, ?, ?, ?, ?, CASE WHEN ? IS NULL THEN NULL ELSE CURRENT_TIMESTAMP END)
            "#,
            params![
                post_id,
                &embed.video_id,
                &embed.source_url,
                &embed.canonical_url,
                title,
                &embed.thumbnail_url,
                &embed.embed_url,
                position,
                title
            ],
        )?;
    }
    Ok(())
}

pub(super) fn notify_post_owner_tx(
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
        && (kind != "like" || likes_public_tx(tx, actor_id)?)
    {
        create_notification_tx(tx, owner, Some(actor_id), Some(link_post_id), kind, message)?;
    }
    Ok(owner)
}

pub(super) fn likes_public_tx(
    tx: &rusqlite::Transaction<'_>,
    user_id: i64,
) -> anyhow::Result<bool> {
    Ok(tx
        .query_row(
            "SELECT liked_posts_public FROM users WHERE id = ? AND is_deleted = 0",
            [user_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .is_some_and(|value| value != 0))
}

pub(super) fn notify_mentioned_users_tx(
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

pub(super) fn mentioned_usernames(text: &str) -> Vec<String> {
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

pub(super) fn is_blocked_tx(
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

pub(super) fn create_notification_tx(
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
