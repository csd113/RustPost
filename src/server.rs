use std::net::SocketAddr;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::connect_info::ConnectInfo;
use axum::extract::{Form, Multipart, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, Uri, header};
use axum::response::{Html, IntoResponse as _, Redirect, Response};
use axum::routing::{get, post};
use rusqlite::{OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;

use crate::auth::{self, CurrentUser};
use crate::config::Settings;
use crate::db::SqlitePool;
use crate::errors::{AppError, AppResult};
use crate::ffmpeg::FfmpegStatus;
use crate::runtime::RuntimePaths;
use crate::{admin, backup, csrf, media, rate_limit, render, social};

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub settings: Settings,
    pub paths: RuntimePaths,
    pub ffmpeg: FfmpegStatus,
    pub tor: crate::tor::TorStatus,
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
        })
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(home))
        .route("/assets/rustpost.js", get(client_script))
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
        .route("/posts/{id}/reply", post(reply_redirect))
        .route("/users/{username}", get(profile))
        .route("/users/{id}/follow", post(follow))
        .route("/users/{id}/unfollow", post(unfollow))
        .route("/users/{id}/block", post(block))
        .route("/users/{id}/unblock", post(unblock))
        .route("/users/{id}/mute", post(mute))
        .route("/settings", get(settings_form).post(settings_update))
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
        .route("/admin/health", get(admin_health))
        .route("/admin/media", get(admin_media))
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
        .layer(TraceLayer::new_for_http())
        .with_state(state)
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
}

#[derive(Deserialize)]
struct CsrfForm {
    csrf: String,
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

struct ParsedProfileUpdate {
    csrf_token: String,
    display_name: String,
    bio: String,
    website: String,
    delete_profile_picture: bool,
    delete_banner: bool,
    profile_picture_media_id: Option<i64>,
    banner_media_id: Option<i64>,
}

struct ParsedPostCreate {
    csrf_token: String,
    text: String,
    parent_post_id: Option<i64>,
    media_ids: Vec<i64>,
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
        render::composer(csrf.as_deref(), None)
    } else {
        String::new()
    };
    let body = format!(
        "{}{}{}",
        render::page_header("Home Feed", "All posts"),
        composer,
        render::posts(&posts, user.as_ref(), csrf.as_deref())
    );
    Ok(Html(
        page_layout(&state, user.as_ref(), csrf.as_deref(), "Home Feed", &body).await?,
    ))
}

async fn layout_context(
    state: &AppState,
    user: Option<&CurrentUser>,
) -> AppResult<render::LayoutContext> {
    let counts = if let Some(user) = user {
        Some(social::follow_counts(&state.pool, user.id).await?)
    } else {
        None
    };
    Ok(render::LayoutContext {
        anonymous_mode_enabled: state.settings.accounts.anonymous_mode_enabled,
        tor_onion_address: state.tor.onion_address(),
        follower_count: counts.map(|(followers, _following)| followers),
        following_count: counts.map(|(_followers, following)| following),
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

async fn login_form(State(state): State<Arc<AppState>>) -> Html<String> {
    let body = render::login_form(None);
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
    let Some(session) = auth::login(&state.pool, &form.username, &form.password).await? else {
        rate_limit::record(&state.pool, rate_limit::Scope::FailedLogin, &actor).await?;
        return Err(AppError::Unauthorized);
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
    Ok(Html(
        page_layout(&state, None, None, "Register", &render::register_form(None)).await?,
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
    let user_id = auth::register_user(
        &state.pool,
        &state.settings,
        &form.username,
        &form.password,
        false,
    )
    .await
    .map_err(|err| AppError::BadRequest(err.to_string()))?;
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
    let form = parse_post_create(&state, user.as_ref().map(|u| u.id), multipart).await?;
    if let Some(parent_id) = form.parent_post_id {
        ensure_parent_post_exists(&state.pool, parent_id).await?;
    }
    if user.is_some() {
        validate_csrf(&state.pool, &headers, &form.csrf_token).await?;
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
                render::thread_post_card(
                    post,
                    user.as_ref(),
                    form_csrf(&state, &headers).await.as_deref(),
                )
            } else {
                render::post_card(
                    post,
                    user.as_ref(),
                    form_csrf(&state, &headers).await.as_deref(),
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
        render::composer(csrf.as_deref(), Some(id))
    } else {
        String::new()
    };
    let body = format!(
        "{}{}{}",
        render::page_header("Thread", "Read the conversation and add a reply."),
        render::thread_posts(&posts, user.as_ref(), csrf.as_deref()),
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
    let fallback = anchored_return(&headers, id, preview.parent_post_id.is_some(), "/home");
    let return_to = query
        .return_to
        .as_deref()
        .and_then(safe_return_target)
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
    let preview = delete_preview(&state.pool, user.id, user.is_admin, id).await?;
    social::delete_post(&state.pool, user.id, id, user.is_admin).await?;
    let fallback = if let Some(parent_id) = preview.parent_post_id {
        format!("/posts/{parent_id}#post-{parent_id}")
    } else {
        "/home".to_owned()
    };
    let target = safe_return_target(&form.return_to).unwrap_or(fallback);
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

async fn reply_redirect(Path(id): Path<i64>) -> Redirect {
    Redirect::to(&format!("/posts/{id}"))
}

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
        SELECT u.id, u.username, u.display_name, u.bio, u.website,
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
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                    ))
                },
            )
            .optional()
            .map_err(Into::into)
        })
        .await?;
    let Some((profile_id, profile_username, display_name, bio, website, picture_path, banner_path)) =
        profile
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
    let body = format!(
        r#"<section class="panel profile">{}<div class="profile-heading">{}<div class="profile-main"><div class="profile-title-row"><div><h1>{}</h1><p class="muted">@{}</p></div>{}</div><p class="counts"><span data-profile-followers="{}">{} followers</span><span data-profile-following="{}">{} following</span></p><p>{}</p>{}</div></div></section>{}"#,
        banner,
        picture,
        html_escape::encode_text(display_name.as_str()),
        html_escape::encode_text(profile_username.as_str()),
        controls,
        profile_id,
        followers,
        profile_id,
        following,
        html_escape::encode_text(bio.as_str()),
        website_link,
        render::posts(&posts, user.as_ref(), csrf.as_deref())
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
    Ok(Redirect::to("/settings").into_response())
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

async fn settings_form(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AppResult<Html<String>> {
    let user = require_user(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers).await.unwrap_or_default();
    let profile = state
        .pool
        .call(move |conn| {
            conn.query_row(
                r#"
        SELECT u.display_name, u.bio, u.website,
          pic.public_path AS profile_picture_path,
          banner.public_path AS banner_path
        FROM users u
        LEFT JOIN media pic ON pic.id = u.profile_picture_media_id
        LEFT JOIN media banner ON banner.id = u.banner_media_id
        WHERE u.id = ?
        "#,
                [user.id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .map_err(Into::into)
        })
        .await?;
    let (display_name, bio, website, picture_path, banner_path) = profile;
    let picture = picture_path.map_or_else(String::new, |path| {
            format!(
            r#"<img class="profile-picture" src="{}" alt=""><label class="check-row"><input type="checkbox" name="delete_profile_picture" value="true"> Delete profile picture</label>"#,
                html_escape::encode_double_quoted_attribute(&path)
            )
        });
    let banner = banner_path.map_or_else(String::new, |path| {
            format!(
                r#"<img class="profile-banner" src="{}" alt=""><label class="check-row"><input type="checkbox" name="delete_banner" value="true"> Delete banner</label>"#,
                html_escape::encode_double_quoted_attribute(&path)
            )
        });
    let banner_input = if state.settings.accounts.allow_profile_banners {
        format!(
            r#"{banner}<label for="banner">Banner</label><input id="banner" name="banner" type="file" accept="image/*">"#
        )
    } else {
        r#"<p>Profile banners are disabled.</p>"#.to_owned()
    };
    let picture_input = if state.settings.accounts.allow_profile_pictures {
        format!(
            r#"{picture}<label for="profile_picture">Profile picture</label><input id="profile_picture" name="profile_picture" type="file" accept="image/*">"#
        )
    } else {
        r#"<p>Profile pictures are disabled.</p>"#.to_owned()
    };
    let blocked = social::blocked_users(&state.pool, user.id).await?;
    let blocked_rows = blocked
        .into_iter()
        .map(|(id, username, display_name)| {
            format!(
                r#"<li><span><strong>{}</strong> <span class="muted">@{}</span></span>{}</li>"#,
                html_escape::encode_text(&display_name),
                html_escape::encode_text(&username),
                small_form(
                    &format!("/users/{id}/unblock"),
                    &csrf,
                    "Unblock",
                    "Unblock this account",
                )
            )
        })
        .collect::<Vec<_>>()
        .join("");
    let blocked_panel = if blocked_rows.is_empty() {
        render::empty_state("No blocked users", "Blocked accounts will appear here.")
    } else {
        format!(r#"<ul class="item-list">{blocked_rows}</ul>"#)
    };
    let body = format!(
        r#"<section class="panel"><h1>Account settings</h1><form method="post" enctype="multipart/form-data"><input type="hidden" name="csrf" value="{}"><label for="display_name">Display name</label><input id="display_name" name="display_name" value="{}"><label for="bio">Bio</label><textarea id="bio" name="bio">{}</textarea><label for="website">Website</label><input id="website" type="url" name="website" value="{}">{}{}<button type="submit">Save settings</button></form></section><section class="panel"><h2>Blocked users</h2>{}</section>"#,
        html_escape::encode_double_quoted_attribute(&csrf),
        html_escape::encode_double_quoted_attribute(display_name.as_str()),
        html_escape::encode_text(bio.as_str()),
        html_escape::encode_double_quoted_attribute(website.as_str()),
        picture_input,
        banner_input,
        blocked_panel,
    );
    Ok(Html(
        page_layout(&state, Some(&user), Some(&csrf), "Settings", &body).await?,
    ))
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
    let display_name = form.display_name.trim().to_owned();
    let bio = form.bio.trim().to_owned();
    let website = form.website.trim().to_owned();
    state
        .pool
        .call(move |conn| {
            conn.execute(
                "UPDATE users SET display_name = ?, bio = ?, website = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
                params![display_name, bio, website, user.id],
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
    Ok(Redirect::to("/settings").into_response())
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
        website: String::new(),
        delete_profile_picture: false,
        delete_banner: false,
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
            "website" => {
                form.website = field
                    .text()
                    .await
                    .map_err(|err| AppError::BadRequest(err.to_string()))?;
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
                  (SELECT COUNT(*) FROM reposts WHERE post_id = p.id),
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
        render::posts(&posts, Some(&user), csrf.as_deref())
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
    let list = items
        .into_iter()
        .map(|(_, kind, message, post_id, created)| {
            let link = post_id.map_or_else(String::new, |id| {
                format!(r#"<a href="/posts/{id}">Open</a>"#)
            });
            format!(
                r#"<li><strong>{}</strong> {} <span>{}</span> {}</li>"#,
                html_escape::encode_text(&kind),
                html_escape::encode_text(&message),
                html_escape::encode_text(&created),
                link
            )
        })
        .collect::<Vec<_>>()
        .join("");
    let list = if list.is_empty() {
        render::empty_state(
            "No notifications",
            "Likes, replies, reposts, and follows will appear here.",
        )
    } else {
        format!(r#"<ul class="item-list">{list}</ul>"#)
    };
    let body = format!(
        r#"<section class="panel"><div class="section-heading"><h1>Notifications</h1><form method="post" action="/notifications/read"><input type="hidden" name="csrf" value="{}"><button type="submit">Mark all read</button></form></div>{}</section>"#,
        html_escape::encode_double_quoted_attribute(&csrf),
        list
    );
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
    let q = query.q.unwrap_or_default();
    let (users, posts) = if q.trim().is_empty() {
        (Vec::new(), Vec::new())
    } else {
        social::search(&state.pool, user.as_ref().map(|u| u.id), q.trim()).await?
    };
    let user_results = users
        .into_iter()
        .map(|(_, username, display)| {
            format!(
                r#"<li><a href="/users/{}">{}</a></li>"#,
                html_escape::encode_double_quoted_attribute(&username),
                html_escape::encode_text(&display)
            )
        })
        .collect::<Vec<_>>()
        .join("");
    let csrf = form_csrf(&state, &headers).await;
    let user_results = if user_results.is_empty() {
        render::empty_state("No users found", "Try a username or display name.")
    } else {
        format!(r#"<ul class="item-list">{user_results}</ul>"#)
    };
    let body = format!(
        r#"<section class="panel"><h1>Search</h1><form method="get"><label for="q">Search {}</label><input id="q" name="q" value="{}"><button type="submit">Search</button></form></section><section class="panel"><h2>Users</h2>{}</section><section><h2 class="section-title">Posts</h2>{}</section>"#,
        html_escape::encode_text(&state.settings.site.name),
        html_escape::encode_double_quoted_attribute(&q),
        user_results,
        render::posts(&posts, user.as_ref(), csrf.as_deref())
    );
    Ok(Html(
        page_layout(&state, user.as_ref(), csrf.as_deref(), "Search", &body).await?,
    ))
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
    let body = format!(
        "{}{}",
        render::page_header(
            "Admin",
            "Manage site health, users, media jobs, and backups."
        ),
        r#"<section class="grid"><a class="panel" href="/admin/health">Site health</a><a class="panel" href="/admin/users">Users</a><a class="panel" href="/admin/media">Media jobs</a><a class="panel" href="/admin/backups">Backups</a></section>"#
    );
    Ok(Html(
        page_layout(&state, Some(&user), Some(&csrf), "Admin", &body).await?,
    ))
}

async fn admin_health(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AppResult<Html<String>> {
    let user = require_admin(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers).await.unwrap_or_default();
    let recent_jobs = admin::recent_media_jobs(&state.pool).await?;
    let jobs = recent_jobs
        .into_iter()
        .take(5)
        .map(|(id, status, stderr)| {
            format!(
                r#"<li>#{} {} <pre>{}</pre></li>"#,
                id,
                html_escape::encode_text(&status),
                html_escape::encode_text(&stderr)
            )
        })
        .collect::<Vec<_>>()
        .join("");
    let jobs = if jobs.is_empty() {
        r#"<p class="muted">No media jobs yet.</p>"#.to_owned()
    } else {
        format!(r#"<ul class="item-list">{jobs}</ul>"#)
    };
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
        r#"<section class="panel"><h1>Site health</h1><dl><dt>DB path</dt><dd>{}</dd><dt>Upload path</dt><dd>{}</dd><dt>ffmpeg</dt><dd>{}</dd><dt>WebP support</dt><dd>{}</dd><dt>VP9 support</dt><dd>{}</dd><dt>Tor</dt><dd>{}</dd><dt>Tor enabled</dt><dd>{}</dd><dt>Tor running</dt><dd>{}</dd><dt>Tor bootstrap</dt><dd>{}</dd><dt>Tor error</dt><dd>{}</dd><dt>Onion address</dt><dd>{}</dd><dt>Anonymous mode</dt><dd>{}</dd><dt>Registration</dt><dd>{}</dd></dl><h2>Recent media jobs</h2>{}</section>"#,
        html_escape::encode_text(&state.paths.database_path.display().to_string()),
        html_escape::encode_text(&state.paths.uploads_originals.display().to_string()),
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
) -> AppResult<Html<String>> {
    let user = require_admin(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers).await.unwrap_or_default();
    let rows = admin::users(&state.pool).await?;
    let list = rows
        .into_iter()
        .map(|(id, username, is_admin, suspended)| {
            format!(
                r#"<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>"#,
                id,
                html_escape::encode_text(&username),
                is_admin,
                suspended,
                small_form(
                    &format!("/admin/users/{id}/suspend"),
                    &csrf,
                    if suspended { "Unsuspend" } else { "Suspend" },
                    if suspended {
                        "Unsuspend this account"
                    } else {
                        "Suspend this account"
                    },
                )
            )
        })
        .collect::<Vec<_>>()
        .join("");
    let body = format!(
        r#"<section class="panel"><h1>Users</h1><table><thead><tr><th>ID</th><th>Username</th><th>Admin</th><th>Suspended</th><th>Action</th></tr></thead><tbody>{}</tbody></table></section>"#,
        list
    );
    Ok(Html(
        page_layout(&state, Some(&user), Some(&csrf), "Admin users", &body).await?,
    ))
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

async fn admin_media(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AppResult<Html<String>> {
    let user = require_admin(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers).await.unwrap_or_default();
    let jobs = admin::recent_media_jobs(&state.pool).await?;
    let rows = jobs
        .into_iter()
        .map(|(id, status, stderr)| {
            format!(
                r#"<tr><td>{}</td><td>{}</td><td><pre>{}</pre></td></tr>"#,
                id,
                html_escape::encode_text(&status),
                html_escape::encode_text(&stderr)
            )
        })
        .collect::<Vec<_>>()
        .join("");
    let body = if rows.is_empty() {
        r#"<section class="panel"><h1>Media jobs</h1><p class="muted">No media jobs yet.</p></section>"#
            .to_owned()
    } else {
        format!(
            r#"<section class="panel"><h1>Media jobs</h1><table><thead><tr><th>ID</th><th>Status</th><th>Summary</th></tr></thead><tbody>{}</tbody></table></section>"#,
            rows
        )
    };
    Ok(Html(
        page_layout(&state, Some(&user), Some(&csrf), "Media jobs", &body).await?,
    ))
}

async fn admin_backups(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AppResult<Html<String>> {
    let user = require_admin(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers).await.unwrap_or_default();
    let body = format!(
        r#"<section class="panel"><h1>Backups</h1><form method="post"><input type="hidden" name="csrf" value="{}"><label><input type="checkbox" name="include_tor_keys" value="true"> Include Tor onion-service keys</label><button>Create backup</button></form></section>"#,
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
    let stored_hash: String = state
        .pool
        .call({
            let token_hash = token_hash.clone();
            move |conn| {
                conn.query_row(
                    "SELECT csrf_token_hash FROM sessions WHERE token_hash = ? AND revoked_at IS NULL",
                    [token_hash],
                    |row| row.get(0),
                )
                .optional()
                .map_err(Into::into)
            }
        })
        .await
        .ok()??;
    // CSRF tokens are only shown immediately after login in memory through the cookie session.
    // For persisted sessions we rotate a new token and update its hash before rendering forms.
    let plain = auth::secure_token();
    let new_hash = auth::hash_token(&plain);
    state
        .pool
        .call(move |conn| {
            conn.execute(
                "UPDATE sessions SET csrf_token_hash = ? WHERE token_hash = ? AND csrf_token_hash = ?",
                params![new_hash, token_hash, stored_hash],
            )?;
            Ok(())
        })
        .await
        .ok()?;
    Some(plain)
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
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    struct TestServer {
        base_url: String,
        _task: tokio::task::JoinHandle<()>,
        _temp: tempfile::TempDir,
    }

    struct TestResponse {
        status: u16,
        headers: Vec<(String, String)>,
        body: String,
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
        assert!(thread.body.contains(r#"class="post-time""#));
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

    async fn spawn_test_server() -> TestServer {
        let temp = tempfile::tempdir().expect("temp dir");
        let paths = RuntimePaths::from_data_dir(temp.path().to_path_buf());
        paths.ensure().expect("paths");
        let pool = crate::db::connect(&paths.database_path)
            .await
            .expect("connect");
        crate::db::migrate(&pool).await.expect("migrate");
        let settings = Settings::default();
        let ffmpeg = FfmpegStatus {
            available: false,
            version: String::new(),
            supports_webp: false,
            supports_vp9: false,
            error: Some("disabled in tests".to_owned()),
        };
        let tor = crate::tor::validate_startup(&settings.tor);
        let app = router(AppState::new(pool, settings, paths, ffmpeg, tor));
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
            _task: task,
            _temp: temp,
        }
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
        let raw = String::from_utf8_lossy(bytes);
        let (head, body) = raw.split_once("\r\n\r\n").expect("response split");
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
            body: body.to_owned(),
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

    fn csrf_token(body: &str) -> String {
        let marker = r#"name="csrf" value=""#;
        let start = body.find(marker).expect("csrf marker") + marker.len();
        let end = body[start..].find('"').expect("csrf end") + start;
        body[start..end].to_owned()
    }
}
