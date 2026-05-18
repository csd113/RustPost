use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::connect_info::ConnectInfo;
use axum::extract::{Form, Multipart, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
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
        .route("/", get(local))
        .route("/local", get(local))
        .route("/home", get(home))
        .route("/login", get(login_form).post(login))
        .route("/register", get(register_form).post(register))
        .route("/logout", post(logout))
        .route("/posts", post(create_post))
        .route("/posts/{id}", get(thread))
        .route("/posts/{id}/delete", post(delete_post))
        .route("/posts/{id}/like", post(toggle_like))
        .route("/posts/{id}/bookmark", post(toggle_bookmark))
        .route("/posts/{id}/repost", post(repost))
        .route("/posts/{id}/reply", post(reply_redirect))
        .route("/users/{username}", get(profile))
        .route("/users/{id}/follow", post(follow))
        .route("/users/{id}/block", post(block))
        .route("/users/{id}/mute", post(mute))
        .route("/settings", get(settings_form).post(settings_update))
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

#[derive(Deserialize)]
struct AuthForm {
    username: String,
    password: String,
}

#[derive(Deserialize)]
struct CsrfForm {
    csrf: String,
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

async fn current(state: &AppState, headers: &HeaderMap) -> AppResult<Option<CurrentUser>> {
    Ok(auth::current_user(&state.pool, headers).await?)
}

async fn local(State(state): State<Arc<AppState>>, headers: HeaderMap) -> AppResult<Html<String>> {
    let user = current(&state, &headers).await?;
    let posts = social::timeline(&state.pool, user.as_ref().map(|u| u.id), "local", None).await?;
    let csrf = form_csrf(&state, &headers).await;
    let composer = if user.is_some() || state.settings.accounts.anonymous_mode_enabled {
        render::composer(csrf.as_deref(), None)
    } else {
        String::new()
    };
    let body = format!(
        "{}{}",
        composer,
        render::posts(&posts, user.as_ref(), csrf.as_deref())
    );
    Ok(Html(render::layout_with_csrf(
        user.as_ref(),
        csrf.as_deref(),
        "Local",
        &body,
    )))
}

async fn home(State(state): State<Arc<AppState>>, headers: HeaderMap) -> AppResult<Html<String>> {
    let user = require_user(&state, &headers).await?;
    let posts = social::timeline(&state.pool, Some(user.id), "home", None).await?;
    let csrf = form_csrf(&state, &headers).await;
    let body = format!(
        "{}{}",
        render::composer(csrf.as_deref(), None),
        render::posts(&posts, Some(&user), csrf.as_deref())
    );
    Ok(Html(render::layout_with_csrf(
        Some(&user),
        csrf.as_deref(),
        "Home",
        &body,
    )))
}

async fn login_form() -> Html<String> {
    Html(render::layout(
        None,
        "Login",
        r#"<section class="panel"><form method="post"><label>Username<input name="username" required></label><label>Password<input name="password" type="password" required></label><button>Log in</button></form></section>"#,
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
    Ok(Html(render::layout(
        None,
        "Register",
        r#"<section class="panel"><form method="post"><label>Username<input name="username" required></label><label>Password<input name="password" type="password" minlength="10" required></label><button>Create account</button></form></section>"#,
    )))
}

async fn register(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Form(form): Form<AuthForm>,
) -> AppResult<Response> {
    if !state.settings.accounts.registration_enabled {
        return Err(AppError::Forbidden);
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
    .await?;
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
    csrf::validate(&state.pool, &headers, &form.csrf).await?;
    if let Some(token) = auth::session_cookie(&headers) {
        auth::revoke_session(&state.pool, &token).await?;
    }
    let mut response = Redirect::to("/local").into_response();
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
    mut multipart: Multipart,
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
    let mut text = String::new();
    let mut csrf_token = String::new();
    let mut parent_post_id = None;
    let mut media_ids = Vec::new();
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
                text = field
                    .text()
                    .await
                    .map_err(|err| AppError::BadRequest(err.to_string()))?
            }
            "csrf" => {
                csrf_token = field
                    .text()
                    .await
                    .map_err(|err| AppError::BadRequest(err.to_string()))?
            }
            "parent_post_id" => {
                let value = field
                    .text()
                    .await
                    .map_err(|err| AppError::BadRequest(err.to_string()))?;
                parent_post_id = value.parse::<i64>().ok();
            }
            "media" if field.file_name().is_some() => {
                let id = media::save_upload(
                    &state.pool,
                    &state.settings,
                    &state.paths,
                    &state.ffmpeg,
                    user.as_ref().map(|u| u.id),
                    field,
                )
                .await?;
                media_ids.push(id);
            }
            _ => {}
        }
    }
    if user.is_some() {
        csrf::validate(&state.pool, &headers, &csrf_token).await?;
    }
    let (scope, actor, max_events, window_secs) = if user.is_none() {
        (
            rate_limit::Scope::AnonymousPost,
            ip_actor(addr),
            state.settings.moderation.anonymous_posts_per_ip_per_hour,
            60 * 60,
        )
    } else if parent_post_id.is_some() {
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
    social::create_post(
        &state.pool,
        &state.settings,
        user.as_ref().map(|u| u.id),
        &text,
        parent_post_id,
        &media_ids,
    )
    .await?;
    Ok(Redirect::to("/local").into_response())
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
        "{}{}",
        render::posts(&posts, user.as_ref(), csrf.as_deref()),
        composer
    );
    Ok(Html(render::layout_with_csrf(
        user.as_ref(),
        csrf.as_deref(),
        "Thread",
        &body,
    )))
}

async fn delete_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> AppResult<Response> {
    let user = require_user(&state, &headers).await?;
    csrf::validate(&state.pool, &headers, &form.csrf).await?;
    social::delete_post(&state.pool, user.id, id, user.is_admin).await?;
    Ok(Redirect::to("/local").into_response())
}

async fn toggle_like(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> AppResult<Response> {
    let user = require_active_user(&state, &headers).await?;
    csrf::validate(&state.pool, &headers, &form.csrf).await?;
    if user_post_relation_exists(&state.pool, "likes", user.id, id).await? {
        social::unlike(&state.pool, user.id, id).await?;
    } else {
        social::like(&state.pool, user.id, id).await?;
    }
    Ok(Redirect::to(&format!("/posts/{id}")).into_response())
}

async fn toggle_bookmark(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> AppResult<Response> {
    let user = require_active_user(&state, &headers).await?;
    csrf::validate(&state.pool, &headers, &form.csrf).await?;
    if user_post_relation_exists(&state.pool, "bookmarks", user.id, id).await? {
        social::unbookmark(&state.pool, user.id, id).await?;
    } else {
        social::bookmark(&state.pool, user.id, id).await?;
    }
    Ok(Redirect::to(&format!("/posts/{id}")).into_response())
}

async fn repost(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> AppResult<Response> {
    let user = require_active_user(&state, &headers).await?;
    csrf::validate(&state.pool, &headers, &form.csrf).await?;
    rate_limit::check_and_record(
        &state.pool,
        rate_limit::Scope::Repost,
        &user_actor(user.id),
        state.settings.moderation.reposts_per_minute,
        60,
    )
    .await
    .map_err(|err| AppError::RateLimited(err.to_string()))?;
    social::repost(&state.pool, user.id, id).await?;
    Ok(Redirect::to("/home").into_response())
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
    let controls = if let (Some(viewer), Some(csrf)) = (&user, &csrf) {
        if viewer.id == profile_id {
            String::new()
        } else {
            format!(
                r#"<div class="actions">{}{}{}</div>"#,
                small_form(&format!("/users/{profile_id}/follow"), csrf, "Follow"),
                small_form(&format!("/users/{profile_id}/block"), csrf, "Block"),
                small_form(&format!("/users/{profile_id}/mute"), csrf, "Mute")
            )
        }
    } else {
        String::new()
    };
    let picture = picture_path.map_or_else(String::new, |path| {
        format!(
            r#"<img class="profile-picture" src="{}" alt="">"#,
            html_escape::encode_double_quoted_attribute(&path)
        )
    });
    let banner = banner_path.map_or_else(String::new, |path| {
        format!(
            r#"<img class="profile-banner" src="{}" alt="">"#,
            html_escape::encode_double_quoted_attribute(&path)
        )
    });
    let body = format!(
        r#"<section class="panel profile">{}<div class="profile-heading">{}<div><h1>{}</h1><p>{}</p><p><a href="{}">{}</a></p></div></div>{}</section>{}"#,
        banner,
        picture,
        html_escape::encode_text(display_name.as_str()),
        html_escape::encode_text(bio.as_str()),
        html_escape::encode_double_quoted_attribute(website.as_str()),
        html_escape::encode_text(website.as_str()),
        controls,
        render::posts(&posts, user.as_ref(), csrf.as_deref())
    );
    Ok(Html(render::layout_with_csrf(
        user.as_ref(),
        csrf.as_deref(),
        &profile_username,
        &body,
    )))
}

async fn follow(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> AppResult<Response> {
    let user = require_active_user(&state, &headers).await?;
    csrf::validate(&state.pool, &headers, &form.csrf).await?;
    social::follow(&state.pool, user.id, id).await?;
    Ok(Redirect::to("/home").into_response())
}

async fn block(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> AppResult<Response> {
    let user = require_active_user(&state, &headers).await?;
    csrf::validate(&state.pool, &headers, &form.csrf).await?;
    social::block(&state.pool, user.id, id).await?;
    Ok(Redirect::to("/home").into_response())
}

async fn mute(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> AppResult<Response> {
    let user = require_active_user(&state, &headers).await?;
    csrf::validate(&state.pool, &headers, &form.csrf).await?;
    social::mute(&state.pool, user.id, id).await?;
    Ok(Redirect::to("/home").into_response())
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
                r#"<img class="profile-picture" src="{}" alt=""><label><input type="checkbox" name="delete_profile_picture" value="true"> Delete profile picture</label>"#,
                html_escape::encode_double_quoted_attribute(&path)
            )
        });
    let banner = banner_path.map_or_else(String::new, |path| {
            format!(
                r#"<img class="profile-banner" src="{}" alt=""><label><input type="checkbox" name="delete_banner" value="true"> Delete banner</label>"#,
                html_escape::encode_double_quoted_attribute(&path)
            )
        });
    let banner_input = if state.settings.accounts.allow_profile_banners {
        format!(
            r#"{banner}<label>Banner<input name="banner" type="file" accept="image/*"></label>"#
        )
    } else {
        r#"<p>Profile banners are disabled.</p>"#.to_owned()
    };
    let picture_input = if state.settings.accounts.allow_profile_pictures {
        format!(
            r#"{picture}<label>Profile picture<input name="profile_picture" type="file" accept="image/*"></label>"#
        )
    } else {
        r#"<p>Profile pictures are disabled.</p>"#.to_owned()
    };
    let body = format!(
        r#"<section class="panel"><form method="post" enctype="multipart/form-data"><input type="hidden" name="csrf" value="{}"><label>Display name<input name="display_name" value="{}"></label><label>Bio<textarea name="bio">{}</textarea></label><label>Website<input type="url" name="website" value="{}"></label>{}{}<button>Save</button></form></section>"#,
        html_escape::encode_double_quoted_attribute(&csrf),
        html_escape::encode_double_quoted_attribute(display_name.as_str()),
        html_escape::encode_text(bio.as_str()),
        html_escape::encode_double_quoted_attribute(website.as_str()),
        picture_input,
        banner_input,
    );
    Ok(Html(render::layout_with_csrf(
        Some(&user),
        Some(&csrf),
        "Settings",
        &body,
    )))
}

async fn settings_update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    multipart: Multipart,
) -> AppResult<Response> {
    let user = require_user(&state, &headers).await?;
    let form = parse_profile_update(&state, user.id, multipart).await?;
    csrf::validate(&state.pool, &headers, &form.csrf_token).await?;
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
                form.profile_picture_media_id = Some(
                    media::save_upload(
                        &state.pool,
                        &state.settings,
                        &state.paths,
                        &state.ffmpeg,
                        Some(user_id),
                        field,
                    )
                    .await?,
                );
            }
            "banner" if field.file_name().is_some() => {
                if !state.settings.accounts.allow_profile_banners {
                    return Err(AppError::Forbidden);
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
                    .await?,
                );
            }
            _ => {}
        }
    }
    Ok(form)
}

async fn bookmarks(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AppResult<Html<String>> {
    let user = require_user(&state, &headers).await?;
    let posts = social::timeline(&state.pool, Some(user.id), "bookmarks", None).await?;
    let csrf = form_csrf(&state, &headers).await;
    Ok(Html(render::layout_with_csrf(
        Some(&user),
        csrf.as_deref(),
        "Bookmarks",
        &render::posts(&posts, Some(&user), csrf.as_deref()),
    )))
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
    let body = format!(
        r#"<section class="panel"><form method="post" action="/notifications/read"><input type="hidden" name="csrf" value="{}"><button>Mark all read</button></form><ul>{}</ul></section>"#,
        html_escape::encode_double_quoted_attribute(&csrf),
        list
    );
    Ok(Html(render::layout_with_csrf(
        Some(&user),
        Some(&csrf),
        "Notifications",
        &body,
    )))
}

async fn mark_notifications_read(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> AppResult<Response> {
    let user = require_user(&state, &headers).await?;
    csrf::validate(&state.pool, &headers, &form.csrf).await?;
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
    let body = format!(
        r#"<section class="panel"><form method="get"><label>Search<input name="q" value="{}"></label><button>Search</button></form><h2>Users</h2><ul>{}</ul><h2>Posts</h2>{}</section>"#,
        html_escape::encode_double_quoted_attribute(&q),
        user_results,
        render::posts(&posts, user.as_ref(), csrf.as_deref())
    );
    Ok(Html(render::layout_with_csrf(
        user.as_ref(),
        csrf.as_deref(),
        "Search",
        &body,
    )))
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
    let body = r#"<section class="grid"><a class="panel" href="/admin/health">Site health</a><a class="panel" href="/admin/users">Users</a><a class="panel" href="/admin/media">Media jobs</a><a class="panel" href="/admin/backups">Backups</a></section>"#;
    Ok(Html(render::layout_with_csrf(
        Some(&user),
        Some(&csrf),
        "Admin",
        body,
    )))
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
        r#"<section class="panel"><h1>Site health</h1><dl><dt>DB path</dt><dd>{}</dd><dt>Upload path</dt><dd>{}</dd><dt>ffmpeg</dt><dd>{}</dd><dt>WebP support</dt><dd>{}</dd><dt>VP9 support</dt><dd>{}</dd><dt>Tor</dt><dd>{}</dd><dt>Tor enabled</dt><dd>{}</dd><dt>Tor running</dt><dd>{}</dd><dt>Tor bootstrap</dt><dd>{}</dd><dt>Tor error</dt><dd>{}</dd><dt>Onion address</dt><dd>{}</dd><dt>Anonymous mode</dt><dd>{}</dd><dt>Registration</dt><dd>{}</dd></dl><h2>Recent media jobs</h2><ul>{}</ul></section>"#,
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
    Ok(Html(render::layout_with_csrf(
        Some(&user),
        Some(&csrf),
        "Site health",
        &body,
    )))
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
                    if suspended { "Unsuspend" } else { "Suspend" }
                )
            )
        })
        .collect::<Vec<_>>()
        .join("");
    Ok(Html(render::layout_with_csrf(
        Some(&user),
        Some(&csrf),
        "Admin users",
        &format!(
            r#"<section class="panel"><table>{}</table></section>"#,
            list
        ),
    )))
}

async fn admin_suspend(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> AppResult<Response> {
    let user = require_admin(&state, &headers).await?;
    csrf::validate(&state.pool, &headers, &form.csrf).await?;
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
    csrf::validate(&state.pool, &headers, &form.csrf).await?;
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
    Ok(Html(render::layout_with_csrf(
        Some(&user),
        Some(&csrf),
        "Media jobs",
        &format!(
            r#"<section class="panel"><table>{}</table></section>"#,
            rows
        ),
    )))
}

async fn admin_backups(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AppResult<Html<String>> {
    let user = require_admin(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers).await.unwrap_or_default();
    let body = format!(
        r#"<section class="panel"><form method="post"><input type="hidden" name="csrf" value="{}"><label><input type="checkbox" name="include_tor_keys" value="true"> Include Tor onion-service keys</label><button>Create backup</button></form></section>"#,
        html_escape::encode_double_quoted_attribute(&csrf)
    );
    Ok(Html(render::layout_with_csrf(
        Some(&user),
        Some(&csrf),
        "Backups",
        &body,
    )))
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
    csrf::validate(&state.pool, &headers, &form.csrf).await?;
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
    Ok(Html(render::layout_with_csrf(
        Some(&user),
        Some(&csrf),
        "Backup created",
        &body,
    )))
}

fn small_form(action: &str, csrf: &str, label: &str) -> String {
    format!(
        r#"<form method="post" action="{}"><input type="hidden" name="csrf" value="{}"><button>{}</button></form>"#,
        html_escape::encode_double_quoted_attribute(action),
        html_escape::encode_double_quoted_attribute(csrf),
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
