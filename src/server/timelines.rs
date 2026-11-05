//! Timelines for the server subsystem.

use super::*;

pub(super) async fn bookmarks(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<HistoryQuery>,
) -> AppResult<Html<String>> {
    let user = require_user(&state, &headers).await?;
    let history = social::history::page(
        &state.pool,
        Some(user.id),
        social::history::Surface::Bookmarks,
        query.older.as_deref(),
        query.newer.as_deref(),
    )
    .await
    .map_err(|e| AppError::BadRequest(e.to_string()))?;
    let posts = &history.posts;
    let navigation = history_navigation("/bookmarks", &history);
    let csrf = form_csrf(&state, &headers).await?;
    let body = format!(
        "{}{}",
        render::page_header("Bookmarks", "Posts you saved for later."),
        render::posts_with_controls_empty_state(
            posts,
            Some(&user),
            csrf.as_deref(),
            blur_nsfw_media(&state, Some(&user)),
            state.settings.posts.post_edit_window_seconds,
            render::EmptyState::new("No bookmarks yet.", "Saved posts will appear here."),
        )
    );
    let body = format!("{body}{navigation}");
    Ok(Html(
        page_layout(&state, Some(&user), csrf.as_deref(), "Bookmarks", &body).await?,
    ))
}

pub(super) async fn following(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<AccountListQuery>,
) -> AppResult<Html<String>> {
    let user = require_user(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers)
        .await?
        .ok_or(AppError::Unauthorized)?;
    let (accounts, has_more) =
        social::following_accounts(&state.pool, user.id, query.after).await?;
    let body = format!(
        "{}{}{}",
        render::page_header("Following", "Accounts you follow."),
        render::accounts(&accounts, &csrf),
        account_list_next_link("/following", &accounts, has_more)
    );
    Ok(Html(
        page_layout(&state, Some(&user), Some(&csrf), "Following", &body).await?,
    ))
}

pub(super) async fn notifications(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AppResult<Html<String>> {
    let user = require_user(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers)
        .await?
        .ok_or(AppError::Unauthorized)?;
    let items = social::notification_groups(&state.pool, user.id).await?;
    let unread_count = social::unread_notification_count(&state.pool, user.id).await?;
    let body = render::notifications_page(&items, unread_count, &csrf);
    Ok(Html(
        page_layout(&state, Some(&user), Some(&csrf), "Notifications", &body).await?,
    ))
}

pub(super) async fn open_notification_group(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<NotificationOpenForm>,
) -> AppResult<Response> {
    let user = require_user(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    if let Some(kind) = form.group_kind.as_deref() {
        let target_post_id = parse_notification_group_target(form.group_target_post_id.as_deref())?;
        social::mark_notification_group_read(&state.pool, user.id, kind, target_post_id).await?;
    } else {
        let notification_ids = parse_notification_ids(&form.notification_ids)?;
        social::mark_notification_ids_read(&state.pool, user.id, &notification_ids).await?;
    }
    let target = safe_return_target(&form.return_to).unwrap_or_else(|| "/notifications".to_owned());
    Ok(Redirect::to(&target).into_response())
}

pub(super) async fn mark_notifications_read(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> AppResult<Response> {
    let user = require_user(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    social::mark_notifications_read(&state.pool, user.id).await?;
    Ok(Redirect::to("/notifications").into_response())
}

pub(super) async fn mention_suggestions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<MentionSuggestionsQuery>,
) -> AppResult<Json<Vec<MentionSuggestionResponse>>> {
    let user = current(&state, &headers).await?;
    let suggestions = social::mention_suggestions(
        &state.pool,
        user.as_ref().map(|user| user.id),
        query.q.as_deref().unwrap_or_default(),
    )
    .await?;
    Ok(Json(
        suggestions
            .into_iter()
            .map(|suggestion| MentionSuggestionResponse {
                username: suggestion.username,
                display_name: suggestion.display_name,
            })
            .collect(),
    ))
}

pub(super) async fn search(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<SearchQuery>,
) -> AppResult<Html<String>> {
    let user = current(&state, &headers).await?;
    let q = normalize_search_query(query.q.as_deref().unwrap_or_default());
    let result = social::search_history(
        &state.pool,
        user.as_ref().map(|u| u.id),
        &q,
        query.cursor,
        query.user_after,
        query.newer,
    )
    .await
    .map_err(|e| AppError::BadRequest(e.to_string()))?;
    let users = &result.users;
    let posts = &result.posts;
    let csrf = form_csrf(&state, &headers).await?;
    let mut body = render::search_page(
        &state.settings.site.name,
        &q,
        users,
        posts,
        user.as_ref(),
        csrf.as_deref(),
        render::SearchRenderOptions {
            blur_nsfw_media: blur_nsfw_media(&state, user.as_ref()),
            post_edit_window_seconds: state.settings.posts.post_edit_window_seconds,
        },
    );
    let encoded = encode_query_component(&q);
    if result.has_more_posts
        && let Some(last) = posts.last()
    {
        let _ = write!(
            body,
            r#"<p><a class="button-link" href="/search?q={encoded}&amp;cursor={}">Older matching posts</a></p>"#,
            last.id
        );
    }
    if result.has_more_users
        && let Some(last) = users.last()
    {
        let _ = write!(
            body,
            r#"<p><a class="button-link" href="/search?q={encoded}&amp;user_after={}">More matching accounts</a></p>"#,
            encode_query_component(&last.username.to_ascii_lowercase())
        );
    }
    Ok(Html(
        page_layout(&state, user.as_ref(), csrf.as_deref(), "Search", &body).await?,
    ))
}

pub(super) fn normalize_search_query(query: &str) -> String {
    query.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(super) async fn tag(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(tag): Path<String>,
) -> AppResult<Html<String>> {
    search(
        State(state),
        headers,
        Query(SearchQuery {
            q: Some(format!("#{tag}")),
            newer: None,
            cursor: None,
            user_after: None,
        }),
    )
    .await
}

fn encode_query_component(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

pub(super) fn history_navigation(base: &str, page: &social::history::Page) -> String {
    let separator = if base.contains('?') { "&" } else { "?" };
    let mut links = String::new();
    for (label, kind, cursor) in [
        ("Newer posts", "newer", &page.newer),
        ("Older posts", "older", &page.older),
    ] {
        if let Some(cursor) = cursor {
            let href = html_escape::encode_double_quoted_attribute(&format!(
                "{base}{separator}{kind}={cursor}"
            ))
            .into_owned();
            let _ = write!(
                links,
                r#"<a class="button-link" href="{href}">{label}</a> "#
            );
        }
    }
    if page.newer.is_some() || page.older.is_some() {
        let _ = write!(
            links,
            r#"<a href="{}">Latest posts</a>"#,
            html_escape::encode_double_quoted_attribute(base)
        );
    }
    format!(r#"<nav class="history-navigation" aria-label="Post history">{links}</nav>"#)
}
