//! Timelines for the server subsystem.

use super::*;

pub(super) async fn bookmarks(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AppResult<Html<String>> {
    let user = require_user(&state, &headers).await?;
    let posts = social::timeline(&state.pool, Some(user.id), "bookmarks", None).await?;
    let csrf = form_csrf(&state, &headers).await?;
    let body = format!(
        "{}{}",
        render::page_header("Bookmarks", "Posts you saved for later."),
        render::posts_with_controls_empty_state(
            &posts,
            Some(&user),
            csrf.as_deref(),
            blur_nsfw_media(&state, Some(&user)),
            state.settings.posts.post_edit_window_seconds,
            render::EmptyState::new("No bookmarks yet.", "Saved posts will appear here."),
        )
    );
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
    let (users, posts) = if q.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        social::search(&state.pool, user.as_ref().map(|u| u.id), &q).await?
    };
    let csrf = form_csrf(&state, &headers).await?;
    let body = render::search_page(
        &state.settings.site.name,
        &q,
        &users,
        &posts,
        user.as_ref(),
        csrf.as_deref(),
        render::SearchRenderOptions {
            blur_nsfw_media: blur_nsfw_media(&state, user.as_ref()),
            post_edit_window_seconds: state.settings.posts.post_edit_window_seconds,
        },
    );
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
        }),
    )
    .await
}
