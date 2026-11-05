//! Posting for the server subsystem.

use super::*;

pub(super) async fn create_post(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Extension(security): Extension<csrf::RequestSecurity>,
    headers: HeaderMap,
    multipart: Multipart,
) -> AppResult<Response> {
    let addr = SocketAddr::new(security.client_ip.unwrap_or_else(|| addr.ip()), addr.port());
    let user = current(&state, &headers).await?;
    if let Some(user) = &user
        && user.is_suspended
    {
        return Err(AppError::Forbidden);
    }
    if user.is_none() && !state.settings.accounts.anonymous_mode_enabled {
        return Err(AppError::Forbidden);
    }
    let mut form =
        match parse_post_create(&state, &headers, user.as_ref().map(|u| u.id), multipart).await {
            Ok(form) => form,
            Err(AppError::BadRequest(message)) => {
                return bad_request_page(&state, user.as_ref(), &message).await;
            }
            Err(err) => return Err(err),
        };
    let outcome = if matches!(form.intent.as_str(), "draft" | "schedule") {
        drafts_routes::save_new(&state, &headers, user.as_ref(), &form).await
    } else if !form.retained_media.is_empty() || !matches!(form.intent.as_str(), "" | "publish") {
        Err(AppError::BadRequest("invalid posting action".to_owned()))
    } else {
        finish_post_create(&state, &headers, addr, user.as_ref(), &form).await
    };
    if outcome.is_err() {
        cleanup_post_uploads(&state, &form.media_ids).await;
    }
    if outcome.is_ok() {
        form.pending_uploads.commit();
    }
    outcome
}

pub(super) async fn finish_post_create(
    state: &AppState,
    headers: &HeaderMap,
    addr: SocketAddr,
    user: Option<&CurrentUser>,
    form: &ParsedPostCreate,
) -> AppResult<Response> {
    let viewer_id = user.map(|u| u.id);
    if user.is_some() {
        validate_csrf(&state.pool, headers, &form.csrf_token).await?;
    }
    let request_actor = viewer_id.map_or_else(|| "anonymous".to_owned(), user_actor);
    let request_key = if form.post_key.is_empty() {
        Uuid::new_v4().to_string()
    } else {
        form.post_key.clone()
    };
    if let Some(post_id) = social::post_for_request(&state.pool, &request_actor, &request_key)
        .await
        .map_err(|e| AppError::BadRequest(e.to_string()))?
    {
        drafts_routes::cleanup_unattached(state, &form.media_ids).await;
        let redirect = format!("/posts/{post_id}");
        if enhanced_request(headers) {
            return enhanced_post_response(state, headers, user, post_id, redirect).await;
        }
        return Ok(Redirect::to(&redirect).into_response());
    }
    if let Some(parent) = form.parent_post_id {
        ensure_parent_post_exists(&state.pool, parent).await?;
    }
    if form.is_nsfw {
        media::set_media_nsfw(&state.pool, &form.media_ids, true).await?;
    }
    let viewer_id = user.map(|user| user.id);
    let (scope, actor, max_events, window_secs) = if viewer_id.is_none() {
        (
            rate_limit::Scope::AnonymousPost,
            ip_actor(addr),
            state.settings.moderation.anonymous_posts_per_ip_per_hour,
            60 * 60,
        )
    } else if form.parent_post_id.is_some() {
        (
            rate_limit::Scope::Reply,
            user_actor(viewer_id.unwrap_or_default()),
            state.settings.moderation.replies_per_minute,
            60,
        )
    } else {
        (
            rate_limit::Scope::Post,
            user_actor(viewer_id.unwrap_or_default()),
            state.settings.moderation.posts_per_minute,
            60,
        )
    };
    let descriptions = form
        .media_ids
        .iter()
        .enumerate()
        .map(|(i, id)| (*id, form.descriptions.get(&i).cloned().unwrap_or_default()))
        .collect();
    drafts_routes::set_new_descriptions(&state.pool, viewer_id, descriptions).await?;
    let poll = drafts_routes::poll_spec(form)?;
    if poll.is_some() && viewer_id.is_none() {
        return Err(AppError::Forbidden);
    }
    let post_id = social::create_post_with_options(
        &state.pool,
        &state.settings,
        viewer_id,
        &form.text,
        form.parent_post_id,
        &form.media_ids,
        social::PostOptions {
            poll,
            request: Some((request_actor, request_key)),
            rate: Some(rate_limit::Limit {
                scope,
                actor,
                max_events,
                window_secs,
            }),
        },
    )
    .await
    .map_err(|err| {
        if err.is::<rate_limit::LimitExceeded>() {
            AppError::RateLimited(err.to_string())
        } else {
            AppError::BadRequest(err.to_string())
        }
    })?;
    drafts_routes::cleanup_unattached(state, &form.media_ids).await;
    let redirect = form.parent_post_id.map_or_else(
        || format!("/home#post-{post_id}"),
        |_| format!("/posts/{post_id}#reply-{post_id}"),
    );
    if enhanced_request(headers) {
        return enhanced_post_response(state, headers, user, post_id, redirect).await;
    }
    Ok(Redirect::to(&redirect).into_response())
}

/// Deletes media rows and files that were uploaded for a post that was never
/// created, so rejected or failed submissions do not leave orphaned uploads.
pub(super) async fn cleanup_post_uploads(state: &AppState, media_ids: &[i64]) {
    for media_id in media_ids {
        if let Err(error) = media::delete_media(&state.pool, &state.paths, *media_id).await {
            tracing::warn!(
                media_id,
                error = %error,
                "failed to clean up media uploaded for a rejected post"
            );
        }
    }
}

pub(super) async fn parse_post_create(
    state: &AppState,
    headers: &HeaderMap,
    user_id: Option<i64>,
    multipart: Multipart,
) -> AppResult<ParsedPostCreate> {
    let mut form = ParsedPostCreate {
        pending_uploads: media::PendingUploads::new(&state.pool, &state.paths),
        csrf_token: String::new(),
        text: String::new(),
        parent_post_id: None,
        media_ids: Vec::new(),
        is_nsfw: false,
        intent: String::new(),
        post_key: String::new(),
        scheduled_at: String::new(),
        poll_options: String::new(),
        poll_multiple: false,
        poll_closes_at: String::new(),
        descriptions: std::collections::BTreeMap::new(),
        retained_media: Vec::new(),
        existing_descriptions: std::collections::BTreeMap::new(),
        revision: None,
    };
    let outcome = fill_post_create_form(state, headers, user_id, multipart, &mut form).await;
    if outcome.is_err() {
        cleanup_post_uploads(state, &form.media_ids).await;
    }
    outcome.map(|()| form)
}

#[expect(
    clippy::too_many_lines,
    reason = "ordered multipart parsing keeps CSRF-before-upload checks and cleanup ownership together"
)]
pub(super) async fn fill_post_create_form(
    state: &AppState,
    headers: &HeaderMap,
    user_id: Option<i64>,
    mut multipart: Multipart,
    form: &mut ParsedPostCreate,
) -> AppResult<()> {
    let mut csrf_seen = false;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|err| AppError::BadRequest(err.to_string()))?
    {
        let Some(name) = field.name().map(ToOwned::to_owned) else {
            continue;
        };
        match name.as_str() {
            "text" => {
                form.text = read_form_text(field).await?;
            }
            "csrf" => {
                if csrf_seen {
                    return Err(AppError::Csrf);
                }
                csrf_seen = true;
                form.csrf_token = read_form_text(field).await?;
                if user_id.is_some() {
                    validate_csrf(&state.pool, headers, &form.csrf_token).await?;
                }
            }
            "parent_post_id" => {
                let value = read_form_text(field).await?;
                let value = value.trim();
                if value.is_empty() {
                    return Err(AppError::BadRequest(
                        "reply target is missing; open the post thread and try again".to_owned(),
                    ));
                }
                form.parent_post_id = Some(value.parse::<i64>().map_err(|_parse_err| {
                    AppError::BadRequest(
                        "reply target is invalid; open the post thread and try again".to_owned(),
                    )
                })?);
            }
            "post_key" => form.post_key = read_form_text(field).await?,
            "intent" => form.intent = read_form_text(field).await?,
            "scheduled_at" => form.scheduled_at = read_form_text(field).await?,
            "poll_options" => form.poll_options = read_form_text(field).await?,
            "poll_multiple" => {
                form.poll_multiple = true;
                let _ignored = read_form_text(field).await?;
            }
            "poll_closes_at" => form.poll_closes_at = read_form_text(field).await?,
            "revision" => {
                form.revision = Some(
                    read_form_text(field)
                        .await?
                        .parse()
                        .map_err(|_| AppError::BadRequest("invalid draft revision".to_owned()))?,
                );
            }
            "retain_media" => {
                if form.retained_media.len() >= state.settings.posts.max_media_per_post {
                    return Err(AppError::BadRequest(
                        "too many retained attachments".to_owned(),
                    ));
                }
                form.retained_media.push(
                    read_form_text(field)
                        .await?
                        .parse()
                        .map_err(|_| AppError::BadRequest("invalid attachment".to_owned()))?,
                );
            }
            "alt_lines" => {
                for (i, line) in read_form_text(field).await?.lines().enumerate() {
                    if i < state.settings.posts.max_media_per_post {
                        form.descriptions.insert(i, line.to_owned());
                    }
                }
            }
            name if name.starts_with("alt_") => {
                let index: usize = name[4..]
                    .parse()
                    .map_err(|_| AppError::BadRequest("invalid description index".to_owned()))?;
                let text = read_form_text(field).await?;
                if index >= state.settings.posts.max_media_per_post || text.chars().count() > 1000 {
                    return Err(AppError::BadRequest("invalid image description".to_owned()));
                }
                form.descriptions.insert(index, text);
            }
            name if name.starts_with("existing_alt_") => {
                let id: i64 = name[13..].parse().map_err(|_| {
                    AppError::BadRequest("invalid attachment description".to_owned())
                })?;
                let text = read_form_text(field).await?;
                if text.chars().count() > 1000
                    || form.existing_descriptions.len() >= state.settings.posts.max_media_per_post
                {
                    return Err(AppError::BadRequest("invalid image description".to_owned()));
                }
                form.existing_descriptions.insert(id, text);
            }
            "nsfw" => {
                form.is_nsfw = true;
                let _ignored = read_form_text(field).await?;
            }
            "media"
                if field
                    .file_name()
                    .is_some_and(|name| !name.trim().is_empty()) =>
            {
                if form.media_ids.len() >= state.settings.posts.max_media_per_post {
                    return Err(AppError::BadRequest(
                        "too many media attachments".to_owned(),
                    ));
                }
                if user_id.is_some() {
                    validate_csrf(&state.pool, headers, &form.csrf_token).await?;
                }
                let media_id = media::save_upload(
                    &state.pool,
                    &state.settings,
                    &state.paths,
                    &state.ffmpeg,
                    user_id,
                    field,
                )
                .await
                .map_err(|err| AppError::BadRequest(err.to_string()))?;
                form.pending_uploads.track(media_id);
                form.media_ids.push(media_id);
            }
            _ => {}
        }
    }
    Ok(())
}

pub(super) async fn bad_request_page(
    state: &AppState,
    user: Option<&CurrentUser>,
    message: &str,
) -> AppResult<Response> {
    let body = format!(
        r#"<section class="panel error-panel"><p class="eyebrow">400 error</p><h1>Check the form</h1><p>{}</p><p><a class="button-link" href="/home">Back to Home Feed</a></p></section>"#,
        html_escape::encode_text(message)
    );
    Ok((
        StatusCode::BAD_REQUEST,
        Html(page_layout(state, user, None, "Check the form", &body).await?),
    )
        .into_response())
}

pub(super) async fn thread(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Query(query): Query<ThreadQuery>,
) -> AppResult<Html<String>> {
    let user = current(&state, &headers).await?;
    let history = social::history::thread_page(
        &state.pool,
        user.as_ref().map(|u| u.id),
        id,
        query.before,
        query.after,
    )
    .await
    .map_err(|e| AppError::BadRequest(e.to_string()))?
    .ok_or(AppError::NotFound)?;
    let mut navigation = format!(
        r#"<nav aria-label="Conversation history"><a href="/posts/{}">Conversation start</a> "#,
        history.root
    );
    if let Some(parent) = history.requested.parent_post_id {
        let _ = write!(navigation, r#"<a href="/posts/{parent}">Parent post</a> "#);
    }
    if let Some(before) = history.before {
        let _ = write!(
            navigation,
            r#"<a class="button-link" href="/posts/{id}?before={before}">Earlier replies</a> "#
        );
    }
    if let Some(after) = history.after {
        let _ = write!(
            navigation,
            r#"<a class="button-link" href="/posts/{id}?after={after}">Later replies</a> "#
        );
    }
    navigation.push_str("</nav>");
    let mut posts = history.posts;
    if !posts.iter().any(|p| p.id == id) {
        posts.insert(0, history.requested);
    }
    let csrf = form_csrf(&state, &headers).await?;
    let composer = if (user.is_some() || state.settings.accounts.anonymous_mode_enabled)
        && posting_enabled_for(&state, user.as_ref()).await?
    {
        render::composer(
            csrf.as_deref(),
            Some(id),
            state.settings.posts.max_text_chars,
        )
    } else {
        String::new()
    };
    let body = format!(
        "{}{navigation}{}{}",
        render::thread_back_control(),
        render::thread_posts_with_controls(
            &posts,
            user.as_ref(),
            csrf.as_deref(),
            blur_nsfw_media(&state, user.as_ref()),
            state.settings.posts.post_edit_window_seconds,
        ),
        composer
    );
    let body = format!("{body}{navigation}");
    Ok(Html(
        page_layout(&state, user.as_ref(), csrf.as_deref(), "Thread", &body).await?,
    ))
}

pub(super) async fn edit_post_form(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Query(query): Query<ReturnQuery>,
) -> AppResult<Html<String>> {
    let user = require_active_user(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers)
        .await?
        .ok_or(AppError::Unauthorized)?;
    let preview = edit_preview(
        &state.pool,
        user.id,
        id,
        state.settings.posts.post_edit_window_seconds,
    )
    .await?;
    let fallback = edit_return_fallback(&headers, id, preview.parent_post_id);
    let return_to = query
        .return_to
        .as_deref()
        .and_then(|target| safe_edit_return_target(target, id))
        .unwrap_or(fallback);
    let body = format!(
        "{}{}",
        render::thread_back_control(),
        render::edit_post_form(
            &csrf,
            id,
            &preview.text,
            state.settings.posts.max_text_chars,
            &return_to,
        )
    );
    Ok(Html(
        page_layout(&state, Some(&user), Some(&csrf), "Edit post", &body).await?,
    ))
}

pub(super) async fn edit_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<EditPostForm>,
) -> AppResult<Response> {
    let user = require_active_user(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    match social::edit_post(&state.pool, &state.settings, user.id, id, &form.text).await {
        Ok(_changed) => {
            let target = form
                .return_to
                .as_deref()
                .and_then(|target| safe_edit_return_target(target, id))
                .unwrap_or_else(|| format!("/posts/{id}"));
            Ok(Redirect::to(&target).into_response())
        }
        Err(social::EditPostError::NotFound) => Err(AppError::NotFound),
        Err(social::EditPostError::Forbidden) => Err(AppError::Forbidden),
        Err(
            err @ (social::EditPostError::WindowExpired | social::EditPostError::Validation(_)),
        ) => bad_request_page(&state, Some(&user), &err.to_string()).await,
        Err(social::EditPostError::Database(message)) => {
            Err(AppError::Anyhow(anyhow::anyhow!(message)))
        }
    }
}

pub(super) async fn delete_confirm(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Query(query): Query<DeleteQuery>,
) -> AppResult<Html<String>> {
    let user = require_user(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers)
        .await?
        .ok_or(AppError::Unauthorized)?;
    let preview = delete_preview(&state.pool, user.id, user.is_admin, id).await?;
    let fallback = delete_return_fallback(&headers, id, preview.parent_post_id);
    let return_to = query
        .return_to
        .as_deref()
        .and_then(|target| safe_delete_return_target(target, id))
        .unwrap_or(fallback);
    let author = preview
        .display_name
        .as_deref()
        .or(preview.username.as_deref())
        .unwrap_or("Deleted user");
    let body = format!(
        r#"<section class="panel"><h1>Delete post?</h1><p class="muted">This will remove the post from timelines and threads.</p><blockquote>{}</blockquote><p class="muted">By {}</p><div class="actions"><form method="post"><input type="hidden" name="csrf" value="{}"><input type="hidden" name="return_to" value="{}"><button class="danger" type="submit">Confirm delete</button></form><a class="button-link" href="{}">Cancel</a></div></section>"#,
        html_escape::encode_text(&preview.text),
        html_escape::encode_text(author),
        html_escape::encode_double_quoted_attribute(&csrf),
        html_escape::encode_double_quoted_attribute(&return_to),
        html_escape::encode_double_quoted_attribute(&return_to)
    );
    Ok(Html(
        page_layout(&state, Some(&user), Some(&csrf), "Delete post", &body).await?,
    ))
}

pub(super) async fn delete_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<DeleteForm>,
) -> AppResult<Response> {
    let user = require_user(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    let preview = match delete_preview(&state.pool, user.id, user.is_admin, id).await {
        Ok(preview) => preview,
        Err(AppError::NotFound) => {
            let target = safe_delete_return_target(&form.return_to, id)
                .unwrap_or_else(|| "/home".to_owned());
            return Ok(Redirect::to(&target).into_response());
        }
        Err(err) => return Err(err),
    };
    media::validate_post_media_deletion(&state.pool, &state.paths, id).await?;
    let recoverable = social::recovery::delete(
        &state.pool,
        user.id,
        id,
        user.is_admin,
        state.settings.posts.post_delete_recovery_seconds,
    )
    .await?;
    if recoverable {
        return Ok(Redirect::to("/settings/deleted-posts").into_response());
    }
    // The post is already deleted; a media cleanup failure should not turn a
    // successful deletion into an error page.
    if let Err(error) = media::delete_post_media(&state.pool, &state.paths, id).await {
        tracing::warn!(post_id = id, error = %error, "post deleted but media cleanup failed");
    }
    let fallback = if let Some(parent_id) = preview.parent_post_id {
        format!("/posts/{parent_id}#post-{parent_id}")
    } else {
        "/home".to_owned()
    };
    let target = safe_delete_return_target(&form.return_to, id).unwrap_or(fallback);
    Ok(Redirect::to(&target).into_response())
}

pub(super) async fn toggle_like(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> AppResult<Response> {
    let user = require_active_user(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    let is_reply = post_is_reply(&state.pool, id).await?;
    if user_post_relation_exists(&state.pool, "likes", user.id, id).await? {
        social::unlike(&state.pool, user.id, id).await?;
    } else {
        social::like(&state.pool, user.id, id).await?;
    }
    if enhanced_request(&headers) {
        return Ok(Json(post_action_response(&state.pool, user.id, id).await?).into_response());
    }
    Ok(redirect_to_post_anchor(&headers, id, is_reply).into_response())
}

pub(super) async fn toggle_bookmark(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> AppResult<Response> {
    let user = require_active_user(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    let is_reply = post_is_reply(&state.pool, id).await?;
    if user_post_relation_exists(&state.pool, "bookmarks", user.id, id).await? {
        social::unbookmark(&state.pool, user.id, id).await?;
    } else {
        social::bookmark(&state.pool, user.id, id).await?;
    }
    if enhanced_request(&headers) {
        return Ok(Json(post_action_response(&state.pool, user.id, id).await?).into_response());
    }
    Ok(redirect_to_post_anchor(&headers, id, is_reply).into_response())
}

pub(super) async fn toggle_pin_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> AppResult<Response> {
    let user = require_active_user(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    let is_reply = post_is_reply(&state.pool, id).await?;
    if social::pinned_post_id(&state.pool, user.id).await? == Some(id) {
        social::unpin_post(&state.pool, user.id, id).await?;
    } else {
        social::pin_post(&state.pool, user.id, id)
            .await
            .map_err(|err| match err {
                social::PinPostError::NotFound => AppError::NotFound,
                social::PinPostError::Forbidden => AppError::Forbidden,
                social::PinPostError::Database(message) => {
                    AppError::Anyhow(anyhow::anyhow!(message))
                }
            })?;
    }
    Ok(redirect_to_post_anchor(&headers, id, is_reply).into_response())
}

pub(super) async fn repost(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> AppResult<Response> {
    let user = require_active_user(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    let is_reply = post_is_reply(&state.pool, id).await?;
    rate_limit::check_and_record(
        &state.pool,
        rate_limit::Scope::Repost,
        &user_actor(user.id),
        state.settings.moderation.reposts_per_minute,
        60,
    )
    .await
    .map_err(|err| AppError::RateLimited(err.to_string()))?;
    if user_post_relation_exists(&state.pool, "reposts", user.id, id).await? {
        social::unrepost(&state.pool, user.id, id)
            .await
            .map_err(|err| {
                tracing::warn!(post_id = id, user_id = user.id, error = %err, "unrepost failed");
                AppError::BadRequest(err.to_string())
            })?;
    } else {
        social::repost(&state.pool, user.id, id)
            .await
            .map_err(|err| {
                tracing::warn!(post_id = id, user_id = user.id, error = %err, "repost rejected");
                AppError::BadRequest(err.to_string())
            })?;
    }
    if enhanced_request(&headers) {
        return Ok(Json(post_action_response(&state.pool, user.id, id).await?).into_response());
    }
    Ok(redirect_to_post_anchor(&headers, id, is_reply).into_response())
}

pub(super) async fn quote_form(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    let user = require_active_user(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers)
        .await?
        .ok_or(AppError::Unauthorized)?;
    let preview = social::quote_target_preview(&state.pool, Some(user.id), id)
        .await
        .map_err(|_err| AppError::NotFound)?;
    if preview.unavailable {
        return Err(AppError::NotFound);
    }
    let body = format!(
        "{}{}",
        render::thread_back_control(),
        render::quote_composer(&csrf, &preview, state.settings.posts.max_text_chars)
    );
    Ok(Html(
        page_layout(&state, Some(&user), Some(&csrf), "Quote post", &body).await?,
    ))
}

pub(super) async fn quote_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<QuoteForm>,
) -> AppResult<Response> {
    let user = require_active_user(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    rate_limit::check_and_record(
        &state.pool,
        rate_limit::Scope::Repost,
        &user_actor(user.id),
        state.settings.moderation.reposts_per_minute,
        60,
    )
    .await
    .map_err(|err| AppError::RateLimited(err.to_string()))?;
    let outcome = social::create_quote_post(&state.pool, &state.settings, user.id, id, &form.text)
        .await
        .map_err(|err| AppError::BadRequest(err.to_string()))?;
    Ok(Redirect::to(&format!("/home#post-{}", outcome.post_id)).into_response())
}

pub(super) async fn reply_redirect(Path(id): Path<i64>) -> Redirect {
    Redirect::to(&format!("/posts/{id}"))
}

async fn enhanced_post_response(
    state: &AppState,
    headers: &HeaderMap,
    user: Option<&CurrentUser>,
    post_id: i64,
    redirect: String,
) -> AppResult<Response> {
    let viewer_id = user.map(|u| u.id);
    let posts = social::post_thread(&state.pool, viewer_id, post_id).await?;
    let post = posts
        .iter()
        .find(|post| post.id == post_id)
        .ok_or(AppError::NotFound)?;
    Ok(Json(PostCreateResponse {
        kind: "post-created",
        post_id,
        parent_post_id: post.parent_post_id,
        redirect,
        html: if post.parent_post_id.is_some() {
            render::thread_post_card_with_controls(
                post,
                user,
                form_csrf(state, headers).await?.as_deref(),
                blur_nsfw_media(state, user),
                state.settings.posts.post_edit_window_seconds,
            )
        } else {
            render::post_card_with_controls(
                post,
                user,
                form_csrf(state, headers).await?.as_deref(),
                blur_nsfw_media(state, user),
                state.settings.posts.post_edit_window_seconds,
            )
        },
    })
    .into_response())
}
