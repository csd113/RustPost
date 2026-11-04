//! Auth routes for the server subsystem.

use super::*;

pub(super) async fn login_form(State(state): State<Arc<AppState>>) -> Html<String> {
    let body = render::login_form(None, state.settings.accounts.min_password_length);
    let context = layout_context(&state, None).await.unwrap_or_default();
    Html(render::layout_with_context(
        None,
        None,
        "Login",
        &body,
        &state.settings.site.name,
        &context,
    ))
}

pub(super) async fn login(
    State(state): State<Arc<AppState>>,
    Extension(security): Extension<csrf::RequestSecurity>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Form(form): Form<AuthForm>,
) -> AppResult<Response> {
    let addr = SocketAddr::new(security.client_ip.unwrap_or_else(|| addr.ip()), addr.port());
    let actor = if security.onion {
        format!(
            "tor-login:{}",
            auth::hash_token(&form.username.trim().to_ascii_lowercase())
        )
    } else {
        ip_actor(addr)
    };
    rate_limit::ensure_under_limit(
        &state.pool,
        rate_limit::Scope::FailedLogin,
        &actor,
        state.settings.moderation.failed_login_attempts_per_15m,
        15 * 60,
    )
    .await
    .map_err(|err| AppError::RateLimited(err.to_string()))?;
    let session = match auth::login(&state.pool, &form.username, &form.password).await? {
        Ok(session) => session,
        Err(failure) => {
            let message = match failure {
                auth::LoginFailure::NoAccount => "No account with that username.",
                auth::LoginFailure::InvalidPassword => "The password is incorrect.",
                auth::LoginFailure::UnavailableAccount => "This account cannot log in.",
            };
            rate_limit::record(&state.pool, rate_limit::Scope::FailedLogin, &actor).await?;
            return auth_form_response(&state, StatusCode::UNAUTHORIZED, message).await;
        }
    };
    let mut response = Redirect::to("/home").into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&auth::set_session_cookie(&session, security.secure_cookie))
            .map_err(|err| AppError::BadRequest(err.to_string()))?,
    );
    Ok(response)
}

pub(super) async fn register_form(State(state): State<Arc<AppState>>) -> AppResult<Html<String>> {
    if !state.settings.accounts.registration_enabled {
        return Err(AppError::Forbidden);
    }
    let instance = instance::load(&state.pool).await?;
    let body = if instance.maintenance_mode {
        format!(
            r#"<section class="panel form-card auth-panel" data-testid="form-card"><h1>Registration is currently closed.</h1>{}</section>"#,
            render::notice(
                "info",
                instance
                    .maintenance_notice()
                    .unwrap_or(instance::DEFAULT_MAINTENANCE_MESSAGE)
            )
        )
    } else {
        register_form_body(&state, None).await?
    };
    Ok(Html(
        page_layout(&state, None, None, "Register", &body).await?,
    ))
}

pub(super) async fn register(
    State(state): State<Arc<AppState>>,
    Extension(security): Extension<csrf::RequestSecurity>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Form(form): Form<RegisterForm>,
) -> AppResult<Response> {
    let addr = SocketAddr::new(security.client_ip.unwrap_or_else(|| addr.ip()), addr.port());
    if !state.settings.accounts.registration_enabled {
        return Err(AppError::Forbidden);
    }
    if instance::load(&state.pool).await?.maintenance_mode {
        return Ok((
            StatusCode::SERVICE_UNAVAILABLE,
            Html(render::maintenance_page(
                &state.settings.site.name,
                instance::DEFAULT_MAINTENANCE_MESSAGE,
            )),
        )
            .into_response());
    }
    let Some(confirm_password) = form.confirm_password.as_deref() else {
        return Err(AppError::BadRequest(
            "please confirm your password".to_owned(),
        ));
    };
    if form.password != confirm_password {
        return Err(AppError::BadRequest(
            "passwords do not match; please enter the same password twice".to_owned(),
        ));
    }
    rate_limit::check_and_record(
        &state.pool,
        rate_limit::Scope::Registration,
        &ip_actor(addr),
        state.settings.moderation.account_creations_per_ip_per_day,
        24 * 60 * 60,
    )
    .await
    .map_err(|err| AppError::RateLimited(err.to_string()))?;
    if state.settings.accounts.registration_captcha_enabled
        && let Err(err) = state
            .registration_captcha
            .validate(
                form.captcha_token.as_deref(),
                form.captcha_answer.as_deref(),
            )
            .await
    {
        return register_form_response(&state, StatusCode::BAD_REQUEST, err.message()).await;
    }
    let user_id = match auth::register_user(
        &state.pool,
        &state.settings,
        &form.username,
        &form.password,
        false,
    )
    .await
    {
        Ok(user_id) => user_id,
        Err(err) => {
            let message = err.to_string();
            if message == auth::USERNAME_TAKEN_MESSAGE {
                return register_form_response(
                    &state,
                    StatusCode::BAD_REQUEST,
                    "That username is already taken.",
                )
                .await;
            }
            if message == "password is too short"
                || message == "password contains control characters"
            {
                return register_form_response(&state, StatusCode::BAD_REQUEST, &message).await;
            }
            return Err(AppError::BadRequest(message));
        }
    };
    let session = auth::create_session(&state.pool, user_id).await?;
    let mut response = Redirect::to("/onboarding").into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&auth::set_session_cookie(&session, security.secure_cookie))
            .map_err(|err| AppError::BadRequest(err.to_string()))?,
    );
    Ok(response)
}

pub(super) async fn auth_form_response(
    state: &AppState,
    status: StatusCode,
    message: &str,
) -> AppResult<Response> {
    let body = render::login_form(Some(message), state.settings.accounts.min_password_length);
    Ok((
        status,
        Html(page_layout(state, None, None, "Login", &body).await?),
    )
        .into_response())
}

pub(super) async fn register_form_response(
    state: &AppState,
    status: StatusCode,
    message: &str,
) -> AppResult<Response> {
    let body = register_form_body(state, Some(message)).await?;
    Ok((
        status,
        Html(page_layout(state, None, None, "Register", &body).await?),
    )
        .into_response())
}

pub(super) async fn register_form_body(
    state: &AppState,
    message: Option<&str>,
) -> AppResult<String> {
    let captcha = if state.settings.accounts.registration_captcha_enabled {
        Some(state.registration_captcha.create_challenge().await?)
    } else {
        None
    };
    Ok(render::register_form(
        message,
        state.settings.accounts.min_password_length,
        captcha.as_ref(),
    ))
}

pub(super) async fn onboarding(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AppResult<Response> {
    let user = require_user(&state, &headers).await?;
    if onboarding_completed(&state.pool, user.id).await? {
        return Ok(Redirect::to("/home").into_response());
    }
    let csrf = form_csrf(&state, &headers)
        .await?
        .ok_or(AppError::Unauthorized)?;
    Ok(Html(onboarding_page(&state, &user, &csrf).await?).into_response())
}

pub(super) async fn onboarding_update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    multipart: Multipart,
) -> AppResult<Response> {
    let user = require_user(&state, &headers).await?;
    let mut form = parse_onboarding_update(&state, &headers, user.id, multipart).await?;
    if let Err(err) = validate_csrf(&state.pool, &headers, &form.csrf_token).await {
        cleanup_onboarding_uploads(&state, &form).await;
        return Err(err);
    }
    if form.intent == "skip" {
        cleanup_onboarding_uploads(&state, &form).await;
        mark_onboarding_complete(&state.pool, user.id).await?;
        return Ok(Redirect::to("/home").into_response());
    }
    if form.intent != "save" {
        cleanup_onboarding_uploads(&state, &form).await;
        return onboarding_response(
            &state,
            &user,
            &headers,
            StatusCode::BAD_REQUEST,
            "Unknown onboarding action.",
        )
        .await;
    }
    if let Err(err) =
        crate::validation::validate_profile_text(&form.display_name, &form.bio, &state.settings)
    {
        cleanup_onboarding_uploads(&state, &form).await;
        return onboarding_response(
            &state,
            &user,
            &headers,
            StatusCode::BAD_REQUEST,
            &err.to_string(),
        )
        .await;
    }
    let follow_user_ids = dedup_user_ids(form.follow_user_ids, user.id);
    let follow_user_ids = social::active_follow_targets(&state.pool, &follow_user_ids).await?;
    let display_name = form.display_name.trim().to_owned();
    let bio = form.bio.trim().to_owned();
    let user_id = user.id;
    let picture = form.profile_picture_media_id.map_or(
        media::ProfileMediaChange::Keep,
        media::ProfileMediaChange::Set,
    );
    let previous = state.pool.call(move |conn| {
        let tx = conn.transaction()?;
        let changed = tx.execute("UPDATE users SET display_name = ?, bio = ?, onboarding_completed_at = COALESCE(onboarding_completed_at, CURRENT_TIMESTAMP), updated_at = CURRENT_TIMESTAMP WHERE id = ? AND is_deleted = 0", params![display_name, bio, user_id])?;
        anyhow::ensure!(changed == 1, "account not found");
        let previous = media::apply_profile_media_changes(&tx, user_id, &[(media::ProfileMediaSlot::Picture, picture)])?;
        for followed_id in follow_user_ids { social::follow_in_transaction(&tx, user_id, followed_id)?; }
        tx.commit()?;
        Ok(previous)
    }).await?;
    media::cleanup_replaced_profile_media(&state.pool, &state.paths, previous).await;
    form.pending_uploads.commit();
    Ok(Redirect::to("/home").into_response())
}

pub(super) async fn cleanup_onboarding_uploads(state: &AppState, form: &ParsedOnboardingUpdate) {
    if let Some(media_id) = form.profile_picture_media_id
        && let Err(error) = media::delete_media(&state.pool, &state.paths, media_id).await
    {
        tracing::warn!(
            media_id,
            error = %error,
            "failed to clean up rejected onboarding profile picture"
        );
    }
}

pub(super) async fn onboarding_response(
    state: &AppState,
    user: &CurrentUser,
    headers: &HeaderMap,
    status: StatusCode,
    message: &str,
) -> AppResult<Response> {
    let csrf = form_csrf(state, headers)
        .await?
        .ok_or(AppError::Unauthorized)?;
    let body = format!(
        "{}{}",
        render::notice("error", message),
        onboarding_page_body(state, user, &csrf).await?
    );
    Ok((
        status,
        Html(page_layout(state, Some(user), Some(&csrf), "Onboarding", &body).await?),
    )
        .into_response())
}

pub(super) async fn onboarding_page(
    state: &AppState,
    user: &CurrentUser,
    csrf: &str,
) -> AppResult<String> {
    let body = onboarding_page_body(state, user, csrf).await?;
    page_layout(state, Some(user), Some(csrf), "Onboarding", &body).await
}

pub(super) async fn onboarding_page_body(
    state: &AppState,
    user: &CurrentUser,
    csrf: &str,
) -> AppResult<String> {
    let profile = settings_profile(&state.pool, user.id).await?;
    let suggestions = social::onboarding_suggestions(&state.pool, user.id, 6).await?;
    Ok(render::onboarding_page(render::OnboardingPage {
        csrf,
        display_name: &profile.display_name,
        bio: &profile.bio,
        picture_path: profile.picture_path.as_deref(),
        suggestions: &suggestions,
        allow_profile_pictures: state.settings.accounts.allow_profile_pictures,
        max_display_name_len: state.settings.accounts.max_display_name_len,
        max_bio_len: state.settings.accounts.max_bio_len,
    }))
}

pub(super) async fn parse_onboarding_update(
    state: &AppState,
    headers: &HeaderMap,
    user_id: i64,
    mut multipart: Multipart,
) -> AppResult<ParsedOnboardingUpdate> {
    let mut form = ParsedOnboardingUpdate {
        pending_uploads: media::PendingUploads::new(&state.pool, &state.paths),
        csrf_token: String::new(),
        intent: "save".to_owned(),
        display_name: String::new(),
        bio: String::new(),
        profile_picture_media_id: None,
        follow_user_ids: Vec::new(),
    };
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
            "csrf" => {
                if csrf_seen {
                    return Err(AppError::Csrf);
                }
                csrf_seen = true;
                form.csrf_token = read_form_text(field).await?;
                validate_csrf(&state.pool, headers, &form.csrf_token).await?;
            }
            "intent" => {
                form.intent = read_form_text(field).await?;
            }
            "display_name" => {
                form.display_name = read_form_text(field).await?;
            }
            "bio" => {
                form.bio = read_form_text(field).await?;
            }
            "follow_user_id" => {
                if form.follow_user_ids.len() >= 100 {
                    return Err(AppError::BadRequest(
                        "Choose at most 100 follow suggestions.".to_owned(),
                    ));
                }
                let value = read_form_text(field).await?;
                form.follow_user_ids
                    .push(value.trim().parse::<i64>().map_err(|_parse_err| {
                        AppError::BadRequest("follow suggestion is invalid".to_owned())
                    })?);
            }
            "profile_picture" if field.file_name().is_some() => {
                if form.profile_picture_media_id.is_some() {
                    return Err(AppError::BadRequest(
                        "choose only one profile picture".to_owned(),
                    ));
                }
                validate_csrf(&state.pool, headers, &form.csrf_token).await?;
                if !state.settings.accounts.allow_profile_pictures {
                    return Err(AppError::Forbidden);
                }
                if field.file_name().is_none_or(|name| name.trim().is_empty()) {
                    continue;
                }
                form.profile_picture_media_id = Some(
                    media::save_profile_picture_upload(
                        &state.pool,
                        &state.settings,
                        &state.paths,
                        &state.ffmpeg,
                        user_id,
                        field,
                    )
                    .await
                    .map_err(|err| AppError::BadRequest(err.to_string()))?,
                );
                if let Some(id) = form.profile_picture_media_id {
                    form.pending_uploads.track(id);
                }
            }
            _ => {}
        }
    }
    Ok(form)
}

pub(super) async fn mark_onboarding_complete(pool: &SqlitePool, user_id: i64) -> AppResult<()> {
    pool.call(move |conn| {
        conn.execute(
            "UPDATE users SET onboarding_completed_at = COALESCE(onboarding_completed_at, CURRENT_TIMESTAMP), updated_at = CURRENT_TIMESTAMP WHERE id = ? AND is_deleted = 0",
            [user_id],
        )?;
        Ok(())
    })
    .await?;
    Ok(())
}

pub(super) async fn onboarding_completed(pool: &SqlitePool, user_id: i64) -> AppResult<bool> {
    Ok(pool
        .call(move |conn| {
            conn.query_row(
                "SELECT onboarding_completed_at IS NOT NULL FROM users WHERE id = ? AND is_deleted = 0",
                [user_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map(|value| value.unwrap_or(0) != 0)
            .map_err(Into::into)
        })
        .await?)
}

pub(super) fn dedup_user_ids(ids: Vec<i64>, current_user_id: i64) -> Vec<i64> {
    let mut deduped = Vec::new();
    for id in ids {
        if id == current_user_id || deduped.contains(&id) {
            continue;
        }
        deduped.push(id);
    }
    deduped
}

pub(super) async fn logout(
    State(state): State<Arc<AppState>>,
    Extension(security): Extension<csrf::RequestSecurity>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> AppResult<Response> {
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    if let Some(token) = auth::session_cookie(&headers) {
        auth::revoke_session(&state.pool, &token).await?;
    }
    let mut response = Redirect::to("/home").into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&auth::clear_session_cookie(security.secure_cookie))
            .map_err(|err| AppError::BadRequest(err.to_string()))?,
    );
    Ok(response)
}
