use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::connect_info::ConnectInfo;
use axum::extract::{DefaultBodyLimit, Form, Multipart, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::middleware;
use axum::response::{Html, IntoResponse as _, Redirect, Response};
use axum::routing::{get, post};
use rusqlite::{OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;

use crate::auth::{self, CurrentUser, Theme};
use crate::config::Settings;
use crate::db::SqlitePool;
use crate::errors::{AppError, AppResult};
use crate::ffmpeg::FfmpegStatus;
use crate::registration_captcha::RegistrationCaptchaStore;
use crate::runtime::RuntimePaths;
use crate::{account, admin, backup, csrf, favicon, media, rate_limit, render, social};

const CSRF_TOKEN_HISTORY_LIMIT: usize = 32;

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub settings: Settings,
    pub paths: RuntimePaths,
    pub ffmpeg: FfmpegStatus,
    pub tor: crate::tor::TorStatus,
    pub registration_captcha: RegistrationCaptchaStore,
}

impl AppState {
    #[must_use]
    pub fn new(
        pool: SqlitePool,
        settings: Settings,
        paths: RuntimePaths,
        ffmpeg: FfmpegStatus,
        tor: crate::tor::TorStatus,
    ) -> Arc<Self> {
        Arc::new(Self {
            pool,
            settings,
            paths,
            ffmpeg,
            tor,
            registration_captcha: RegistrationCaptchaStore::default(),
        })
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    let upload_body_limit = upload_body_limit(&state.settings);
    Router::new()
        .route("/", get(home))
        .route("/assets/rustpost.js", get(client_script))
        .route("/favicon.ico", get(site_favicon))
        .route("/local", get(local_redirect))
        .route("/home", get(home))
        .route("/login", get(login_form).post(login))
        .route("/register", get(register_form).post(register))
        .route("/logout", post(logout))
        .route("/posts", post(create_post))
        .route("/posts/{id}", get(thread))
        .route("/posts/{id}/delete", get(delete_confirm).post(delete_post))
        .route("/posts/{id}/like", post(toggle_like))
        .route("/posts/{id}/bookmark", post(toggle_bookmark))
        .route("/posts/{id}/repost", post(repost))
        .route("/posts/{id}/quote", get(quote_form).post(quote_post))
        .route("/posts/{id}/reply", post(reply_redirect))
        .route("/users/{username}", get(profile))
        .route("/users/{username}/followers", get(profile_followers))
        .route("/users/{username}/following", get(profile_following))
        .route("/users/{id}/follow", post(follow))
        .route("/users/{id}/unfollow", post(unfollow))
        .route("/users/{id}/block", post(block))
        .route("/users/{id}/unblock", post(unblock))
        .route("/users/{id}/mute", post(mute))
        .route("/users/{id}/unmute", post(unmute))
        .route("/settings", get(settings_form).post(settings_update))
        .route("/settings/muted-words", post(add_muted_word))
        .route("/settings/muted-words/{id}/remove", post(remove_muted_word))
        .route("/settings/password", post(change_password))
        .route("/settings/delete", get(delete_account_warning))
        .route(
            "/settings/delete/confirm",
            get(delete_account_final_warning).post(delete_account_final),
        )
        .route("/account-deleted", get(account_deleted))
        .route("/following", get(following))
        .route("/bookmarks", get(bookmarks))
        .route("/notifications", get(notifications))
        .route("/notifications/read", post(mark_notifications_read))
        .route("/search", get(search))
        .route("/tags/{tag}", get(tag))
        .route("/admin", get(admin_dashboard))
        .route("/admin/users", get(admin_users))
        .route("/admin/users/{id}/suspend", post(admin_suspend))
        .route("/admin/posts/{id}/delete", post(admin_delete_post))
        .route("/admin/posts/{id}/nsfw", post(admin_toggle_post_nsfw))
        .route("/admin/health", get(admin_health))
        .route("/admin/media", get(admin_media))
        .route(
            "/admin/deep-settings",
            get(admin_deep_settings).post(admin_deep_settings_update),
        )
        .route("/admin/favicon", post(admin_favicon_upload))
        .route("/admin/favicon/remove", post(admin_favicon_remove))
        .route(
            "/admin/backups",
            get(admin_backups).post(admin_create_backup),
        )
        .nest_service(
            "/uploads/originals",
            ServeDir::new(state.paths.uploads_originals.clone()),
        )
        .nest_service(
            "/uploads/images",
            ServeDir::new(state.paths.uploads_images.clone()),
        )
        .nest_service(
            "/uploads/videos",
            ServeDir::new(state.paths.uploads_videos.clone()),
        )
        .nest_service(
            "/uploads/thumbs",
            ServeDir::new(state.paths.uploads_thumbs.clone()),
        )
        .layer(DefaultBodyLimit::max(upload_body_limit))
        .layer(middleware::from_fn(
            crate::compression::response_compression,
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

fn upload_body_limit(settings: &Settings) -> usize {
    let limit = settings.media.max_video_size.saturating_add(1024 * 1024);
    match usize::try_from(limit) {
        Ok(limit) => limit,
        Err(_overflow) => usize::MAX,
    }
}

async fn client_script() -> Response {
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        render::client_script(),
    )
        .into_response()
}

async fn site_favicon(State(state): State<Arc<AppState>>) -> Response {
    favicon::response(&state.paths).await
}

#[derive(Deserialize)]
struct AuthForm {
    username: String,
    password: String,
}

#[derive(Deserialize)]
struct RegisterForm {
    username: String,
    password: String,
    confirm_password: Option<String>,
    captcha_token: Option<String>,
    captcha_answer: Option<String>,
}

#[derive(Deserialize)]
struct CsrfForm {
    csrf: String,
}

#[derive(Deserialize)]
struct QuoteForm {
    csrf: String,
    text: String,
}

#[derive(Deserialize)]
struct DeleteForm {
    csrf: String,
    return_to: String,
}

#[derive(Deserialize)]
struct DeleteQuery {
    return_to: Option<String>,
}

#[derive(Deserialize)]
struct SearchQuery {
    q: Option<String>,
}

#[derive(Deserialize)]
struct AdminUsersQuery {
    user_q: Option<String>,
    post_q: Option<String>,
}

#[derive(Deserialize)]
struct SettingsQuery {
    saved: Option<String>,
}

#[derive(Deserialize)]
struct DeepSettingsQuery {
    saved: Option<String>,
    discarded: Option<String>,
}

#[derive(Deserialize)]
struct MutedWordForm {
    csrf: String,
    term: String,
}

#[derive(Deserialize)]
struct PasswordChangeForm {
    csrf: String,
    current_password: String,
    new_password: String,
    confirm_new_password: String,
}

#[derive(Deserialize)]
struct DeleteAccountPasswordForm {
    csrf: String,
    delete_intent: Option<String>,
    password: String,
}

struct ParsedProfileUpdate {
    csrf_token: String,
    display_name: String,
    bio: String,
    location: String,
    website: String,
    theme: Theme,
    delete_profile_picture: bool,
    delete_banner: bool,
    nsfw_blur_enabled: bool,
    profile_picture_media_id: Option<i64>,
    banner_media_id: Option<i64>,
}

struct ParsedFaviconUpload {
    uploaded: bool,
}

struct ParsedPostCreate {
    csrf_token: String,
    text: String,
    parent_post_id: Option<i64>,
    media_ids: Vec<i64>,
    is_nsfw: bool,
}

#[derive(Deserialize)]
struct AdminNsfwForm {
    csrf: String,
    nsfw: String,
}

struct DeletePreview {
    text: String,
    username: Option<String>,
    display_name: Option<String>,
    parent_post_id: Option<i64>,
}

#[derive(Serialize)]
struct FollowActionResponse {
    kind: &'static str,
    user_id: i64,
    following: bool,
    followers: i64,
    following_count: i64,
    action: String,
}

#[derive(Serialize)]
struct PostActionResponse {
    kind: &'static str,
    post_id: i64,
    likes: i64,
    reposts: i64,
    replies: i64,
    liked: bool,
    bookmarked: bool,
    reposted: bool,
}

#[derive(Serialize)]
struct PostCreateResponse {
    kind: &'static str,
    post_id: i64,
    parent_post_id: Option<i64>,
    redirect: String,
    html: String,
}

async fn current(state: &AppState, headers: &HeaderMap) -> AppResult<Option<CurrentUser>> {
    Ok(auth::current_user(&state.pool, headers).await?)
}

async fn local_redirect() -> Redirect {
    Redirect::to("/home")
}

async fn home(State(state): State<Arc<AppState>>, headers: HeaderMap) -> AppResult<Html<String>> {
    let user = current(&state, &headers).await?;
    let posts = social::timeline(&state.pool, user.as_ref().map(|u| u.id), "local", None).await?;
    let csrf = form_csrf(&state, &headers).await;
    let composer = if user.is_some() || state.settings.accounts.anonymous_mode_enabled {
        render::composer(csrf.as_deref(), None, state.settings.posts.max_text_chars)
    } else {
        String::new()
    };
    let body = format!(
        "{}{}{}",
        render::page_header("Home Feed", "All posts"),
        composer,
        render::posts_with_nsfw_blur(
            &posts,
            user.as_ref(),
            csrf.as_deref(),
            blur_nsfw_media(&state, user.as_ref()),
        )
    );
    Ok(Html(
        page_layout(&state, user.as_ref(), csrf.as_deref(), "Home Feed", &body).await?,
    ))
}

async fn layout_context(
    state: &AppState,
    user: Option<&CurrentUser>,
) -> AppResult<render::LayoutContext> {
    let (counts, notification_unread_count) = if let Some(user) = user {
        (
            Some(social::follow_counts(&state.pool, user.id).await?),
            Some(social::unread_notification_count(&state.pool, user.id).await?),
        )
    } else {
        (None, None)
    };
    Ok(render::LayoutContext {
        anonymous_mode_enabled: state.settings.accounts.anonymous_mode_enabled,
        tor_onion_address: state.tor.onion_address().or_else(|| {
            (!state.settings.tor.display_onion_address.is_empty())
                .then(|| state.settings.tor.display_onion_address.clone())
        }),
        follower_count: counts.map(|(followers, _following)| followers),
        following_count: counts.map(|(_followers, following)| following),
        notification_unread_count,
        favicon_content_type: favicon::current(&state.paths).content_type(),
    })
}

async fn page_layout(
    state: &AppState,
    user: Option<&CurrentUser>,
    csrf: Option<&str>,
    title: &str,
    body: &str,
) -> AppResult<String> {
    let context = layout_context(state, user).await?;
    Ok(render::layout_with_context(
        user,
        csrf,
        title,
        body,
        &state.settings.site.name,
        &context,
    ))
}

fn blur_nsfw_media(state: &AppState, user: Option<&CurrentUser>) -> bool {
    let global_blur = Settings::load(&state.paths.settings_path)
        .map_or(state.settings.media.nsfw_blur_enabled, |settings| {
            settings.media.nsfw_blur_enabled
        });
    global_blur && user.is_none_or(|user| user.nsfw_blur_enabled)
}

async fn login_form(State(state): State<Arc<AppState>>) -> Html<String> {
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

async fn login(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Form(form): Form<AuthForm>,
) -> AppResult<Response> {
    let actor = ip_actor(addr);
    rate_limit::ensure_under_limit(
        &state.pool,
        rate_limit::Scope::FailedLogin,
        &actor,
        state.settings.moderation.failed_login_attempts_per_15m,
        15 * 60,
    )
    .await
    .map_err(|err| AppError::RateLimited(err.to_string()))?;
    if let Err(err) = crate::validation::validate_password(&form.password, &state.settings) {
        rate_limit::record(&state.pool, rate_limit::Scope::FailedLogin, &actor).await?;
        return auth_form_response(&state, StatusCode::BAD_REQUEST, &err.to_string()).await;
    }
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
        HeaderValue::from_str(&auth::set_session_cookie(
            &session,
            state.settings.server.cookie_secure,
        ))
        .map_err(|err| AppError::BadRequest(err.to_string()))?,
    );
    Ok(response)
}

async fn register_form(State(state): State<Arc<AppState>>) -> AppResult<Html<String>> {
    if !state.settings.accounts.registration_enabled {
        return Err(AppError::Forbidden);
    }
    let body = register_form_body(&state, None).await?;
    Ok(Html(
        page_layout(&state, None, None, "Register", &body).await?,
    ))
}

async fn register(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Form(form): Form<RegisterForm>,
) -> AppResult<Response> {
    if !state.settings.accounts.registration_enabled {
        return Err(AppError::Forbidden);
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
    let mut response = Redirect::to("/home").into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&auth::set_session_cookie(
            &session,
            state.settings.server.cookie_secure,
        ))
        .map_err(|err| AppError::BadRequest(err.to_string()))?,
    );
    Ok(response)
}

async fn auth_form_response(
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

async fn register_form_response(
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

async fn register_form_body(state: &AppState, message: Option<&str>) -> AppResult<String> {
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

async fn logout(
    State(state): State<Arc<AppState>>,
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
        HeaderValue::from_str(&auth::clear_session_cookie(
            state.settings.server.cookie_secure,
        ))
        .map_err(|err| AppError::BadRequest(err.to_string()))?,
    );
    Ok(response)
}

async fn create_post(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    multipart: Multipart,
) -> AppResult<Response> {
    let user = current(&state, &headers).await?;
    if let Some(user) = &user
        && user.is_suspended
    {
        return Err(AppError::Forbidden);
    }
    if user.is_none() && !state.settings.accounts.anonymous_mode_enabled {
        return Err(AppError::Forbidden);
    }
    let form = match parse_post_create(&state, user.as_ref().map(|u| u.id), multipart).await {
        Ok(form) => form,
        Err(AppError::BadRequest(message)) => {
            return bad_request_page(&state, user.as_ref(), &message).await;
        }
        Err(err) => return Err(err),
    };
    if let Some(parent_id) = form.parent_post_id {
        ensure_parent_post_exists(&state.pool, parent_id).await?;
    }
    if user.is_some() {
        validate_csrf(&state.pool, &headers, &form.csrf_token).await?;
    }
    if form.is_nsfw {
        media::set_media_nsfw(&state.pool, &form.media_ids, true).await?;
    }
    let (scope, actor, max_events, window_secs) = if user.is_none() {
        (
            rate_limit::Scope::AnonymousPost,
            ip_actor(addr),
            state.settings.moderation.anonymous_posts_per_ip_per_hour,
            60 * 60,
        )
    } else if form.parent_post_id.is_some() {
        (
            rate_limit::Scope::Reply,
            user_actor(user.as_ref().map(|u| u.id).unwrap_or_default()),
            state.settings.moderation.replies_per_minute,
            60,
        )
    } else {
        (
            rate_limit::Scope::Post,
            user_actor(user.as_ref().map(|u| u.id).unwrap_or_default()),
            state.settings.moderation.posts_per_minute,
            60,
        )
    };
    rate_limit::check_and_record(&state.pool, scope, &actor, max_events, window_secs)
        .await
        .map_err(|err| AppError::RateLimited(err.to_string()))?;
    let post_id = social::create_post(
        &state.pool,
        &state.settings,
        user.as_ref().map(|u| u.id),
        &form.text,
        form.parent_post_id,
        &form.media_ids,
    )
    .await
    .map_err(|err| AppError::BadRequest(err.to_string()))?;
    let redirect = form.parent_post_id.map_or_else(
        || format!("/home#post-{post_id}"),
        |_| format!("/posts/{post_id}#reply-{post_id}"),
    );
    if enhanced_request(&headers) {
        let posts = social::post_thread(&state.pool, user.as_ref().map(|u| u.id), post_id).await?;
        let post = posts
            .iter()
            .find(|post| post.id == post_id)
            .ok_or(AppError::NotFound)?;
        return Ok(Json(PostCreateResponse {
            kind: "post-created",
            post_id,
            parent_post_id: form.parent_post_id,
            redirect,
            html: if form.parent_post_id.is_some() {
                render::thread_post_card_with_nsfw_blur(
                    post,
                    user.as_ref(),
                    form_csrf(&state, &headers).await.as_deref(),
                    blur_nsfw_media(&state, user.as_ref()),
                )
            } else {
                render::post_card_with_nsfw_blur(
                    post,
                    user.as_ref(),
                    form_csrf(&state, &headers).await.as_deref(),
                    blur_nsfw_media(&state, user.as_ref()),
                )
            },
        })
        .into_response());
    }
    Ok(Redirect::to(&redirect).into_response())
}

async fn parse_post_create(
    state: &AppState,
    user_id: Option<i64>,
    mut multipart: Multipart,
) -> AppResult<ParsedPostCreate> {
    let mut form = ParsedPostCreate {
        csrf_token: String::new(),
        text: String::new(),
        parent_post_id: None,
        media_ids: Vec::new(),
        is_nsfw: false,
    };
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
                form.text = field
                    .text()
                    .await
                    .map_err(|err| AppError::BadRequest(err.to_string()))?;
            }
            "csrf" => {
                form.csrf_token = field
                    .text()
                    .await
                    .map_err(|err| AppError::BadRequest(err.to_string()))?;
            }
            "parent_post_id" => {
                let value = field
                    .text()
                    .await
                    .map_err(|err| AppError::BadRequest(err.to_string()))?;
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
            "nsfw" => {
                form.is_nsfw = true;
                let _ignored = field
                    .text()
                    .await
                    .map_err(|err| AppError::BadRequest(err.to_string()))?;
            }
            "media"
                if field
                    .file_name()
                    .is_some_and(|name| !name.trim().is_empty()) =>
            {
                form.media_ids.push(
                    media::save_upload(
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
            }
            _ => {}
        }
    }
    Ok(form)
}

async fn bad_request_page(
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

async fn thread(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    let user = current(&state, &headers).await?;
    let posts = social::post_thread(&state.pool, user.as_ref().map(|u| u.id), id).await?;
    if posts.is_empty() {
        return Err(AppError::NotFound);
    }
    let csrf = form_csrf(&state, &headers).await;
    let composer = if user.is_some() || state.settings.accounts.anonymous_mode_enabled {
        render::composer(
            csrf.as_deref(),
            Some(id),
            state.settings.posts.max_text_chars,
        )
    } else {
        String::new()
    };
    let body = format!(
        "{}{}{}",
        render::thread_back_control(),
        render::thread_posts_with_nsfw_blur(
            &posts,
            user.as_ref(),
            csrf.as_deref(),
            blur_nsfw_media(&state, user.as_ref()),
        ),
        composer
    );
    Ok(Html(
        page_layout(&state, user.as_ref(), csrf.as_deref(), "Thread", &body).await?,
    ))
}

async fn delete_confirm(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Query(query): Query<DeleteQuery>,
) -> AppResult<Html<String>> {
    let user = require_user(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers).await.unwrap_or_default();
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

async fn delete_post(
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
    social::delete_post(&state.pool, user.id, id, user.is_admin).await?;
    let fallback = if let Some(parent_id) = preview.parent_post_id {
        format!("/posts/{parent_id}#post-{parent_id}")
    } else {
        "/home".to_owned()
    };
    let target = safe_delete_return_target(&form.return_to, id).unwrap_or(fallback);
    Ok(Redirect::to(&target).into_response())
}

async fn toggle_like(
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

async fn toggle_bookmark(
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

async fn repost(
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

async fn quote_form(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    let user = require_active_user(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers).await.unwrap_or_default();
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

async fn quote_post(
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

async fn reply_redirect(Path(id): Path<i64>) -> Redirect {
    Redirect::to(&format!("/posts/{id}"))
}

#[expect(
    clippy::too_many_lines,
    reason = "profile rendering combines existing viewer controls and page assembly in one route handler"
)]
async fn profile(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(username): Path<String>,
) -> AppResult<Html<String>> {
    let user = current(&state, &headers).await?;
    let profile = state
        .pool
        .call(move |conn| {
            conn.query_row(
                r#"
        SELECT u.id, u.username, u.display_name, u.bio, u.location, u.website,
          pic.public_path AS profile_picture_path,
          banner.public_path AS banner_path
        FROM users u
        LEFT JOIN media pic ON pic.id = u.profile_picture_media_id
        LEFT JOIN media banner ON banner.id = u.banner_media_id
        WHERE u.normalized_username = ? AND u.is_deleted = 0
        "#,
                [username.to_ascii_lowercase()],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, Option<String>>(7)?,
                    ))
                },
            )
            .optional()
            .map_err(Into::into)
        })
        .await?;
    let Some((
        profile_id,
        profile_username,
        display_name,
        bio,
        location,
        website,
        picture_path,
        banner_path,
    )) = profile
    else {
        return Err(AppError::NotFound);
    };
    let csrf = form_csrf(&state, &headers).await;
    let posts =
        social::profile_timeline(&state.pool, user.as_ref().map(|u| u.id), profile_id).await?;
    let (followers, following) = social::follow_counts(&state.pool, profile_id).await?;
    let controls = profile_controls(&state, user.as_ref(), csrf.as_deref(), profile_id).await?;
    let picture = picture_path.map_or_else(
        || r#"<div class="profile-picture" aria-hidden="true"></div>"#.to_owned(),
        |path| {
            format!(
                r#"<img class="profile-picture" src="{}" alt="">"#,
                html_escape::encode_double_quoted_attribute(&path)
            )
        },
    );
    let banner = banner_path.map_or_else(
        || r#"<div class="profile-banner" aria-hidden="true"></div>"#.to_owned(),
        |path| {
            format!(
                r#"<img class="profile-banner" src="{}" alt="">"#,
                html_escape::encode_double_quoted_attribute(&path)
            )
        },
    );
    let website_link = if website.trim().is_empty() {
        String::new()
    } else {
        format!(
            r#"<p><a href="{}">{}</a></p>"#,
            html_escape::encode_double_quoted_attribute(website.as_str()),
            html_escape::encode_text(website.as_str())
        )
    };
    let location_line = if location.trim().is_empty() {
        String::new()
    } else {
        format!(
            r#"<p class="profile-meta">{}</p>"#,
            html_escape::encode_text(location.as_str())
        )
    };
    let body = format!(
        r#"<section class="panel profile">{}<div class="profile-heading">{}<div class="profile-main"><div class="profile-title-row"><div><h1>{}</h1><p class="muted">@{}</p></div>{}</div><p class="counts"><span data-profile-followers="{}">{} followers</span><span data-profile-following="{}">{} following</span></p>{}<p>{}</p>{}</div></div></section>{}"#,
        banner,
        picture,
        html_escape::encode_text(display_name.as_str()),
        html_escape::encode_text(profile_username.as_str()),
        controls,
        profile_id,
        followers,
        profile_id,
        following,
        location_line,
        html_escape::encode_text(bio.as_str()),
        website_link,
        render::posts_with_nsfw_blur(
            &posts,
            user.as_ref(),
            csrf.as_deref(),
            blur_nsfw_media(&state, user.as_ref()),
        )
    );
    Ok(Html(
        page_layout(
            &state,
            user.as_ref(),
            csrf.as_deref(),
            &profile_username,
            &body,
        )
        .await?,
    ))
}

async fn profile_identity(
    pool: &SqlitePool,
    username: &str,
) -> anyhow::Result<Option<(i64, String, String)>> {
    let normalized_username = username.to_ascii_lowercase();
    pool.call(move |conn| {
        conn.query_row(
            r#"
            SELECT id, username, display_name
            FROM users
            WHERE normalized_username = ? AND is_deleted = 0
            "#,
            [normalized_username],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(Into::into)
    })
    .await
}

async fn profile_followers(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(username): Path<String>,
) -> AppResult<Html<String>> {
    let user = current(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers).await;
    let Some((profile_id, profile_username, display_name)) =
        profile_identity(&state.pool, &username).await?
    else {
        return Err(AppError::NotFound);
    };
    let accounts =
        social::followers_accounts(&state.pool, profile_id, user.as_ref().map(|user| user.id))
            .await?;
    let body = format!(
        "{}{}",
        render::page_header(
            &format!("{display_name} followers"),
            &format!("Users who follow @{profile_username}.")
        ),
        render::account_links(&accounts, "No followers yet.")
    );
    Ok(Html(
        page_layout(
            &state,
            user.as_ref(),
            csrf.as_deref(),
            &format!("{display_name} followers"),
            &body,
        )
        .await?,
    ))
}

async fn profile_following(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(username): Path<String>,
) -> AppResult<Html<String>> {
    let user = current(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers).await;
    let Some((profile_id, profile_username, display_name)) =
        profile_identity(&state.pool, &username).await?
    else {
        return Err(AppError::NotFound);
    };
    let accounts = social::following_accounts_for_profile(
        &state.pool,
        profile_id,
        user.as_ref().map(|user| user.id),
    )
    .await?;
    let body = format!(
        "{}{}",
        render::page_header(
            &format!("{display_name} following"),
            &format!("Users @{profile_username} follows.")
        ),
        render::account_links(&accounts, "Not following anyone yet.")
    );
    Ok(Html(
        page_layout(
            &state,
            user.as_ref(),
            csrf.as_deref(),
            &format!("{display_name} following"),
            &body,
        )
        .await?,
    ))
}

async fn profile_controls(
    state: &AppState,
    user: Option<&CurrentUser>,
    csrf: Option<&str>,
    profile_id: i64,
) -> AppResult<String> {
    let (Some(viewer), Some(csrf)) = (user, csrf) else {
        return Ok(String::new());
    };
    if viewer.id == profile_id {
        return Ok(
            r#"<div class="actions profile-actions"><a class="button-link" href="/settings">Settings</a></div>"#
                .to_owned(),
        );
    }
    let follow_action = render::follow_form(
        profile_id,
        csrf,
        social::is_following(&state.pool, viewer.id, profile_id).await?,
    );
    Ok(format!(
        r#"<div class="actions profile-actions">{}<span class="actions profile-secondary">{}{}</span></div>"#,
        follow_action,
        small_form(
            &format!("/users/{profile_id}/block"),
            csrf,
            "Block",
            "Block this account"
        ),
        small_form(
            &format!("/users/{profile_id}/mute"),
            csrf,
            "Mute",
            "Mute this account"
        )
    ))
}

async fn follow(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> AppResult<Response> {
    let user = require_active_user(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    social::follow(&state.pool, user.id, id)
        .await
        .map_err(|err| AppError::BadRequest(err.to_string()))?;
    if enhanced_request(&headers) {
        return Ok(Json(follow_action_response(&state.pool, user.id, id).await?).into_response());
    }
    Ok(Redirect::to(&account_action_return(&state.pool, &headers, id).await?).into_response())
}

async fn unfollow(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> AppResult<Response> {
    let user = require_active_user(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    social::unfollow(&state.pool, user.id, id).await?;
    if enhanced_request(&headers) {
        return Ok(Json(follow_action_response(&state.pool, user.id, id).await?).into_response());
    }
    Ok(Redirect::to(&account_action_return(&state.pool, &headers, id).await?).into_response())
}

async fn block(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> AppResult<Response> {
    let user = require_active_user(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    social::block(&state.pool, user.id, id)
        .await
        .map_err(|err| {
            tracing::warn!(blocker_id = user.id, blocked_id = id, error = %err, "block rejected");
            AppError::BadRequest(err.to_string())
        })?;
    Ok(Redirect::to(&account_action_return(&state.pool, &headers, id).await?).into_response())
}

async fn unblock(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> AppResult<Response> {
    let user = require_active_user(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    social::unblock(&state.pool, user.id, id).await?;
    Ok(Redirect::to("/settings?saved=profile").into_response())
}

fn settings_query_notice(saved: Option<&str>) -> Option<(&'static str, &'static str)> {
    match saved {
        Some("profile") => Some(("success", "Profile settings saved.")),
        Some("muted-word") => Some(("success", "Muted word saved.")),
        Some("muted-word-removed") => Some(("success", "Muted word removed.")),
        Some("password") => Some(("success", "Password changed.")),
        _ => None,
    }
}

struct SettingsProfile {
    display_name: String,
    bio: String,
    location: String,
    website: String,
    theme: String,
    nsfw_blur_enabled: bool,
    picture_path: Option<String>,
    banner_path: Option<String>,
}

async fn settings_profile(pool: &SqlitePool, user_id: i64) -> AppResult<SettingsProfile> {
    pool.call(move |conn| {
        conn.query_row(
            r#"
        SELECT u.display_name, u.bio, u.location, u.website, u.theme, u.nsfw_blur_enabled,
          pic.public_path AS profile_picture_path,
          banner.public_path AS banner_path
        FROM users u
        LEFT JOIN media pic ON pic.id = u.profile_picture_media_id
        LEFT JOIN media banner ON banner.id = u.banner_media_id
        WHERE u.id = ?
        "#,
            [user_id],
            |row| {
                Ok(SettingsProfile {
                    display_name: row.get(0)?,
                    bio: row.get(1)?,
                    location: row.get(2)?,
                    website: row.get(3)?,
                    theme: row.get(4)?,
                    nsfw_blur_enabled: row.get::<_, i64>(5)? != 0,
                    picture_path: row.get(6)?,
                    banner_path: row.get(7)?,
                })
            },
        )
        .map_err(Into::into)
    })
    .await
    .map_err(Into::into)
}

fn settings_profile_media(
    picture_path: Option<&str>,
    banner_path: Option<&str>,
    allow_profile_pictures: bool,
    allow_profile_banners: bool,
) -> String {
    let banner = banner_path.map_or_else(
        || {
            r#"<div class="settings-banner-preview placeholder" aria-hidden="true"></div>"#
                .to_owned()
        },
        |path| {
            format!(
                r#"<img class="settings-banner-preview" src="{}" alt="">"#,
                html_escape::encode_double_quoted_attribute(path)
            )
        },
    );
    let picture = picture_path.map_or_else(
        || {
            r#"<div class="settings-picture-preview placeholder" aria-hidden="true"></div>"#
                .to_owned()
        },
        |path| {
            format!(
                r#"<img class="settings-picture-preview" src="{}" alt="">"#,
                html_escape::encode_double_quoted_attribute(path)
            )
        },
    );
    let banner_control = if allow_profile_banners {
        let delete = banner_path.map_or_else(String::new, |_| {
            r#"<label class="check-row"><input type="checkbox" name="delete_banner" value="true"> Remove banner</label>"#
                .to_owned()
        });
        format!(
            r#"<div class="media-control-row"><label class="file-control" for="banner">Change banner<input id="banner" name="banner" type="file" accept="image/*"></label>{delete}</div>"#
        )
    } else {
        r#"<p class="muted">Profile banners are disabled.</p>"#.to_owned()
    };
    let picture_control = if allow_profile_pictures {
        let delete = picture_path.map_or_else(String::new, |_| {
            r#"<label class="check-row"><input type="checkbox" name="delete_profile_picture" value="true"> Remove profile picture</label>"#
                .to_owned()
        });
        format!(
            r#"<div class="media-control-row"><label class="file-control" for="profile_picture">Change profile picture<input id="profile_picture" name="profile_picture" type="file" accept="image/*"></label>{delete}</div>"#
        )
    } else {
        r#"<p class="muted">Profile pictures are disabled.</p>"#.to_owned()
    };
    format!(
        r#"<div class="settings-profile-media"><div class="settings-banner-wrap">{banner}</div><div class="settings-picture-row">{picture}<div class="settings-media-controls">{banner_control}{picture_control}</div></div></div>"#
    )
}

fn settings_user_list(
    users: &[(i64, String, String)],
    action_suffix: &str,
    csrf: &str,
    label: &str,
    empty_title: &str,
    empty_message: &str,
) -> String {
    if users.is_empty() {
        return compact_empty_state(empty_title, empty_message);
    }
    let rows = users
        .iter()
        .map(|(id, username, display_name)| {
            format!(
                r#"<li><span><strong>{}</strong> <span class="muted">@{}</span></span>{}</li>"#,
                html_escape::encode_text(display_name),
                html_escape::encode_text(username),
                small_form(&format!("/users/{id}{action_suffix}"), csrf, label, label,)
            )
        })
        .collect::<Vec<_>>()
        .join("");
    format!(r#"<ul class="settings-item-list">{rows}</ul>"#)
}

fn settings_muted_word_list(words: &[social::MutedWord], csrf: &str) -> String {
    if words.is_empty() {
        return compact_empty_state(
            "No muted words",
            "Posts containing muted words will be hidden.",
        );
    }
    let rows = words
        .iter()
        .map(|word| {
            format!(
                r#"<li><span>{}</span>{}</li>"#,
                html_escape::encode_text(&word.term),
                small_form(
                    &format!("/settings/muted-words/{}/remove", word.id),
                    csrf,
                    "Remove",
                    "Remove muted word",
                )
            )
        })
        .collect::<Vec<_>>()
        .join("");
    format!(r#"<ul class="settings-item-list">{rows}</ul>"#)
}

fn compact_empty_state(title: &str, message: &str) -> String {
    format!(
        r#"<div class="compact-empty"><strong>{}</strong><p>{}</p></div>"#,
        html_escape::encode_text(title),
        html_escape::encode_text(message)
    )
}

fn validate_profile_location(location: &str) -> AppResult<()> {
    if location.chars().count() > 100 {
        return Err(AppError::BadRequest("location is too long".to_owned()));
    }
    if location.chars().any(char::is_control) {
        return Err(AppError::BadRequest(
            "location contains unsupported control characters".to_owned(),
        ));
    }
    Ok(())
}

async fn mute(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> AppResult<Response> {
    let user = require_active_user(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    social::mute(&state.pool, user.id, id)
        .await
        .map_err(|err| {
            tracing::warn!(muter_id = user.id, muted_id = id, error = %err, "mute rejected");
            AppError::BadRequest(err.to_string())
        })?;
    Ok(Redirect::to(&account_action_return(&state.pool, &headers, id).await?).into_response())
}

async fn unmute(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> AppResult<Response> {
    let user = require_active_user(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    social::unmute(&state.pool, user.id, id).await?;
    Ok(Redirect::to("/settings").into_response())
}

async fn settings_form(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<SettingsQuery>,
) -> AppResult<Html<String>> {
    let user = require_user(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers).await.unwrap_or_default();
    let notice = settings_query_notice(query.saved.as_deref());
    Ok(Html(settings_page(&state, &user, &csrf, notice).await?))
}

async fn settings_page(
    state: &AppState,
    user: &CurrentUser,
    csrf: &str,
    notice: Option<(&str, &str)>,
) -> AppResult<String> {
    let profile = settings_profile(&state.pool, user.id).await?;
    let blocked = social::blocked_users(&state.pool, user.id).await?;
    let muted = social::muted_users(&state.pool, user.id).await?;
    let muted_words = social::muted_words(&state.pool, user.id).await?;
    let dark_checked = if Theme::from(profile.theme.as_str()) == Theme::Dark {
        " checked"
    } else {
        ""
    };
    let nsfw_checked = if profile.nsfw_blur_enabled {
        " checked"
    } else {
        ""
    };
    let notice_html =
        notice.map_or_else(String::new, |(kind, message)| render::notice(kind, message));
    let password_hint = if state.settings.accounts.min_password_length == 0 {
        "No minimum password length is currently required.".to_owned()
    } else {
        format!(
            "Password must be at least {} characters.",
            state.settings.accounts.min_password_length
        )
    };
    let new_password_attrs = render::password_length_attrs(
        state.settings.accounts.min_password_length,
        "new-password-requirement",
    );
    let confirm_new_password_attrs = render::password_length_attrs(
        state.settings.accounts.min_password_length,
        "confirm-new-password-requirement",
    );
    let profile_media = settings_profile_media(
        profile.picture_path.as_deref(),
        profile.banner_path.as_deref(),
        state.settings.accounts.allow_profile_pictures,
        state.settings.accounts.allow_profile_banners,
    );
    let body = format!(
        r#"{notice_html}<section class="panel settings-card settings-profile-editor" data-testid="settings-card"><div class="settings-editor-bar"><div><h1>Account settings</h1><p class="muted">Profile, privacy, and account controls.</p></div><button class="primary" type="submit" form="profile-settings-form">Save settings</button></div><form id="profile-settings-form" method="post" enctype="multipart/form-data" class="settings-profile-form"><input type="hidden" name="csrf" value="{}">{}<label class="theme-toggle" for="dark_mode"><input id="dark_mode" name="dark_mode" type="checkbox" value="true"{}> Dark mode</label><label class="theme-toggle" for="nsfw_blur_enabled"><input id="nsfw_blur_enabled" name="nsfw_blur_enabled" type="checkbox" value="true"{}> Blur NSFW media</label><div class="settings-fields"><label for="display_name">Display name</label><input id="display_name" name="display_name" value="{}"><label for="bio">Bio</label><textarea id="bio" name="bio">{}</textarea><label for="location">Location</label><input id="location" name="location" value="{}"><label for="website">Website</label><input id="website" type="url" name="website" value="{}"></div></form></section><div class="settings-grid"><section class="panel settings-card compact-panel" data-testid="settings-card"><h2>Blocked users</h2>{}</section><section class="panel settings-card compact-panel" data-testid="settings-card"><h2>Muted users</h2>{}</section></div><section class="panel settings-card compact-panel" data-testid="settings-card"><h2>Muted words</h2><form method="post" action="/settings/muted-words" class="inline-settings-form"><input type="hidden" name="csrf" value="{}"><label class="sr-only" for="muted-word">Word or phrase to mute</label><input id="muted-word" name="term" placeholder="Word or phrase" required><button type="submit">Add muted word</button></form>{}</section><section class="panel settings-card compact-panel" data-testid="settings-card"><h2>Change password</h2><form method="post" action="/settings/password" class="settings-password-form"><input type="hidden" name="csrf" value="{}"><label for="current_password">Current password</label><div class="password-control"><input id="current_password" name="current_password" type="password" autocomplete="current-password"><button type="button" class="password-toggle" data-password-toggle="current_password" aria-label="Show current password">Show</button></div><label for="new_password">New password</label><p class="field-help" id="new-password-requirement">{}</p><div class="password-control"><input id="new_password" name="new_password" type="password" autocomplete="new-password"{}><button type="button" class="password-toggle" data-password-toggle="new_password" aria-label="Show new password">Show</button></div><label for="confirm_new_password">Confirm new password</label><p class="field-help" id="confirm-new-password-requirement">{}</p><div class="password-control"><input id="confirm_new_password" name="confirm_new_password" type="password" autocomplete="new-password"{}><button type="button" class="password-toggle" data-password-toggle="confirm_new_password" aria-label="Show new password confirmation">Show</button></div><button type="submit">Change password</button></form></section><section class="panel settings-card danger-panel" data-testid="settings-card"><h2>Delete account</h2><p>This permanently removes your profile, posts, media, sessions, and account relationships.</p><p><a class="button-link danger-link" href="/settings/delete">Start delete account flow</a></p></section>"#,
        html_escape::encode_double_quoted_attribute(&csrf),
        profile_media,
        dark_checked,
        nsfw_checked,
        html_escape::encode_double_quoted_attribute(profile.display_name.as_str()),
        html_escape::encode_text(profile.bio.as_str()),
        html_escape::encode_double_quoted_attribute(profile.location.as_str()),
        html_escape::encode_double_quoted_attribute(profile.website.as_str()),
        settings_user_list(
            &blocked,
            "/unblock",
            csrf,
            "Unblock",
            "No blocked users",
            "Blocked accounts will appear here."
        ),
        settings_user_list(
            &muted,
            "/unmute",
            csrf,
            "Unmute",
            "No muted users",
            "Muted accounts will appear here."
        ),
        html_escape::encode_double_quoted_attribute(&csrf),
        settings_muted_word_list(&muted_words, csrf),
        html_escape::encode_double_quoted_attribute(&csrf),
        html_escape::encode_text(&password_hint),
        new_password_attrs,
        html_escape::encode_text(&password_hint),
        confirm_new_password_attrs,
    );
    page_layout(state, Some(user), Some(csrf), "Settings", &body).await
}

async fn settings_update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    multipart: Multipart,
) -> AppResult<Response> {
    let user = require_user(&state, &headers).await?;
    let form = parse_profile_update(&state, user.id, multipart).await?;
    validate_csrf(&state.pool, &headers, &form.csrf_token).await?;
    crate::validation::validate_profile_text(&form.display_name, &form.bio, &state.settings)?;
    validate_profile_location(&form.location)?;
    let display_name = form.display_name.trim().to_owned();
    let bio = form.bio.trim().to_owned();
    let location = form.location.trim().to_owned();
    let website = form.website.trim().to_owned();
    let theme = form.theme.as_str().to_owned();
    let nsfw_blur_enabled = i64::from(form.nsfw_blur_enabled);
    state
        .pool
        .call(move |conn| {
            conn.execute(
                "UPDATE users SET display_name = ?, bio = ?, location = ?, website = ?, theme = ?, nsfw_blur_enabled = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
                params![display_name, bio, location, website, theme, nsfw_blur_enabled, user.id],
            )?;
            Ok(())
        })
        .await?;
    if form.delete_profile_picture {
        media::clear_profile_media(&state.pool, user.id, media::ProfileMediaSlot::Picture).await?;
    }
    if form.delete_banner {
        media::clear_profile_media(&state.pool, user.id, media::ProfileMediaSlot::Banner).await?;
    }
    if let Some(media_id) = form.profile_picture_media_id {
        media::set_profile_media(
            &state.pool,
            user.id,
            media::ProfileMediaSlot::Picture,
            media_id,
        )
        .await?;
    }
    if let Some(media_id) = form.banner_media_id {
        media::set_profile_media(
            &state.pool,
            user.id,
            media::ProfileMediaSlot::Banner,
            media_id,
        )
        .await?;
    }
    Ok(Redirect::to("/settings?saved=profile").into_response())
}

async fn add_muted_word(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<MutedWordForm>,
) -> AppResult<Response> {
    let user = require_active_user(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    match social::add_muted_word(&state.pool, user.id, &form.term).await {
        Ok(()) => Ok(Redirect::to("/settings?saved=muted-word").into_response()),
        Err(err) => {
            settings_response(
                &state,
                &user,
                &headers,
                StatusCode::BAD_REQUEST,
                "error",
                &err.to_string(),
            )
            .await
        }
    }
}

async fn remove_muted_word(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> AppResult<Response> {
    let user = require_active_user(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    social::remove_muted_word(&state.pool, user.id, id).await?;
    Ok(Redirect::to("/settings?saved=muted-word-removed").into_response())
}

async fn change_password(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<PasswordChangeForm>,
) -> AppResult<Response> {
    let user = require_active_user(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    match auth::change_password(
        &state.pool,
        &state.settings,
        user.id,
        &form.current_password,
        &form.new_password,
        &form.confirm_new_password,
    )
    .await
    {
        Ok(()) => Ok(Redirect::to("/settings?saved=password").into_response()),
        Err(err) => {
            settings_response(
                &state,
                &user,
                &headers,
                StatusCode::BAD_REQUEST,
                "error",
                &err.to_string(),
            )
            .await
        }
    }
}

async fn settings_response(
    state: &AppState,
    user: &CurrentUser,
    headers: &HeaderMap,
    status: StatusCode,
    kind: &'static str,
    message: &str,
) -> AppResult<Response> {
    let csrf = form_csrf(state, headers).await.unwrap_or_default();
    Ok((
        status,
        Html(settings_page(state, user, &csrf, Some((kind, message))).await?),
    )
        .into_response())
}

async fn delete_account_warning(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AppResult<Html<String>> {
    let user = require_active_user(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers).await.unwrap_or_default();
    let body = render_delete_account_warning();
    Ok(Html(
        page_layout(&state, Some(&user), Some(&csrf), "Delete account", &body).await?,
    ))
}

async fn delete_account_final_warning(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AppResult<Html<String>> {
    let user = require_active_user(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers).await.unwrap_or_default();
    let delete_intent = create_delete_account_intent(&state, &headers, user.id).await?;
    let body = render_delete_account_final_warning(&csrf, &delete_intent, None);
    Ok(Html(
        page_layout(
            &state,
            Some(&user),
            Some(&csrf),
            "Confirm delete account",
            &body,
        )
        .await?,
    ))
}

async fn delete_account_final(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<DeleteAccountPasswordForm>,
) -> AppResult<Response> {
    let user = require_active_user(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    if !consume_delete_account_intent(
        &state,
        &headers,
        user.id,
        form.delete_intent.as_deref().unwrap_or_default(),
    )
    .await?
    {
        return delete_account_final_response(
            &state,
            &user,
            &headers,
            StatusCode::BAD_REQUEST,
            "Delete confirmation expired. Start the delete account flow again.",
        )
        .await;
    }
    match account::delete_account(&state.pool, &state.paths, user.id, &form.password).await {
        Ok(_summary) => {
            let mut response = Redirect::to("/account-deleted").into_response();
            response.headers_mut().insert(
                header::SET_COOKIE,
                HeaderValue::from_str(&auth::clear_session_cookie(
                    state.settings.server.cookie_secure,
                ))
                .map_err(|err| AppError::BadRequest(err.to_string()))?,
            );
            Ok(response)
        }
        Err(account::DeleteAccountError::WrongPassword) => {
            delete_account_final_response(
                &state,
                &user,
                &headers,
                StatusCode::UNAUTHORIZED,
                "Password is incorrect.",
            )
            .await
        }
        Err(err) => {
            tracing::warn!(user_id = user.id, error = %err, "account deletion failed");
            delete_account_final_response(
                &state,
                &user,
                &headers,
                StatusCode::BAD_REQUEST,
                "Account deletion could not be completed. Review uploaded media paths and try again.",
            )
            .await
        }
    }
}

async fn delete_account_final_response(
    state: &AppState,
    user: &CurrentUser,
    headers: &HeaderMap,
    status: StatusCode,
    message: &str,
) -> AppResult<Response> {
    let csrf = form_csrf(state, headers).await.unwrap_or_default();
    let delete_intent = create_delete_account_intent(state, headers, user.id).await?;
    let body = render_delete_account_final_warning(&csrf, &delete_intent, Some(message));
    Ok((
        status,
        Html(
            page_layout(
                state,
                Some(user),
                Some(&csrf),
                "Confirm delete account",
                &body,
            )
            .await?,
        ),
    )
        .into_response())
}

async fn create_delete_account_intent(
    state: &AppState,
    headers: &HeaderMap,
    user_id: i64,
) -> AppResult<String> {
    let token = auth::session_cookie(headers).ok_or(AppError::Forbidden)?;
    let token_hash = auth::hash_token(&token);
    let delete_intent = auth::secure_token();
    let delete_intent_hash = auth::hash_token(&delete_intent);
    let updated = state
        .pool
        .call(move |conn| {
            Ok(conn.execute(
                r#"
                UPDATE sessions
                SET delete_account_token_hash = ?,
                    delete_account_token_expires_at = datetime('now', '+10 minutes')
                WHERE token_hash = ?
                  AND user_id = ?
                  AND revoked_at IS NULL
                  AND expires_at > CURRENT_TIMESTAMP
                "#,
                params![delete_intent_hash, token_hash, user_id],
            )?)
        })
        .await
        .map_err(AppError::Anyhow)?;
    if updated != 1 {
        return Err(AppError::Forbidden);
    }
    Ok(delete_intent)
}

async fn consume_delete_account_intent(
    state: &AppState,
    headers: &HeaderMap,
    user_id: i64,
    delete_intent: &str,
) -> AppResult<bool> {
    let Some(token) = auth::session_cookie(headers) else {
        return Ok(false);
    };
    if delete_intent.trim().is_empty() {
        return Ok(false);
    }
    let token_hash = auth::hash_token(&token);
    let delete_intent_hash = auth::hash_token(delete_intent);
    state
        .pool
        .call(move |conn| {
            Ok(conn.execute(
                r#"
                UPDATE sessions
                SET delete_account_token_hash = NULL,
                    delete_account_token_expires_at = NULL
                WHERE token_hash = ?
                  AND user_id = ?
                  AND revoked_at IS NULL
                  AND expires_at > CURRENT_TIMESTAMP
                  AND delete_account_token_hash = ?
                  AND delete_account_token_expires_at > CURRENT_TIMESTAMP
                "#,
                params![token_hash, user_id, delete_intent_hash],
            )? == 1)
        })
        .await
        .map_err(AppError::Anyhow)
}

async fn account_deleted(State(state): State<Arc<AppState>>) -> AppResult<Html<String>> {
    let body = r#"<section class="panel"><h1>Account deleted</h1><p>Your account and its owned content have been removed.</p><p><a class="button-link" href="/login">Log in</a></p></section>"#;
    Ok(Html(
        page_layout(&state, None, None, "Account deleted", body).await?,
    ))
}

fn render_delete_account_warning() -> String {
    r#"<section class="panel danger-panel delete-account-panel"><h1>Delete account</h1><p>This is permanent. RustPost will remove your profile, posts, reposts, likes, follows, blocks, mutes, bookmarks, sessions, and uploaded media owned by your account.</p><div class="actions"><form method="get" action="/settings/delete/confirm"><button class="danger" type="submit">Confirm delete account</button></form><a class="button-link" href="/settings">Cancel</a></div></section>"#
        .to_owned()
}

fn render_delete_account_final_warning(
    csrf: &str,
    delete_intent: &str,
    error: Option<&str>,
) -> String {
    let notice = error.map_or_else(String::new, |message| render::notice("error", message));
    format!(
        r#"{notice}<section class="panel danger-panel delete-account-panel"><h1>Final warning</h1><p>Deleting your account cannot be undone. Enter your password to permanently delete this account.</p><form method="post" action="/settings/delete/confirm" class="settings-password-form"><input type="hidden" name="csrf" value="{}"><input type="hidden" name="delete_intent" value="{}"><label for="delete_password">Password</label><div class="password-control"><input id="delete_password" name="password" type="password" autocomplete="current-password"><button type="button" class="password-toggle" data-password-toggle="delete_password" aria-label="Show password">Show</button></div><div class="actions"><button class="danger" type="submit">Delete account permanently</button><a class="button-link" href="/settings">Cancel</a></div></form></section>"#,
        html_escape::encode_double_quoted_attribute(csrf),
        html_escape::encode_double_quoted_attribute(delete_intent)
    )
}

// Multipart parsing is kept in one place so uploaded profile media and text
// fields share one validation path before any database updates happen.
#[expect(
    clippy::too_many_lines,
    reason = "multipart profile parsing keeps validation and file handling in one transaction-sized flow"
)]
async fn parse_profile_update(
    state: &AppState,
    user_id: i64,
    mut multipart: Multipart,
) -> AppResult<ParsedProfileUpdate> {
    let mut form = ParsedProfileUpdate {
        csrf_token: String::new(),
        display_name: String::new(),
        bio: String::new(),
        location: String::new(),
        website: String::new(),
        theme: Theme::Light,
        delete_profile_picture: false,
        delete_banner: false,
        nsfw_blur_enabled: false,
        profile_picture_media_id: None,
        banner_media_id: None,
    };
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
                form.csrf_token = field
                    .text()
                    .await
                    .map_err(|err| AppError::BadRequest(err.to_string()))?;
            }
            "display_name" => {
                form.display_name = field
                    .text()
                    .await
                    .map_err(|err| AppError::BadRequest(err.to_string()))?;
            }
            "bio" => {
                form.bio = field
                    .text()
                    .await
                    .map_err(|err| AppError::BadRequest(err.to_string()))?;
            }
            "location" => {
                form.location = field
                    .text()
                    .await
                    .map_err(|err| AppError::BadRequest(err.to_string()))?;
            }
            "website" => {
                form.website = field
                    .text()
                    .await
                    .map_err(|err| AppError::BadRequest(err.to_string()))?;
            }
            "dark_mode" => {
                form.theme = Theme::Dark;
            }
            "nsfw_blur_enabled" => {
                form.nsfw_blur_enabled = true;
            }
            "delete_profile_picture" => {
                form.delete_profile_picture = true;
            }
            "delete_banner" => {
                form.delete_banner = true;
            }
            "profile_picture" if field.file_name().is_some() => {
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
                    .map_err(|err| {
                        tracing::warn!(error = %err, "profile picture upload rejected");
                        AppError::BadRequest(err.to_string())
                    })?,
                );
            }
            "banner" if field.file_name().is_some() => {
                if !state.settings.accounts.allow_profile_banners {
                    return Err(AppError::Forbidden);
                }
                if field.file_name().is_none_or(|name| name.trim().is_empty()) {
                    continue;
                }
                form.banner_media_id = Some(
                    media::save_upload(
                        &state.pool,
                        &state.settings,
                        &state.paths,
                        &state.ffmpeg,
                        Some(user_id),
                        field,
                    )
                    .await
                    .map_err(|err| {
                        tracing::warn!(error = %err, "profile banner upload rejected");
                        AppError::BadRequest(err.to_string())
                    })?,
                );
            }
            _ => {}
        }
    }
    Ok(form)
}

async fn ensure_parent_post_exists(pool: &SqlitePool, parent_id: i64) -> AppResult<()> {
    let exists = pool
        .call(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT 1 FROM posts WHERE id = ? AND is_deleted = 0",
                    [parent_id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some())
        })
        .await?;
    if exists {
        Ok(())
    } else {
        Err(AppError::BadRequest(
            "reply target was not found; it may have been deleted".to_owned(),
        ))
    }
}

async fn delete_preview(
    pool: &SqlitePool,
    actor_id: i64,
    is_admin: bool,
    post_id: i64,
) -> AppResult<DeletePreview> {
    let preview = pool
        .call(move |conn| {
            conn.query_row(
                r#"
                SELECT p.user_id, p.text, u.username, u.display_name, p.parent_post_id
                FROM posts p
                LEFT JOIN users u ON u.id = p.user_id
                WHERE p.id = ? AND p.is_deleted = 0
                "#,
                [post_id],
                |row| {
                    Ok((
                        row.get::<_, Option<i64>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<i64>>(4)?,
                    ))
                },
            )
            .optional()
            .map_err(Into::into)
        })
        .await?;
    let Some((owner, text, username, display_name, parent_post_id)) = preview else {
        return Err(AppError::NotFound);
    };
    if !is_admin && owner != Some(actor_id) {
        return Err(AppError::Forbidden);
    }
    Ok(DeletePreview {
        text,
        username,
        display_name,
        parent_post_id,
    })
}

async fn post_is_reply(pool: &SqlitePool, post_id: i64) -> AppResult<bool> {
    Ok(pool
        .call(move |conn| {
            conn.query_row(
                "SELECT parent_post_id IS NOT NULL FROM posts WHERE id = ? AND is_deleted = 0",
                [post_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map(|value| value.unwrap_or(0) != 0)
            .map_err(Into::into)
        })
        .await?)
}

fn redirect_to_post_anchor(headers: &HeaderMap, post_id: i64, is_reply: bool) -> Redirect {
    let target = anchored_return(headers, post_id, is_reply, "/home");
    Redirect::to(&target)
}

fn enhanced_request(headers: &HeaderMap) -> bool {
    headers
        .get("x-rustpost-enhance")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == "1")
}

async fn follow_action_response(
    pool: &SqlitePool,
    viewer_id: i64,
    profile_id: i64,
) -> AppResult<FollowActionResponse> {
    let following = social::is_following(pool, viewer_id, profile_id).await?;
    let (followers, following_count) = social::follow_counts(pool, profile_id).await?;
    Ok(FollowActionResponse {
        kind: "follow",
        user_id: profile_id,
        following,
        followers,
        following_count,
        action: if following {
            format!("/users/{profile_id}/unfollow")
        } else {
            format!("/users/{profile_id}/follow")
        },
    })
}

async fn post_action_response(
    pool: &SqlitePool,
    viewer_id: i64,
    post_id: i64,
) -> AppResult<PostActionResponse> {
    let state = pool
        .call(move |conn| {
            conn.query_row(
                r#"
                SELECT
                  (SELECT COUNT(*) FROM likes WHERE post_id = p.id),
                  ((SELECT COUNT(*) FROM reposts WHERE post_id = p.id) +
                   (SELECT COUNT(*) FROM posts qp WHERE qp.quote_post_id = p.id AND qp.is_deleted = 0)),
                  (SELECT COUNT(*) FROM posts replies WHERE replies.parent_post_id = p.id AND replies.is_deleted = 0),
                  EXISTS(SELECT 1 FROM likes WHERE user_id = ? AND post_id = p.id),
                  EXISTS(SELECT 1 FROM bookmarks WHERE user_id = ? AND post_id = p.id),
                  EXISTS(SELECT 1 FROM reposts WHERE user_id = ? AND post_id = p.id)
                FROM posts p
                WHERE p.id = ? AND p.is_deleted = 0
                "#,
                params![viewer_id, viewer_id, viewer_id, post_id],
                |row| {
                    Ok(PostActionResponse {
                        kind: "post-action",
                        post_id,
                        likes: row.get(0)?,
                        reposts: row.get(1)?,
                        replies: row.get(2)?,
                        liked: row.get::<_, i64>(3)? != 0,
                        bookmarked: row.get::<_, i64>(4)? != 0,
                        reposted: row.get::<_, i64>(5)? != 0,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
        })
        .await?;
    state.ok_or(AppError::NotFound)
}

async fn account_action_return(
    pool: &SqlitePool,
    headers: &HeaderMap,
    profile_id: i64,
) -> AppResult<String> {
    if let Some(target) = referer_target(headers)
        .as_deref()
        .and_then(safe_return_target)
    {
        return Ok(target);
    }
    Ok(user_profile_path(pool, profile_id)
        .await?
        .unwrap_or_else(|| "/home".to_owned()))
}

async fn user_profile_path(pool: &SqlitePool, user_id: i64) -> AppResult<Option<String>> {
    Ok(pool
        .call(move |conn| {
            conn.query_row(
                "SELECT username FROM users WHERE id = ? AND is_deleted = 0",
                [user_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(Into::into)
        })
        .await?
        .map(|username| format!("/users/{username}")))
}

fn anchored_return(headers: &HeaderMap, post_id: i64, is_reply: bool, fallback: &str) -> String {
    let anchor = if is_reply {
        format!("reply-{post_id}")
    } else {
        format!("post-{post_id}")
    };
    let base = referer_target(headers)
        .as_deref()
        .and_then(safe_return_target)
        .unwrap_or_else(|| fallback.to_owned());
    let base = base.split('#').next().unwrap_or(fallback);
    format!("{base}#{anchor}")
}

fn delete_return_fallback(
    headers: &HeaderMap,
    post_id: i64,
    parent_post_id: Option<i64>,
) -> String {
    let fallback = anchored_return(headers, post_id, parent_post_id.is_some(), "/home");
    if safe_delete_return_target(&fallback, post_id).is_some() {
        return fallback;
    }
    parent_post_id.map_or_else(
        || format!("/home#post-{post_id}"),
        |parent_id| format!("/posts/{parent_id}#post-{parent_id}"),
    )
}

fn safe_delete_return_target(value: &str, post_id: i64) -> Option<String> {
    let target = safe_return_target(value)?;
    let self_path = format!("/posts/{post_id}/delete");
    let thread_path = format!("/posts/{post_id}");
    let path = path_without_query_or_fragment(&target);
    if path == self_path || path == thread_path {
        None
    } else {
        Some(target)
    }
}

fn path_without_query_or_fragment(target: &str) -> &str {
    target.split(['?', '#']).next().unwrap_or_default()
}

fn safe_return_target(value: &str) -> Option<String> {
    let target = if value.starts_with('/') && !value.starts_with("//") {
        value.to_owned()
    } else {
        value.parse::<Uri>().ok().and_then(|uri| {
            uri.path_and_query()
                .map(|path| path.as_str().to_owned())
                .filter(|path| path.starts_with('/'))
        })?
    };
    let path = target.split('#').next().unwrap_or_default();
    if matches!(
        path,
        "/home" | "/bookmarks" | "/notifications" | "/search" | "/"
    ) || path.starts_with("/posts/")
        || path.starts_with("/users/")
        || path.starts_with("/tags/")
    {
        Some(target)
    } else {
        None
    }
}

fn referer_target(headers: &HeaderMap) -> Option<String> {
    let value = headers
        .get(header::REFERER)
        .and_then(|value| value.to_str().ok())?;
    if value.starts_with('/') && !value.starts_with("//") {
        Some(value.to_owned())
    } else {
        let uri = value.parse::<Uri>().ok()?;
        uri.path_and_query()
            .map(|path| path.as_str().to_owned())
            .filter(|path| path.starts_with('/'))
    }
}

async fn bookmarks(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AppResult<Html<String>> {
    let user = require_user(&state, &headers).await?;
    let posts = social::timeline(&state.pool, Some(user.id), "bookmarks", None).await?;
    let csrf = form_csrf(&state, &headers).await;
    let body = format!(
        "{}{}",
        render::page_header("Bookmarks", "Posts you saved for later."),
        render::posts_with_nsfw_blur(
            &posts,
            Some(&user),
            csrf.as_deref(),
            blur_nsfw_media(&state, Some(&user)),
        )
    );
    Ok(Html(
        page_layout(&state, Some(&user), csrf.as_deref(), "Bookmarks", &body).await?,
    ))
}

async fn following(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AppResult<Html<String>> {
    let user = require_user(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers).await.unwrap_or_default();
    let accounts = social::following_accounts(&state.pool, user.id).await?;
    let body = format!(
        "{}{}",
        render::page_header("Following", "Accounts you follow."),
        render::accounts(&accounts, &csrf)
    );
    Ok(Html(
        page_layout(&state, Some(&user), Some(&csrf), "Following", &body).await?,
    ))
}

async fn notifications(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AppResult<Html<String>> {
    let user = require_user(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers).await.unwrap_or_default();
    let items = social::notifications(&state.pool, user.id).await?;
    let unread_count = social::unread_notification_count(&state.pool, user.id).await?;
    let body = render::notifications_page(&items, unread_count, &csrf);
    Ok(Html(
        page_layout(&state, Some(&user), Some(&csrf), "Notifications", &body).await?,
    ))
}

async fn mark_notifications_read(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> AppResult<Response> {
    let user = require_user(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    social::mark_notifications_read(&state.pool, user.id).await?;
    Ok(Redirect::to("/notifications").into_response())
}

async fn search(
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
    let csrf = form_csrf(&state, &headers).await;
    let body = render::search_page(
        &state.settings.site.name,
        &q,
        &users,
        &posts,
        user.as_ref(),
        csrf.as_deref(),
        blur_nsfw_media(&state, user.as_ref()),
    );
    Ok(Html(
        page_layout(&state, user.as_ref(), csrf.as_deref(), "Search", &body).await?,
    ))
}

fn normalize_search_query(query: &str) -> String {
    query.split_whitespace().collect::<Vec<_>>().join(" ")
}

async fn tag(
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

async fn admin_dashboard(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AppResult<Html<String>> {
    let user = require_admin(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers).await.unwrap_or_default();
    let favicon_asset = favicon::current(&state.paths);
    let remove_form = if favicon_asset.is_custom() {
        small_form(
            "/admin/favicon/remove",
            &csrf,
            "Remove favicon",
            "Reset to the built-in favicon",
        )
    } else {
        String::new()
    };
    let favicon_panel = format!(
        r#"<section class="panel admin-card" data-testid="admin-card"><h2>Favicon</h2><p class="muted">{}</p><p><img class="favicon-preview" src="/favicon.ico" alt="Current favicon"></p><form method="post" action="/admin/favicon" enctype="multipart/form-data"><input type="hidden" name="csrf" value="{}"><label for="favicon">Upload favicon</label><input id="favicon" name="favicon" type="file" accept=".ico,image/png,image/svg+xml"><p class="muted">Accepted: .ico, .png, .svg. Maximum size: 256 KiB.</p><button type="submit">Save favicon</button></form><div class="actions">{}</div></section>"#,
        html_escape::encode_text(favicon_asset.state_label()),
        html_escape::encode_double_quoted_attribute(&csrf),
        remove_form
    );
    let body = format!(
        "{}{}{}",
        render::page_header(
            "Admin",
            "Manage site health, users, media jobs, settings, and backups."
        ),
        r#"<section class="grid"><a class="panel admin-card" data-testid="admin-card" href="/admin/health">Site health</a><a class="panel admin-card" data-testid="admin-card" href="/admin/users">Users</a><a class="panel admin-card" data-testid="admin-card" href="/admin/media">Media jobs</a><a class="panel admin-card" data-testid="admin-card" href="/admin/deep-settings">Deep server settings</a><a class="panel admin-card" data-testid="admin-card" href="/admin/backups">Backups</a></section>"#,
        favicon_panel
    );
    Ok(Html(
        page_layout(&state, Some(&user), Some(&csrf), "Admin", &body).await?,
    ))
}

async fn admin_favicon_upload(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    multipart: Multipart,
) -> AppResult<Response> {
    require_admin(&state, &headers).await?;
    let parsed = parse_favicon_upload(&state, &headers, multipart).await?;
    if !parsed.uploaded {
        return Err(AppError::BadRequest(
            "choose a .ico, .png, or .svg favicon to upload".to_owned(),
        ));
    }
    Ok(Redirect::to("/admin").into_response())
}

async fn admin_favicon_remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> AppResult<Response> {
    require_admin(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    favicon::reset(&state.paths).await?;
    Ok(Redirect::to("/admin").into_response())
}

async fn parse_favicon_upload(
    state: &AppState,
    headers: &HeaderMap,
    mut multipart: Multipart,
) -> AppResult<ParsedFaviconUpload> {
    let mut parsed = ParsedFaviconUpload { uploaded: false };
    let mut csrf_validated = false;
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
                let token = field
                    .text()
                    .await
                    .map_err(|err| AppError::BadRequest(err.to_string()))?;
                validate_csrf(&state.pool, headers, &token).await?;
                csrf_validated = true;
            }
            "favicon" if field.file_name().is_some() => {
                if field.file_name().is_none_or(|name| name.trim().is_empty()) {
                    continue;
                }
                if !csrf_validated {
                    return Err(AppError::Forbidden);
                }
                favicon::save_upload(&state.paths, field)
                    .await
                    .map_err(|err| {
                        tracing::warn!(error = %err, "favicon upload rejected");
                        AppError::BadRequest(err.to_string())
                    })?;
                parsed.uploaded = true;
            }
            _ => {}
        }
    }
    if !csrf_validated {
        return Err(AppError::Forbidden);
    }
    Ok(parsed)
}

async fn admin_health(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AppResult<Html<String>> {
    let user = require_admin(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers).await.unwrap_or_default();
    let media_jobs = admin::media_jobs_report(&state.pool).await?;
    let jobs = render_media_jobs_report(&media_jobs);
    let onion = state
        .tor
        .onion_address()
        .unwrap_or_else(|| "unavailable".to_owned());
    let tor_error = state.tor.error().unwrap_or_else(|| "none".to_owned());
    let bootstrap = state
        .tor
        .bootstrap_status()
        .unwrap_or_else(|| "unavailable".to_owned());
    let body = format!(
        r#"<section class="panel admin-card" data-testid="admin-card"><h1>Site health</h1><dl><dt>DB path</dt><dd>{}</dd><dt>Upload path</dt><dd>{}</dd><dt>Media path</dt><dd>{}</dd><dt>Logs path</dt><dd>{}</dd><dt>Backup path</dt><dd>{}</dd><dt>ffmpeg</dt><dd>{}</dd><dt>WebP support</dt><dd>{}</dd><dt>VP9 support</dt><dd>{}</dd><dt>Tor</dt><dd>{}</dd><dt>Tor enabled</dt><dd>{}</dd><dt>Tor running</dt><dd>{}</dd><dt>Tor bootstrap</dt><dd>{}</dd><dt>Tor error</dt><dd>{}</dd><dt>Onion address</dt><dd>{}</dd><dt>Anonymous mode</dt><dd>{}</dd><dt>Registration</dt><dd>{}</dd></dl><h2>Recent media jobs</h2>{}</section>"#,
        html_escape::encode_text(&state.paths.database_path.display().to_string()),
        html_escape::encode_text(&state.paths.uploads_originals.display().to_string()),
        html_escape::encode_text(&state.paths.uploads_images.display().to_string()),
        html_escape::encode_text(&state.paths.logs_dir.display().to_string()),
        html_escape::encode_text(&state.paths.backups_dir.display().to_string()),
        html_escape::encode_text(&state.ffmpeg.summary()),
        state.ffmpeg.supports_webp,
        state.ffmpeg.supports_vp9,
        html_escape::encode_text(&state.tor.summary()),
        state.tor.enabled(),
        state.tor.running(),
        html_escape::encode_text(&bootstrap),
        html_escape::encode_text(&tor_error),
        html_escape::encode_text(&onion),
        state.settings.accounts.anonymous_mode_enabled,
        state.settings.accounts.registration_enabled,
        jobs
    );
    Ok(Html(
        page_layout(&state, Some(&user), Some(&csrf), "Site health", &body).await?,
    ))
}

async fn admin_users(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<AdminUsersQuery>,
) -> AppResult<Html<String>> {
    let user = require_admin(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers).await.unwrap_or_default();
    let user_query = query.user_q.unwrap_or_default();
    let post_query = query.post_q.unwrap_or_default();
    let search = admin::AdminUserSearch::new(&user_query, &post_query);
    let malformed_quotes = search.post_search.malformed_quotes;
    let has_filter = search.has_filter();
    let rows = admin::users(&state.pool, search).await?;
    let quote_notice = if malformed_quotes {
        r#"<p class="notice error">Post search had unmatched quotes, so it was treated as plain keyword search.</p>"#
    } else {
        ""
    };
    let list = admin_user_rows(&rows, &csrf, has_filter);
    let body = format!(
        r#"<section class="panel admin-card admin-users-panel" data-testid="admin-card"><h1>Users</h1><form method="get" action="/admin/users" class="admin-user-search"><div><label for="admin-user-q">Username, display name, or handle</label><input id="admin-user-q" name="user_q" type="search" value="{}" autocomplete="off" placeholder="alice or @alice"></div><div><label for="admin-post-q">Post keywords</label><input id="admin-post-q" name="post_q" type="search" value="{}" autocomplete="off" placeholder="keyword or &quot;exact phrase&quot;"></div><div class="admin-user-search-actions"><button type="submit">Search</button><a class="button-link" href="/admin/users">Reset</a></div></form>{}{}</section>"#,
        html_escape::encode_double_quoted_attribute(&user_query),
        html_escape::encode_double_quoted_attribute(&post_query),
        quote_notice,
        list
    );
    Ok(Html(
        page_layout(&state, Some(&user), Some(&csrf), "Admin users", &body).await?,
    ))
}

fn admin_user_rows(rows: &[admin::AdminUserInvestigation], csrf: &str, searched: bool) -> String {
    if rows.is_empty() {
        let message = if searched {
            "No users matched those filters."
        } else {
            "No users found."
        };
        return format!(
            r#"<div class="compact-empty admin-users-empty"><strong>{}</strong><p>Try a different username, handle, display name, or post keyword.</p></div>"#,
            html_escape::encode_text(message)
        );
    }

    rows.iter()
        .map(|row| admin_user_row(row, csrf, searched))
        .collect::<Vec<_>>()
        .join("")
}

fn admin_user_row(row: &admin::AdminUserInvestigation, csrf: &str, searched: bool) -> String {
    let display_name = if row.display_name.trim().is_empty() {
        row.username.as_str()
    } else {
        row.display_name.as_str()
    };
    let statuses = [
        if row.is_admin { "Admin" } else { "Member" },
        if row.is_suspended {
            "Suspended"
        } else {
            "Active"
        },
        if row.is_deleted {
            "Deleted"
        } else {
            "Not deleted"
        },
    ]
    .iter()
    .map(|status| {
        format!(
            r#"<span class="admin-user-pill">{}</span>"#,
            html_escape::encode_text(status)
        )
    })
    .collect::<Vec<_>>()
    .join("");
    let match_labels = admin_user_match_labels(row, searched);
    let preview = row
        .post_match_preview
        .as_ref()
        .map_or_else(String::new, |text| {
            format!(
                r#"<p class="admin-post-preview"><strong>Post match preview:</strong> {}</p>"#,
                html_escape::encode_text(&short_preview(text))
            )
        });
    let action = small_form(
        &format!("/admin/users/{}/suspend", row.id),
        csrf,
        if row.is_suspended {
            "Unsuspend"
        } else {
            "Suspend"
        },
        if row.is_suspended {
            "Unsuspend this account"
        } else {
            "Suspend this account"
        },
    );
    format!(
        r#"<article class="admin-user-row"><div class="admin-user-main"><div class="admin-user-heading"><a class="author-name" href="/users/{}">{}</a> <span class="username">@{}</span> <span class="muted">#{}</span></div><div class="admin-user-statuses">{}</div>{}<dl class="admin-user-meta"><dt>Created</dt><dd>{}</dd><dt>Updated</dt><dd>{}</dd><dt>Last session</dt><dd>{}</dd><dt>Last post</dt><dd>{}</dd><dt>Total posts</dt><dd>{}</dd><dt>Uploaded media</dt><dd>{}</dd><dt>Reports on posts</dt><dd>{}</dd><dt>Moderation actions</dt><dd>{}</dd><dt>Matching posts</dt><dd>{}</dd></dl>{}</div><div class="admin-user-actions">{}</div></article>"#,
        html_escape::encode_double_quoted_attribute(&row.username),
        html_escape::encode_text(display_name),
        html_escape::encode_text(&row.username),
        row.id,
        statuses,
        match_labels,
        html_escape::encode_text(&row.created_at),
        html_escape::encode_text(&row.updated_at),
        html_escape::encode_text(row.last_session_at.as_deref().unwrap_or("No session")),
        html_escape::encode_text(row.last_post_at.as_deref().unwrap_or("No posts")),
        row.total_posts,
        row.uploaded_media_count,
        row.reports_on_posts_count,
        row.moderation_action_count,
        row.matching_post_count,
        preview,
        action
    )
}

fn admin_user_match_labels(row: &admin::AdminUserInvestigation, searched: bool) -> String {
    if !searched {
        return String::new();
    }

    let mut labels = Vec::new();
    if row.matched_name {
        labels.push("Matched name".to_owned());
    }
    if row.matching_post_count > 0 {
        labels.push(format!("Matched post content: {}", row.matching_post_count));
    }
    if labels.is_empty() {
        return String::new();
    }
    let labels = labels
        .iter()
        .map(|label| {
            format!(
                r#"<span class="admin-user-match">{}</span>"#,
                html_escape::encode_text(label)
            )
        })
        .collect::<Vec<_>>()
        .join("");
    format!(r#"<div class="admin-user-matches">{labels}</div>"#)
}

fn short_preview(text: &str) -> String {
    const LIMIT: usize = 140;
    let trimmed = text.trim();
    let mut preview = trimmed.chars().take(LIMIT).collect::<String>();
    if trimmed.chars().count() > LIMIT {
        preview.push_str("...");
    }
    preview
}

async fn admin_suspend(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> AppResult<Response> {
    let user = require_admin(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    let current: i64 = state
        .pool
        .call(move |conn| {
            Ok(
                conn.query_row("SELECT is_suspended FROM users WHERE id = ?", [id], |row| {
                    row.get(0)
                })?,
            )
        })
        .await?;
    admin::set_user_suspended(&state.pool, user.id, id, current == 0).await?;
    Ok(Redirect::to("/admin/users").into_response())
}

async fn admin_delete_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> AppResult<Response> {
    let user = require_admin(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    social::delete_post(&state.pool, user.id, id, true).await?;
    admin::audit(&state.pool, user.id, "delete_post", &format!("post:{id}")).await?;
    Ok(Redirect::to("/admin").into_response())
}

async fn admin_toggle_post_nsfw(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<AdminNsfwForm>,
) -> AppResult<Response> {
    let user = require_admin(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    let is_nsfw = match form.nsfw.as_str() {
        "true" => true,
        "false" => false,
        _ => return Err(AppError::BadRequest("invalid NSFW setting".to_owned())),
    };
    let changed = social::set_post_media_nsfw(&state.pool, id, is_nsfw)
        .await
        .map_err(|err| AppError::BadRequest(err.to_string()))?;
    if changed == 0 {
        return Err(AppError::BadRequest("post has no media".to_owned()));
    }
    admin::audit(
        &state.pool,
        user.id,
        if is_nsfw {
            "mark_post_nsfw"
        } else {
            "unmark_post_nsfw"
        },
        &format!("post:{id}"),
    )
    .await?;
    Ok(redirect_to_post_anchor(&headers, id, false).into_response())
}

async fn admin_media(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AppResult<Html<String>> {
    let user = require_admin(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers).await.unwrap_or_default();
    let jobs = admin::media_jobs_report(&state.pool).await?;
    let body = format!(
        r#"<section class="panel admin-card" data-testid="admin-card"><h1>Media jobs</h1>{}</section>"#,
        render_media_jobs_report(&jobs)
    );
    Ok(Html(
        page_layout(&state, Some(&user), Some(&csrf), "Media jobs", &body).await?,
    ))
}

async fn admin_deep_settings(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<DeepSettingsQuery>,
) -> AppResult<Html<String>> {
    let user = require_admin(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers).await.unwrap_or_default();
    let current = load_deep_settings(&state)?;
    let values = admin::DeepSettingsValues::from_settings(&current);
    let notice = if query.saved.is_some() {
        Some(("success", "Settings saved successfully"))
    } else if query.discarded.is_some() {
        Some(("info", "Changes discarded."))
    } else {
        None
    };
    let body = render_deep_settings_form(&csrf, &values, notice);
    deep_settings_html(&state, &user, &csrf, &body).await
}

async fn admin_deep_settings_update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<admin::DeepSettingsForm>,
) -> AppResult<Html<String>> {
    let user = require_admin(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    let csrf = form_csrf(&state, &headers).await.unwrap_or_default();
    let current = load_deep_settings(&state)?;
    if form.intent.as_deref() == Some("discard") {
        let values = admin::DeepSettingsValues::from_settings(&current);
        let body = render_deep_settings_form(&csrf, &values, Some(("info", "Changes discarded.")));
        return deep_settings_html(&state, &user, &csrf, &body).await;
    }

    let values = match admin::parse_deep_settings_form(&form, &current) {
        Ok(values) => values,
        Err(err) => {
            let fallback = admin::DeepSettingsValues::from_settings(&current);
            let body =
                render_deep_settings_form(&csrf, &fallback, Some(("error", &err.to_string())));
            return deep_settings_html(&state, &user, &csrf, &body).await;
        }
    };
    let changes = admin::diff_deep_settings(&current, &values);
    if changes.is_empty() {
        let body =
            render_deep_settings_form(&csrf, &values, Some(("info", "No settings changed.")));
        return deep_settings_html(&state, &user, &csrf, &body).await;
    }

    if form.intent.as_deref() == Some("confirm") {
        let updated = values.apply_to(&current);
        if let Err(err) = admin::write_deep_settings(&state.paths.settings_path, &updated) {
            tracing::error!(error = %err, "failed to save deep server settings");
            let body = render_deep_settings_confirmation(
                &csrf,
                &values,
                &changes,
                Some((
                    "error",
                    "Settings could not be saved. Check the server logs for details.",
                )),
            );
            return deep_settings_html(&state, &user, &csrf, &body).await;
        }
        admin::audit(
            &state.pool,
            user.id,
            "update_deep_settings",
            "settings.toml",
        )
        .await?;
        let saved = load_deep_settings(&state).unwrap_or(updated);
        let values = admin::DeepSettingsValues::from_settings(&saved);
        let body = render_deep_settings_form(
            &csrf,
            &values,
            Some(("success", "Settings saved successfully")),
        );
        return deep_settings_html(&state, &user, &csrf, &body).await;
    }

    let body = render_deep_settings_confirmation(&csrf, &values, &changes, None);
    deep_settings_html(&state, &user, &csrf, &body).await
}

async fn deep_settings_html(
    state: &AppState,
    user: &CurrentUser,
    csrf: &str,
    body: &str,
) -> AppResult<Html<String>> {
    Ok(Html(
        page_layout(state, Some(user), Some(csrf), "Deep server settings", body).await?,
    ))
}

fn load_deep_settings(state: &AppState) -> AppResult<Settings> {
    let settings = Settings::load(&state.paths.settings_path)?;
    settings.validate()?;
    Ok(settings)
}

fn render_deep_settings_form(
    csrf: &str,
    values: &admin::DeepSettingsValues,
    notice: Option<(&str, &str)>,
) -> String {
    let notice_html =
        notice.map_or_else(String::new, |(kind, message)| render::notice(kind, message));
    let fields = render_deep_settings_groups(values);
    format!(
        r#"{notice_html}<section class="panel admin-card deep-settings-panel" data-testid="admin-card"><div class="settings-editor-bar"><div><h1>Deep server settings</h1><p class="muted">Durable settings from settings.toml. Saved changes require a RustPost restart before this running server uses them.</p></div><button class="primary" type="submit" form="deep-settings-form">Save</button></div><form id="deep-settings-form" method="post" action="/admin/deep-settings" class="deep-settings-form"><input type="hidden" name="csrf" value="{}">{fields}<input type="hidden" name="intent" value="preview"></form></section>"#,
        html_escape::encode_double_quoted_attribute(csrf),
    )
}

fn render_deep_settings_groups(values: &admin::DeepSettingsValues) -> String {
    let mut body = String::new();
    let mut active_section = "";
    for field in admin::DeepSettingsField::ALL {
        if field.section() != active_section {
            if !active_section.is_empty() {
                body.push_str("</fieldset>");
            }
            active_section = field.section();
            let _ = write!(
                body,
                r#"<fieldset class="deep-settings-group"><legend>{}</legend>"#,
                html_escape::encode_text(active_section)
            );
        }
        body.push_str(&render_deep_settings_field(field, values));
    }
    if !active_section.is_empty() {
        body.push_str("</fieldset>");
    }
    body
}

fn render_deep_settings_field(
    field: admin::DeepSettingsField,
    values: &admin::DeepSettingsValues,
) -> String {
    let name = field.form_name();
    let id = format!("deep-{name}");
    let value = values.form_value(field);
    let control = match field.input_kind() {
        admin::DeepSettingsInputKind::Text => format!(
            r#"<input id="{id}" name="{name}" type="text" value="{}">"#,
            html_escape::encode_double_quoted_attribute(&value),
        ),
        admin::DeepSettingsInputKind::Number => format!(
            r#"<input id="{id}" name="{name}" type="text" inputmode="numeric" pattern="[0-9]+" value="{}">"#,
            html_escape::encode_double_quoted_attribute(&value),
        ),
        admin::DeepSettingsInputKind::Boolean => {
            let true_selected = if value == "true" { " selected" } else { "" };
            let false_selected = if value == "false" { " selected" } else { "" };
            format!(
                r#"<select id="{id}" name="{name}"><option value="true"{true_selected}>true</option><option value="false"{false_selected}>false</option></select>"#
            )
        }
    };
    let helper = field.helper().map_or_else(String::new, |helper| {
        format!(
            r#"<p class="muted field-help">{}</p>"#,
            html_escape::encode_text(helper)
        )
    });
    format!(
        r#"<div class="deep-settings-field"><label for="{id}">{}</label>{control}{helper}</div>"#,
        html_escape::encode_text(field.label()),
    )
}

fn render_deep_settings_confirmation(
    csrf: &str,
    values: &admin::DeepSettingsValues,
    changes: &[admin::DeepSettingsChange],
    notice: Option<(&str, &str)>,
) -> String {
    let notice_html =
        notice.map_or_else(String::new, |(kind, message)| render::notice(kind, message));
    let rows = changes
        .iter()
        .map(|change| {
            format!(
                r#"<li><strong>{}</strong><br><span>{} -&gt; {}</span></li>"#,
                html_escape::encode_text(change.label),
                html_escape::encode_text(&change.old_value),
                html_escape::encode_text(&change.new_value),
            )
        })
        .collect::<Vec<_>>()
        .join("");
    let hidden = render_deep_settings_hidden_fields(values);
    format!(
        r#"{notice_html}<section class="panel admin-card deep-settings-confirm" data-testid="admin-card"><h1>These settings are about to be changed</h1><p class="muted">Review the changed values before writing settings.toml. Saved changes require a RustPost restart before this running server uses them.</p><ul class="settings-item-list">{rows}</ul><div class="actions"><form method="post" action="/admin/deep-settings"><input type="hidden" name="csrf" value="{}"><input type="hidden" name="intent" value="confirm">{hidden}<button class="primary" type="submit">Confirm/Save</button></form><form method="post" action="/admin/deep-settings"><input type="hidden" name="csrf" value="{}"><input type="hidden" name="intent" value="discard">{hidden}<button type="submit">Discard Changes</button></form></div></section>"#,
        html_escape::encode_double_quoted_attribute(csrf),
        html_escape::encode_double_quoted_attribute(csrf),
    )
}

fn render_deep_settings_hidden_fields(values: &admin::DeepSettingsValues) -> String {
    admin::DeepSettingsField::ALL
        .iter()
        .copied()
        .fold(String::new(), |mut fields, field| {
            let _ = write!(
                fields,
                r#"<input type="hidden" name="{}" value="{}">"#,
                field.form_name(),
                html_escape::encode_double_quoted_attribute(&values.form_value(field))
            );
            fields
        })
}

fn render_media_jobs_report(report: &admin::MediaJobsReport) -> String {
    if report.total == 0 {
        return r#"<p class="muted">No media jobs yet.</p>"#.to_owned();
    }

    let mut out = String::new();
    let pending_age = match (
        report.newest_pending_age_seconds,
        report.oldest_pending_age_seconds,
    ) {
        (Some(newest), Some(oldest)) => {
            format!(
                "{} newest / {} oldest",
                format_age(newest),
                format_age(oldest)
            )
        }
        _ => "none".to_owned(),
    };
    let _ = write!(
        out,
        r#"<table><thead><tr><th>Total</th><th>Pending</th><th>Running</th><th>Succeeded</th><th>Failed</th><th>Pending age</th></tr></thead><tbody><tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr></tbody></table>"#,
        report.total,
        report.pending,
        report.running,
        report.succeeded,
        report.failed,
        html_escape::encode_text(&pending_age),
    );

    if report.recent_failures.is_empty() {
        if report.failed == 0 {
            out.push_str(r#"<p class="muted">No recent media job failures.</p>"#);
        }
        return out;
    }

    out.push_str(
        r#"<h2>Recent failures</h2><table><thead><tr><th>Job</th><th>Media</th><th>Kind</th><th>Age</th><th>Error</th></tr></thead><tbody>"#,
    );
    for failure in &report.recent_failures {
        let media = failure
            .media_path
            .as_deref()
            .map(|path| compact_text(path, 48))
            .or_else(|| failure.media_id.map(|id| format!("#{id}")))
            .unwrap_or_else(|| "unknown".to_owned());
        let kind = failure.job_kind.as_deref().unwrap_or("media");
        let age = failure
            .age_seconds
            .map_or_else(|| "unknown".to_owned(), format_age);
        let error = compact_text(&failure.error_summary, 80);
        let _ = write!(
            out,
            r#"<tr><td>#{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>"#,
            failure.id,
            html_escape::encode_text(&media),
            html_escape::encode_text(kind),
            html_escape::encode_text(&age),
            html_escape::encode_text(&error),
        );
    }
    out.push_str("</tbody></table>");
    out
}

fn compact_text(input: &str, max_chars: usize) -> String {
    let compact = input.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= max_chars {
        return compact;
    }

    let take = max_chars.saturating_sub(3);
    let mut shortened = compact.chars().take(take).collect::<String>();
    shortened.push_str("...");
    shortened
}

fn format_age(seconds: i64) -> String {
    let seconds = seconds.max(0);
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let hours = minutes / 60;
    if hours < 48 {
        return format!("{hours}h");
    }
    format!("{}d", hours / 24)
}

async fn admin_backups(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AppResult<Html<String>> {
    let user = require_admin(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers).await.unwrap_or_default();
    let body = format!(
        r#"<section class="panel admin-card" data-testid="admin-card"><h1>Backups</h1><form method="post"><input type="hidden" name="csrf" value="{}"><label><input type="checkbox" name="include_tor_keys" value="true"> Include Tor onion-service keys</label><button>Create backup</button></form></section>"#,
        html_escape::encode_double_quoted_attribute(&csrf)
    );
    Ok(Html(
        page_layout(&state, Some(&user), Some(&csrf), "Backups", &body).await?,
    ))
}

#[derive(Deserialize)]
struct BackupForm {
    csrf: String,
    include_tor_keys: Option<String>,
}

async fn admin_create_backup(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<BackupForm>,
) -> AppResult<Html<String>> {
    let user = require_admin(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    let include_tor = form.include_tor_keys.is_some();
    let archive = backup::create_backup(&state.paths, include_tor)?;
    admin::audit(
        &state.pool,
        user.id,
        "create_backup",
        archive.to_string_lossy().as_ref(),
    )
    .await?;
    let body = format!(
        r#"<section class="panel"><p>Backup created: {}</p></section>"#,
        html_escape::encode_text(&archive.display().to_string())
    );
    let csrf = form_csrf(&state, &headers).await.unwrap_or_default();
    Ok(Html(
        page_layout(&state, Some(&user), Some(&csrf), "Backup created", &body).await?,
    ))
}

fn small_form(action: &str, csrf: &str, label: &str, title: &str) -> String {
    format!(
        r#"<form method="post" action="{}"><input type="hidden" name="csrf" value="{}"><button type="submit" aria-label="{}" title="{}">{}</button></form>"#,
        html_escape::encode_double_quoted_attribute(action),
        html_escape::encode_double_quoted_attribute(csrf),
        html_escape::encode_double_quoted_attribute(title),
        html_escape::encode_double_quoted_attribute(title),
        html_escape::encode_text(label)
    )
}

async fn require_user(state: &AppState, headers: &HeaderMap) -> AppResult<CurrentUser> {
    current(state, headers).await?.ok_or(AppError::Unauthorized)
}

async fn require_active_user(state: &AppState, headers: &HeaderMap) -> AppResult<CurrentUser> {
    let user = require_user(state, headers).await?;
    if user.is_suspended {
        return Err(AppError::Forbidden);
    }
    Ok(user)
}

async fn require_admin(state: &AppState, headers: &HeaderMap) -> AppResult<CurrentUser> {
    let user = require_user(state, headers).await?;
    if !user.is_admin {
        return Err(AppError::Forbidden);
    }
    Ok(user)
}

async fn validate_csrf(pool: &SqlitePool, headers: &HeaderMap, token: &str) -> AppResult<()> {
    csrf::validate(pool, headers, token)
        .await
        .map_err(|_csrf_err| AppError::Forbidden)
}

async fn form_csrf(state: &AppState, headers: &HeaderMap) -> Option<String> {
    let token = auth::session_cookie(headers)?;
    let token_hash = auth::hash_token(&token);
    let (stored_hash, previous_hashes): (String, Option<String>) = state
        .pool
        .call({
            let token_hash = token_hash.clone();
            move |conn| {
                conn.query_row(
                    "SELECT csrf_token_hash, previous_csrf_token_hash FROM sessions WHERE token_hash = ? AND revoked_at IS NULL",
                    [token_hash],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(Into::into)
            }
        })
        .await
        .ok()??;
    let previous_hashes = csrf_history_with(&stored_hash, previous_hashes.as_deref());
    let plain = auth::secure_token();
    let new_hash = auth::hash_token(&plain);
    let updated = state
        .pool
        .call(move |conn| {
            let changed = conn.execute(
                "UPDATE sessions SET csrf_token_hash = ?, previous_csrf_token_hash = ? WHERE token_hash = ? AND csrf_token_hash = ?",
                params![new_hash, previous_hashes, token_hash, stored_hash],
            )?;
            Ok(changed == 1)
        })
        .await
        .ok()?;
    updated.then_some(plain)
}

fn csrf_history_with(current_hash: &str, previous_hashes: Option<&str>) -> String {
    std::iter::once(current_hash)
        .chain(
            previous_hashes
                .into_iter()
                .flat_map(str::lines)
                .filter(|hash| !hash.is_empty() && *hash != current_hash),
        )
        .take(CSRF_TOKEN_HISTORY_LIMIT.saturating_sub(1))
        .collect::<Vec<_>>()
        .join("\n")
}

async fn user_post_relation_exists(
    pool: &SqlitePool,
    table: &str,
    user_id: i64,
    post_id: i64,
) -> AppResult<bool> {
    let sql = format!("SELECT 1 FROM {table} WHERE user_id = ? AND post_id = ?");
    Ok(pool
        .call(move |conn| {
            Ok(conn
                .query_row(&sql, params![user_id, post_id], |_| Ok(()))
                .optional()?
                .is_some())
        })
        .await?)
}

fn ip_actor(addr: SocketAddr) -> String {
    format!("ip:{}", addr.ip())
}

fn user_actor(user_id: i64) -> String {
    format!("user:{user_id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::GzDecoder;
    use std::io::Read as _;
    use std::path::PathBuf;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    struct TestServer {
        base_url: String,
        data_dir: PathBuf,
        pool: SqlitePool,
        registration_captcha: RegistrationCaptchaStore,
        _task: tokio::task::JoinHandle<()>,
        _temp: tempfile::TempDir,
    }

    struct TestResponse {
        status: u16,
        headers: Vec<(String, String)>,
        body_bytes: Vec<u8>,
        body: String,
    }

    #[test]
    fn csrf_history_is_bounded_and_keeps_recent_hashes() {
        let prior = (1..CSRF_TOKEN_HISTORY_LIMIT + 3)
            .map(|idx| format!("token-{idx}"))
            .collect::<Vec<_>>()
            .join("\n");

        let history = csrf_history_with("current", Some(&prior));
        let hashes = history.lines().collect::<Vec<_>>();

        assert_eq!(hashes.len(), CSRF_TOKEN_HISTORY_LIMIT - 1);
        assert_eq!(hashes[0], "current");
        assert_eq!(hashes[1], "token-1");
        assert_eq!(hashes[CSRF_TOKEN_HISTORY_LIMIT - 2], "token-30");
    }

    #[test]
    fn compact_text_collapses_whitespace_and_truncates() {
        let compact = compact_text(
            "/uploads/originals/a/very/long/path.png\nffmpeg stderr repeated detail",
            24,
        );

        assert_eq!(compact, "/uploads/originals/a/...");
    }

    #[test]
    fn media_jobs_report_keeps_healthy_state_short() {
        let report = admin::MediaJobsReport {
            total: 3,
            succeeded: 3,
            ..admin::MediaJobsReport::default()
        };

        let output = render_media_jobs_report(&report);

        assert!(output.contains("<th>Total</th>"));
        assert!(output.contains("<td>3</td>"));
        assert!(output.contains("No recent media job failures."));
        assert!(!output.contains("Recent failures"));
        assert!(!output.contains("<pre>"));
    }

    #[test]
    fn media_jobs_report_shows_compact_recent_failures() {
        let report = admin::MediaJobsReport {
            total: 8,
            pending: 1,
            running: 1,
            succeeded: 4,
            failed: 2,
            newest_pending_age_seconds: Some(90),
            oldest_pending_age_seconds: Some(3_900),
            recent_failures: vec![admin::MediaJobFailure {
                id: 42,
                media_id: Some(7),
                media_path: Some("/media/uploads/a/really/long/path/that/should/not/dominate/report/video.webm".to_owned()),
                job_kind: Some("video".to_owned()),
                age_seconds: Some(3_600),
                error_summary: "ffmpeg failed\nwith a very long diagnostic that should be clipped before it fills the admin table".to_owned(),
            }],
        };

        let output = render_media_jobs_report(&report);

        assert!(output.contains("Recent failures"));
        assert!(output.contains("#42"));
        assert!(output.contains("video"));
        assert!(output.contains("1h"));
        assert!(output.contains("1m newest / 1h oldest"));
        assert!(output.contains("..."));
        assert!(!output.contains("should be clipped before it fills"));
        assert!(!output.contains("<pre>"));
    }

    #[test]
    fn delete_return_target_rejects_self_referential_confirmation_paths() {
        assert_eq!(safe_delete_return_target("/posts/42/delete", 42), None);
        assert_eq!(
            safe_delete_return_target("/posts/42/delete#post-42", 42),
            None
        );
        assert_eq!(
            safe_delete_return_target("http://127.0.0.1:18080/posts/42/delete?return_to=/home", 42),
            None
        );
        assert_eq!(safe_delete_return_target("/posts/42#post-42", 42), None);
        assert_eq!(
            safe_delete_return_target("/home#post-42", 42),
            Some("/home#post-42".to_owned())
        );
    }

    #[tokio::test]
    async fn non_admin_cannot_access_admin_users() {
        let server = spawn_test_server_with_admin().await;
        let member_cookie = register_test_user(&server, "member").await;

        let response = get_with_cookie(&server, "/admin/users", &member_cookie).await;

        assert_eq!(response.status, 403);
    }

    #[tokio::test]
    async fn admin_users_page_shows_expanded_user_details() {
        let server = spawn_test_server_with_admin().await;
        let admin_cookie = admin_session_cookie(&server).await;
        let settings = Settings::default();
        let alice = auth::register_user(
            &server.pool,
            &settings,
            "alice",
            "very secure password",
            false,
        )
        .await
        .expect("alice");
        let reporter = auth::register_user(
            &server.pool,
            &settings,
            "reporter",
            "very secure password",
            false,
        )
        .await
        .expect("reporter");
        let post = social::create_post(
            &server.pool,
            &settings,
            Some(alice),
            "expanded admin detail post",
            None,
            &[],
        )
        .await
        .expect("post");
        server
            .pool
            .call(move |conn| {
                conn.execute(
                    "UPDATE users SET display_name = 'Alice Admin' WHERE id = ?",
                    [alice],
                )?;
                conn.execute(
                    "INSERT INTO media (owner_user_id, original_filename, stored_path, public_path, mime_type, media_kind, byte_len) VALUES (?, 'alice.png', '/tmp/alice.png', '/uploads/images/alice.png', 'image/png', 'image', 12)",
                    [alice],
                )?;
                conn.execute(
                    "INSERT INTO reports (reporter_user_id, post_id, reason) VALUES (?, ?, 'spam')",
                    params![reporter, post],
                )?;
                Ok(())
            })
            .await
            .expect("seed details");

        let response = get_with_cookie(&server, "/admin/users", &admin_cookie).await;

        assert_eq!(response.status, 200);
        assert!(response.body.contains("Alice Admin"));
        assert!(response.body.contains("@alice"));
        assert!(response.body.contains("Created"));
        assert!(response.body.contains("Last post"));
        assert!(response.body.contains("Total posts"));
        assert!(response.body.contains("Uploaded media"));
        assert!(response.body.contains("Reports on posts"));
        assert!(response.body.contains("Moderation actions"));
        assert!(response.body.contains(">1</dd>"));
    }

    #[tokio::test]
    async fn admin_users_username_search_returns_expected_users() {
        let server = spawn_test_server_with_admin().await;
        let admin_cookie = admin_session_cookie(&server).await;
        let settings = Settings::default();
        auth::register_user(
            &server.pool,
            &settings,
            "alice",
            "very secure password",
            false,
        )
        .await
        .expect("alice");
        auth::register_user(
            &server.pool,
            &settings,
            "bob",
            "very secure password",
            false,
        )
        .await
        .expect("bob");

        let response = get_with_cookie(&server, "/admin/users?user_q=ali", &admin_cookie).await;

        assert_eq!(response.status, 200);
        assert!(response.body.contains("@alice"));
        assert!(!response.body.contains("@bob"));
        assert!(response.body.contains("Matched name"));
    }

    #[tokio::test]
    async fn admin_users_post_keyword_search_returns_matching_accounts() {
        let server = spawn_test_server_with_admin().await;
        let admin_cookie = admin_session_cookie(&server).await;
        let settings = Settings::default();
        let alice = auth::register_user(
            &server.pool,
            &settings,
            "alice",
            "very secure password",
            false,
        )
        .await
        .expect("alice");
        let bob = auth::register_user(
            &server.pool,
            &settings,
            "bob",
            "very secure password",
            false,
        )
        .await
        .expect("bob");
        social::create_post(
            &server.pool,
            &settings,
            Some(alice),
            "admin keyword needle <script>",
            None,
            &[],
        )
        .await
        .expect("alice post");
        social::create_post(
            &server.pool,
            &settings,
            Some(bob),
            "ordinary post",
            None,
            &[],
        )
        .await
        .expect("bob post");

        let response = get_with_cookie(&server, "/admin/users?post_q=needle", &admin_cookie).await;

        assert_eq!(response.status, 200);
        assert!(response.body.contains("@alice"));
        assert!(!response.body.contains("@bob"));
        assert!(response.body.contains("Matched post content: 1"));
        assert!(
            response
                .body
                .contains("admin keyword needle &lt;script&gt;")
        );
    }

    #[tokio::test]
    async fn admin_users_quoted_phrase_search_requires_exact_post_substring() {
        let server = spawn_test_server_with_admin().await;
        let admin_cookie = admin_session_cookie(&server).await;
        let settings = Settings::default();
        let alice = auth::register_user(
            &server.pool,
            &settings,
            "alice",
            "very secure password",
            false,
        )
        .await
        .expect("alice");
        let bob = auth::register_user(
            &server.pool,
            &settings,
            "bob",
            "very secure password",
            false,
        )
        .await
        .expect("bob");
        social::create_post(
            &server.pool,
            &settings,
            Some(alice),
            "hello world exact",
            None,
            &[],
        )
        .await
        .expect("alice post");
        social::create_post(
            &server.pool,
            &settings,
            Some(bob),
            "hello careful world",
            None,
            &[],
        )
        .await
        .expect("bob post");

        let response = get_with_cookie(
            &server,
            "/admin/users?post_q=%22hello%20world%22",
            &admin_cookie,
        )
        .await;

        assert_eq!(response.status, 200);
        assert!(response.body.contains("@alice"));
        assert!(!response.body.contains("@bob"));
    }

    #[tokio::test]
    async fn admin_users_search_has_empty_state_for_no_matches() {
        let server = spawn_test_server_with_admin().await;
        let admin_cookie = admin_session_cookie(&server).await;

        let response = get_with_cookie(
            &server,
            "/admin/users?user_q=missing&post_q=absent",
            &admin_cookie,
        )
        .await;

        assert_eq!(response.status, 200);
        assert!(response.body.contains("No users matched those filters."));
    }

    #[tokio::test]
    async fn missing_login_account_renders_login_form_message() {
        let server = spawn_test_server().await;

        let response = request(
            &server.base_url,
            "POST",
            "/login",
            &[("content-type", "application/x-www-form-urlencoded")],
            b"username=missing-user&password=not%20the%20password".to_vec(),
        )
        .await;

        assert_eq!(response.status, 401);
        assert!(response.body.contains("<h1>Log in</h1>"));
        assert!(response.body.contains("No account with that username."));
        assert!(
            response
                .body
                .contains(r#"<button class="auth-submit" type="submit">Log in</button>"#)
        );
        assert!(!response.body.contains("Authentication required"));
    }

    #[tokio::test]
    async fn auth_forms_show_password_minimum_and_short_passwords_return_forms() {
        let server = spawn_test_server().await;

        let login = request(&server.base_url, "GET", "/login", &[], Vec::new()).await;
        assert_eq!(login.status, 200);
        assert!(
            login
                .body
                .contains("Password must be at least 10 characters.")
        );
        assert!(
            login
                .body
                .contains(r#"aria-describedby="password-requirement""#)
        );

        let register = request(&server.base_url, "GET", "/register", &[], Vec::new()).await;
        assert_eq!(register.status, 200);
        assert!(
            register
                .body
                .contains("Password must be at least 10 characters.")
        );
        assert!(
            register
                .body
                .contains(r#"aria-describedby="confirm-password-requirement""#)
        );

        let short_login = request(
            &server.base_url,
            "POST",
            "/login",
            &[("content-type", "application/x-www-form-urlencoded")],
            b"username=alice&password=short".to_vec(),
        )
        .await;
        assert_eq!(short_login.status, 400);
        assert!(short_login.body.contains("<h1>Log in</h1>"));
        assert!(short_login.body.contains("password is too short"));

        let short_register = request(
            &server.base_url,
            "POST",
            "/register",
            &[("content-type", "application/x-www-form-urlencoded")],
            b"username=alice&password=short&confirm_password=short".to_vec(),
        )
        .await;
        assert_eq!(short_register.status, 400);
        assert!(short_register.body.contains("<h1>Create account</h1>"));
        assert!(short_register.body.contains("password is too short"));
    }

    #[tokio::test]
    async fn duplicate_registration_renders_register_form_message() {
        let server = spawn_test_server().await;
        let first = request(
            &server.base_url,
            "POST",
            "/register",
            &[("content-type", "application/x-www-form-urlencoded")],
            b"username=alice&password=very%20secure%20password&confirm_password=very%20secure%20password".to_vec(),
        )
        .await;
        assert_eq!(first.status, 303);

        let duplicate = request(
            &server.base_url,
            "POST",
            "/register",
            &[("content-type", "application/x-www-form-urlencoded")],
            b"username=Alice&password=very%20secure%20password&confirm_password=very%20secure%20password".to_vec(),
        )
        .await;

        assert_eq!(duplicate.status, 400);
        assert!(duplicate.body.contains("<h1>Create account</h1>"));
        assert!(duplicate.body.contains("That username is already taken."));
        assert!(
            duplicate
                .body
                .contains(r#"<button class="auth-submit" type="submit">Create account</button>"#)
        );
        assert!(!duplicate.body.contains("Check the form"));
    }

    #[tokio::test]
    async fn logged_in_ui_post_succeeds_with_empty_media_part_and_appears_on_home_feed() {
        let server = spawn_test_server().await;
        let registered = request(
            &server.base_url,
            "POST",
            "/register",
            &[("content-type", "application/x-www-form-urlencoded")],
            b"username=alice&password=very%20secure%20password&confirm_password=very%20secure%20password".to_vec(),
        )
        .await;
        assert_eq!(registered.status, 303);
        let cookie = session_cookie(&registered);

        let home = request(
            &server.base_url,
            "GET",
            "/home",
            &[("cookie", &cookie)],
            Vec::new(),
        )
        .await;
        assert_eq!(home.status, 200);
        assert!(home.body.contains("<p>All posts</p>"));
        assert!(!home.body.contains("Top-level posts from your"));
        assert!(home.body.contains(r#"action="/posts""#));
        let csrf = csrf_token(&home.body);

        let body = multipart_body(
            "post-boundary",
            &[
                ("csrf", csrf.as_str()),
                ("text", "hello from the browser-shaped form"),
            ],
            true,
        );
        let posted = request(
            &server.base_url,
            "POST",
            "/posts",
            &[
                ("cookie", &cookie),
                (
                    "content-type",
                    "multipart/form-data; boundary=post-boundary",
                ),
            ],
            body,
        )
        .await;
        assert_eq!(posted.status, 303);

        let home = request(
            &server.base_url,
            "GET",
            "/home",
            &[("cookie", &cookie)],
            Vec::new(),
        )
        .await;
        assert_eq!(home.status, 200);
        assert!(home.body.contains("hello from the browser-shaped form"));
        assert!(home.body.contains(r#"class="post""#));
        assert!(home.body.contains(r#"data-card-href="/posts/1""#));
        assert!(home.body.contains(r#"href="/posts/1">Open post</a>"#));
        assert!(!home.body.contains("Open thread"));
        assert!(!home.body.contains(r#"class="post-time""#));

        let thread = request(
            &server.base_url,
            "GET",
            "/posts/1",
            &[("cookie", &cookie)],
            Vec::new(),
        )
        .await;
        assert_eq!(thread.status, 200);
        assert!(thread.body.contains(r#"class="thread-nav""#));
        assert!(thread.body.contains(r#"aria-label="Back""#));
        assert!(!thread.body.contains(r#"class="page-header""#));
        assert!(
            !thread
                .body
                .contains("Read the conversation and add a reply.")
        );
        assert!(thread.body.contains(r#"class="post-time""#));
        assert!(!thread.body.contains(r#"class="post-time" href="/posts/1""#));
        assert!(!thread.body.contains(r#"data-card-href="/posts/1""#));
        assert!(!thread.body.contains(r#"href="/posts/1">Open post</a>"#));
    }

    #[tokio::test]
    async fn post_auth_csrf_anonymous_and_validation_fail_cleanly() {
        let server = spawn_test_server().await;
        let registered = request(
            &server.base_url,
            "POST",
            "/register",
            &[("content-type", "application/x-www-form-urlencoded")],
            b"username=bob&password=very%20secure%20password&confirm_password=very%20secure%20password".to_vec(),
        )
        .await;
        assert_eq!(registered.status, 303);
        let cookie = session_cookie(&registered);

        let missing_csrf = request(
            &server.base_url,
            "POST",
            "/posts",
            &[
                ("cookie", &cookie),
                (
                    "content-type",
                    "multipart/form-data; boundary=post-boundary",
                ),
            ],
            multipart_body("post-boundary", &[("text", "missing csrf")], false),
        )
        .await;
        assert_eq!(missing_csrf.status, 403);
        assert!(missing_csrf.body.contains("Access denied"));
        assert!(!missing_csrf.body.contains("missing csrf session"));

        let logged_out = request(
            &server.base_url,
            "POST",
            "/posts",
            &[(
                "content-type",
                "multipart/form-data; boundary=post-boundary",
            )],
            multipart_body("post-boundary", &[("text", "logged out")], false),
        )
        .await;
        assert_eq!(logged_out.status, 403);
        assert!(logged_out.body.contains("Access denied"));

        let anonymous_home = request(&server.base_url, "GET", "/home", &[], Vec::new()).await;
        assert_eq!(anonymous_home.status, 200);
        assert!(!anonymous_home.body.contains(r#"action="/posts""#));

        let home = request(
            &server.base_url,
            "GET",
            "/home",
            &[("cookie", &cookie)],
            Vec::new(),
        )
        .await;
        let csrf = csrf_token(&home.body);
        let too_long = "x".repeat(281);
        let too_long = request(
            &server.base_url,
            "POST",
            "/posts",
            &[
                ("cookie", &cookie),
                (
                    "content-type",
                    "multipart/form-data; boundary=post-boundary",
                ),
            ],
            multipart_body(
                "post-boundary",
                &[("csrf", csrf.as_str()), ("text", too_long.as_str())],
                false,
            ),
        )
        .await;
        assert_eq!(too_long.status, 400);
        assert!(too_long.body.contains("post is too long"));
        assert!(!too_long.body.contains("internal server error"));
    }

    #[tokio::test]
    async fn registration_requires_matching_password_confirmation() {
        let server = spawn_test_server().await;
        let matching = request(
            &server.base_url,
            "POST",
            "/register",
            &[("content-type", "application/x-www-form-urlencoded")],
            b"username=carol&password=very%20secure%20password&confirm_password=very%20secure%20password".to_vec(),
        )
        .await;
        assert_eq!(matching.status, 303);

        let mismatched = request(
            &server.base_url,
            "POST",
            "/register",
            &[("content-type", "application/x-www-form-urlencoded")],
            b"username=dave&password=very%20secure%20password&confirm_password=different%20password".to_vec(),
        )
        .await;
        assert_eq!(mismatched.status, 400);
        assert!(mismatched.body.contains("passwords do not match"));

        let missing = request(
            &server.base_url,
            "POST",
            "/register",
            &[("content-type", "application/x-www-form-urlencoded")],
            b"username=erin&password=very%20secure%20password".to_vec(),
        )
        .await;
        assert_eq!(missing.status, 400);
        assert!(missing.body.contains("please confirm your password"));
    }

    #[tokio::test]
    async fn registration_captcha_disabled_by_default_preserves_registration_flow() {
        let server = spawn_test_server().await;

        let page = request(&server.base_url, "GET", "/register", &[], Vec::new()).await;
        assert_eq!(page.status, 200);
        assert!(!page.body.contains("Registration CAPTCHA"));
        assert!(!page.body.contains(r#"name="captcha_answer""#));

        let registered = request(
            &server.base_url,
            "POST",
            "/register",
            &[("content-type", "application/x-www-form-urlencoded")],
            b"username=no-captcha&password=very%20secure%20password&confirm_password=very%20secure%20password".to_vec(),
        )
        .await;
        assert_eq!(registered.status, 303);
    }

    #[tokio::test]
    async fn registration_captcha_rejects_missing_wrong_expired_and_reused_answers() {
        let mut settings = Settings::default();
        settings.accounts.registration_captcha_enabled = true;
        settings.moderation.account_creations_per_ip_per_day = 20;
        let server = spawn_test_server_with_settings(settings).await;

        let page = request(&server.base_url, "GET", "/register", &[], Vec::new()).await;
        assert_eq!(page.status, 200);
        assert!(page.body.contains("Registration CAPTCHA"));
        assert!(page.body.contains(r#"name="captcha_token""#));
        assert!(page.body.contains(r#"name="captcha_answer""#));
        assert!(page.body.contains("data:image/png;base64,"));

        let missing = request(
            &server.base_url,
            "POST",
            "/register",
            &[("content-type", "application/x-www-form-urlencoded")],
            b"username=missing-captcha&password=very%20secure%20password&confirm_password=very%20secure%20password".to_vec(),
        )
        .await;
        assert_eq!(missing.status, 400);
        assert!(missing.body.contains("CAPTCHA challenge is missing"));
        assert!(missing.body.contains("Registration CAPTCHA"));

        let wrong_challenge = server
            .registration_captcha
            .create_challenge()
            .await
            .expect("captcha");
        let wrong = request(
            &server.base_url,
            "POST",
            "/register",
            &[("content-type", "application/x-www-form-urlencoded")],
            registration_body("wrong-captcha", Some(&wrong_challenge.token), Some("WRONG")),
        )
        .await;
        assert_eq!(wrong.status, 400);
        assert!(wrong.body.contains("CAPTCHA answer was incorrect"));
        assert!(!wrong.body.contains(&wrong_challenge.answer));

        let reused_after_wrong = request(
            &server.base_url,
            "POST",
            "/register",
            &[("content-type", "application/x-www-form-urlencoded")],
            registration_body(
                "reused-after-wrong",
                Some(&wrong_challenge.token),
                Some(&wrong_challenge.answer),
            ),
        )
        .await;
        assert_eq!(reused_after_wrong.status, 400);
        assert!(
            reused_after_wrong
                .body
                .contains("expired or was already used")
        );

        let expired_challenge = server
            .registration_captcha
            .create_challenge()
            .await
            .expect("captcha");
        server
            .registration_captcha
            .expire_for_test(&expired_challenge.token)
            .await;
        let expired = request(
            &server.base_url,
            "POST",
            "/register",
            &[("content-type", "application/x-www-form-urlencoded")],
            registration_body(
                "expired-captcha",
                Some(&expired_challenge.token),
                Some(&expired_challenge.answer),
            ),
        )
        .await;
        assert_eq!(expired.status, 400);
        assert!(expired.body.contains("expired or was already used"));
    }

    #[tokio::test]
    async fn registration_captcha_accepts_correct_answer_once() {
        let mut settings = Settings::default();
        settings.accounts.registration_captcha_enabled = true;
        settings.moderation.account_creations_per_ip_per_day = 20;
        let server = spawn_test_server_with_settings(settings).await;
        let challenge = server
            .registration_captcha
            .create_challenge()
            .await
            .expect("captcha");

        let registered = request(
            &server.base_url,
            "POST",
            "/register",
            &[("content-type", "application/x-www-form-urlencoded")],
            registration_body(
                "captcha-ok",
                Some(&challenge.token),
                Some(&challenge.answer.to_lowercase()),
            ),
        )
        .await;
        assert_eq!(registered.status, 303);

        let reused = request(
            &server.base_url,
            "POST",
            "/register",
            &[("content-type", "application/x-www-form-urlencoded")],
            registration_body(
                "captcha-reused",
                Some(&challenge.token),
                Some(&challenge.answer),
            ),
        )
        .await;
        assert_eq!(reused.status, 400);
        assert!(reused.body.contains("expired or was already used"));
    }

    #[tokio::test]
    async fn post_actions_redirect_to_anchored_context_and_repost_errors_are_validation_failures() {
        let server = spawn_test_server().await;
        let registered = request(
            &server.base_url,
            "POST",
            "/register",
            &[("content-type", "application/x-www-form-urlencoded")],
            b"username=alice&password=very%20secure%20password&confirm_password=very%20secure%20password".to_vec(),
        )
        .await;
        assert_eq!(registered.status, 303);
        let cookie = session_cookie(&registered);
        let home = request(
            &server.base_url,
            "GET",
            "/home",
            &[("cookie", &cookie)],
            Vec::new(),
        )
        .await;
        let csrf = csrf_token(&home.body);
        let posted = request(
            &server.base_url,
            "POST",
            "/posts",
            &[
                ("cookie", &cookie),
                (
                    "content-type",
                    "multipart/form-data; boundary=post-boundary",
                ),
            ],
            multipart_body(
                "post-boundary",
                &[("csrf", csrf.as_str()), ("text", "anchored post")],
                false,
            ),
        )
        .await;
        assert_eq!(posted.status, 303);
        assert_eq!(location(&posted), "/home#post-1");

        let home = request(
            &server.base_url,
            "GET",
            "/home",
            &[("cookie", &cookie)],
            Vec::new(),
        )
        .await;
        assert!(home.body.contains(r#"id="post-1""#));
        let csrf = csrf_token(&home.body);
        let liked = request(
            &server.base_url,
            "POST",
            "/posts/1/like",
            &[
                ("cookie", &cookie),
                ("referer", "/home"),
                ("content-type", "application/x-www-form-urlencoded"),
            ],
            format!("csrf={csrf}").into_bytes(),
        )
        .await;
        assert_eq!(liked.status, 303);
        assert_eq!(location(&liked), "/home#post-1");

        let bookmarked = request(
            &server.base_url,
            "POST",
            "/posts/1/bookmark",
            &[
                ("cookie", &cookie),
                ("referer", "/home"),
                ("content-type", "application/x-www-form-urlencoded"),
            ],
            format!("csrf={csrf}").into_bytes(),
        )
        .await;
        assert_eq!(bookmarked.status, 303);
        assert_eq!(location(&bookmarked), "/home#post-1");

        let self_repost = request(
            &server.base_url,
            "POST",
            "/posts/1/repost",
            &[
                ("cookie", &cookie),
                ("referer", "/home"),
                ("content-type", "application/x-www-form-urlencoded"),
            ],
            format!("csrf={csrf}").into_bytes(),
        )
        .await;
        assert_eq!(self_repost.status, 400);
        assert!(self_repost.body.contains("cannot repost your own post"));
        assert!(!self_repost.body.contains("internal server error"));
    }

    #[tokio::test]
    async fn quote_repost_can_be_created_once_and_renders_original_preview() {
        let server = spawn_test_server().await;
        let alice_cookie = register_test_user(&server, "alice").await;
        create_text_post(&server, &alice_cookie, "original quote target").await;

        let bob_cookie = register_test_user(&server, "bob").await;
        let bob_home = get_with_cookie(&server, "/home", &bob_cookie).await;
        assert!(
            bob_home
                .body
                .contains(r#"class="icon-button quote-fallback" href="/posts/1/quote" aria-label="Quote post" title="Quote post""#)
        );
        assert!(bob_home.body.contains("data-repost-menu-button"));

        let quote_form = get_with_cookie(&server, "/posts/1/quote", &bob_cookie).await;
        assert_eq!(quote_form.status, 200);
        assert!(
            quote_form
                .body
                .contains("<h1 id=\"composer-title\">Quote post</h1>")
        );
        assert!(quote_form.body.contains("original quote target"));

        let quote_body = quote_form_body(&quote_form, "bob adds context");
        let quote =
            post_form_with_cookie(&server, "/posts/1/quote", &bob_cookie, &quote_body).await;
        assert_eq!(quote.status, 303);
        assert_eq!(location(&quote), "/home#post-2");

        let duplicate =
            post_form_with_cookie(&server, "/posts/1/quote", &bob_cookie, &quote_body).await;
        assert_eq!(duplicate.status, 303);
        assert_eq!(location(&duplicate), "/home#post-2");

        let bob_home = get_with_cookie(&server, "/home", &bob_cookie).await;
        assert!(bob_home.body.contains("bob adds context"));
        assert!(bob_home.body.contains(r#"class="quote-preview""#));
        assert!(bob_home.body.contains("original quote target"));
        assert_eq!(bob_home.body.matches("bob adds context").count(), 1);
    }

    #[tokio::test]
    async fn quote_repost_handles_deleted_original_gracefully() {
        let server = spawn_test_server().await;
        let alice_cookie = register_test_user(&server, "alice").await;
        create_text_post(&server, &alice_cookie, "soon deleted original").await;

        let bob_cookie = register_test_user(&server, "bob").await;
        let quote_form = get_with_cookie(&server, "/posts/1/quote", &bob_cookie).await;
        let quote_body = quote_form_body(&quote_form, "quote survives deletion");
        let quote =
            post_form_with_cookie(&server, "/posts/1/quote", &bob_cookie, &quote_body).await;
        assert_eq!(quote.status, 303);

        let alice_home = get_with_cookie(&server, "/home", &alice_cookie).await;
        let delete_csrf = csrf_token(&alice_home.body);
        let deleted_body = format!("csrf={delete_csrf}&return_to=/home%23post-1");
        let deleted =
            post_form_with_cookie(&server, "/posts/1/delete", &alice_cookie, &deleted_body).await;
        assert_eq!(deleted.status, 303);

        let bob_home = get_with_cookie(&server, "/home", &bob_cookie).await;
        assert!(bob_home.body.contains("quote survives deletion"));
        assert!(
            bob_home
                .body
                .contains("Quoted post is no longer available.")
        );
    }

    #[tokio::test]
    async fn post_actions_accept_token_from_recent_page_render_history() {
        let server = spawn_test_server().await;
        let registered = request(
            &server.base_url,
            "POST",
            "/register",
            &[("content-type", "application/x-www-form-urlencoded")],
            b"username=alice&password=very%20secure%20password&confirm_password=very%20secure%20password".to_vec(),
        )
        .await;
        let cookie = session_cookie(&registered);

        let home = request(
            &server.base_url,
            "GET",
            "/home",
            &[("cookie", &cookie)],
            Vec::new(),
        )
        .await;
        let csrf = csrf_token(&home.body);
        let posted = request(
            &server.base_url,
            "POST",
            "/posts",
            &[
                ("cookie", &cookie),
                (
                    "content-type",
                    "multipart/form-data; boundary=post-boundary",
                ),
            ],
            multipart_body(
                "post-boundary",
                &[("csrf", csrf.as_str()), ("text", "liked after back")],
                false,
            ),
        )
        .await;
        assert_eq!(posted.status, 303);

        let home = request(
            &server.base_url,
            "GET",
            "/home",
            &[("cookie", &cookie)],
            Vec::new(),
        )
        .await;
        let home_csrf = csrf_token(&home.body);
        for _ in 0..5 {
            let thread = request(
                &server.base_url,
                "GET",
                "/posts/1",
                &[("cookie", &cookie)],
                Vec::new(),
            )
            .await;
            assert_eq!(thread.status, 200);
        }

        let liked = request(
            &server.base_url,
            "POST",
            "/posts/1/like",
            &[
                ("cookie", &cookie),
                ("referer", "/home"),
                ("content-type", "application/x-www-form-urlencoded"),
            ],
            format!("csrf={home_csrf}").into_bytes(),
        )
        .await;
        assert_eq!(liked.status, 303);
        assert_eq!(location(&liked), "/home#post-1");
    }

    #[tokio::test]
    async fn image_uploads_over_default_body_limit_are_accepted() {
        let server = spawn_test_server().await;
        let registered = request(
            &server.base_url,
            "POST",
            "/register",
            &[("content-type", "application/x-www-form-urlencoded")],
            b"username=alice&password=very%20secure%20password&confirm_password=very%20secure%20password".to_vec(),
        )
        .await;
        let cookie = session_cookie(&registered);
        let home = request(
            &server.base_url,
            "GET",
            "/home",
            &[("cookie", &cookie)],
            Vec::new(),
        )
        .await;
        let csrf = csrf_token(&home.body);

        let mut image = tiny_png_bytes();
        image.resize((2 * 1024 * 1024) + 1, 0);
        let posted = request(
            &server.base_url,
            "POST",
            "/posts",
            &[
                ("cookie", &cookie),
                (
                    "content-type",
                    "multipart/form-data; boundary=post-boundary",
                ),
            ],
            multipart_body_with_file(
                "post-boundary",
                &[("csrf", csrf.as_str()), ("text", "large image")],
                "media",
                "large.png",
                "image/png",
                &image,
            ),
        )
        .await;

        assert_eq!(posted.status, 303);
        assert_eq!(location(&posted), "/home#post-1");
    }

    #[tokio::test]
    async fn post_multipart_errors_keep_authenticated_layout() {
        let server = spawn_test_server().await;
        let registered = request(
            &server.base_url,
            "POST",
            "/register",
            &[("content-type", "application/x-www-form-urlencoded")],
            b"username=alice&password=very%20secure%20password&confirm_password=very%20secure%20password".to_vec(),
        )
        .await;
        let cookie = session_cookie(&registered);
        let failed = request(
            &server.base_url,
            "POST",
            "/posts",
            &[
                ("cookie", &cookie),
                (
                    "content-type",
                    "multipart/form-data; boundary=post-boundary",
                ),
            ],
            b"--post-boundary\r\nContent-Disposition: form-data; name=\"text\"\r\n\r\nunterminated"
                .to_vec(),
        )
        .await;

        assert_eq!(failed.status, 400);
        assert!(failed.body.contains("Check the form"));
        assert!(failed.body.contains(r#"href="/users/alice""#));
    }

    #[tokio::test]
    async fn enhanced_like_updates_in_place_with_stable_action_markup() {
        let server = spawn_test_server().await;
        let registered = request(
            &server.base_url,
            "POST",
            "/register",
            &[("content-type", "application/x-www-form-urlencoded")],
            b"username=alice&password=very%20secure%20password&confirm_password=very%20secure%20password".to_vec(),
        )
        .await;
        let cookie = session_cookie(&registered);
        let home = request(
            &server.base_url,
            "GET",
            "/home",
            &[("cookie", &cookie)],
            Vec::new(),
        )
        .await;
        let csrf = csrf_token(&home.body);
        let posted = request(
            &server.base_url,
            "POST",
            "/posts",
            &[
                ("cookie", &cookie),
                (
                    "content-type",
                    "multipart/form-data; boundary=post-boundary",
                ),
            ],
            multipart_body(
                "post-boundary",
                &[("csrf", csrf.as_str()), ("text", "enhanced like")],
                false,
            ),
        )
        .await;
        assert_eq!(posted.status, 303);

        let home = request(
            &server.base_url,
            "GET",
            "/home",
            &[("cookie", &cookie)],
            Vec::new(),
        )
        .await;
        assert!(home.body.contains(r#"data-enhance="post-action""#));
        assert!(home.body.contains(r#"data-count="likes""#));
        assert!(home.body.contains(r#"data-action-kind="like""#));
        let csrf = csrf_token(&home.body);
        let liked = request(
            &server.base_url,
            "POST",
            "/posts/1/like",
            &[
                ("cookie", &cookie),
                ("referer", "/home"),
                ("content-type", "application/x-www-form-urlencoded"),
                ("x-rustpost-enhance", "1"),
                ("accept", "application/json"),
            ],
            format!("csrf={csrf}").into_bytes(),
        )
        .await;
        assert_eq!(liked.status, 200);
        assert!(liked.body.contains(r#""kind":"post-action""#));
        assert!(liked.body.contains(r#""post_id":1"#));
        assert!(liked.body.contains(r#""liked":true"#));
        assert!(liked.body.contains(r#""likes":1"#));

        let bookmarked = request(
            &server.base_url,
            "POST",
            "/posts/1/bookmark",
            &[
                ("cookie", &cookie),
                ("referer", "/home"),
                ("content-type", "application/x-www-form-urlencoded"),
                ("x-rustpost-enhance", "1"),
                ("accept", "application/json"),
            ],
            format!("csrf={csrf}").into_bytes(),
        )
        .await;
        assert_eq!(bookmarked.status, 200);
        assert!(bookmarked.body.contains(r#""kind":"post-action""#));
        assert!(bookmarked.body.contains(r#""post_id":1"#));
        assert!(bookmarked.body.contains(r#""bookmarked":true"#));
    }

    #[tokio::test]
    async fn enhanced_post_and_reply_return_rendered_cards_without_redirects() {
        let server = spawn_test_server().await;
        let registered = request(
            &server.base_url,
            "POST",
            "/register",
            &[("content-type", "application/x-www-form-urlencoded")],
            b"username=alice&password=very%20secure%20password&confirm_password=very%20secure%20password".to_vec(),
        )
        .await;
        let cookie = session_cookie(&registered);
        let home = request(
            &server.base_url,
            "GET",
            "/home",
            &[("cookie", &cookie)],
            Vec::new(),
        )
        .await;
        assert!(home.body.contains(r#"data-enhance="post-create""#));
        let csrf = csrf_token(&home.body);

        let posted = request(
            &server.base_url,
            "POST",
            "/posts",
            &[
                ("cookie", &cookie),
                (
                    "content-type",
                    "multipart/form-data; boundary=post-boundary",
                ),
                ("x-rustpost-enhance", "1"),
                ("accept", "application/json"),
            ],
            multipart_body(
                "post-boundary",
                &[("csrf", csrf.as_str()), ("text", "enhanced post")],
                false,
            ),
        )
        .await;
        assert_eq!(posted.status, 200);
        assert!(posted.body.contains(r#""kind":"post-created""#));
        assert!(posted.body.contains(r#""post_id":1"#));
        assert!(posted.body.contains(r#""parent_post_id":null"#));
        assert!(posted.body.contains("enhanced post"));
        assert!(posted.body.contains(r#"id=\"post-1\""#));

        let thread = request(
            &server.base_url,
            "GET",
            "/posts/1",
            &[("cookie", &cookie)],
            Vec::new(),
        )
        .await;
        let csrf = csrf_token(&thread.body);
        let replied = request(
            &server.base_url,
            "POST",
            "/posts",
            &[
                ("cookie", &cookie),
                (
                    "content-type",
                    "multipart/form-data; boundary=post-boundary",
                ),
                ("x-rustpost-enhance", "1"),
                ("accept", "application/json"),
            ],
            multipart_body(
                "post-boundary",
                &[
                    ("csrf", csrf.as_str()),
                    ("parent_post_id", "1"),
                    ("text", "enhanced reply"),
                ],
                false,
            ),
        )
        .await;
        assert_eq!(replied.status, 200);
        assert!(replied.body.contains(r#""post_id":2"#));
        assert!(replied.body.contains(r#""parent_post_id":1"#));
        assert!(replied.body.contains("enhanced reply"));
        assert!(replied.body.contains(r#"reply-post"#));
    }

    #[tokio::test]
    async fn follow_profile_stays_on_profile_and_renders_following_state() {
        let server = spawn_test_server().await;
        let bob = request(
            &server.base_url,
            "POST",
            "/register",
            &[("content-type", "application/x-www-form-urlencoded")],
            b"username=bob&password=very%20secure%20password&confirm_password=very%20secure%20password".to_vec(),
        )
        .await;
        assert_eq!(bob.status, 303);
        let alice = request(
            &server.base_url,
            "POST",
            "/register",
            &[("content-type", "application/x-www-form-urlencoded")],
            b"username=alice&password=very%20secure%20password&confirm_password=very%20secure%20password".to_vec(),
        )
        .await;
        assert_eq!(alice.status, 303);
        let alice_cookie = session_cookie(&alice);

        let profile = request(
            &server.base_url,
            "GET",
            "/users/bob",
            &[("cookie", &alice_cookie)],
            Vec::new(),
        )
        .await;
        assert_eq!(profile.status, 200);
        assert!(profile.body.contains(r#"class="actions profile-actions""#));
        assert!(profile.body.contains(r#"class="follow-button""#));
        assert!(profile.body.contains(r#">Follow</button>"#));
        assert!(
            profile
                .body
                .contains(r#"data-profile-followers="1">0 followers"#)
        );
        assert!(
            profile
                .body
                .contains(r#"class="actions profile-secondary""#)
        );
        let csrf = csrf_token(&profile.body);

        let followed = request(
            &server.base_url,
            "POST",
            "/users/1/follow",
            &[
                ("cookie", &alice_cookie),
                ("referer", "/users/bob"),
                ("content-type", "application/x-www-form-urlencoded"),
            ],
            format!("csrf={csrf}").into_bytes(),
        )
        .await;
        assert_eq!(followed.status, 303);
        assert_eq!(location(&followed), "/users/bob");

        let profile = request(
            &server.base_url,
            "GET",
            "/users/bob",
            &[("cookie", &alice_cookie)],
            Vec::new(),
        )
        .await;
        assert_eq!(profile.status, 200);
        assert!(profile.body.contains(r#"class="follow-button active""#));
        assert!(profile.body.contains(r#">Following</button>"#));
        assert!(
            profile
                .body
                .contains(r#"data-profile-followers="1">1 followers"#)
        );
        assert!(!profile.body.contains(">Unfollow</button>"));
    }

    #[tokio::test]
    async fn enhanced_follow_returns_button_and_count_state() {
        let server = spawn_test_server().await;
        let bob = request(
            &server.base_url,
            "POST",
            "/register",
            &[("content-type", "application/x-www-form-urlencoded")],
            b"username=bob&password=very%20secure%20password&confirm_password=very%20secure%20password".to_vec(),
        )
        .await;
        assert_eq!(bob.status, 303);
        let alice = request(
            &server.base_url,
            "POST",
            "/register",
            &[("content-type", "application/x-www-form-urlencoded")],
            b"username=alice&password=very%20secure%20password&confirm_password=very%20secure%20password".to_vec(),
        )
        .await;
        let alice_cookie = session_cookie(&alice);
        let profile = request(
            &server.base_url,
            "GET",
            "/users/bob",
            &[("cookie", &alice_cookie)],
            Vec::new(),
        )
        .await;
        let csrf = csrf_token(&profile.body);

        let followed = request(
            &server.base_url,
            "POST",
            "/users/1/follow",
            &[
                ("cookie", &alice_cookie),
                ("referer", "/users/bob"),
                ("content-type", "application/x-www-form-urlencoded"),
                ("x-rustpost-enhance", "1"),
                ("accept", "application/json"),
            ],
            format!("csrf={csrf}").into_bytes(),
        )
        .await;
        assert_eq!(followed.status, 200);
        assert!(followed.body.contains(r#""kind":"follow""#));
        assert!(followed.body.contains(r#""user_id":1"#));
        assert!(followed.body.contains(r#""following":true"#));
        assert!(followed.body.contains(r#""followers":1"#));
        assert!(followed.body.contains(r#""action":"/users/1/unfollow""#));
    }

    #[tokio::test]
    async fn notifications_page_renders_unread_badges_and_mark_all_read() {
        let server = spawn_test_server().await;
        let alice_cookie = register_test_user(&server, "alice").await;
        create_text_post(&server, &alice_cookie, "alice original post").await;
        let bob_cookie = register_test_user(&server, "bob").await;

        let thread = get_with_cookie(&server, "/posts/1", &bob_cookie).await;
        let csrf = csrf_token(&thread.body);
        let reply = request(
            &server.base_url,
            "POST",
            "/posts",
            &[
                ("cookie", &bob_cookie),
                (
                    "content-type",
                    "multipart/form-data; boundary=post-boundary",
                ),
            ],
            multipart_body(
                "post-boundary",
                &[
                    ("csrf", csrf.as_str()),
                    ("parent_post_id", "1"),
                    ("text", "bob reply"),
                ],
                false,
            ),
        )
        .await;
        assert_eq!(reply.status, 303);

        let bob_home = get_with_cookie(&server, "/home", &bob_cookie).await;
        let csrf = csrf_token(&bob_home.body);
        let liked = post_form_with_cookie(
            &server,
            "/posts/1/like",
            &bob_cookie,
            &format!("csrf={csrf}"),
        )
        .await;
        assert_eq!(liked.status, 303);
        let reposted = post_form_with_cookie(
            &server,
            "/posts/1/repost",
            &bob_cookie,
            &format!("csrf={csrf}"),
        )
        .await;
        assert_eq!(reposted.status, 303);
        let followed = post_form_with_cookie(
            &server,
            "/users/1/follow",
            &bob_cookie,
            &format!("csrf={csrf}"),
        )
        .await;
        assert_eq!(followed.status, 303);

        let notifications = get_with_cookie(&server, "/notifications", &alice_cookie).await;
        assert_eq!(notifications.status, 200);
        assert_populated_notifications_page(&notifications.body);

        let csrf = csrf_token(&notifications.body);
        let read = post_form_with_cookie(
            &server,
            "/notifications/read",
            &alice_cookie,
            &format!("csrf={csrf}"),
        )
        .await;
        assert_eq!(read.status, 303);
        assert_eq!(location(&read), "/notifications");

        let notifications = get_with_cookie(&server, "/notifications", &alice_cookie).await;
        assert!(notifications.body.contains("No unread notifications"));
        assert!(
            notifications
                .body
                .contains("All caught up. Everything here has been read.")
        );
        assert!(!notifications.body.contains(r#"<span class="nav-badge""#));
        assert!(
            !notifications
                .body
                .contains(r#"class="notification-row unread""#)
        );
    }

    #[tokio::test]
    async fn notifications_page_has_polished_empty_state() {
        let server = spawn_test_server().await;
        let cookie = register_test_user(&server, "alice").await;

        let notifications = get_with_cookie(&server, "/notifications", &cookie).await;

        assert_eq!(notifications.status, 200);
        assert!(notifications.body.contains(r#"class="notifications-hero""#));
        assert!(notifications.body.contains("No unread notifications"));
        assert!(notifications.body.contains("No notifications yet"));
        assert!(
            notifications
                .body
                .contains("Replies, likes, reposts, follows, and mentions will appear here.")
        );
    }

    #[tokio::test]
    async fn default_favicon_route_and_html_link_are_present() {
        let server = spawn_test_server().await;

        let favicon = request(&server.base_url, "GET", "/favicon.ico", &[], Vec::new()).await;
        assert_eq!(favicon.status, 200);
        assert_header(&favicon, "content-type", "image/x-icon");
        assert_header(&favicon, "cache-control", "public, max-age=3600");

        let home = request(&server.base_url, "GET", "/home", &[], Vec::new()).await;
        assert_eq!(home.status, 200);
        assert!(
            home.body
                .contains(r#"<link rel="icon" href="/favicon.ico" type="image/x-icon">"#)
        );
    }

    #[tokio::test]
    async fn html_response_with_gzip_accept_encoding_is_compressed() {
        let server = spawn_test_server().await;

        let response = request(
            &server.base_url,
            "GET",
            "/home",
            &[("accept-encoding", "gzip")],
            Vec::new(),
        )
        .await;

        assert_eq!(response.status, 200);
        assert_header(&response, "content-encoding", "gzip");
        assert_vary_contains_accept_encoding(&response);
        assert_eq!(
            content_length(&response),
            Some(response.body_bytes.len()),
            "compressed content-length should match wire body length"
        );
        let body = gzip_decode(&response.body_bytes);
        assert!(body.contains("<title>Home Feed - RustPost</title>"));
    }

    #[tokio::test]
    async fn html_response_without_accept_encoding_is_not_compressed() {
        let server = spawn_test_server().await;

        let response = request(&server.base_url, "GET", "/home", &[], Vec::new()).await;

        assert_eq!(response.status, 200);
        assert_no_header(&response, "content-encoding");
        assert!(
            response
                .body
                .contains("<title>Home Feed - RustPost</title>")
        );
    }

    #[tokio::test]
    async fn uploaded_media_response_is_not_compressed() {
        let server = spawn_test_server().await;
        let cookie = register_test_user(&server, "alice").await;
        let home = get_with_cookie(&server, "/home", &cookie).await;
        let csrf = csrf_token(&home.body);
        let posted = request(
            &server.base_url,
            "POST",
            "/posts",
            &[
                ("cookie", &cookie),
                (
                    "content-type",
                    "multipart/form-data; boundary=post-boundary",
                ),
            ],
            multipart_body_with_file(
                "post-boundary",
                &[("csrf", csrf.as_str()), ("text", "image post")],
                "media",
                "photo.png",
                "image/png",
                &tiny_png_bytes(),
            ),
        )
        .await;
        assert_eq!(posted.status, 303);

        let conn = rusqlite::Connection::open(server.data_dir.join("db/rustpost.sqlite3"))
            .expect("open database");
        let public_path: String = conn
            .query_row("SELECT public_path FROM media LIMIT 1", [], |row| {
                row.get(0)
            })
            .expect("media path");
        drop(conn);

        let media = request(
            &server.base_url,
            "GET",
            &public_path,
            &[("accept-encoding", "gzip")],
            Vec::new(),
        )
        .await;

        assert_eq!(media.status, 200);
        assert_no_header(&media, "content-encoding");
        assert!(media.body_bytes.starts_with(&tiny_png_bytes()[..8]));
    }

    #[tokio::test]
    async fn user_can_flag_uploaded_media_as_nsfw_and_blur_is_safe_by_default() {
        let server = spawn_test_server().await;
        let cookie = register_test_user(&server, "alice").await;
        let home = get_with_cookie(&server, "/home", &cookie).await;
        assert!(home.body.contains("Mark media as NSFW"));
        let csrf = csrf_token(&home.body);

        let posted = request(
            &server.base_url,
            "POST",
            "/posts",
            &[
                ("cookie", &cookie),
                (
                    "content-type",
                    "multipart/form-data; boundary=post-boundary",
                ),
            ],
            multipart_body_with_file(
                "post-boundary",
                &[
                    ("csrf", csrf.as_str()),
                    ("text", "flagged image post"),
                    ("nsfw", "true"),
                ],
                "media",
                "photo.png",
                "image/png",
                &tiny_png_bytes(),
            ),
        )
        .await;
        assert_eq!(posted.status, 303);

        let is_nsfw: i64 = server
            .pool
            .call(|conn| {
                Ok(conn.query_row("SELECT is_nsfw FROM media LIMIT 1", [], |row| row.get(0))?)
            })
            .await
            .expect("nsfw flag");
        assert_eq!(is_nsfw, 1);

        let home = get_with_cookie(&server, "/home", &cookie).await;
        assert!(home.body.contains(r#"data-testid="nsfw-media""#));
        assert!(home.body.contains(r#"aria-label="Show NSFW media""#));
        assert!(home.body.contains(">Show<span"));
        assert!(home.body.contains("Open media"));
    }

    #[tokio::test]
    async fn user_nsfw_blur_setting_persists_and_controls_rendering() {
        let server = spawn_test_server().await;
        let cookie = register_test_user(&server, "alice").await;
        let home = get_with_cookie(&server, "/home", &cookie).await;
        let csrf = csrf_token(&home.body);
        let posted = request(
            &server.base_url,
            "POST",
            "/posts",
            &[
                ("cookie", &cookie),
                (
                    "content-type",
                    "multipart/form-data; boundary=post-boundary",
                ),
            ],
            multipart_body_with_file(
                "post-boundary",
                &[
                    ("csrf", csrf.as_str()),
                    ("text", "preference test image"),
                    ("nsfw", "true"),
                ],
                "media",
                "photo.png",
                "image/png",
                &tiny_png_bytes(),
            ),
        )
        .await;
        assert_eq!(posted.status, 303);

        let settings = get_with_cookie(&server, "/settings", &cookie).await;
        let csrf = csrf_token(&settings.body);
        let disabled = request(
            &server.base_url,
            "POST",
            "/settings",
            &[
                ("cookie", &cookie),
                (
                    "content-type",
                    "multipart/form-data; boundary=settings-boundary",
                ),
            ],
            multipart_body(
                "settings-boundary",
                &[
                    ("csrf", csrf.as_str()),
                    ("display_name", "alice"),
                    ("bio", ""),
                    ("location", ""),
                    ("website", ""),
                ],
                false,
            ),
        )
        .await;
        assert_eq!(disabled.status, 303);
        let home = get_with_cookie(&server, "/home", &cookie).await;
        assert!(!home.body.contains(r#"data-testid="nsfw-media""#));

        let settings = get_with_cookie(&server, "/settings", &cookie).await;
        let csrf = csrf_token(&settings.body);
        let enabled = request(
            &server.base_url,
            "POST",
            "/settings",
            &[
                ("cookie", &cookie),
                (
                    "content-type",
                    "multipart/form-data; boundary=settings-boundary",
                ),
            ],
            multipart_body(
                "settings-boundary",
                &[
                    ("csrf", csrf.as_str()),
                    ("display_name", "alice"),
                    ("bio", ""),
                    ("location", ""),
                    ("website", ""),
                    ("nsfw_blur_enabled", "true"),
                ],
                false,
            ),
        )
        .await;
        assert_eq!(enabled.status, 303);
        let home = get_with_cookie(&server, "/home", &cookie).await;
        assert!(home.body.contains(r#"data-testid="nsfw-media""#));
    }

    #[tokio::test]
    async fn admin_can_mark_and_unmark_existing_media_post_as_nsfw_without_js() {
        let server = spawn_test_server_with_admin().await;
        let cookie = admin_session_cookie(&server).await;
        let home = get_with_cookie(&server, "/home", &cookie).await;
        let csrf = csrf_token(&home.body);
        let posted = request(
            &server.base_url,
            "POST",
            "/posts",
            &[
                ("cookie", &cookie),
                (
                    "content-type",
                    "multipart/form-data; boundary=post-boundary",
                ),
            ],
            multipart_body_with_file(
                "post-boundary",
                &[("csrf", csrf.as_str()), ("text", "admin toggle image")],
                "media",
                "photo.png",
                "image/png",
                &tiny_png_bytes(),
            ),
        )
        .await;
        assert_eq!(posted.status, 303);

        let home = get_with_cookie(&server, "/home", &cookie).await;
        assert!(home.body.contains("Mark NSFW"));
        let csrf = csrf_token(&home.body);
        let marked = post_form_with_cookie(
            &server,
            "/admin/posts/1/nsfw",
            &cookie,
            &format!("csrf={csrf}&nsfw=true"),
        )
        .await;
        assert_eq!(marked.status, 303);

        let home = get_with_cookie(&server, "/home", &cookie).await;
        assert!(home.body.contains(r#"data-testid="nsfw-media""#));
        assert!(home.body.contains("Unmark NSFW"));
        let csrf = csrf_token(&home.body);
        let unmarked = post_form_with_cookie(
            &server,
            "/admin/posts/1/nsfw",
            &cookie,
            &format!("csrf={csrf}&nsfw=false"),
        )
        .await;
        assert_eq!(unmarked.status, 303);

        let home = get_with_cookie(&server, "/home", &cookie).await;
        assert!(!home.body.contains(r#"data-testid="nsfw-media""#));
        assert!(home.body.contains("Mark NSFW"));
    }

    #[tokio::test]
    async fn global_nsfw_blur_setting_controls_logged_out_safe_default() {
        let server = spawn_test_server().await;
        let cookie = register_test_user(&server, "alice").await;
        let home = get_with_cookie(&server, "/home", &cookie).await;
        let csrf = csrf_token(&home.body);
        let posted = request(
            &server.base_url,
            "POST",
            "/posts",
            &[
                ("cookie", &cookie),
                (
                    "content-type",
                    "multipart/form-data; boundary=post-boundary",
                ),
            ],
            multipart_body_with_file(
                "post-boundary",
                &[
                    ("csrf", csrf.as_str()),
                    ("text", "logged out default image"),
                    ("nsfw", "true"),
                ],
                "media",
                "photo.png",
                "image/png",
                &tiny_png_bytes(),
            ),
        )
        .await;
        assert_eq!(posted.status, 303);

        let logged_out = request(&server.base_url, "GET", "/home", &[], Vec::new()).await;
        assert!(logged_out.body.contains(r#"data-testid="nsfw-media""#));

        let mut settings = Settings::load(&server.data_dir.join("settings.toml")).expect("load");
        settings.media.nsfw_blur_enabled = false;
        std::fs::write(
            server.data_dir.join("settings.toml"),
            toml::to_string(&settings).expect("settings toml"),
        )
        .expect("write settings");
        let unblurred = request(&server.base_url, "GET", "/home", &[], Vec::new()).await;
        assert!(!unblurred.body.contains(r#"data-testid="nsfw-media""#));

        settings.media.nsfw_blur_enabled = true;
        std::fs::write(
            server.data_dir.join("settings.toml"),
            toml::to_string(&settings).expect("settings toml"),
        )
        .expect("write settings");
        let safe_again = request(&server.base_url, "GET", "/home", &[], Vec::new()).await;
        assert!(safe_again.body.contains(r#"data-testid="nsfw-media""#));
    }

    #[tokio::test]
    async fn head_response_uses_compression_headers_without_body() {
        let server = spawn_test_server().await;

        let response = request(
            &server.base_url,
            "HEAD",
            "/home",
            &[("accept-encoding", "gzip")],
            Vec::new(),
        )
        .await;

        assert_eq!(response.status, 200);
        assert_header(&response, "content-encoding", "gzip");
        assert_vary_contains_accept_encoding(&response);
        assert_eq!(response.body_bytes.len(), 0);
        assert!(
            content_length(&response).is_some_and(|len| len > 0),
            "compressed HEAD response should keep the GET content length"
        );
    }

    #[tokio::test]
    async fn head_response_without_accept_encoding_keeps_get_length_without_body() {
        let server = spawn_test_server().await;
        let get = request(&server.base_url, "GET", "/home", &[], Vec::new()).await;
        let head = request(&server.base_url, "HEAD", "/home", &[], Vec::new()).await;

        assert_eq!(head.status, 200);
        assert_no_header(&head, "content-encoding");
        assert_eq!(head.body_bytes.len(), 0);
        assert_eq!(content_length(&head), Some(get.body_bytes.len()));
    }

    #[tokio::test]
    async fn head_response_for_skipped_favicon_keeps_length_without_compression() {
        let server = spawn_test_server().await;
        let get = request(
            &server.base_url,
            "GET",
            "/favicon.ico",
            &[("accept-encoding", "gzip")],
            Vec::new(),
        )
        .await;
        let head = request(
            &server.base_url,
            "HEAD",
            "/favicon.ico",
            &[("accept-encoding", "gzip")],
            Vec::new(),
        )
        .await;

        assert_eq!(head.status, 200);
        assert_no_header(&head, "content-encoding");
        assert_eq!(head.body_bytes.len(), 0);
        assert_eq!(content_length(&head), Some(get.body_bytes.len()));
    }

    #[tokio::test]
    async fn admin_can_upload_replace_and_remove_png_favicon() {
        let server = spawn_test_server_with_admin().await;
        let cookie = admin_session_cookie(&server).await;
        let admin = request(
            &server.base_url,
            "GET",
            "/admin",
            &[("cookie", &cookie)],
            Vec::new(),
        )
        .await;
        assert_eq!(admin.status, 200);
        assert!(admin.body.contains("Using built-in default favicon"));
        let csrf = csrf_token(&admin.body);

        let uploaded = request(
            &server.base_url,
            "POST",
            "/admin/favicon",
            &[
                ("cookie", &cookie),
                (
                    "content-type",
                    "multipart/form-data; boundary=favicon-boundary",
                ),
            ],
            multipart_body_with_file(
                "favicon-boundary",
                &[("csrf", csrf.as_str())],
                "favicon",
                "site.png",
                "image/png",
                &tiny_png_bytes(),
            ),
        )
        .await;
        assert_eq!(uploaded.status, 303);
        assert_eq!(location(&uploaded), "/admin");
        assert!(server.data_dir.join("assets/favicon.png").is_file());

        let favicon = request(&server.base_url, "GET", "/favicon.ico", &[], Vec::new()).await;
        assert_eq!(favicon.status, 200);
        assert_header(&favicon, "content-type", "image/png");

        let admin = request(
            &server.base_url,
            "GET",
            "/admin",
            &[("cookie", &cookie)],
            Vec::new(),
        )
        .await;
        assert!(admin.body.contains("Custom favicon configured"));
        assert!(admin.body.contains("Remove favicon"));
        let csrf = csrf_token(&admin.body);

        let replacement = request(
            &server.base_url,
            "POST",
            "/admin/favicon",
            &[
                ("cookie", &cookie),
                (
                    "content-type",
                    "multipart/form-data; boundary=favicon-boundary",
                ),
            ],
            multipart_body_with_file(
                "favicon-boundary",
                &[("csrf", csrf.as_str())],
                "favicon",
                "site.ico",
                "image/x-icon",
                &[0, 0, 1, 0, 1, 0],
            ),
        )
        .await;
        assert_eq!(replacement.status, 303);
        assert!(server.data_dir.join("assets/favicon.ico").is_file());
        assert!(!server.data_dir.join("assets/favicon.png").exists());

        let admin = request(
            &server.base_url,
            "GET",
            "/admin",
            &[("cookie", &cookie)],
            Vec::new(),
        )
        .await;
        let csrf = csrf_token(&admin.body);
        let removed = request(
            &server.base_url,
            "POST",
            "/admin/favicon/remove",
            &[
                ("cookie", &cookie),
                ("content-type", "application/x-www-form-urlencoded"),
            ],
            format!("csrf={csrf}").into_bytes(),
        )
        .await;
        assert_eq!(removed.status, 303);
        assert!(!server.data_dir.join("assets/favicon.ico").exists());
    }

    #[tokio::test]
    async fn non_admin_cannot_access_deep_server_settings() {
        let server = spawn_test_server().await;
        let register = request(
            &server.base_url,
            "POST",
            "/register",
            &[("content-type", "application/x-www-form-urlencoded")],
            b"username=member&password=very%20secure%20password&confirm_password=very%20secure%20password".to_vec(),
        )
        .await;
        assert_eq!(register.status, 303);
        let cookie = session_cookie(&register);

        let response = request(
            &server.base_url,
            "GET",
            "/admin/deep-settings",
            &[("cookie", &cookie)],
            Vec::new(),
        )
        .await;

        assert_eq!(response.status, 403);
    }

    #[tokio::test]
    async fn admin_get_renders_deep_server_settings_groups() {
        let server = spawn_test_server_with_admin().await;
        let cookie = admin_session_cookie(&server).await;

        let response = request(
            &server.base_url,
            "GET",
            "/admin/deep-settings",
            &[("cookie", &cookie)],
            Vec::new(),
        )
        .await;

        assert_eq!(response.status, 200);
        assert!(response.body.contains("Deep server settings"));
        assert!(response.body.contains("<legend>Site</legend>"));
        assert!(response.body.contains("<legend>Posts</legend>"));
        assert!(response.body.contains("<legend>Accounts</legend>"));
        assert!(response.body.contains("<legend>Media</legend>"));
        assert!(response.body.contains(r#"name="allow_reposts""#));
        assert!(
            response
                .body
                .contains(r#"name="registration_captcha_enabled""#)
        );
        assert!(response.body.contains("<select"));
        assert!(response.body.contains(r#"name="max_bio_len" type="text""#));
    }

    #[test]
    fn deep_settings_confirmation_forms_include_explicit_intents() {
        let values = crate::admin::DeepSettingsValues::from_settings(&Settings::default());
        let changes = [crate::admin::DeepSettingsChange {
            label: "Maximum bio length",
            old_value: "240 characters".to_owned(),
            new_value: "300 characters".to_owned(),
        }];

        let html = render_deep_settings_confirmation("csrf-token", &values, &changes, None);

        assert!(html.contains(r#"<input type="hidden" name="intent" value="confirm">"#));
        assert!(html.contains(r#"<input type="hidden" name="intent" value="discard">"#));
        assert!(html.contains(r#"<button class="primary" type="submit">Confirm/Save</button>"#));
        assert!(html.contains(r#"<button type="submit">Discard Changes</button>"#));
    }

    #[tokio::test]
    async fn admin_save_preview_shows_changed_values_without_writing_settings() {
        let server = spawn_test_server_with_admin().await;
        let cookie = admin_session_cookie(&server).await;
        let page = request(
            &server.base_url,
            "GET",
            "/admin/deep-settings",
            &[("cookie", &cookie)],
            Vec::new(),
        )
        .await;
        let csrf = csrf_token(&page.body);
        let before =
            std::fs::read_to_string(server.data_dir.join("settings.toml")).expect("settings");

        let response = request(
            &server.base_url,
            "POST",
            "/admin/deep-settings",
            &[
                ("cookie", &cookie),
                ("content-type", "application/x-www-form-urlencoded"),
            ],
            deep_settings_form_body(
                &csrf,
                "preview",
                &[("max_bio_len", "300"), ("allow_profile_pictures", "false")],
            ),
        )
        .await;
        let after =
            std::fs::read_to_string(server.data_dir.join("settings.toml")).expect("settings");

        assert_eq!(response.status, 200);
        assert_eq!(before, after);
        assert!(
            response
                .body
                .contains("These settings are about to be changed")
        );
        assert!(response.body.contains("Maximum bio length"));
        assert!(
            response
                .body
                .contains("240 characters -&gt; 300 characters")
        );
        assert!(response.body.contains("Allow profile pictures"));
        assert!(response.body.contains("true -&gt; false"));
    }

    #[tokio::test]
    async fn admin_discard_returns_to_persisted_deep_settings_without_writing() {
        let server = spawn_test_server_with_admin().await;
        let cookie = admin_session_cookie(&server).await;
        let page = request(
            &server.base_url,
            "GET",
            "/admin/deep-settings",
            &[("cookie", &cookie)],
            Vec::new(),
        )
        .await;
        let csrf = csrf_token(&page.body);
        let before =
            std::fs::read_to_string(server.data_dir.join("settings.toml")).expect("settings");

        let response = request(
            &server.base_url,
            "POST",
            "/admin/deep-settings",
            &[
                ("cookie", &cookie),
                ("content-type", "application/x-www-form-urlencoded"),
            ],
            deep_settings_form_body(&csrf, "discard", &[("max_bio_len", "300")]),
        )
        .await;
        let after =
            std::fs::read_to_string(server.data_dir.join("settings.toml")).expect("settings");

        assert_eq!(response.status, 200);
        assert_eq!(before, after);
        assert!(response.body.contains("Changes discarded."));
        assert!(response.body.contains(
            r#"name="max_bio_len" type="text" inputmode="numeric" pattern="[0-9]+" value="240""#
        ));
    }

    #[tokio::test]
    async fn admin_confirm_writes_deep_settings_and_fresh_load_shows_saved_values() {
        let server = spawn_test_server_with_admin().await;
        let cookie = admin_session_cookie(&server).await;
        let page = request(
            &server.base_url,
            "GET",
            "/admin/deep-settings",
            &[("cookie", &cookie)],
            Vec::new(),
        )
        .await;
        let csrf = csrf_token(&page.body);

        let response = request(
            &server.base_url,
            "POST",
            "/admin/deep-settings",
            &[
                ("cookie", &cookie),
                ("content-type", "application/x-www-form-urlencoded"),
            ],
            deep_settings_form_body(
                &csrf,
                "confirm",
                &[
                    ("max_bio_len", "300"),
                    ("allow_profile_pictures", "false"),
                    ("nsfw_blur_enabled", "false"),
                    ("registration_captcha_enabled", "true"),
                ],
            ),
        )
        .await;
        let saved = Settings::load(&server.data_dir.join("settings.toml")).expect("settings");

        assert_eq!(response.status, 200);
        assert!(response.body.contains("Settings saved successfully"));
        assert_eq!(saved.accounts.max_bio_len, 300);
        assert!(!saved.accounts.allow_profile_pictures);
        assert!(!saved.media.nsfw_blur_enabled);
        assert!(saved.accounts.registration_captcha_enabled);

        let fresh = request(
            &server.base_url,
            "GET",
            "/admin/deep-settings",
            &[("cookie", &cookie)],
            Vec::new(),
        )
        .await;
        assert!(fresh.body.contains(
            r#"name="max_bio_len" type="text" inputmode="numeric" pattern="[0-9]+" value="300""#
        ));
        assert!(
            fresh
                .body
                .contains(r#"<option value="false" selected>false</option>"#)
        );
    }

    #[tokio::test]
    async fn invalid_deep_settings_submission_shows_error_without_writing() {
        let server = spawn_test_server_with_admin().await;
        let cookie = admin_session_cookie(&server).await;
        let page = request(
            &server.base_url,
            "GET",
            "/admin/deep-settings",
            &[("cookie", &cookie)],
            Vec::new(),
        )
        .await;
        let csrf = csrf_token(&page.body);
        let before =
            std::fs::read_to_string(server.data_dir.join("settings.toml")).expect("settings");

        let response = request(
            &server.base_url,
            "POST",
            "/admin/deep-settings",
            &[
                ("cookie", &cookie),
                ("content-type", "application/x-www-form-urlencoded"),
            ],
            deep_settings_form_body(&csrf, "preview", &[("min_password_length", "-1")]),
        )
        .await;
        let after =
            std::fs::read_to_string(server.data_dir.join("settings.toml")).expect("settings");

        assert_eq!(response.status, 200);
        assert_eq!(before, after);
        assert!(
            response
                .body
                .contains("Minimum password length must not be negative")
        );
    }

    #[tokio::test]
    async fn favicon_upload_rejects_unsupported_content_and_unsafe_names() {
        let server = spawn_test_server_with_admin().await;
        let cookie = admin_session_cookie(&server).await;
        let admin = request(
            &server.base_url,
            "GET",
            "/admin",
            &[("cookie", &cookie)],
            Vec::new(),
        )
        .await;
        let csrf = csrf_token(&admin.body);

        let unsupported = request(
            &server.base_url,
            "POST",
            "/admin/favicon",
            &[
                ("cookie", &cookie),
                (
                    "content-type",
                    "multipart/form-data; boundary=favicon-boundary",
                ),
            ],
            multipart_body_with_file(
                "favicon-boundary",
                &[("csrf", csrf.as_str())],
                "favicon",
                "favicon.gif",
                "image/gif",
                b"GIF89a",
            ),
        )
        .await;
        assert_eq!(unsupported.status, 400);
        assert!(unsupported.body.contains("unsupported favicon type"));
        assert!(!server.data_dir.join("assets/favicon.gif").exists());

        let invalid_png = request(
            &server.base_url,
            "POST",
            "/admin/favicon",
            &[
                ("cookie", &cookie),
                (
                    "content-type",
                    "multipart/form-data; boundary=favicon-boundary",
                ),
            ],
            multipart_body_with_file(
                "favicon-boundary",
                &[("csrf", csrf.as_str())],
                "favicon",
                "favicon.png",
                "image/png",
                b"<html></html>",
            ),
        )
        .await;
        assert_eq!(invalid_png.status, 400);
        assert!(invalid_png.body.contains("invalid file signature"));
        assert!(!server.data_dir.join("assets/favicon.png").exists());

        let traversal = request(
            &server.base_url,
            "POST",
            "/admin/favicon",
            &[
                ("cookie", &cookie),
                (
                    "content-type",
                    "multipart/form-data; boundary=favicon-boundary",
                ),
            ],
            multipart_body_with_file(
                "favicon-boundary",
                &[("csrf", csrf.as_str())],
                "favicon",
                "../favicon.png",
                "image/png",
                &tiny_png_bytes(),
            ),
        )
        .await;
        assert_eq!(traversal.status, 400);
        assert!(traversal.body.contains("unsafe upload filename"));
        assert!(!server.data_dir.join("assets/favicon.png").exists());
    }

    #[tokio::test]
    async fn delete_requires_confirmation_and_preserves_cancel_target() {
        let server = spawn_test_server().await;
        let registered = request(
            &server.base_url,
            "POST",
            "/register",
            &[("content-type", "application/x-www-form-urlencoded")],
            b"username=alice&password=very%20secure%20password&confirm_password=very%20secure%20password".to_vec(),
        )
        .await;
        let cookie = session_cookie(&registered);
        let home = request(
            &server.base_url,
            "GET",
            "/home",
            &[("cookie", &cookie)],
            Vec::new(),
        )
        .await;
        let csrf = csrf_token(&home.body);
        let posted = request(
            &server.base_url,
            "POST",
            "/posts",
            &[
                ("cookie", &cookie),
                (
                    "content-type",
                    "multipart/form-data; boundary=post-boundary",
                ),
            ],
            multipart_body(
                "post-boundary",
                &[("csrf", csrf.as_str()), ("text", "delete me")],
                false,
            ),
        )
        .await;
        assert_eq!(posted.status, 303);

        let confirm = request(
            &server.base_url,
            "GET",
            "/posts/1/delete",
            &[("cookie", &cookie), ("referer", "/home")],
            Vec::new(),
        )
        .await;
        assert_eq!(confirm.status, 200);
        assert!(confirm.body.contains("Delete post?"));
        assert!(confirm.body.contains("Confirm delete"));
        assert!(confirm.body.contains(r#"href="/home#post-1""#));
        let csrf = csrf_token(&confirm.body);
        let deleted = request(
            &server.base_url,
            "POST",
            "/posts/1/delete",
            &[
                ("cookie", &cookie),
                ("content-type", "application/x-www-form-urlencoded"),
            ],
            format!("csrf={csrf}&return_to=/home%23post-1").into_bytes(),
        )
        .await;
        assert_eq!(deleted.status, 303);
        assert_eq!(location(&deleted), "/home#post-1");
    }

    #[tokio::test]
    async fn delete_account_requires_server_side_intent() {
        let server = spawn_test_server().await;
        let registered = request(
            &server.base_url,
            "POST",
            "/register",
            &[("content-type", "application/x-www-form-urlencoded")],
            b"username=alice&password=very%20secure%20password&confirm_password=very%20secure%20password".to_vec(),
        )
        .await;
        assert_eq!(registered.status, 303);
        let cookie = session_cookie(&registered);
        let home = request(
            &server.base_url,
            "GET",
            "/home",
            &[("cookie", &cookie)],
            Vec::new(),
        )
        .await;
        let csrf = csrf_token(&home.body);

        let deleted = request(
            &server.base_url,
            "POST",
            "/settings/delete/confirm",
            &[
                ("cookie", &cookie),
                ("content-type", "application/x-www-form-urlencoded"),
            ],
            format!("csrf={csrf}&password=very%20secure%20password").into_bytes(),
        )
        .await;

        assert_eq!(deleted.status, 400);
        assert!(deleted.body.contains("Delete confirmation expired"));
        let login = request(
            &server.base_url,
            "POST",
            "/login",
            &[("content-type", "application/x-www-form-urlencoded")],
            b"username=alice&password=very%20secure%20password".to_vec(),
        )
        .await;
        assert_eq!(login.status, 303);
    }

    #[tokio::test]
    async fn delete_account_intent_and_password_control_final_delete() {
        let server = spawn_test_server().await;
        let registered = request(
            &server.base_url,
            "POST",
            "/register",
            &[("content-type", "application/x-www-form-urlencoded")],
            b"username=alice&password=very%20secure%20password&confirm_password=very%20secure%20password".to_vec(),
        )
        .await;
        assert_eq!(registered.status, 303);
        let cookie = session_cookie(&registered);
        let confirm = request(
            &server.base_url,
            "GET",
            "/settings/delete/confirm",
            &[("cookie", &cookie)],
            Vec::new(),
        )
        .await;
        assert_eq!(confirm.status, 200);
        let csrf = csrf_token(&confirm.body);
        let delete_intent = hidden_value(&confirm.body, "delete_intent");

        let wrong_password = request(
            &server.base_url,
            "POST",
            "/settings/delete/confirm",
            &[
                ("cookie", &cookie),
                ("content-type", "application/x-www-form-urlencoded"),
            ],
            format!("csrf={csrf}&delete_intent={delete_intent}&password=wrong").into_bytes(),
        )
        .await;
        assert_eq!(wrong_password.status, 401);
        assert!(wrong_password.body.contains("Password is incorrect."));

        let reused_intent = request(
            &server.base_url,
            "POST",
            "/settings/delete/confirm",
            &[
                ("cookie", &cookie),
                ("content-type", "application/x-www-form-urlencoded"),
            ],
            format!("csrf={csrf}&delete_intent={delete_intent}&password=very%20secure%20password")
                .into_bytes(),
        )
        .await;
        assert_eq!(reused_intent.status, 400);
        assert!(reused_intent.body.contains("Delete confirmation expired"));

        let confirm = request(
            &server.base_url,
            "GET",
            "/settings/delete/confirm",
            &[("cookie", &cookie)],
            Vec::new(),
        )
        .await;
        assert_eq!(confirm.status, 200);
        let csrf = csrf_token(&confirm.body);
        let delete_intent = hidden_value(&confirm.body, "delete_intent");
        let deleted = request(
            &server.base_url,
            "POST",
            "/settings/delete/confirm",
            &[
                ("cookie", &cookie),
                ("content-type", "application/x-www-form-urlencoded"),
            ],
            format!("csrf={csrf}&delete_intent={delete_intent}&password=very%20secure%20password")
                .into_bytes(),
        )
        .await;

        assert_eq!(deleted.status, 303);
        assert_eq!(location(&deleted), "/account-deleted");
        assert!(
            deleted
                .headers
                .iter()
                .any(|(name, value)| name == "set-cookie" && value.contains("Max-Age=0"))
        );
        let login = request(
            &server.base_url,
            "POST",
            "/login",
            &[("content-type", "application/x-www-form-urlencoded")],
            b"username=alice&password=very%20secure%20password".to_vec(),
        )
        .await;
        assert_eq!(login.status, 401);
    }

    #[tokio::test]
    async fn delete_account_route_succeeds_after_file_cleanup_failure() {
        let server = spawn_test_server().await;
        let registered = request(
            &server.base_url,
            "POST",
            "/register",
            &[("content-type", "application/x-www-form-urlencoded")],
            b"username=alice&password=very%20secure%20password&confirm_password=very%20secure%20password".to_vec(),
        )
        .await;
        assert_eq!(registered.status, 303);
        let cookie = session_cookie(&registered);
        let paths = RuntimePaths::from_data_dir(server.data_dir.clone());
        let media_path = paths.uploads_images.join("directory-media");
        std::fs::create_dir(&media_path).expect("media directory");
        let media_path_string = media_path.to_string_lossy().to_string();
        let conn = rusqlite::Connection::open(&paths.database_path).expect("open db");
        let alice: i64 = conn
            .query_row(
                "SELECT id FROM users WHERE normalized_username = 'alice'",
                [],
                |row| row.get(0),
            )
            .expect("alice id");
        conn.execute(
            "INSERT INTO media (owner_user_id, original_filename, stored_path, public_path, mime_type, media_kind, byte_len) VALUES (?, 'directory-media', ?, '/uploads/images/directory-media', 'image/webp', 'image', 1)",
            params![alice, media_path_string],
        )
        .expect("media row");
        drop(conn);
        let confirm = request(
            &server.base_url,
            "GET",
            "/settings/delete/confirm",
            &[("cookie", &cookie)],
            Vec::new(),
        )
        .await;
        assert_eq!(confirm.status, 200);
        let csrf = csrf_token(&confirm.body);
        let delete_intent = hidden_value(&confirm.body, "delete_intent");

        let deleted = request(
            &server.base_url,
            "POST",
            "/settings/delete/confirm",
            &[
                ("cookie", &cookie),
                ("content-type", "application/x-www-form-urlencoded"),
            ],
            format!("csrf={csrf}&delete_intent={delete_intent}&password=very%20secure%20password")
                .into_bytes(),
        )
        .await;

        assert_eq!(deleted.status, 303);
        assert_eq!(location(&deleted), "/account-deleted");
        assert!(media_path.exists());
        let conn = rusqlite::Connection::open(&paths.database_path).expect("open db");
        let users: i64 = conn
            .query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))
            .expect("users count");
        assert_eq!(users, 0);
    }

    fn multipart_body(
        boundary: &str,
        fields: &[(&str, &str)],
        include_empty_media: bool,
    ) -> Vec<u8> {
        let mut body = String::new();
        for (name, value) in fields {
            body.push_str(&format!("--{boundary}\r\n"));
            body.push_str(&format!(
                "Content-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            ));
        }
        if include_empty_media {
            body.push_str(&format!("--{boundary}\r\n"));
            body.push_str("Content-Disposition: form-data; name=\"media\"; filename=\"\"\r\n");
            body.push_str("Content-Type: application/octet-stream\r\n\r\n\r\n");
        }
        body.push_str(&format!("--{boundary}--\r\n"));
        body.into_bytes()
    }

    fn multipart_body_with_file(
        boundary: &str,
        fields: &[(&str, &str)],
        file_field: &str,
        filename: &str,
        content_type: &str,
        file: &[u8],
    ) -> Vec<u8> {
        let mut body = multipart_body_without_close(boundary, fields);
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"{file_field}\"; filename=\"{filename}\"\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(format!("Content-Type: {content_type}\r\n\r\n").as_bytes());
        body.extend_from_slice(file);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        body
    }

    fn multipart_body_without_close(boundary: &str, fields: &[(&str, &str)]) -> Vec<u8> {
        let mut body = Vec::new();
        for (name, value) in fields {
            body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n")
                    .as_bytes(),
            );
        }
        body
    }

    fn tiny_png_bytes() -> Vec<u8> {
        vec![
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9c, 0x63, 0xf8, 0xcf, 0xc0, 0x00, 0x00, 0x04, 0x00, 0x01, 0xfe, 0xa7, 0x69, 0x9d,
            0x16, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ]
    }

    async fn register_test_user(server: &TestServer, username: &str) -> String {
        let body = format!(
            "username={}&password=very%20secure%20password&confirm_password=very%20secure%20password",
            form_encode(username)
        );
        let response = request(
            &server.base_url,
            "POST",
            "/register",
            &[("content-type", "application/x-www-form-urlencoded")],
            body.into_bytes(),
        )
        .await;
        assert_eq!(response.status, 303);
        session_cookie(&response)
    }

    async fn get_with_cookie(server: &TestServer, path: &str, cookie: &str) -> TestResponse {
        request(
            &server.base_url,
            "GET",
            path,
            &[("cookie", cookie)],
            Vec::new(),
        )
        .await
    }

    async fn post_form_with_cookie(
        server: &TestServer,
        path: &str,
        cookie: &str,
        body: &str,
    ) -> TestResponse {
        request(
            &server.base_url,
            "POST",
            path,
            &[
                ("cookie", cookie),
                ("content-type", "application/x-www-form-urlencoded"),
            ],
            body.as_bytes().to_vec(),
        )
        .await
    }

    async fn create_text_post(server: &TestServer, cookie: &str, text: &str) {
        let home = get_with_cookie(server, "/home", cookie).await;
        let csrf = csrf_token(&home.body);
        let posted = request(
            &server.base_url,
            "POST",
            "/posts",
            &[
                ("cookie", cookie),
                (
                    "content-type",
                    "multipart/form-data; boundary=post-boundary",
                ),
            ],
            multipart_body(
                "post-boundary",
                &[("csrf", csrf.as_str()), ("text", text)],
                false,
            ),
        )
        .await;
        assert_eq!(posted.status, 303);
    }

    fn assert_populated_notifications_page(body: &str) {
        assert!(body.contains("4 unread notifications"));
        assert!(
            body.contains(
                r#"<span class="nav-badge" aria-label="4 unread notifications">4</span>"#
            )
        );
        assert!(body.contains(r#"<h2 class="notification-group">New</h2>"#));
        assert_eq!(
            body.matches(r#"class="notification-row unread""#).count(),
            4
        );
        assert!(body.contains(">bob</a> <span class=\"username\">@bob</span>"));
        assert!(body.contains("replied to your post"));
        assert!(body.contains("liked your post"));
        assert!(body.contains("reposted your post"));
        assert!(body.contains("followed you"));
        assert!(body.contains("alice original post"));
        assert!(body.contains(r#"data-card-href="/posts/1""#));
        assert!(body.contains(r#"data-card-href="/posts/2""#));
        assert!(body.contains(r#"data-card-href="/users/bob""#));
    }

    fn quote_form_body(response: &TestResponse, text: &str) -> String {
        let csrf = csrf_token(&response.body);
        format!("csrf={}&text={}", form_encode(&csrf), form_encode(text))
    }

    async fn spawn_test_server() -> TestServer {
        spawn_test_server_inner(false, Settings::default()).await
    }

    async fn spawn_test_server_with_admin() -> TestServer {
        spawn_test_server_inner(true, Settings::default()).await
    }

    async fn spawn_test_server_with_settings(settings: Settings) -> TestServer {
        spawn_test_server_inner(false, settings).await
    }

    async fn spawn_test_server_inner(create_admin: bool, settings: Settings) -> TestServer {
        let temp = tempfile::tempdir().expect("temp dir");
        let paths = RuntimePaths::from_data_dir(temp.path().to_path_buf());
        paths.ensure().expect("paths");
        crate::config::write_default_if_missing(&paths.settings_path).expect("settings");
        std::fs::write(
            &paths.settings_path,
            toml::to_string(&settings).expect("serialize settings"),
        )
        .expect("write test settings");
        let data_dir = paths.data_dir.clone();
        let pool = crate::db::connect(&paths.database_path)
            .await
            .expect("connect");
        crate::db::migrate(&pool).await.expect("migrate");
        if create_admin {
            crate::admin::create_admin(&pool, &settings, "siteowner", "very secure password")
                .await
                .expect("admin");
        }
        let ffmpeg = FfmpegStatus {
            available: false,
            version: String::new(),
            supports_webp: false,
            supports_vp9: false,
            error: Some("disabled in tests".to_owned()),
        };
        let tor = crate::tor::validate_startup(&settings.tor);
        let state = AppState::new(pool.clone(), settings, paths, ffmpeg, tor);
        let registration_captcha = state.registration_captcha.clone();
        let app = router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .expect("serve");
        });
        TestServer {
            base_url: format!("127.0.0.1:{}", addr.port()),
            data_dir,
            pool,
            registration_captcha,
            _task: task,
            _temp: temp,
        }
    }

    async fn admin_session_cookie(server: &TestServer) -> String {
        let login = request(
            &server.base_url,
            "POST",
            "/login",
            &[("content-type", "application/x-www-form-urlencoded")],
            b"username=siteowner&password=very%20secure%20password".to_vec(),
        )
        .await;
        assert_eq!(login.status, 303);
        session_cookie(&login)
    }

    async fn request(
        base_url: &str,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Vec<u8>,
    ) -> TestResponse {
        let mut stream = tokio::net::TcpStream::connect(base_url)
            .await
            .expect("connect");
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {base_url}\r\nConnection: close\r\nContent-Length: {}\r\n",
            body.len()
        );
        for (name, value) in headers {
            request.push_str(name);
            request.push_str(": ");
            request.push_str(value);
            request.push_str("\r\n");
        }
        request.push_str("\r\n");
        stream
            .write_all(request.as_bytes())
            .await
            .expect("write headers");
        stream.write_all(&body).await.expect("write body");
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).await.expect("read");
        parse_response(&bytes)
    }

    fn parse_response(bytes: &[u8]) -> TestResponse {
        let split = bytes
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("response split");
        let head = String::from_utf8_lossy(&bytes[..split]);
        let body_bytes = bytes[split + 4..].to_vec();
        let body = String::from_utf8_lossy(&body_bytes).into_owned();
        let mut lines = head.lines();
        let status = lines
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|value| value.parse::<u16>().ok())
            .expect("status");
        let headers = lines
            .filter_map(|line| {
                let (name, value) = line.split_once(':')?;
                Some((name.to_ascii_lowercase(), value.trim().to_owned()))
            })
            .collect();
        TestResponse {
            status,
            headers,
            body_bytes,
            body,
        }
    }

    fn session_cookie(response: &TestResponse) -> String {
        response
            .headers
            .iter()
            .find(|(name, _)| name == "set-cookie")
            .map(|(_, value)| value.split(';').next().unwrap_or_default().to_owned())
            .expect("session cookie")
    }

    fn location(response: &TestResponse) -> &str {
        response
            .headers
            .iter()
            .find(|(name, _)| name == "location")
            .map(|(_, value)| value.as_str())
            .expect("location")
    }

    fn assert_header(response: &TestResponse, name: &str, expected: &str) {
        assert_eq!(header_value(response, name), Some(expected));
    }

    fn assert_no_header(response: &TestResponse, name: &str) {
        assert_eq!(header_value(response, name), None);
    }

    fn assert_vary_contains_accept_encoding(response: &TestResponse) {
        let vary = header_value(response, "vary").expect("vary header");
        assert!(
            vary.split(',')
                .any(|part| part.trim().eq_ignore_ascii_case("accept-encoding")),
            "Vary header should include Accept-Encoding: {vary}"
        );
    }

    fn header_value<'a>(response: &'a TestResponse, name: &str) -> Option<&'a str> {
        response
            .headers
            .iter()
            .find(|(header_name, _)| header_name == name)
            .map(|(_, value)| value.as_str())
    }

    fn content_length(response: &TestResponse) -> Option<usize> {
        header_value(response, "content-length")?.parse().ok()
    }

    fn gzip_decode(bytes: &[u8]) -> String {
        let mut decoder = GzDecoder::new(bytes);
        let mut output = String::new();
        decoder.read_to_string(&mut output).expect("gzip body");
        output
    }

    fn deep_settings_form_body(csrf: &str, intent: &str, overrides: &[(&str, &str)]) -> Vec<u8> {
        let values = crate::admin::DeepSettingsValues::from_settings(&Settings::default());
        let mut pairs = vec![
            ("csrf".to_owned(), csrf.to_owned()),
            ("intent".to_owned(), intent.to_owned()),
        ];
        for field in crate::admin::DeepSettingsField::ALL {
            let value = overrides
                .iter()
                .find(|(name, _)| *name == field.form_name())
                .map_or_else(
                    || values.form_value(field),
                    |(_, value)| (*value).to_owned(),
                );
            pairs.push((field.form_name().to_owned(), value));
        }
        pairs
            .iter()
            .map(|(name, value)| format!("{}={}", form_encode(name), form_encode(value)))
            .collect::<Vec<_>>()
            .join("&")
            .into_bytes()
    }

    fn registration_body(
        username: &str,
        captcha_token: Option<&str>,
        captcha_answer: Option<&str>,
    ) -> Vec<u8> {
        let mut pairs = vec![
            ("username", username),
            ("password", "very secure password"),
            ("confirm_password", "very secure password"),
        ];
        if let Some(token) = captcha_token {
            pairs.push(("captcha_token", token));
        }
        if let Some(answer) = captcha_answer {
            pairs.push(("captcha_answer", answer));
        }
        pairs
            .iter()
            .map(|(name, value)| format!("{}={}", form_encode(name), form_encode(value)))
            .collect::<Vec<_>>()
            .join("&")
            .into_bytes()
    }

    fn form_encode(value: &str) -> String {
        let mut encoded = String::new();
        for byte in value.bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    encoded.push(char::from(byte));
                }
                b' ' => encoded.push('+'),
                _ => {
                    let _ = write!(encoded, "%{byte:02X}");
                }
            }
        }
        encoded
    }

    fn csrf_token(body: &str) -> String {
        let marker = r#"name="csrf" value=""#;
        let start = body.find(marker).expect("csrf marker") + marker.len();
        let end = body[start..].find('"').expect("csrf end") + start;
        body[start..end].to_owned()
    }

    fn hidden_value(body: &str, name: &str) -> String {
        let marker = format!(r#"name="{name}" value=""#);
        let start = body.find(&marker).expect("hidden marker") + marker.len();
        let end = body[start..].find('"').expect("hidden end") + start;
        body[start..end].to_owned()
    }
}
