use std::fmt::Write as _;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::connect_info::ConnectInfo;
use axum::extract::{DefaultBodyLimit, Form, Multipart, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::middleware;
use axum::response::{Html, IntoResponse as _, Redirect, Response};
use axum::routing::{get, post};
use rusqlite::{OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;
use uuid::Uuid;

use crate::auth::{self, CurrentUser, Theme};
use crate::config::Settings;
use crate::db::SqlitePool;
use crate::errors::{AppError, AppResult};
use crate::ffmpeg::FfmpegStatus;
use crate::registration_captcha::RegistrationCaptchaStore;
use crate::runtime::RuntimePaths;
use crate::{
    account, admin, backup, csrf, favicon, identity, instance, media, portability, rate_limit,
    render, social,
};

const CSRF_TOKEN_HISTORY_LIMIT: usize = 32;

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    configuration_write_lock: Arc<tokio::sync::Mutex<()>>,
    pub settings: Settings,
    pub paths: RuntimePaths,
    pub ffmpeg: FfmpegStatus,
    pub tor: crate::tor::TorStatus,
    pub registration_captcha: RegistrationCaptchaStore,
    /// Cached `media.nsfw_blur_enabled` so page rendering does not read and
    /// parse `settings.toml` on every request. Updated when an admin saves
    /// deep settings.
    pub nsfw_blur_default: Arc<std::sync::atomic::AtomicBool>,
    /// Set after an in-process restore swaps the runtime directories. The
    /// running process still holds the previous `SQLite` connection, so writes
    /// are refused until the operator restarts `RustPost`.
    pub restart_required: Arc<std::sync::atomic::AtomicBool>,
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
        let nsfw_blur_default = settings.media.nsfw_blur_enabled;
        Arc::new(Self {
            pool,
            configuration_write_lock: Arc::new(tokio::sync::Mutex::new(())),
            settings,
            paths,
            ffmpeg,
            tor,
            registration_captcha: RegistrationCaptchaStore::default(),
            nsfw_blur_default: Arc::new(std::sync::atomic::AtomicBool::new(nsfw_blur_default)),
            restart_required: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }
}

/// Refuses state-changing requests after an in-process restore, because the
/// running process would write them to the replaced (unlinked) database file.
async fn restart_required_guard(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
    next: middleware::Next,
) -> Response {
    use axum::http::Method;

    if state
        .restart_required
        .load(std::sync::atomic::Ordering::Relaxed)
        && !matches!(
            *request.method(),
            Method::GET | Method::HEAD | Method::OPTIONS
        )
    {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Html(render::error_page(
                StatusCode::SERVICE_UNAVAILABLE,
                "A backup was restored. Restart RustPost before making changes so your data is written to the restored database.",
            )),
        )
            .into_response();
    }
    next.run(request).await
}

/// Paths that stay reachable while an account is restricted to a forced
/// password change. Static assets, logout, the password form, account
/// deletion, and data export must remain available.
fn password_change_exempt(path: &str) -> bool {
    if path.starts_with("/assets/") || path.starts_with("/uploads/") {
        return true;
    }
    matches!(
        path,
        "/login"
            | "/settings/password"
            | "/logout"
            | "/favicon.ico"
            | "/local"
            | "/settings/delete"
            | "/settings/delete/confirm"
            | "/settings/delete/cancel"
            | "/settings/export"
            | "/account-deleted"
    )
}

/// State-changing paths still allowed while an account is pending deletion.
/// Everything else is read-only until the deletion is cancelled.
fn pending_deletion_exempt(path: &str) -> bool {
    matches!(
        path,
        "/logout" | "/settings/password" | "/settings/delete/cancel" | "/settings/delete/confirm"
    )
}

fn is_state_changing(method: &axum::http::Method) -> bool {
    !matches!(
        *method,
        axum::http::Method::GET
            | axum::http::Method::HEAD
            | axum::http::Method::OPTIONS
            | axum::http::Method::TRACE
    )
}

/// Enforces forced-password-reset and pending-deletion restrictions for the
/// authenticated viewer on every route, so a long-lived session cannot bypass
/// either account state.
async fn account_state_guard(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
    next: middleware::Next,
) -> Response {
    let guarded_path = request.uri().path();
    if guarded_path.starts_with("/assets/")
        || guarded_path.starts_with("/uploads/")
        || guarded_path == "/favicon.ico"
    {
        return next.run(request).await;
    }
    if auth::session_cookie(request.headers()).is_none() {
        return next.run(request).await;
    }
    let user = match auth::current_user(&state.pool, request.headers()).await {
        Ok(Some(user)) => user,
        Ok(None) => return next.run(request).await,
        Err(error) => {
            // Fail closed: an authenticated request whose account state cannot
            // be verified must not bypass forced password or deletion checks.
            tracing::warn!(error = %error, "account state lookup failed");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Html(render::error_page(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Your account state could not be verified. Try again shortly.",
                )),
            )
                .into_response();
        }
    };
    let path = request.uri().path();
    if user.must_change_password && !password_change_exempt(path) {
        return Redirect::to("/settings/password?required=1").into_response();
    }
    if user.deletion_scheduled_at.is_some()
        && is_state_changing(request.method())
        && !pending_deletion_exempt(path)
    {
        return (
            StatusCode::FORBIDDEN,
            Html(render::error_page(
                StatusCode::FORBIDDEN,
                "This account is scheduled for deletion. Cancel the deletion in account settings to keep using it.",
            )),
        )
            .into_response();
    }
    next.run(request).await
}

/// Central maintenance-mode policy for mutating routes.
///
/// Only routes that publish or rewrite public content are candidates. Reads,
/// authentication, account/security operations, social graph actions, and the
/// admin UI stay available while maintenance mode is on. Registration is
/// blocked for everyone; publish routes are blocked for non-administrators so
/// administrators can verify the instance and later disable maintenance.
///
/// Keep this table authoritative: any new route that publishes content
/// (creates or rewrites posts/replies/quotes/reposts, or imports archives that
/// publish posts) must be added here, with a matching table-driven test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MaintenancePolicy {
    /// Always reachable.
    Allowed,
    /// Blocked for every request.
    Blocked,
    /// Blocked for anonymous users and non-administrators.
    BlockedUnlessAdmin,
}

fn maintenance_policy(method: &axum::http::Method, path: &str) -> MaintenancePolicy {
    if *method != axum::http::Method::POST {
        return MaintenancePolicy::Allowed;
    }
    match path {
        "/register" => MaintenancePolicy::Blocked,
        "/posts" | "/settings/import" => MaintenancePolicy::BlockedUnlessAdmin,
        _ if path.starts_with("/posts/")
            && (path.ends_with("/quote")
                || path.ends_with("/repost")
                || path.ends_with("/edit")) =>
        {
            MaintenancePolicy::BlockedUnlessAdmin
        }
        _ => MaintenancePolicy::Allowed,
    }
}

async fn maintenance_guard(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
    next: middleware::Next,
) -> Response {
    let policy = maintenance_policy(request.method(), request.uri().path());
    if policy == MaintenancePolicy::Allowed {
        return next.run(request).await;
    }
    let settings = match instance::load(&state.pool).await {
        Ok(settings) => settings,
        Err(error) => {
            tracing::warn!(error = %error, "failed to load instance settings; refusing state change");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Html(render::maintenance_page(
                    &state.settings.site.name,
                    instance::DEFAULT_MAINTENANCE_MESSAGE,
                )),
            )
                .into_response();
        }
    };
    if !settings.maintenance_mode {
        return next.run(request).await;
    }
    if policy == MaintenancePolicy::BlockedUnlessAdmin
        && let Ok(Some(user)) = auth::current_user(&state.pool, request.headers()).await
        && user.is_admin
    {
        return next.run(request).await;
    }
    let notice = settings
        .maintenance_notice()
        .unwrap_or(instance::DEFAULT_MAINTENANCE_MESSAGE);
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Html(render::maintenance_page(&state.settings.site.name, notice)),
    )
        .into_response()
}

/// Periodically prunes expired operational rows and abandoned temp files.
///
/// Startup performs an aggressive temp cleanup (nothing can be in flight yet);
/// later cycles keep a grace period so active uploads are never removed.
pub fn spawn_maintenance_scheduler(
    pool: SqlitePool,
    paths: RuntimePaths,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        // The longest configured rate-limit window is one hour; keep two hours.
        const RATE_LIMIT_RETENTION_SECS: i64 = 2 * 60 * 60;
        const MAINTENANCE_INTERVAL: std::time::Duration = std::time::Duration::from_mins(30);
        let mut first_cycle = true;
        loop {
            match crate::account::recover_pending_media_deletions(&paths).await {
                Ok(0) => {}
                Ok(recovered) => tracing::info!(
                    recovered,
                    "recovered interrupted account deletion media cleanup"
                ),
                Err(error) => {
                    tracing::warn!(error = %error, "pending media deletion recovery failed");
                }
            }
            let max_temp_age = if first_cycle {
                std::time::Duration::ZERO
            } else {
                std::time::Duration::from_hours(1)
            };
            match paths.cleanup_stale_temp_files(max_temp_age) {
                Ok(0) => {}
                Ok(removed) => tracing::info!(removed, "removed stale upload staging files"),
                Err(error) => tracing::warn!(error = %error, "temp file cleanup failed"),
            }
            if let Err(error) = crate::rate_limit::prune_old(&pool, RATE_LIMIT_RETENTION_SECS).await
            {
                tracing::warn!(error = %error, "rate limit event pruning failed");
            }
            match crate::auth::prune_stale_sessions(&pool, 7).await {
                Ok(0) => {}
                Ok(removed) => tracing::info!(removed, "removed stale sessions"),
                Err(error) => tracing::warn!(error = %error, "session pruning failed"),
            }
            match crate::account::finalize_due_deletions(&pool, &paths).await {
                Ok(0) => {}
                Ok(removed) => tracing::info!(removed, "finalized scheduled account deletions"),
                Err(error) => {
                    tracing::warn!(error = %error, "scheduled account deletion finalization failed");
                }
            }
            first_cycle = false;
            tokio::select! {
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
                () = tokio::time::sleep(MAINTENANCE_INTERVAL) => {}
            }
        }
    });
}

#[expect(
    clippy::too_many_lines,
    reason = "the route table is one flat registration list, matching the original layout"
)]
pub fn router(state: Arc<AppState>) -> Router {
    let upload_body_limit = upload_body_limit(&state.settings);
    let import_body_limit = import_upload_body_limit(&state.settings);
    Router::new()
        .route("/", get(home))
        .route("/assets/rustpost-boot.js", get(client_boot_script))
        .route("/assets/rustpost.js", get(client_script))
        .route("/favicon.ico", get(site_favicon))
        .route("/local", get(local_redirect))
        .route("/home", get(home))
        .route("/login", get(login_form).post(login))
        .route("/register", get(register_form).post(register))
        .route("/onboarding", get(onboarding).post(onboarding_update))
        .route("/logout", post(logout))
        .route("/posts", post(create_post))
        .route("/posts/{id}", get(thread))
        .route("/posts/{id}/edit", get(edit_post_form).post(edit_post))
        .route("/posts/{id}/delete", get(delete_confirm).post(delete_post))
        .route("/posts/{id}/like", post(toggle_like))
        .route("/posts/{id}/bookmark", post(toggle_bookmark))
        .route("/posts/{id}/pin", post(toggle_pin_post))
        .route("/posts/{id}/repost", post(repost))
        .route("/posts/{id}/quote", get(quote_form).post(quote_post))
        .route("/posts/{id}/reply", post(reply_redirect))
        .route("/users/{username}", get(profile))
        .route("/users/{username}/followers", get(profile_followers))
        .route("/users/{username}/following", get(profile_following))
        .route("/users/{id}/follow", post(follow))
        .route("/users/{id}/follow/approve", post(approve_follow_request))
        .route("/users/{id}/follow/reject", post(reject_follow_request))
        .route("/users/{id}/follow/cancel", post(cancel_follow_request))
        .route("/users/{id}/unfollow", post(unfollow))
        .route("/follow-requests", get(follow_requests))
        .route("/users/{id}/block", post(block))
        .route("/users/{id}/unblock", post(unblock))
        .route("/users/{id}/mute", post(mute))
        .route("/users/{id}/unmute", post(unmute))
        .route("/settings", get(settings_form).post(settings_update))
        .route("/settings/muted-words", post(add_muted_word))
        .route("/settings/muted-words/{id}/remove", post(remove_muted_word))
        .route(
            "/settings/password",
            get(password_change_page).post(change_password),
        )
        .route("/settings/username", post(change_username))
        .route("/settings/export", get(export_account))
        .route(
            "/settings/import",
            get(import_account_form)
                .post(import_account)
                .layer(DefaultBodyLimit::max(import_body_limit)),
        )
        .route("/settings/delete/cancel", post(cancel_deletion))
        .route("/settings/delete", get(delete_account_warning))
        .route(
            "/settings/delete/confirm",
            get(delete_account_final_warning).post(delete_account_final),
        )
        .route("/account-deleted", get(account_deleted))
        .route("/following", get(following))
        .route("/bookmarks", get(bookmarks))
        .route("/notifications", get(notifications))
        .route("/notifications/open", post(open_notification_group))
        .route("/notifications/read", post(mark_notifications_read))
        .route("/mentions", get(mention_suggestions))
        .route("/search", get(search))
        .route("/tags/{tag}", get(tag))
        .route("/admin", get(admin_dashboard))
        .route("/admin/users", get(admin_users))
        .route("/admin/users/{id}/suspend", post(admin_suspend))
        .route(
            "/admin/users/{id}/require-password-reset",
            post(admin_require_password_reset),
        )
        .route(
            "/admin/users/{id}/revoke-sessions",
            post(admin_revoke_sessions),
        )
        .route("/admin/announcement", post(admin_update_announcement))
        .route("/admin/maintenance", post(admin_update_maintenance))
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
        .route("/admin/backups", get(admin_backups))
        .route("/admin/backups/create", post(admin_create_backup))
        .route(
            "/admin/backups/download/{filename}",
            get(admin_download_backup),
        )
        .route("/admin/backups/restore", post(admin_restore_backup))
        .route(
            "/admin/backups/settings",
            post(admin_backup_settings_update),
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
        .layer(middleware::from_fn_with_state(
            state.clone(),
            account_state_guard,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            maintenance_guard,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            restart_required_guard,
        ))
        .layer(middleware::from_fn(
            crate::compression::response_compression,
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

fn upload_body_limit(settings: &Settings) -> usize {
    let limit = upload_payload_limit(settings).saturating_add(1024 * 1024);
    match usize::try_from(limit) {
        Ok(limit) => limit,
        Err(_overflow) => usize::MAX,
    }
}

/// Body limit for `POST /settings/import`. The configured compressed archive
/// ceiling plus a small multipart/framing allowance so an oversized archive is
/// rejected by the streaming handler with a clear 413 page instead of a
/// transport-level body limit failure. The global media body limit stays
/// independent of archive imports.
fn import_upload_body_limit(settings: &Settings) -> usize {
    const MULTIPART_SLACK_BYTES: u64 = 1024 * 1024;
    let limit = settings
        .accounts
        .max_archive_upload_bytes
        .saturating_add(MULTIPART_SLACK_BYTES);
    usize::try_from(limit).unwrap_or(usize::MAX)
}

fn upload_payload_limit(settings: &Settings) -> u64 {
    let max_media = settings.posts.max_media_per_post;
    if settings.media.max_video_size >= settings.media.max_image_size {
        let videos = settings.posts.max_videos_per_post.min(max_media);
        let images = settings
            .posts
            .max_images_per_post
            .min(max_media.saturating_sub(videos));
        return attachment_bytes(videos, settings.media.max_video_size)
            .saturating_add(attachment_bytes(images, settings.media.max_image_size));
    }
    let images = settings.posts.max_images_per_post.min(max_media);
    let videos = settings
        .posts
        .max_videos_per_post
        .min(max_media.saturating_sub(images));
    attachment_bytes(images, settings.media.max_image_size)
        .saturating_add(attachment_bytes(videos, settings.media.max_video_size))
}

fn attachment_bytes(count: usize, max_size: u64) -> u64 {
    u64::try_from(count)
        .unwrap_or(u64::MAX)
        .saturating_mul(max_size)
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

async fn client_boot_script() -> Response {
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        render::client_boot_script(),
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
struct NotificationOpenForm {
    csrf: String,
    notification_ids: String,
    group_kind: Option<String>,
    group_target_post_id: Option<String>,
    return_to: String,
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
struct ReturnQuery {
    return_to: Option<String>,
}

#[derive(Deserialize)]
struct SearchQuery {
    q: Option<String>,
}

#[derive(Deserialize)]
struct ProfileQuery {
    tab: Option<String>,
}

#[derive(Deserialize)]
struct AccountListQuery {
    after: Option<String>,
}

#[derive(Deserialize)]
struct MentionSuggestionsQuery {
    q: Option<String>,
}

#[derive(Serialize)]
struct MentionSuggestionResponse {
    username: String,
    display_name: String,
}

#[derive(Deserialize)]
struct AdminUsersQuery {
    user_q: Option<String>,
    post_q: Option<String>,
}

#[derive(Deserialize)]
struct SettingsQuery {
    saved: Option<String>,
    required: Option<String>,
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
    liked_posts_public: bool,
    follow_approval_required: bool,
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

struct ParsedOnboardingUpdate {
    csrf_token: String,
    intent: String,
    display_name: String,
    bio: String,
    profile_picture_media_id: Option<i64>,
    follow_user_ids: Vec<i64>,
}

#[derive(Deserialize)]
struct AdminNsfwForm {
    csrf: String,
    nsfw: String,
}

#[derive(Deserialize)]
struct EditPostForm {
    csrf: String,
    text: String,
    return_to: Option<String>,
}

struct DeletePreview {
    text: String,
    username: Option<String>,
    display_name: Option<String>,
    parent_post_id: Option<i64>,
}

struct EditPreview {
    text: String,
    parent_post_id: Option<i64>,
}

#[derive(Serialize)]
struct FollowActionResponse {
    kind: &'static str,
    user_id: i64,
    following: bool,
    /// A pending follow request exists (button shows "Requested").
    requested: bool,
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
    let composer = if (user.is_some() || state.settings.accounts.anonymous_mode_enabled)
        && posting_enabled_for(&state, user.as_ref()).await?
    {
        render::composer(csrf.as_deref(), None, state.settings.posts.max_text_chars)
    } else {
        String::new()
    };
    let body = format!(
        "{}{}{}",
        render::page_header("Home Feed", "All posts"),
        composer,
        render::posts_with_controls(
            &posts,
            user.as_ref(),
            csrf.as_deref(),
            blur_nsfw_media(&state, user.as_ref()),
            state.settings.posts.post_edit_window_seconds,
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
    let (counts, notification_unread_count, pending_follow_requests) = if let Some(user) = user {
        (
            Some(social::follow_counts(&state.pool, user.id).await?),
            Some(social::unread_notification_count(&state.pool, user.id).await?),
            Some(social::pending_follow_request_count(&state.pool, user.id).await?),
        )
    } else {
        (None, None, None)
    };
    let instance = instance::load(&state.pool).await.unwrap_or_else(|error| {
        tracing::warn!(error = %error, "failed to load instance settings");
        instance::InstanceSettings::default()
    });
    Ok(render::LayoutContext {
        anonymous_mode_enabled: state.settings.accounts.anonymous_mode_enabled,
        tor_onion_address: state.tor.onion_address(),
        follower_count: counts.map(|(followers, _following)| followers),
        following_count: counts.map(|(_followers, following)| following),
        notification_unread_count,
        favicon_content_type: favicon::current(&state.paths).content_type(),
        announcement: instance.announcement_text().map(ToOwned::to_owned),
        maintenance_notice: instance.maintenance_notice().map(ToOwned::to_owned),
        account_notice: user.and_then(|user| {
            user.deletion_scheduled_at
                .as_ref()
                .map(|deadline| format!("This account is scheduled for deletion on {deadline}."))
        }),
        pending_follow_requests,
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
    let admin_body;
    let body = if user.is_some_and(|user| user.is_admin)
        && matches!(
            title,
            "Admin"
                | "Site health"
                | "Admin users"
                | "Media jobs"
                | "Deep server settings"
                | "Backups"
        ) {
        let navigation = [
            ("/admin", "Overview", "Admin"),
            ("/admin/health", "Site health", "Site health"),
            ("/admin/users", "Users", "Admin users"),
            ("/admin/media", "Media jobs", "Media jobs"),
            (
                "/admin/deep-settings",
                "Configuration",
                "Deep server settings",
            ),
            ("/admin/backups", "Backups", "Backups"),
        ]
        .into_iter()
        .fold(String::new(), |mut links, (href, label, page)| {
            let current = if title == page {
                " aria-current=\"page\""
            } else {
                ""
            };
            let _ = write!(links, r#"<a href="{href}"{current}>{label}</a>"#);
            links
        });
        admin_body = format!(
            r#"<nav class="admin-console-nav" aria-label="Administration">{navigation}</nav>{body}"#
        );
        &admin_body
    } else {
        body
    };
    Ok(render::layout_with_context(
        user,
        csrf,
        title,
        body,
        &state.settings.site.name,
        &context,
    ))
}

/// Whether the viewer may publish content right now. Maintenance mode blocks
/// posting for everyone except administrators.
async fn posting_enabled_for(state: &AppState, user: Option<&CurrentUser>) -> AppResult<bool> {
    let instance = instance::load(&state.pool).await?;
    Ok(!instance.maintenance_mode || user.is_some_and(|user| user.is_admin))
}

fn blur_nsfw_media(state: &AppState, user: Option<&CurrentUser>) -> bool {
    let global_blur = state
        .nsfw_blur_default
        .load(std::sync::atomic::Ordering::Relaxed);
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
    headers: HeaderMap,
    Form(form): Form<AuthForm>,
) -> AppResult<Response> {
    reject_cross_site_form_post(&headers)?;
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

async fn register(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Form(form): Form<RegisterForm>,
) -> AppResult<Response> {
    reject_cross_site_form_post(&headers)?;
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

async fn onboarding(State(state): State<Arc<AppState>>, headers: HeaderMap) -> AppResult<Response> {
    let user = require_user(&state, &headers).await?;
    if onboarding_completed(&state.pool, user.id).await? {
        return Ok(Redirect::to("/home").into_response());
    }
    let csrf = form_csrf(&state, &headers).await.unwrap_or_default();
    Ok(Html(onboarding_page(&state, &user, &csrf).await?).into_response())
}

async fn onboarding_update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    multipart: Multipart,
) -> AppResult<Response> {
    let user = require_user(&state, &headers).await?;
    let form = parse_onboarding_update(&state, user.id, multipart).await?;
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
    update_onboarding_profile(&state.pool, user.id, &form.display_name, &form.bio).await?;
    if let Some(media_id) = form.profile_picture_media_id {
        media::set_profile_media(
            &state.pool,
            &state.paths,
            user.id,
            media::ProfileMediaSlot::Picture,
            media_id,
        )
        .await?;
    }
    let follow_user_ids = dedup_user_ids(form.follow_user_ids, user.id);
    let follow_user_ids = social::active_follow_targets(&state.pool, &follow_user_ids).await?;
    for followed_id in follow_user_ids {
        social::follow(&state.pool, user.id, followed_id)
            .await
            .map_err(|err| AppError::BadRequest(err.to_string()))?;
    }
    mark_onboarding_complete(&state.pool, user.id).await?;
    Ok(Redirect::to("/home").into_response())
}

async fn cleanup_onboarding_uploads(state: &AppState, form: &ParsedOnboardingUpdate) {
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

async fn onboarding_response(
    state: &AppState,
    user: &CurrentUser,
    headers: &HeaderMap,
    status: StatusCode,
    message: &str,
) -> AppResult<Response> {
    let csrf = form_csrf(state, headers).await.unwrap_or_default();
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

async fn onboarding_page(state: &AppState, user: &CurrentUser, csrf: &str) -> AppResult<String> {
    let body = onboarding_page_body(state, user, csrf).await?;
    page_layout(state, Some(user), Some(csrf), "Onboarding", &body).await
}

async fn onboarding_page_body(
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

async fn parse_onboarding_update(
    state: &AppState,
    user_id: i64,
    mut multipart: Multipart,
) -> AppResult<ParsedOnboardingUpdate> {
    let mut form = ParsedOnboardingUpdate {
        csrf_token: String::new(),
        intent: "save".to_owned(),
        display_name: String::new(),
        bio: String::new(),
        profile_picture_media_id: None,
        follow_user_ids: Vec::new(),
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
            "intent" => {
                form.intent = field
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
            "follow_user_id" => {
                let value = field
                    .text()
                    .await
                    .map_err(|err| AppError::BadRequest(err.to_string()))?;
                form.follow_user_ids
                    .push(value.trim().parse::<i64>().map_err(|_parse_err| {
                        AppError::BadRequest("follow suggestion is invalid".to_owned())
                    })?);
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
                    .map_err(|err| AppError::BadRequest(err.to_string()))?,
                );
            }
            _ => {}
        }
    }
    Ok(form)
}

async fn update_onboarding_profile(
    pool: &SqlitePool,
    user_id: i64,
    display_name: &str,
    bio: &str,
) -> AppResult<()> {
    let display_name = display_name.trim().to_owned();
    let bio = bio.trim().to_owned();
    pool.call(move |conn| {
        conn.execute(
            "UPDATE users SET display_name = ?, bio = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ? AND is_deleted = 0",
            params![display_name, bio, user_id],
        )?;
        Ok(())
    })
    .await?;
    Ok(())
}

async fn mark_onboarding_complete(pool: &SqlitePool, user_id: i64) -> AppResult<()> {
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

async fn onboarding_completed(pool: &SqlitePool, user_id: i64) -> AppResult<bool> {
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

fn dedup_user_ids(ids: Vec<i64>, current_user_id: i64) -> Vec<i64> {
    let mut deduped = Vec::new();
    for id in ids {
        if id == current_user_id || deduped.contains(&id) {
            continue;
        }
        deduped.push(id);
    }
    deduped
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
    let outcome = finish_post_create(&state, &headers, addr, user.as_ref(), &form).await;
    if outcome.is_err() {
        cleanup_post_uploads(&state, &form.media_ids).await;
    }
    outcome
}

async fn finish_post_create(
    state: &AppState,
    headers: &HeaderMap,
    addr: SocketAddr,
    user: Option<&CurrentUser>,
    form: &ParsedPostCreate,
) -> AppResult<Response> {
    if let Some(parent_id) = form.parent_post_id {
        ensure_parent_post_exists(&state.pool, parent_id).await?;
    }
    if user.is_some() {
        validate_csrf(&state.pool, headers, &form.csrf_token).await?;
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
    rate_limit::check_and_record(&state.pool, scope, &actor, max_events, window_secs)
        .await
        .map_err(|err| AppError::RateLimited(err.to_string()))?;
    let post_id = social::create_post(
        &state.pool,
        &state.settings,
        viewer_id,
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
    if enhanced_request(headers) {
        let posts = social::post_thread(&state.pool, viewer_id, post_id).await?;
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
                render::thread_post_card_with_controls(
                    post,
                    user,
                    form_csrf(state, headers).await.as_deref(),
                    blur_nsfw_media(state, user),
                    state.settings.posts.post_edit_window_seconds,
                )
            } else {
                render::post_card_with_controls(
                    post,
                    user,
                    form_csrf(state, headers).await.as_deref(),
                    blur_nsfw_media(state, user),
                    state.settings.posts.post_edit_window_seconds,
                )
            },
        })
        .into_response());
    }
    Ok(Redirect::to(&redirect).into_response())
}

/// Deletes media rows and files that were uploaded for a post that was never
/// created, so rejected or failed submissions do not leave orphaned uploads.
async fn cleanup_post_uploads(state: &AppState, media_ids: &[i64]) {
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

async fn parse_post_create(
    state: &AppState,
    user_id: Option<i64>,
    multipart: Multipart,
) -> AppResult<ParsedPostCreate> {
    let mut form = ParsedPostCreate {
        csrf_token: String::new(),
        text: String::new(),
        parent_post_id: None,
        media_ids: Vec::new(),
        is_nsfw: false,
    };
    let outcome = fill_post_create_form(state, user_id, multipart, &mut form).await;
    if outcome.is_err() {
        cleanup_post_uploads(state, &form.media_ids).await;
    }
    outcome.map(|()| form)
}

async fn fill_post_create_form(
    state: &AppState,
    user_id: Option<i64>,
    mut multipart: Multipart,
    form: &mut ParsedPostCreate,
) -> AppResult<()> {
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
                if form.media_ids.len() >= state.settings.posts.max_media_per_post {
                    return Err(AppError::BadRequest(
                        "too many media attachments".to_owned(),
                    ));
                }
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
    Ok(())
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
        "{}{}{}",
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
    Ok(Html(
        page_layout(&state, user.as_ref(), csrf.as_deref(), "Thread", &body).await?,
    ))
}

async fn edit_post_form(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Query(query): Query<ReturnQuery>,
) -> AppResult<Html<String>> {
    let user = require_active_user(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers).await.unwrap_or_default();
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

async fn edit_post(
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
    media::validate_post_media_deletion(&state.pool, &state.paths, id).await?;
    social::delete_post(&state.pool, user.id, id, user.is_admin).await?;
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

async fn toggle_pin_post(
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

/// Renders the page for a handle no account currently owns.
///
/// When the handle appears in `username_history`, the page lists the accounts
/// that previously held it instead of silently resolving to a different
/// person. Unknown handles keep the normal 404 response.
async fn historical_username_page(
    state: &AppState,
    user: Option<&CurrentUser>,
    headers: &HeaderMap,
    requested: &str,
) -> AppResult<Html<String>> {
    let normalized = requested.trim().to_ascii_lowercase();
    let holders = identity::historical_username_holders(&state.pool, &normalized).await?;
    if holders.is_empty() {
        if instance::username_was_released(&state.pool, &normalized).await? {
            let csrf = form_csrf(state, headers).await;
            let body = render::released_username_page(&normalized);
            return Ok(Html(
                page_layout(state, user, csrf.as_deref(), "Released username", &body).await?,
            ));
        }
        return Err(AppError::NotFound);
    }
    let csrf = form_csrf(state, headers).await;
    let body = render::historical_username_page(&normalized, &holders);
    Ok(Html(
        page_layout(state, user, csrf.as_deref(), "Username history", &body).await?,
    ))
}

fn profile_tab_from_query(tab: Option<&str>) -> social::ProfileTimelineTab {
    match tab {
        Some("replies") => social::ProfileTimelineTab::Replies,
        Some("media") => social::ProfileTimelineTab::Media,
        Some("likes") => social::ProfileTimelineTab::Likes,
        _ => social::ProfileTimelineTab::Posts,
    }
}

fn profile_tab_empty_state(tab: social::ProfileTimelineTab) -> render::EmptyState<'static> {
    match tab {
        social::ProfileTimelineTab::Posts => {
            render::EmptyState::new("No posts yet.", "This profile has not posted yet.")
        }
        social::ProfileTimelineTab::Replies => {
            render::EmptyState::new("No replies yet.", "This profile has not replied yet.")
        }
        social::ProfileTimelineTab::Media => render::EmptyState::new(
            "No media posts yet.",
            "Media posts and replies will appear here.",
        ),
        social::ProfileTimelineTab::Likes => {
            render::EmptyState::new("No likes yet.", "Liked posts will appear here.")
        }
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "profile rendering combines existing viewer controls and page assembly in one route handler"
)]
async fn profile(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(username): Path<String>,
    Query(query): Query<ProfileQuery>,
) -> AppResult<Html<String>> {
    let user = current(&state, &headers).await?;
    let active_tab = profile_tab_from_query(query.tab.as_deref());
    let requested_username = username.clone();
    let profile = state
        .pool
        .call(move |conn| {
            conn.query_row(
                r#"
        SELECT u.id, u.username, u.display_name, u.bio, u.location, u.website,
          pic.public_path AS profile_picture_path,
          banner.public_path AS banner_path,
          u.liked_posts_public,
          u.is_suspended
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
                        row.get::<_, i64>(8)? != 0,
                        row.get::<_, i64>(9)? != 0,
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
        liked_posts_public,
        is_suspended,
    )) = profile
    else {
        return historical_username_page(&state, user.as_ref(), &headers, &requested_username)
            .await;
    };
    let csrf = form_csrf(&state, &headers).await;
    let viewer_id = user.as_ref().map(|u| u.id);
    let owner_or_admin =
        viewer_id == Some(profile_id) || user.as_ref().is_some_and(|viewer| viewer.is_admin);
    let relationship = social::profile_relationship(&state.pool, viewer_id, profile_id).await?;
    let activity_visible = (!is_suspended || owner_or_admin)
        && social::profile_activity_visible(&state.pool, viewer_id, profile_id).await?;
    let likes_visible = activity_visible && (liked_posts_public || viewer_id == Some(profile_id));
    let pinned_post = if active_tab == social::ProfileTimelineTab::Posts && activity_visible {
        social::profile_pinned_post(&state.pool, viewer_id, profile_id).await?
    } else {
        None
    };
    let mut posts = if !activity_visible
        || (active_tab == social::ProfileTimelineTab::Likes && !likes_visible)
    {
        Vec::new()
    } else {
        social::profile_tab_timeline(&state.pool, viewer_id, profile_id, active_tab).await?
    };
    if let Some(pinned) = pinned_post.as_ref() {
        posts.retain(|post| {
            !(post.event_kind == social::TimelineEventKind::Post && post.id == pinned.id)
        });
    }
    let (followers, following) = social::follow_counts(&state.pool, profile_id).await?;
    let controls = profile_controls(user.as_ref(), csrf.as_deref(), profile_id, relationship);
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
    let website_link = render_profile_website_link(&website);
    let location_line = if location.trim().is_empty() {
        String::new()
    } else {
        format!(
            r#"<p class="profile-meta">{}</p>"#,
            html_escape::encode_text(location.as_str())
        )
    };
    let bio_line = if bio.trim().is_empty() {
        String::new()
    } else {
        format!(
            r#"<p class="profile-bio">{}</p>"#,
            html_escape::encode_text(bio.as_str())
        )
    };
    let profile_state_note =
        profile_state_note(is_suspended, owner_or_admin, relationship, activity_visible);
    let mut username_history =
        render::username_history_note(&identity::username_history(&state.pool, profile_id).await?);
    if instance::username_was_released(&state.pool, &profile_username.to_ascii_lowercase()).await? {
        username_history.push_str(&render::released_username_profile_note());
    }
    let counts = format!(
        r#"<p class="counts"><a href="/users/{}/followers" data-profile-followers="{}">{}</a><a href="/users/{}/following" data-profile-following="{}">{}</a></p>"#,
        html_escape::encode_double_quoted_attribute(&profile_username),
        profile_id,
        count_label(followers, "follower", "followers"),
        html_escape::encode_double_quoted_attribute(&profile_username),
        profile_id,
        count_label(following, "following", "following"),
    );
    let pinned = pinned_post.as_ref().map_or_else(String::new, |post| {
        render::pinned_post_with_controls(
            post,
            user.as_ref(),
            csrf.as_deref(),
            blur_nsfw_media(&state, user.as_ref()),
            state.settings.posts.post_edit_window_seconds,
        )
    });
    let tabs = render::profile_tabs(&profile_username, active_tab);
    let timeline = if !activity_visible {
        match profile_state_note {
            Some((title, message)) => render::empty_state(title, message),
            None => render::posts_with_controls_empty_state(
                &posts,
                user.as_ref(),
                csrf.as_deref(),
                blur_nsfw_media(&state, user.as_ref()),
                state.settings.posts.post_edit_window_seconds,
                profile_tab_empty_state(active_tab),
            ),
        }
    } else if active_tab == social::ProfileTimelineTab::Likes && !likes_visible {
        render::empty_state("This user’s likes are private", "")
    } else {
        render::posts_with_controls_empty_state(
            &posts,
            user.as_ref(),
            csrf.as_deref(),
            blur_nsfw_media(&state, user.as_ref()),
            state.settings.posts.post_edit_window_seconds,
            profile_tab_empty_state(active_tab),
        )
    };
    let body = format!(
        r#"<section class="panel profile">{}<div class="profile-heading">{}<div class="profile-main"><div class="profile-title-row"><div><h1>{}</h1><p class="muted">@{}</p></div>{}</div>{}{}{}{}{}</div></div></section>{}{}{}"#,
        banner,
        picture,
        html_escape::encode_text(display_name.as_str()),
        html_escape::encode_text(profile_username.as_str()),
        controls,
        counts,
        username_history,
        bio_line,
        location_line,
        website_link,
        pinned,
        tabs,
        timeline
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

/// A human-readable explanation for why a profile's activity is hidden.
fn profile_state_note(
    is_suspended: bool,
    owner_or_admin: bool,
    relationship: social::ProfileRelationship,
    activity_visible: bool,
) -> Option<(&'static str, &'static str)> {
    if activity_visible || owner_or_admin {
        return None;
    }
    if relationship.blocked {
        return Some((
            "You blocked this account",
            "Unblock this account to see their posts and interact again.",
        ));
    }
    if relationship.blocks_viewer {
        return Some((
            "Activity unavailable",
            "This account's posts and replies are not visible to you.",
        ));
    }
    if relationship.muted {
        return Some((
            "You muted this account",
            "Their posts stay hidden from your feeds until you unmute them.",
        ));
    }
    if is_suspended {
        return Some((
            "Account suspended",
            "This account's posts and replies are not visible.",
        ));
    }
    None
}

/// Renders "1 follower" / "2 followers" without a separate plural forms crate.
fn count_label(count: i64, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("{count} {singular}")
    } else {
        format!("{count} {plural}")
    }
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
    Query(query): Query<AccountListQuery>,
) -> AppResult<Html<String>> {
    let user = current(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers).await;
    let Some((profile_id, profile_username, display_name)) =
        profile_identity(&state.pool, &username).await?
    else {
        return Err(AppError::NotFound);
    };
    let (accounts, has_more) = social::followers_accounts(
        &state.pool,
        profile_id,
        user.as_ref().map(|user| user.id),
        query.after,
    )
    .await?;
    let body = format!(
        "{}{}{}",
        render::page_header(
            &format!("{display_name} followers"),
            &format!("Users who follow @{profile_username}.")
        ),
        render::account_links_with_empty_state(
            &accounts,
            render::EmptyState::new("No followers yet.", "Followers will appear here.",),
        ),
        account_list_next_link(
            &format!("/users/{profile_username}/followers"),
            &accounts,
            has_more
        )
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
    Query(query): Query<AccountListQuery>,
) -> AppResult<Html<String>> {
    let user = current(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers).await;
    let Some((profile_id, profile_username, display_name)) =
        profile_identity(&state.pool, &username).await?
    else {
        return Err(AppError::NotFound);
    };
    let (accounts, has_more) = social::following_accounts_for_profile(
        &state.pool,
        profile_id,
        user.as_ref().map(|user| user.id),
        query.after,
    )
    .await?;
    let body = format!(
        "{}{}{}",
        render::page_header(
            &format!("{display_name} following"),
            &format!("Users @{profile_username} follows.")
        ),
        render::account_links_with_empty_state(
            &accounts,
            render::EmptyState::new(
                "Not following anyone yet.",
                "Followed accounts will appear here.",
            ),
        ),
        account_list_next_link(
            &format!("/users/{profile_username}/following"),
            &accounts,
            has_more
        )
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

/// Renders the "Show more" link for keyset-paginated account lists.
fn account_list_next_link(base: &str, accounts: &[social::AccountView], has_more: bool) -> String {
    if !has_more {
        return String::new();
    }
    let Some(last) = accounts.last() else {
        return String::new();
    };
    let cursor = last.username.to_ascii_lowercase();
    let cursor = html_escape::encode_double_quoted_attribute(&cursor);
    let base = html_escape::encode_double_quoted_attribute(base);
    format!(
        r#"<p class="account-list-more"><a class="button-link" href="{base}?after={cursor}">Show more accounts</a></p>"#
    )
}

fn profile_controls(
    user: Option<&CurrentUser>,
    csrf: Option<&str>,
    profile_id: i64,
    relationship: social::ProfileRelationship,
) -> String {
    let (Some(viewer), Some(csrf)) = (user, csrf) else {
        return String::new();
    };
    if viewer.id == profile_id {
        return r#"<div class="actions profile-actions"><a class="button-link" href="/settings">Settings</a></div>"#
            .to_owned();
    }
    if relationship.blocks_viewer {
        return r#"<div class="actions profile-actions"><span class="profile-state-note">This account blocked you.</span></div>"#
            .to_owned();
    }
    if relationship.blocked {
        return format!(
            r#"<div class="actions profile-actions"><span class="profile-state-note">You blocked this account.</span><span class="actions profile-secondary">{}</span></div>"#,
            small_form(
                &format!("/users/{profile_id}/unblock"),
                csrf,
                "Unblock",
                "Unblock this account"
            )
        );
    }
    let follow_action = render::follow_form(
        profile_id,
        csrf,
        render::FollowButtonState::from_flags(relationship.following, relationship.requested),
    );
    let mute_action = if relationship.muted {
        small_form(
            &format!("/users/{profile_id}/unmute"),
            csrf,
            "Unmute",
            "Unmute this account",
        )
    } else {
        small_form(
            &format!("/users/{profile_id}/mute"),
            csrf,
            "Mute",
            "Mute this account",
        )
    };
    format!(
        r#"<div class="actions profile-actions">{}<span class="actions profile-secondary">{}{}</span></div>"#,
        follow_action,
        small_form(
            &format!("/users/{profile_id}/block"),
            csrf,
            "Block",
            "Block this account"
        ),
        mute_action
    )
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

async fn approve_follow_request(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> AppResult<Response> {
    let user = require_active_user(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    social::approve_follow_request(&state.pool, user.id, id)
        .await
        .map_err(|err| AppError::BadRequest(err.to_string()))?;
    Ok(Redirect::to("/follow-requests").into_response())
}

async fn reject_follow_request(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> AppResult<Response> {
    let user = require_active_user(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    social::reject_follow_request(&state.pool, user.id, id)
        .await
        .map_err(|err| AppError::BadRequest(err.to_string()))?;
    Ok(Redirect::to("/follow-requests").into_response())
}

async fn cancel_follow_request(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> AppResult<Response> {
    let user = require_active_user(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    social::cancel_follow_request(&state.pool, user.id, id)
        .await
        .map_err(|err| AppError::BadRequest(err.to_string()))?;
    if enhanced_request(&headers) {
        return Ok(Json(follow_action_response(&state.pool, user.id, id).await?).into_response());
    }
    Ok(Redirect::to("/follow-requests").into_response())
}

async fn follow_requests(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AppResult<Html<String>> {
    let user = require_active_user(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers).await.unwrap_or_default();
    let incoming = social::incoming_follow_requests(&state.pool, user.id).await?;
    let outgoing = social::outgoing_follow_requests(&state.pool, user.id).await?;
    let body = render::follow_requests_page(&incoming, &outgoing, &csrf);
    Ok(Html(
        page_layout(&state, Some(&user), Some(&csrf), "Follow requests", &body).await?,
    ))
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
        Some("username") => Some(("success", "Username changed.")),
        Some("delete-requested") => Some((
            "success",
            "Account deletion requested. You can cancel it until the deadline.",
        )),
        Some("delete-cancelled") => Some(("success", "Account deletion cancelled.")),
        _ => None,
    }
}

/// Username change form plus the account's recorded previous handles.
fn settings_username_panel(
    username: &str,
    history: &[identity::UsernameHistoryEntry],
    csrf: &str,
) -> String {
    let history_html = if history.is_empty() {
        r#"<p class="username-history">No previous usernames.</p>"#.to_owned()
    } else {
        let rows = history
            .iter()
            .map(|entry| {
                format!(
                    r#"<li>@{} <span class="muted">changed {}</span></li>"#,
                    html_escape::encode_text(&entry.username),
                    html_escape::encode_text(&entry.changed_at)
                )
            })
            .collect::<Vec<_>>()
            .join("");
        format!(
            r#"<h3>Previous usernames</h3><p class="username-history">Handles you used before stay reserved so nobody else can claim them. You can always switch back to one of them.</p><ul class="username-history-list">{rows}</ul>"#
        )
    };
    format!(
        r#"<section class="panel settings-card" data-testid="settings-card"><h2>Username</h2><p class="settings-section-help">Your public @handle is used in profile links and mentions.</p><form method="post" action="/settings/username" class="settings-password-form"><input type="hidden" name="csrf" value="{csrf}"><label for="new_username">New username</label><input id="new_username" name="new_username" value="{username}" autocomplete="username" required><label for="username_password">Current password</label><div class="password-control"><input id="username_password" name="password" type="password" autocomplete="current-password" required><button type="button" class="password-toggle" data-password-toggle="username_password" aria-label="Show password">Show</button></div><div class="settings-form-actions"><button type="submit">Change username</button></div></form>{history_html}</section>"#,
        csrf = html_escape::encode_double_quoted_attribute(csrf),
        username = html_escape::encode_double_quoted_attribute(username),
    )
}

/// Export and import entry points for portable account archives.
fn settings_account_data_panel() -> String {
    r#"<section class="panel settings-card" data-testid="settings-card"><h2>Account data</h2><p class="settings-section-help">Export your posts, profile, media, follows, and settings as a portable archive, or import an archive exported from another RustPost instance.</p><p class="actions"><a class="button-link" href="/settings/export">Export account archive</a> <a class="button-link" href="/settings/import">Import account archive</a></p><p class="muted">Archives never include your password, sessions, tokens, or administrator access.</p></section>"#
        .to_owned()
}

/// Danger panel for requesting or cancelling account deletion.
fn settings_delete_panel(deletion: Option<&account::DeletionRequest>, csrf: &str) -> String {
    deletion.map_or_else(
        || {
            r#"<section class="panel settings-card danger-panel" data-testid="settings-card"><h2>Delete account</h2><p>Deleting your account starts a countdown before permanent removal.</p><p class="settings-danger-action"><a class="button-link danger-link" href="/settings/delete">Start delete account flow</a></p></section>"#.to_owned()
        },
        |deletion| {
            format!(
                r#"<section class="panel settings-card danger-panel" data-testid="settings-card"><h2>Delete account</h2><p data-testid="deletion-deadline">This account is scheduled for permanent deletion on {}.</p><p>You can cancel the deletion until then. Until it completes, this account keeps your posts but cannot publish or change account data.</p><form method="post" action="/settings/delete/cancel"><input type="hidden" name="csrf" value="{}"><button type="submit">Cancel deletion</button></form></section>"#,
                html_escape::encode_text(&deletion.scheduled_at),
                html_escape::encode_double_quoted_attribute(csrf),
            )
        },
    )
}

struct SettingsProfile {
    display_name: String,
    bio: String,
    location: String,
    website: String,
    theme: String,
    nsfw_blur_enabled: bool,
    liked_posts_public: bool,
    follow_approval_required: bool,
    picture_path: Option<String>,
    banner_path: Option<String>,
}

async fn settings_profile(pool: &SqlitePool, user_id: i64) -> AppResult<SettingsProfile> {
    pool.call(move |conn| {
        Ok(conn
            .query_row(
                r#"
        SELECT u.display_name, u.bio, u.location, u.website, u.theme, u.nsfw_blur_enabled,
          u.liked_posts_public, u.follow_approval_required,
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
                        liked_posts_public: row.get::<_, i64>(6)? != 0,
                        follow_approval_required: row.get::<_, i64>(7)? != 0,
                        picture_path: row.get(8)?,
                        banner_path: row.get(9)?,
                    })
                },
            )
            .optional()?)
    })
    .await?
    .ok_or(AppError::Unauthorized)
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
    let banner_controls = if allow_profile_banners {
        settings_media_actions(
            "settings-banner-actions",
            "banner",
            "banner",
            "Change banner",
            banner_path.map(|_| ("delete_banner", "delete_banner", "Remove banner")),
        )
    } else {
        settings_media_disabled("Profile banners are disabled.")
    };
    let picture_controls = if allow_profile_pictures {
        settings_media_actions(
            "settings-picture-actions",
            "profile_picture",
            "profile_picture",
            "Change profile picture",
            picture_path.map(|_| {
                (
                    "delete_profile_picture",
                    "delete_profile_picture",
                    "Remove profile picture",
                )
            }),
        )
    } else {
        settings_media_disabled("Profile pictures are disabled.")
    };
    format!(
        r#"<div class="settings-profile-media"><div class="settings-banner-wrap settings-media-frame" data-profile-media-frame>{banner}{banner_controls}</div><div class="settings-picture-row"><div class="settings-picture-wrap settings-media-frame" data-profile-media-frame>{picture}{picture_controls}</div></div></div>"#
    )
}

fn settings_media_actions(
    action_class: &str,
    file_id: &str,
    file_name: &str,
    change_label: &str,
    delete: Option<(&str, &str, &str)>,
) -> String {
    let delete_control =
        delete.map_or_else(String::new, |(delete_id, delete_name, delete_label)| {
            settings_media_delete_control(delete_id, delete_name, delete_label)
        });
    format!(
        r#"<div class="settings-media-actions {action_class}">{}{delete_control}</div><span class="sr-only" aria-live="polite" data-profile-media-status></span>"#,
        settings_media_file_control(file_id, file_name, change_label),
        action_class = html_escape::encode_double_quoted_attribute(action_class),
    )
}

fn settings_media_file_control(input_id: &str, name: &str, label: &str) -> String {
    let input_id = html_escape::encode_double_quoted_attribute(input_id);
    let name = html_escape::encode_double_quoted_attribute(name);
    let label_attr = html_escape::encode_double_quoted_attribute(label);
    let label_text = html_escape::encode_text(label);
    format!(
        r#"<span class="settings-media-control"><input class="settings-media-input" id="{input_id}" name="{name}" type="file" accept="image/*" aria-label="{label_attr}" data-profile-media-file><label class="settings-media-icon-button settings-media-change" for="{input_id}" title="{label_attr}">{}<span class="sr-only">{label_text}</span></label></span>"#,
        render::icon_svg("edit"),
    )
}

fn settings_media_delete_control(input_id: &str, name: &str, label: &str) -> String {
    let input_id = html_escape::encode_double_quoted_attribute(input_id);
    let name = html_escape::encode_double_quoted_attribute(name);
    let label_attr = html_escape::encode_double_quoted_attribute(label);
    let label_text = html_escape::encode_text(label);
    format!(
        r#"<span class="settings-media-control"><input class="settings-media-delete-input" id="{input_id}" name="{name}" type="checkbox" value="true" aria-label="{label_attr}" data-profile-media-delete><label class="settings-media-icon-button settings-media-remove" for="{input_id}" title="{label_attr}">{}<span class="sr-only">{label_text}</span></label></span>"#,
        render::icon_svg("trash"),
    )
}

fn settings_media_disabled(message: &str) -> String {
    format!(
        r#"<p class="settings-media-disabled">{}</p>"#,
        html_escape::encode_text(message),
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
        return render::compact_empty_state(empty_title, empty_message);
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
        return render::compact_empty_state(
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

fn validate_profile_website(website: &str) -> AppResult<()> {
    let website = website.trim();
    if website.is_empty() {
        return Ok(());
    }
    if website.chars().count() > 2_048 {
        return Err(AppError::BadRequest("website URL is too long".to_owned()));
    }
    if website.chars().any(char::is_control) {
        return Err(AppError::BadRequest(
            "website URL contains unsupported control characters".to_owned(),
        ));
    }
    if !is_safe_profile_website_url(website) {
        return Err(AppError::BadRequest(
            "website URL must start with http:// or https://".to_owned(),
        ));
    }
    Ok(())
}

fn is_safe_profile_website_url(website: &str) -> bool {
    website.parse::<Uri>().is_ok_and(|uri| {
        matches!(uri.scheme_str(), Some("http" | "https")) && uri.authority().is_some()
    })
}

fn render_profile_website_link(website: &str) -> String {
    let website = website.trim();
    if website.is_empty() || !is_safe_profile_website_url(website) {
        return String::new();
    }
    format!(
        r#"<p class="profile-meta profile-website"><a href="{}" rel="noopener noreferrer nofollow">{}</a></p>"#,
        html_escape::encode_double_quoted_attribute(website),
        html_escape::encode_text(website)
    )
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

#[expect(
    clippy::too_many_lines,
    reason = "the settings page template is assembled in one place so panels keep a consistent layout"
)]
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
    let username_history = identity::username_history(&state.pool, user.id).await?;
    let deletion = account::deletion_status(&state.pool, user.id).await?;
    let username_panel = settings_username_panel(&user.username, &username_history, csrf);
    let delete_panel = settings_delete_panel(deletion.as_ref(), csrf);
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
    let profile_fields = format!(
        r#"<div class="settings-fields settings-profile-fields"><label for="display_name">Display name</label><input id="display_name" name="display_name" value="{}"><label for="bio">Bio</label><textarea id="bio" name="bio">{}</textarea><label for="location">Location</label><input id="location" name="location" value="{}"><label for="website">Website</label><input id="website" type="url" name="website" value="{}"></div>"#,
        html_escape::encode_double_quoted_attribute(profile.display_name.as_str()),
        html_escape::encode_text(profile.bio.as_str()),
        html_escape::encode_double_quoted_attribute(profile.location.as_str()),
        html_escape::encode_double_quoted_attribute(profile.website.as_str()),
    );
    let preference_switches = [
        settings_switch(
            "dark_mode",
            Theme::from(profile.theme.as_str()) == Theme::Dark,
            "Dark mode",
            "Use the darker color theme for your signed-in session.",
        ),
        settings_switch(
            "nsfw_blur_enabled",
            profile.nsfw_blur_enabled,
            "Blur NSFW media",
            "Hide NSFW media previews behind a reveal control.",
        ),
        settings_switch(
            "liked_posts_public",
            profile.liked_posts_public,
            "Make liked posts public",
            "Allow other people to view your Likes tab on your profile.",
        ),
        settings_switch(
            "follow_approval_required",
            profile.follow_approval_required,
            "Require approval for new followers",
            "New followers must be approved before they can follow you. Existing followers are kept, and pending requests are not affected.",
        ),
    ]
    .join("");
    let body = format!(
        r#"{notice_html}<section class="panel settings-card settings-profile-editor" data-testid="settings-card"><div class="settings-editor-bar"><div><h1>Account settings</h1><p class="muted">Profile, privacy, media, and account controls.</p></div></div><form id="profile-settings-form" method="post" enctype="multipart/form-data" class="settings-profile-form"><input type="hidden" name="csrf" value="{}"><div class="settings-section settings-section-media"><div class="settings-section-heading"><h2>Profile media</h2><p class="settings-section-help">Avatar and banner images for your profile header.</p></div>{}</div><div class="settings-section settings-section-profile"><div class="settings-section-heading"><h2>Profile</h2><p class="settings-section-help">Your name, bio, location, and link as shown on your profile.</p></div>{}</div><div class="settings-section settings-section-preferences"><div class="settings-section-heading"><h2>Preferences and privacy</h2><p class="settings-section-help">Control your display theme, NSFW media blur, and profile activity visibility.</p></div><div class="settings-switch-list">{}</div></div><div class="settings-form-actions"><button class="primary" type="submit">Save profile settings</button></div></form></section><div class="settings-grid"><section class="panel settings-card compact-panel settings-list-panel" data-testid="settings-card"><h2>Blocked users</h2><p class="settings-section-help">Blocked accounts cannot follow or interact with you.</p>{}</section><section class="panel settings-card compact-panel settings-list-panel" data-testid="settings-card"><h2>Muted users</h2><p class="settings-section-help">Muted accounts stay hidden from your views.</p>{}</section></div><section class="panel settings-card compact-panel settings-list-panel" data-testid="settings-card"><h2>Muted words</h2><p class="settings-section-help">Hide posts containing specific words or phrases.</p><form method="post" action="/settings/muted-words" class="inline-settings-form"><input type="hidden" name="csrf" value="{}"><label class="sr-only" for="muted-word">Word or phrase to mute</label><input id="muted-word" name="term" placeholder="Word or phrase" required><button type="submit">Add muted word</button></form>{}</section><section class="panel settings-card compact-panel settings-security-panel" data-testid="settings-card"><h2>Change password</h2><p class="settings-section-help">Update the password used to sign in to this account.</p><form method="post" action="/settings/password" class="settings-password-form"><input type="hidden" name="csrf" value="{}"><label for="current_password">Current password</label><div class="password-control"><input id="current_password" name="current_password" type="password" autocomplete="current-password"><button type="button" class="password-toggle" data-password-toggle="current_password" aria-label="Show current password">Show</button></div><label for="new_password">New password</label><p class="field-help" id="new-password-requirement">{}</p><div class="password-control"><input id="new_password" name="new_password" type="password" autocomplete="new-password"{}><button type="button" class="password-toggle" data-password-toggle="new_password" aria-label="Show new password">Show</button></div><label for="confirm_new_password">Confirm new password</label><p class="field-help" id="confirm-new-password-requirement">{}</p><div class="password-control"><input id="confirm_new_password" name="confirm_new_password" type="password" autocomplete="new-password"{}><button type="button" class="password-toggle" data-password-toggle="confirm_new_password" aria-label="Show new password confirmation">Show</button></div><div class="settings-form-actions"><button type="submit">Change password</button></div></form></section>{username_panel}{account_data_panel}{delete_panel}"#,
        html_escape::encode_double_quoted_attribute(&csrf),
        profile_media,
        profile_fields,
        preference_switches,
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
        username_panel = username_panel,
        account_data_panel = settings_account_data_panel(),
        delete_panel = delete_panel,
    );
    page_layout(state, Some(user), Some(csrf), "Settings", &body).await
}

fn settings_switch(id: &str, checked: bool, label: &str, help: &str) -> String {
    let checked_attr = if checked { " checked" } else { "" };
    let help_id = format!("{id}-help");
    format!(
        r#"<label class="settings-switch-row" for="{id}"><span class="settings-switch-copy"><span class="settings-switch-label">{label}</span><span class="settings-switch-help" id="{help_id}">{help}</span></span><span class="settings-switch-toggle"><input class="settings-switch-input" id="{id}" name="{id}" type="checkbox" role="switch" value="true"{checked_attr} aria-describedby="{help_id}"><span class="settings-switch-control" aria-hidden="true"></span></span></label>"#,
        id = html_escape::encode_double_quoted_attribute(id),
        help_id = html_escape::encode_double_quoted_attribute(&help_id),
        label = html_escape::encode_text(label),
        help = html_escape::encode_text(help),
    )
}

async fn settings_update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    multipart: Multipart,
) -> AppResult<Response> {
    let user = require_user(&state, &headers).await?;
    let form = parse_profile_update(&state, user.id, multipart).await?;
    let outcome = apply_profile_update(&state, &headers, &user, &form).await;
    if outcome.is_err() {
        cleanup_profile_uploads(&state, &form).await;
    }
    outcome
}

async fn apply_profile_update(
    state: &AppState,
    headers: &HeaderMap,
    user: &CurrentUser,
    form: &ParsedProfileUpdate,
) -> AppResult<Response> {
    validate_csrf(&state.pool, headers, &form.csrf_token).await?;
    crate::validation::validate_profile_text(&form.display_name, &form.bio, &state.settings)?;
    validate_profile_location(&form.location)?;
    validate_profile_website(&form.website)?;
    let display_name = form.display_name.trim().to_owned();
    let bio = form.bio.trim().to_owned();
    let location = form.location.trim().to_owned();
    let website = form.website.trim().to_owned();
    let theme = form.theme.as_str().to_owned();
    let nsfw_blur_enabled = i64::from(form.nsfw_blur_enabled);
    let liked_posts_public = i64::from(form.liked_posts_public);
    let follow_approval_required = i64::from(form.follow_approval_required);
    let user_id = user.id;
    state
        .pool
        .call(move |conn| {
            conn.execute(
                "UPDATE users SET display_name = ?, bio = ?, location = ?, website = ?, theme = ?, nsfw_blur_enabled = ?, liked_posts_public = ?, follow_approval_required = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
                params![
                    display_name,
                    bio,
                    location,
                    website,
                    theme,
                    nsfw_blur_enabled,
                    liked_posts_public,
                    follow_approval_required,
                    user_id
                ],
            )?;
            Ok(())
        })
        .await?;
    if form.delete_profile_picture {
        media::clear_profile_media(
            &state.pool,
            &state.paths,
            user.id,
            media::ProfileMediaSlot::Picture,
        )
        .await?;
    }
    if form.delete_banner {
        media::clear_profile_media(
            &state.pool,
            &state.paths,
            user.id,
            media::ProfileMediaSlot::Banner,
        )
        .await?;
    }
    if let Some(media_id) = form.profile_picture_media_id {
        media::set_profile_media(
            &state.pool,
            &state.paths,
            user.id,
            media::ProfileMediaSlot::Picture,
            media_id,
        )
        .await?;
    }
    if let Some(media_id) = form.banner_media_id {
        media::set_profile_media(
            &state.pool,
            &state.paths,
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
    let current_session_token = auth::session_cookie(&headers);
    match auth::change_password(
        &state.pool,
        &state.settings,
        user.id,
        &form.current_password,
        &form.new_password,
        &form.confirm_new_password,
        current_session_token.as_deref(),
    )
    .await
    {
        Ok(()) => Ok(Redirect::to("/settings?saved=password").into_response()),
        Err(err) => {
            password_response(
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

/// Renders password-change failures on the restricted page while a forced
/// reset is active, and on the full settings page otherwise.
async fn password_response(
    state: &AppState,
    user: &CurrentUser,
    headers: &HeaderMap,
    status: StatusCode,
    kind: &'static str,
    message: &str,
) -> AppResult<Response> {
    if user.must_change_password {
        let csrf = form_csrf(state, headers).await.unwrap_or_default();
        return Ok((
            status,
            Html(password_change_page_html(
                state,
                user,
                &csrf,
                Some((kind, message)),
            )),
        )
            .into_response());
    }
    settings_response(state, user, headers, status, kind, message).await
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
    let deletion = account::deletion_status(&state.pool, user.id).await?;
    let body = deletion.map_or_else(
        || render_delete_account_warning(state.settings.accounts.deletion_grace_period_days),
        |deletion| render_delete_account_pending(&deletion, &csrf),
    );
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
    if state.settings.accounts.deletion_grace_period_days > 0 {
        let password_ok = auth::verify_user_password(&state.pool, user.id, &form.password).await?;
        if !password_ok {
            return delete_account_final_response(
                &state,
                &user,
                &headers,
                StatusCode::UNAUTHORIZED,
                "Password is incorrect.",
            )
            .await;
        }
        return match account::request_deletion(&state.pool, &state.settings, user.id).await {
            Ok(_request) => Ok(Redirect::to("/settings?saved=delete-requested").into_response()),
            Err(err) => {
                tracing::warn!(user_id = user.id, error = %err, "account deletion request failed");
                delete_account_final_response(
                    &state,
                    &user,
                    &headers,
                    StatusCode::BAD_REQUEST,
                    "Account deletion could not be scheduled. Try again later.",
                )
                .await
            }
        };
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

fn render_delete_account_warning(grace_days: u64) -> String {
    let grace = if grace_days == 0 {
        "With the current configuration the account is removed immediately after password confirmation."
            .to_owned()
    } else {
        format!(
            "After you confirm with your password, RustPost starts a {grace_days}-day countdown. You can cancel any time before the deadline; permanent removal happens after it."
        )
    };
    format!(
        r#"<section class="panel danger-panel delete-account-panel"><h1>Delete account</h1><p>Permanent removal deletes your profile, posts, reposts, likes, follows, blocks, mutes, bookmarks, sessions, and uploaded media owned by your account.</p><p>{}</p><div class="actions"><form method="get" action="/settings/delete/confirm"><button class="danger" type="submit">Confirm delete account</button></form><a class="button-link" href="/settings">Cancel</a></div></section>"#,
        html_escape::encode_text(&grace),
    )
}

fn render_delete_account_pending(deletion: &account::DeletionRequest, csrf: &str) -> String {
    format!(
        r#"<section class="panel danger-panel delete-account-panel" data-testid="deletion-pending"><h1>Deletion scheduled</h1><p>This account is scheduled for permanent deletion on <strong>{}</strong>.</p><p>Until then your posts stay visible, but publishing and account changes are disabled. You can cancel the deletion below.</p><form method="post" action="/settings/delete/cancel"><input type="hidden" name="csrf" value="{}"><button type="submit">Cancel deletion</button></form><p class="muted">Requested {}</p></section>"#,
        html_escape::encode_text(&deletion.scheduled_at),
        html_escape::encode_double_quoted_attribute(csrf),
        html_escape::encode_text(&deletion.requested_at),
    )
}

fn render_delete_account_final_warning(
    csrf: &str,
    delete_intent: &str,
    error: Option<&str>,
) -> String {
    let notice = error.map_or_else(String::new, |message| render::notice("error", message));
    format!(
        r#"{notice}<section class="panel danger-panel delete-account-panel"><h1>Final warning</h1><p>Deleting your account cannot be undone. Enter your password to permanently delete this account.</p><form method="post" action="/settings/delete/confirm" class="settings-password-form"><input type="hidden" name="csrf" value="{}"><input type="hidden" name="delete_intent" value="{}"><label for="delete_password">Password</label><div class="password-control"><input id="delete_password" name="password" type="password" autocomplete="current-password" required><button type="button" class="password-toggle" data-password-toggle="delete_password" aria-label="Show password">Show</button></div><div class="actions"><button class="danger" type="submit">Delete account permanently</button><a class="button-link" href="/settings">Cancel</a></div></form></section>"#,
        html_escape::encode_double_quoted_attribute(csrf),
        html_escape::encode_double_quoted_attribute(delete_intent)
    )
}

/// Standalone password form used for the forced-password-reset flow and
/// direct visits to `/settings/password`.
async fn password_change_page(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<SettingsQuery>,
) -> AppResult<Html<String>> {
    let user = require_user(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers).await.unwrap_or_default();
    let notice = if query.required.as_deref() == Some("1") {
        Some((
            "error",
            "You must change your password before using the rest of your account.",
        ))
    } else {
        settings_query_notice(query.saved.as_deref())
    };
    Ok(Html(password_change_page_html(
        &state, &user, &csrf, notice,
    )))
}

fn password_change_page_html(
    state: &AppState,
    user: &CurrentUser,
    csrf: &str,
    notice: Option<(&str, &str)>,
) -> String {
    let notice_html =
        notice.map_or_else(String::new, |(kind, message)| render::notice(kind, message));
    let hint = password_hint(state);
    let new_password_attrs = render::password_length_attrs(
        state.settings.accounts.min_password_length,
        "new-password-requirement",
    );
    let confirm_new_password_attrs = render::password_length_attrs(
        state.settings.accounts.min_password_length,
        "confirm-new-password-requirement",
    );
    let logout = small_form("/logout", csrf, "Log out", "Log out of this session");
    let restricted = if user.must_change_password {
        r#"<p class="settings-section-help">An administrator requires this account to set a new password before it can be used again.</p>"#
    } else {
        ""
    };
    format!(
        r#"{notice_html}<section class="panel settings-card settings-security-panel" data-testid="settings-card"><h2>Change password</h2><p class="settings-section-help">Update the password used to sign in to this account.</p>{restricted}{}<div class="actions">{logout}</div></section>"#,
        password_form_html(
            csrf,
            &hint,
            &new_password_attrs,
            &confirm_new_password_attrs
        ),
    )
}

fn password_hint(state: &AppState) -> String {
    if state.settings.accounts.min_password_length == 0 {
        "No minimum password length is currently required.".to_owned()
    } else {
        format!(
            "Password must be at least {} characters.",
            state.settings.accounts.min_password_length
        )
    }
}

fn password_form_html(
    csrf: &str,
    hint: &str,
    new_password_attrs: &str,
    confirm_new_password_attrs: &str,
) -> String {
    format!(
        r#"<form method="post" action="/settings/password" class="settings-password-form"><input type="hidden" name="csrf" value="{}"><label for="current_password">Current password</label><div class="password-control"><input id="current_password" name="current_password" type="password" autocomplete="current-password"><button type="button" class="password-toggle" data-password-toggle="current_password" aria-label="Show current password">Show</button></div><label for="new_password">New password</label><p class="field-help" id="new-password-requirement">{}</p><div class="password-control"><input id="new_password" name="new_password" type="password" autocomplete="new-password"{}><button type="button" class="password-toggle" data-password-toggle="new_password" aria-label="Show new password">Show</button></div><label for="confirm_new_password">Confirm new password</label><p class="field-help" id="confirm-new-password-requirement">{}</p><div class="password-control"><input id="confirm_new_password" name="confirm_new_password" type="password" autocomplete="new-password"{}><button type="button" class="password-toggle" data-password-toggle="confirm_new_password" aria-label="Show new password confirmation">Show</button></div><div class="settings-form-actions"><button type="submit">Change password</button></div></form>"#,
        html_escape::encode_double_quoted_attribute(csrf),
        html_escape::encode_text(hint),
        new_password_attrs,
        html_escape::encode_text(hint),
        confirm_new_password_attrs,
    )
}

#[derive(Deserialize)]
struct UsernameChangeForm {
    csrf: String,
    new_username: String,
    password: String,
}

async fn change_username(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<UsernameChangeForm>,
) -> AppResult<Response> {
    let user = require_active_user(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    let password_ok = auth::verify_user_password(&state.pool, user.id, &form.password).await?;
    if !password_ok {
        return settings_response(
            &state,
            &user,
            &headers,
            StatusCode::UNAUTHORIZED,
            "error",
            "Password is incorrect.",
        )
        .await;
    }
    match identity::change_username(&state.pool, &state.settings, user.id, &form.new_username).await
    {
        Ok(_change) => Ok(Redirect::to("/settings?saved=username").into_response()),
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

async fn cancel_deletion(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> AppResult<Response> {
    let user = require_active_user(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    account::cancel_deletion(&state.pool, user.id).await?;
    Ok(Redirect::to("/settings?saved=delete-cancelled").into_response())
}

async fn export_account(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AppResult<Response> {
    let user = require_active_user(&state, &headers).await?;
    // The guard keeps runtime temp cleanup from removing the staged export
    // archive while it is being written.
    let _operation = crate::runtime::begin_temp_operation();
    let filename = format!(
        "rustpost-account-{}-{}.tar.gz",
        safe_filename_component(&user.username),
        Uuid::new_v4().simple()
    );
    let destination =
        state
            .paths
            .tmp_dir
            .join(format!("{}{}", portability::EXPORT_TMP_PREFIX, filename));
    let export = match portability::export_account(
        &state.pool,
        &state.paths,
        user.id,
        &destination,
        portability::ArchiveLimits::from_settings(&state.settings),
    )
    .await
    {
        Ok(export) => export,
        Err(err) => {
            tracing::warn!(user_id = user.id, error = %err, "account export failed");
            let _ = tokio::fs::remove_file(&destination).await;
            return Err(AppError::BadRequest(err.to_string()));
        }
    };
    stream_staged_file(&export.archive_path, &filename, "application/gzip").await
}

/// Validates a value for use inside an attachment filename.
fn safe_filename_component(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        .take(32)
        .collect()
}

async fn stream_staged_file(
    path: &std::path::Path,
    download_name: &str,
    content_type: &'static str,
) -> AppResult<Response> {
    let metadata = tokio::fs::metadata(path).await?;
    let file = tokio::fs::File::open(path).await?;
    let cleanup_path = path.to_owned();
    let stream = futures_util::stream::unfold(
        (file, cleanup_path),
        |(mut file, cleanup_path)| async move {
            let mut buffer = vec![0u8; 64 * 1024];
            match file.read(&mut buffer).await {
                Ok(0) => {
                    let _ = tokio::fs::remove_file(&cleanup_path).await;
                    None
                }
                Ok(read) => {
                    buffer.truncate(read);
                    Some((
                        Ok::<Bytes, io::Error>(Bytes::from(buffer)),
                        (file, cleanup_path),
                    ))
                }
                Err(error) => {
                    let _ = tokio::fs::remove_file(&cleanup_path).await;
                    Some((Err(error), (file, cleanup_path)))
                }
            }
        },
    );
    let disposition = format!(
        "attachment; filename=\"{}\"",
        download_name.replace('"', "")
    );
    let mut response = Body::from_stream(stream).into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response.headers_mut().insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&metadata.len().to_string())
            .map_err(|err| AppError::BadRequest(err.to_string()))?,
    );
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&disposition).map_err(|err| AppError::BadRequest(err.to_string()))?,
    );
    Ok(response)
}

async fn import_account_form(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AppResult<Html<String>> {
    let user = require_active_user(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers).await.unwrap_or_default();
    let instance = instance::load(&state.pool).await?;
    let body = if instance.maintenance_mode {
        format!(
            r#"<section class="panel"><h1>Import account archive</h1>{}</section>"#,
            render::notice(
                "error",
                instance
                    .maintenance_notice()
                    .unwrap_or(instance::DEFAULT_MAINTENANCE_MESSAGE)
            )
        )
    } else {
        render_import_account_form(
            &csrf,
            user.must_change_password,
            state.settings.accounts.max_archive_upload_bytes,
            state.settings.accounts.max_archive_expanded_bytes,
        )
    };
    Ok(Html(
        page_layout(&state, Some(&user), Some(&csrf), "Import account", &body).await?,
    ))
}

fn render_import_account_form(
    csrf: &str,
    forced_password_change: bool,
    max_upload_bytes: u64,
    max_expanded_bytes: u64,
) -> String {
    let disabled = if forced_password_change {
        r#"<p class="settings-section-help">Finish the required password change before importing an archive.</p>"#
    } else {
        ""
    };
    format!(
        r#"<section class="panel" data-testid="import-account-panel"><h1>Import account archive</h1><p>Import a RustPost account archive that you exported from another instance. The archive adds posts, media, muted words, and outgoing follows to your account. Usernames, passwords, administrator flags, and sessions in an archive are ignored.</p>{}{disabled}<form method="post" action="/settings/import" enctype="multipart/form-data"><input type="hidden" name="csrf" value="{}"><label for="account_archive">RustPost account archive (.tar.gz)</label><input id="account_archive" name="archive" type="file" accept=".tar.gz,application/gzip" required><p class="muted">Maximum compressed archive size: {}. Maximum expanded media size: {}. Limits are enforced while the archive is streamed.</p><div class="actions"><button type="submit">Import archive</button></div></form><p><a class="button-link" href="/settings">Back to settings</a></p></section>"#,
        render::notice(
            "info",
            "Follows to protected accounts become pending requests and still require approval.",
        ),
        html_escape::encode_double_quoted_attribute(csrf),
        format_bytes(max_upload_bytes),
        format_bytes(max_expanded_bytes),
    )
}

async fn import_account(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    multipart: Multipart,
) -> AppResult<Response> {
    let user = require_active_user(&state, &headers).await?;
    // The guard keeps runtime temp cleanup from removing this operation's
    // staged upload or export archive while it is in flight.
    let _operation = crate::runtime::begin_temp_operation();
    // Imports publish content, so they share the post rate-limit budget.
    rate_limit::check_and_record(
        &state.pool,
        rate_limit::Scope::Post,
        &user_actor(user.id),
        state.settings.moderation.posts_per_minute,
        60,
    )
    .await
    .map_err(|err| AppError::RateLimited(err.to_string()))?;
    let staged = stage_account_import(&state, &headers, multipart).await?;
    let report =
        portability::import_account(&state.pool, &state.paths, &state.settings, user.id, &staged)
            .await;
    let _ = tokio::fs::remove_file(&staged).await;
    match report {
        Ok(report) => {
            let csrf = form_csrf(&state, &headers).await.unwrap_or_default();
            let body = render_import_report(&report, &csrf);
            Ok(
                Html(
                    page_layout(&state, Some(&user), Some(&csrf), "Import complete", &body).await?,
                )
                .into_response(),
            )
        }
        Err(err) => {
            tracing::warn!(user_id = user.id, error = %err, "account import failed");
            let csrf = form_csrf(&state, &headers).await.unwrap_or_default();
            let body = format!(
                r#"{}{}"#,
                render::notice("error", &err.to_string()),
                render_import_account_form(
                    &csrf,
                    user.must_change_password,
                    state.settings.accounts.max_archive_upload_bytes,
                    state.settings.accounts.max_archive_expanded_bytes,
                )
            );
            Ok((
                StatusCode::BAD_REQUEST,
                Html(page_layout(&state, Some(&user), Some(&csrf), "Import account", &body).await?),
            )
                .into_response())
        }
    }
}

async fn stage_account_import(
    state: &AppState,
    headers: &HeaderMap,
    mut multipart: Multipart,
) -> AppResult<PathBuf> {
    /// Smallest accepted `csrf` field; larger values are rejected so a
    /// malicious request cannot make the server buffer an arbitrary string.
    const MAX_CSRF_FIELD_BYTES: u64 = 4 * 1024;
    /// Bound for unrelated multipart fields that are drained and ignored.
    const MAX_IGNORED_FIELD_BYTES: u64 = 64 * 1024;

    let mut csrf: Option<String> = None;
    let mut staged: Option<PathBuf> = None;
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|err| AppError::BadRequest(err.to_string()))?
    {
        let name = field.name().map(ToOwned::to_owned).unwrap_or_default();
        if name == "csrf" {
            csrf = Some(read_limited_text_field(&mut field, MAX_CSRF_FIELD_BYTES).await?);
            continue;
        }
        if name != "archive" || field.file_name().is_none() {
            drain_limited_field(&mut field, MAX_IGNORED_FIELD_BYTES).await?;
            continue;
        }
        let csrf_token = csrf.clone().ok_or(AppError::Forbidden)?;
        validate_csrf(&state.pool, headers, &csrf_token).await?;
        if staged.is_some() {
            return Err(AppError::BadRequest(
                "only one archive can be imported at a time".to_owned(),
            ));
        }
        let path = state.paths.tmp_dir.join(format!(
            "{}{}.tar.gz",
            portability::IMPORT_TMP_PREFIX,
            Uuid::new_v4().simple()
        ));
        let written = write_multipart_field_to_file(
            field,
            &path,
            Some(state.settings.accounts.max_archive_upload_bytes),
        )
        .await;
        match written {
            Ok(_bytes) => staged = Some(path),
            Err(err) => {
                let _ = tokio::fs::remove_file(&path).await;
                return Err(err);
            }
        }
    }
    staged.ok_or_else(|| AppError::BadRequest("choose a RustPost account archive".to_owned()))
}

/// Reads a text multipart field with a byte cap so an oversized field cannot
/// force unbounded buffering.
async fn read_limited_text_field(
    field: &mut axum::extract::multipart::Field<'_>,
    max_bytes: u64,
) -> AppResult<String> {
    let mut bytes = Vec::new();
    while let Some(chunk) = field
        .chunk()
        .await
        .map_err(|err| AppError::BadRequest(err.to_string()))?
    {
        if u64::try_from(bytes.len() + chunk.len()).unwrap_or(u64::MAX) > max_bytes {
            return Err(AppError::BadRequest(
                "a multipart form field is too large".to_owned(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).map_err(|_utf8| AppError::BadRequest("invalid form value".to_owned()))
}

/// Drains an ignored multipart field while enforcing a small cap.
async fn drain_limited_field(
    field: &mut axum::extract::multipart::Field<'_>,
    max_bytes: u64,
) -> AppResult<()> {
    let mut read = 0u64;
    while let Some(chunk) = field
        .chunk()
        .await
        .map_err(|err| AppError::BadRequest(err.to_string()))?
    {
        read = read.saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
        if read > max_bytes {
            return Err(AppError::BadRequest(
                "a multipart form field is too large".to_owned(),
            ));
        }
    }
    Ok(())
}

fn render_import_report(report: &portability::ImportReport, csrf: &str) -> String {
    format!(
        r#"<section class="panel" data-testid="import-report"><h1>Import complete</h1><dl><dt>Archive account</dt><dd>@{}</dd><dt>Posts imported</dt><dd>{}</dd><dt>Media imported</dt><dd>{}</dd><dt>Follows added</dt><dd>{}</dd><dt>Follow requests pending approval</dt><dd>{}</dd><dt>Follows skipped</dt><dd>{}</dd><dt>Post references unresolved</dt><dd>{}</dd><dt>Muted words imported</dt><dd>{}</dd><dt>Profile fields applied</dt><dd>{}</dd><dt>Profile fields kept</dt><dd>{}</dd></dl><p><a class="button-link" href="/settings">Back to settings</a></p>{}</section>"#,
        html_escape::encode_text(&report.archive_username),
        report.posts_imported,
        report.media_imported,
        report.follows_imported,
        report.follows_pending,
        report.follows_skipped,
        report.post_references_dropped,
        report.muted_words_imported,
        report.profile_fields_applied,
        report.profile_fields_skipped,
        small_form(
            "/settings/import",
            csrf,
            "Import another archive",
            "Import another account archive"
        ),
    )
}

// Multipart parsing is kept in one place so uploaded profile media and text
// fields share one validation path before any database updates happen.
async fn parse_profile_update(
    state: &AppState,
    user_id: i64,
    multipart: Multipart,
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
        liked_posts_public: false,
        follow_approval_required: false,
        profile_picture_media_id: None,
        banner_media_id: None,
    };
    let outcome = fill_profile_update_form(state, user_id, multipart, &mut form).await;
    if outcome.is_err() {
        cleanup_profile_uploads(state, &form).await;
    }
    outcome.map(|()| form)
}

async fn fill_profile_update_form(
    state: &AppState,
    user_id: i64,
    mut multipart: Multipart,
    form: &mut ParsedProfileUpdate,
) -> AppResult<()> {
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|err| AppError::BadRequest(err.to_string()))?
    {
        let Some(name) = field.name().map(ToOwned::to_owned) else {
            continue;
        };
        match name.as_str() {
            "csrf" | "display_name" | "bio" | "location" | "website" => {
                fill_profile_text_field(form, &name, field).await?;
            }
            "dark_mode" => form.theme = Theme::Dark,
            "nsfw_blur_enabled" => form.nsfw_blur_enabled = true,
            "liked_posts_public" => form.liked_posts_public = true,
            "follow_approval_required" => form.follow_approval_required = true,
            "delete_profile_picture" => form.delete_profile_picture = true,
            "delete_banner" => form.delete_banner = true,
            "profile_picture" if field.file_name().is_some() => {
                fill_profile_picture_upload(state, user_id, form, field).await?;
            }
            "banner" if field.file_name().is_some() => {
                fill_banner_upload(state, user_id, form, field).await?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Reads one of the plain text profile fields into the parsed form.
async fn fill_profile_text_field(
    form: &mut ParsedProfileUpdate,
    name: &str,
    field: axum::extract::multipart::Field<'_>,
) -> AppResult<()> {
    let text = field
        .text()
        .await
        .map_err(|err| AppError::BadRequest(err.to_string()))?;
    match name {
        "csrf" => form.csrf_token = text,
        "display_name" => form.display_name = text,
        "bio" => form.bio = text,
        "location" => form.location = text,
        "website" => form.website = text,
        _ => {}
    }
    Ok(())
}

async fn fill_profile_picture_upload(
    state: &AppState,
    user_id: i64,
    form: &mut ParsedProfileUpdate,
    field: axum::extract::multipart::Field<'_>,
) -> AppResult<()> {
    if !state.settings.accounts.allow_profile_pictures {
        return Err(AppError::Forbidden);
    }
    if field.file_name().is_none_or(|name| name.trim().is_empty()) {
        return Ok(());
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
    Ok(())
}

async fn fill_banner_upload(
    state: &AppState,
    user_id: i64,
    form: &mut ParsedProfileUpdate,
    field: axum::extract::multipart::Field<'_>,
) -> AppResult<()> {
    if !state.settings.accounts.allow_profile_banners {
        return Err(AppError::Forbidden);
    }
    if field.file_name().is_none_or(|name| name.trim().is_empty()) {
        return Ok(());
    }
    form.banner_media_id = Some(
        media::save_banner_upload(
            &state.pool,
            &state.settings,
            &state.paths,
            &state.ffmpeg,
            user_id,
            field,
        )
        .await
        .map_err(|err| {
            tracing::warn!(error = %err, "profile banner upload rejected");
            AppError::BadRequest(err.to_string())
        })?,
    );
    Ok(())
}

/// Deletes profile media that was uploaded for a settings save that did not
/// complete, so rejected saves do not leave orphaned uploads behind.
async fn cleanup_profile_uploads(state: &AppState, form: &ParsedProfileUpdate) {
    for media_id in [form.profile_picture_media_id, form.banner_media_id]
        .into_iter()
        .flatten()
    {
        if let Err(error) = media::delete_media(&state.pool, &state.paths, media_id).await {
            tracing::warn!(
                media_id,
                error = %error,
                "failed to clean up media uploaded for a rejected settings save"
            );
        }
    }
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

async fn edit_preview(
    pool: &SqlitePool,
    actor_id: i64,
    post_id: i64,
    edit_window_seconds: u64,
) -> AppResult<EditPreview> {
    let edit_window_modifier = edit_window_modifier(edit_window_seconds);
    let preview = pool
        .call(move |conn| {
            conn.query_row(
                r#"
                SELECT p.user_id, p.text, p.parent_post_id,
                  CASE
                    WHEN ? IS NULL THEN 0
                    ELSE p.created_at >= datetime('now', ?)
                  END AS within_window
                FROM posts p
                WHERE p.id = ? AND p.is_deleted = 0
                "#,
                params![edit_window_modifier.clone(), edit_window_modifier, post_id],
                |row| {
                    Ok((
                        row.get::<_, Option<i64>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, i64>(3)? != 0,
                    ))
                },
            )
            .optional()
            .map_err(Into::into)
        })
        .await?;
    let Some((owner, text, parent_post_id, within_window)) = preview else {
        return Err(AppError::NotFound);
    };
    if owner != Some(actor_id) {
        return Err(AppError::Forbidden);
    }
    if !within_window {
        return Err(AppError::BadRequest(
            "the edit window for this post has expired".to_owned(),
        ));
    }
    Ok(EditPreview {
        text,
        parent_post_id,
    })
}

fn edit_window_modifier(seconds: u64) -> Option<String> {
    if seconds == 0 {
        return None;
    }
    i64::try_from(seconds)
        .ok()
        .map(|seconds| format!("-{seconds} seconds"))
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
    let relationship = social::profile_relationship(pool, Some(viewer_id), profile_id).await?;
    let (followers, following_count) = social::follow_counts(pool, profile_id).await?;
    let action = if relationship.following {
        format!("/users/{profile_id}/unfollow")
    } else if relationship.requested {
        format!("/users/{profile_id}/follow/cancel")
    } else {
        format!("/users/{profile_id}/follow")
    };
    Ok(FollowActionResponse {
        kind: "follow",
        user_id: profile_id,
        following: relationship.following,
        requested: relationship.requested,
        followers,
        following_count,
        action,
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
                  (
                    SELECT COUNT(*)
                    FROM likes l
                    JOIN users u ON u.id = l.user_id AND u.is_deleted = 0
                    WHERE l.post_id = p.id AND (u.liked_posts_public != 0 OR l.user_id = ?)
                  ),
                  ((SELECT COUNT(*) FROM reposts WHERE post_id = p.id) +
                   (SELECT COUNT(*) FROM posts qp WHERE qp.quote_post_id = p.id AND qp.is_deleted = 0)),
                  (SELECT COUNT(*) FROM posts replies WHERE replies.parent_post_id = p.id AND replies.is_deleted = 0),
                  EXISTS(SELECT 1 FROM likes WHERE user_id = ? AND post_id = p.id),
                  EXISTS(SELECT 1 FROM bookmarks WHERE user_id = ? AND post_id = p.id),
                  EXISTS(SELECT 1 FROM reposts WHERE user_id = ? AND post_id = p.id)
                FROM posts p
                LEFT JOIN users author ON author.id = p.user_id
                WHERE p.id = ? AND p.is_deleted = 0
                  AND (p.user_id IS NULL OR (author.is_deleted = 0 AND author.is_suspended = 0))
                  AND (
                    p.user_id IS NULL
                    OR p.user_id = ?
                    OR (
                      p.user_id NOT IN (SELECT blocked_id FROM blocks WHERE blocker_id = ?)
                      AND NOT EXISTS (SELECT 1 FROM blocks WHERE blocker_id = p.user_id AND blocked_id = ?)
                    )
                  )
                "#,
                params![
                    viewer_id, viewer_id, viewer_id, viewer_id, post_id, viewer_id, viewer_id,
                    viewer_id
                ],
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

fn edit_return_fallback(headers: &HeaderMap, post_id: i64, parent_post_id: Option<i64>) -> String {
    let fallback = anchored_return(headers, post_id, parent_post_id.is_some(), "/home");
    if safe_edit_return_target(&fallback, post_id).is_some() {
        return fallback;
    }
    parent_post_id.map_or_else(
        || format!("/posts/{post_id}"),
        |parent_id| format!("/posts/{parent_id}#reply-{post_id}"),
    )
}

fn safe_edit_return_target(value: &str, post_id: i64) -> Option<String> {
    let target = safe_return_target(value)?;
    let self_path = format!("/posts/{post_id}/edit");
    if path_without_query_or_fragment(&target) == self_path {
        None
    } else {
        Some(target)
    }
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

fn parse_notification_ids(value: &str) -> AppResult<Vec<i64>> {
    if value.len() > 1024 {
        return Err(AppError::BadRequest(
            "notification group is too large".to_owned(),
        ));
    }
    let mut ids = Vec::new();
    for part in value.split(',') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        let id = trimmed
            .parse::<i64>()
            .map_err(|_err| AppError::BadRequest("notification group is invalid".to_owned()))?;
        if id <= 0 {
            return Err(AppError::BadRequest(
                "notification group is invalid".to_owned(),
            ));
        }
        if !ids.contains(&id) {
            ids.push(id);
        }
        if ids.len() > 80 {
            return Err(AppError::BadRequest(
                "notification group is too large".to_owned(),
            ));
        }
    }
    if ids.is_empty() {
        return Err(AppError::BadRequest(
            "notification group is invalid".to_owned(),
        ));
    }
    Ok(ids)
}

fn parse_notification_group_target(value: Option<&str>) -> AppResult<Option<i64>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let target = trimmed
        .parse::<i64>()
        .map_err(|_err| AppError::BadRequest("notification group target is invalid".to_owned()))?;
    if target <= 0 {
        return Err(AppError::BadRequest(
            "notification group target is invalid".to_owned(),
        ));
    }
    Ok(Some(target))
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

fn reject_cross_site_form_post(headers: &HeaderMap) -> AppResult<()> {
    if headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("cross-site"))
    {
        return Err(AppError::Forbidden);
    }
    let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    else {
        return Ok(());
    };
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return Err(AppError::Forbidden);
    };
    let origin_host = origin.parse::<Uri>().ok().and_then(|uri| {
        uri.authority()
            .map(|authority| authority.as_str().to_owned())
    });
    if origin_host.is_none_or(|origin_host| !origin_host.eq_ignore_ascii_case(host)) {
        return Err(AppError::Forbidden);
    }
    Ok(())
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

async fn following(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<AccountListQuery>,
) -> AppResult<Html<String>> {
    let user = require_user(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers).await.unwrap_or_default();
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

async fn notifications(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AppResult<Html<String>> {
    let user = require_user(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers).await.unwrap_or_default();
    let items = social::notification_groups(&state.pool, user.id).await?;
    let unread_count = social::unread_notification_count(&state.pool, user.id).await?;
    let body = render::notifications_page(&items, unread_count, &csrf);
    Ok(Html(
        page_layout(&state, Some(&user), Some(&csrf), "Notifications", &body).await?,
    ))
}

async fn open_notification_group(
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

async fn mention_suggestions(
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
        render::SearchRenderOptions {
            blur_nsfw_media: blur_nsfw_media(&state, user.as_ref()),
            post_edit_window_seconds: state.settings.posts.post_edit_window_seconds,
        },
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

#[derive(Deserialize)]
struct AdminDashboardQuery {
    saved: Option<String>,
}

async fn admin_dashboard(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<AdminDashboardQuery>,
) -> AppResult<Html<String>> {
    let user = require_admin(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers).await.unwrap_or_default();
    let saved_notice = match query.saved.as_deref() {
        Some("announcement") => Some(render::notice("success", "Announcement updated.")),
        Some("maintenance") => Some(render::notice("success", "Maintenance setting updated.")),
        _ => None,
    }
    .unwrap_or_default();
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
    let instance = instance::load(&state.pool).await?;
    let instance_panels = admin_instance_panels(&instance, &csrf);
    let body = format!(
        "{}{}{}{}{}",
        render::page_header(
            "Admin",
            "Manage site health, users, media jobs, settings, and backups."
        ),
        saved_notice,
        r#"<section class="grid admin-nav-grid"><a class="panel admin-card admin-nav-card" data-testid="admin-card" href="/admin/health">Site health</a><a class="panel admin-card admin-nav-card" data-testid="admin-card" href="/admin/users">Users</a><a class="panel admin-card admin-nav-card" data-testid="admin-card" href="/admin/media">Media jobs</a><a class="panel admin-card admin-nav-card" data-testid="admin-card" href="/admin/deep-settings">Deep server settings</a><a class="panel admin-card admin-nav-card" data-testid="admin-card" href="/admin/backups">Backups</a></section>"#,
        instance_panels,
        favicon_panel
    );
    Ok(Html(
        page_layout(&state, Some(&user), Some(&csrf), "Admin", &body).await?,
    ))
}

/// Announcement and maintenance controls, persisted in `instance_settings`.
fn admin_instance_panels(instance: &instance::InstanceSettings, csrf: &str) -> String {
    let announcement_enabled = if instance.announcement_enabled {
        " checked"
    } else {
        ""
    };
    let maintenance_enabled = if instance.maintenance_mode {
        " checked"
    } else {
        ""
    };
    let announcement_status = if instance.announcement_text().is_some() {
        "Currently visible in the top bar."
    } else {
        "Not currently shown."
    };
    let maintenance_status = if instance.maintenance_mode {
        "Maintenance mode is ON. Posting and registration are disabled."
    } else {
        "Maintenance mode is off. The site is fully available."
    };
    format!(
        r#"<section class="panel admin-card" data-testid="admin-announcement-panel"><h2>Announcement</h2><p class="muted">{announcement_status}</p><form method="post" action="/admin/announcement"><input type="hidden" name="csrf" value="{csrf}"><label for="announcement">Announcement text</label><textarea id="announcement" name="announcement" maxlength="280" rows="3">{announcement}</textarea><label class="check-row"><input type="checkbox" name="enabled" value="true"{announcement_enabled}> Show this announcement</label><div class="actions"><button type="submit" name="intent" value="save">Save announcement</button><button type="submit" name="intent" value="clear">Clear announcement</button></div></form></section><section class="panel admin-card" data-testid="admin-maintenance-panel"><h2>Maintenance mode</h2><p class="muted">{maintenance_status}</p><p class="muted">While maintenance mode is on, registration and post creation (posts, replies, quotes, and reposts) are blocked. Administrators can still post, and the site stays readable.</p><form method="post" action="/admin/maintenance"><input type="hidden" name="csrf" value="{csrf}"><label class="check-row"><input type="checkbox" name="enabled" value="true"{maintenance_enabled}> Enable maintenance mode</label><label for="maintenance_message">Message shown to visitors</label><textarea id="maintenance_message" name="message" maxlength="280" rows="3">{maintenance_message}</textarea><div class="actions"><button type="submit" name="intent" value="save">Save maintenance setting</button></div></form></section>"#,
        csrf = html_escape::encode_double_quoted_attribute(csrf),
        announcement = html_escape::encode_text(&instance.announcement),
        maintenance_message = html_escape::encode_text(&instance.maintenance_message),
        announcement_status = html_escape::encode_text(announcement_status),
        maintenance_status = html_escape::encode_text(maintenance_status),
    )
}

#[derive(Deserialize)]
struct AnnouncementForm {
    csrf: String,
    announcement: Option<String>,
    enabled: Option<String>,
    intent: Option<String>,
}

async fn admin_update_announcement(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<AnnouncementForm>,
) -> AppResult<Response> {
    let user = require_admin(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    if form.intent.as_deref() == Some("clear") {
        instance::save_announcement(&state.pool, "", false).await?;
        admin::audit(
            &state.pool,
            user.id,
            "update_announcement",
            "instance_settings",
        )
        .await?;
        return Ok(Redirect::to("/admin?saved=announcement").into_response());
    }
    let text = form.announcement.unwrap_or_default();
    let enabled = form.enabled.as_deref() == Some("true");
    match instance::save_announcement(&state.pool, &text, enabled).await {
        Ok(()) => {
            admin::audit(
                &state.pool,
                user.id,
                "update_announcement",
                "instance_settings",
            )
            .await?;
            Ok(Redirect::to("/admin?saved=announcement").into_response())
        }
        Err(err) => admin_panel_error(&state, &user, &headers, &err.to_string()).await,
    }
}

#[derive(Deserialize)]
struct MaintenanceForm {
    csrf: String,
    message: Option<String>,
    enabled: Option<String>,
}

async fn admin_update_maintenance(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<MaintenanceForm>,
) -> AppResult<Response> {
    let user = require_admin(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    let enabled = form.enabled.as_deref() == Some("true");
    let message = form.message.unwrap_or_default();
    match instance::save_maintenance(&state.pool, enabled, &message).await {
        Ok(()) => {
            admin::audit(
                &state.pool,
                user.id,
                if enabled {
                    "enable_maintenance"
                } else {
                    "disable_maintenance"
                },
                "instance_settings",
            )
            .await?;
            Ok(Redirect::to("/admin?saved=maintenance").into_response())
        }
        Err(err) => admin_panel_error(&state, &user, &headers, &err.to_string()).await,
    }
}

async fn admin_panel_error(
    state: &AppState,
    user: &CurrentUser,
    headers: &HeaderMap,
    message: &str,
) -> AppResult<Response> {
    let csrf = form_csrf(state, headers).await.unwrap_or_default();
    let instance = instance::load(&state.pool).await?;
    let body = format!(
        "{}{}{}",
        render::notice("error", message),
        admin_instance_panels(&instance, &csrf),
        r#"<p><a class="button-link" href="/admin">Back to admin</a></p>"#
    );
    Ok((
        StatusCode::BAD_REQUEST,
        Html(page_layout(state, Some(user), Some(&csrf), "Admin", &body).await?),
    )
        .into_response())
}

async fn admin_require_password_reset(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> AppResult<Response> {
    let user = require_admin(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    identity::require_password_reset(&state.pool, user.id, id)
        .await
        .map_err(|err| AppError::BadRequest(err.to_string()))?;
    Ok(Redirect::to("/admin/users").into_response())
}

async fn admin_revoke_sessions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> AppResult<Response> {
    let user = require_admin(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    identity::revoke_user_sessions(&state.pool, user.id, id)
        .await
        .map_err(|err| AppError::BadRequest(err.to_string()))?;
    Ok(Redirect::to("/admin/users").into_response())
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
    let schema = crate::db::schema_report(&state.pool).await?;
    let schema_version = if schema.is_compatible() {
        schema
            .version()
            .map_or_else(|| "unknown".to_owned(), |version| version.to_string())
    } else {
        "incompatible".to_owned()
    };
    let schema_summary = if schema.is_compatible() {
        schema.summary()
    } else {
        format!("INCOMPATIBLE: {}", schema.summary())
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
        r#"<section class="panel admin-card" data-testid="admin-card"><h1>Site health</h1><dl><dt>DB path</dt><dd>{}</dd><dt>DB schema version</dt><dd>{}</dd><dt>DB diagnostics</dt><dd>{}</dd><dt>Upload path</dt><dd>{}</dd><dt>Media path</dt><dd>{}</dd><dt>Logs path</dt><dd>{}</dd><dt>Backup path</dt><dd>{}</dd><dt>ffmpeg</dt><dd>{}</dd><dt>WebP support</dt><dd>{}</dd><dt>VP9 support</dt><dd>{}</dd><dt>Tor</dt><dd>{}</dd><dt>Tor enabled</dt><dd>{}</dd><dt>Tor service active</dt><dd>{}</dd><dt>Tor bootstrap</dt><dd>{}</dd><dt>Tor error</dt><dd>{}</dd><dt>Onion address</dt><dd>{}</dd><dt>Anonymous mode</dt><dd>{}</dd><dt>Registration</dt><dd>{}</dd></dl><h2>Recent media jobs</h2>{}</section>"#,
        html_escape::encode_text(&state.paths.database_path.display().to_string()),
        html_escape::encode_text(&schema_version),
        html_escape::encode_text(&schema_summary),
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
        return render::compact_empty_state_with_class(
            "admin-users-empty",
            message,
            "Try a different username, handle, display name, or post keyword.",
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
    let reset_password = small_form(
        &format!("/admin/users/{}/require-password-reset", row.id),
        csrf,
        "Require password reset",
        "Require this account to choose a new password before using the site again",
    );
    let revoke_sessions = small_form(
        &format!("/admin/users/{}/revoke-sessions", row.id),
        csrf,
        "Log out sessions",
        "Revoke every active session for this account",
    );
    let actions = format!("{action}{reset_password}{revoke_sessions}");
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
        actions
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
    media::validate_post_media_deletion(&state.pool, &state.paths, id).await?;
    social::delete_post(&state.pool, user.id, id, true).await?;
    if let Err(error) = media::delete_post_media(&state.pool, &state.paths, id).await {
        tracing::warn!(post_id = id, error = %error, "post deleted but media cleanup failed");
    }
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
    let _configuration_guard = state.configuration_write_lock.lock().await;
    let current = load_deep_settings(&state)?;
    let values = admin::DeepSettingsValues::from_settings(&current);
    let notice = if query.saved.is_some() {
        Some((
            "success",
            "Settings saved successfully. Restart required for startup settings; blur applies immediately and backup policy takes effect on the next check.",
        ))
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
    let _configuration_guard = state.configuration_write_lock.lock().await;
    let current = load_deep_settings(&state)?;
    if form.intent.as_deref() == Some("discard") {
        let values = admin::DeepSettingsValues::from_settings(&current);
        let body = render_deep_settings_form(&csrf, &values, Some(("info", "Changes discarded.")));
        return deep_settings_html(&state, &user, &csrf, &body).await;
    }

    if form.revision.as_ref().is_none_or(|revision| {
        configuration_revision(&state).map_or(true, |current| &current != revision)
    }) {
        let values = admin::DeepSettingsValues::from_settings(&current);
        let body = render_deep_settings_submission(
            &csrf,
            &values,
            Some((
                "error",
                "Settings changed since this form was opened. Reload the page and review the latest values before saving.",
            )),
            None,
        );
        return deep_settings_html(&state, &user, &csrf, &body).await;
    }
    if !matches!(form.intent.as_deref(), Some("preview" | "confirm")) {
        return Err(AppError::BadRequest("Unknown settings action".to_owned()));
    }
    let values = match admin::parse_deep_settings_form(&form, &current) {
        Ok(values) => values,
        Err(err) => {
            let fallback = admin::DeepSettingsValues::from_settings(&current);
            let body = render_deep_settings_submission(
                &csrf,
                &fallback,
                Some(("error", &err.to_string())),
                Some(&form),
            );
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
        state.nsfw_blur_default.store(
            updated.media.nsfw_blur_enabled,
            std::sync::atomic::Ordering::Relaxed,
        );
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
            Some((
                "success",
                "Settings saved successfully. Restart required for startup settings; blur applies immediately and backup policy takes effect on the next check.",
            )),
        );
        return deep_settings_html(&state, &user, &csrf, &body).await;
    }

    let body = render_deep_settings_confirmation(&csrf, &values, &changes, None);
    deep_settings_html(&state, &user, &csrf, &body).await
}

fn configuration_revision(state: &AppState) -> AppResult<String> {
    use sha2::{Digest as _, Sha256};
    let raw = std::fs::read(&state.paths.settings_path)?;
    Ok(Sha256::digest(&raw)
        .iter()
        .fold(String::new(), |mut output, byte| {
            let _ = write!(output, "{byte:02x}");
            output
        }))
}

async fn deep_settings_html(
    state: &AppState,
    user: &CurrentUser,
    csrf: &str,
    body: &str,
) -> AppResult<Html<String>> {
    let revision = configuration_revision(state)?;
    let body = body.replace(r#"<input type="hidden" name="csrf""#,
        &format!(r#"<input type="hidden" name="revision" value="{revision}"><input type="hidden" name="csrf""#));
    let settings = load_deep_settings(state)?;
    let configured = admin::DeepSettingsValues::from_settings(&settings);
    let running = admin::DeepSettingsValues::from_settings(&state.settings);
    let mut body = body;
    for field in admin::DeepSettingsField::ALL {
        if !field.applies_live() && configured.form_value(field) != running.form_value(field) {
            let marker = format!(
                r#"id="deep-{}-help" class="muted field-help">"#,
                field.form_name()
            );
            let effective = format!(
                "{marker}<strong>Restart pending. Running value: {}.</strong> ",
                html_escape::encode_text(&running.form_value(field))
            );
            body = body.replace(&marker, &effective);
        }
    }
    for (key, value) in [
        ("media.ffmpeg_path", settings.media.ffmpeg_path.as_str()),
        ("tor.data_dir", settings.tor.data_dir.as_str()),
        ("backup.backup_dir", settings.backup.backup_dir.as_str()),
        (
            "tor.include_tor_keys_in_backups_by_default",
            if settings.tor.include_tor_keys_in_backups_by_default {
                "true"
            } else {
                "false"
            },
        ),
    ] {
        let marker = format!("<strong>{key}</strong>:");
        body = body.replace(
            &marker,
            &format!(
                "{marker} Configured value: <code>{}</code>. ",
                html_escape::encode_text(value)
            ),
        );
    }
    let source = format!(
        r#"<section class="panel configuration-source"><p>Source: <strong>{}</strong>. No environment or CLI value overrides apply to these settings. CLI options select the configuration file and data directory.</p><p>Stored values are shown below. Startup settings keep their running values until restart.</p></section>"#,
        html_escape::encode_text(&state.paths.settings_path.display().to_string())
    );
    Ok(Html(
        page_layout(
            state,
            Some(user),
            Some(csrf),
            "Deep server settings",
            &body.replace(
                r#"<nav class="settings-category-nav""#,
                &format!(r#"{source}<nav class="settings-category-nav""#),
            ),
        )
        .await?,
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
    render_deep_settings_submission(csrf, values, notice, None)
}

fn render_deep_settings_submission(
    csrf: &str,
    values: &admin::DeepSettingsValues,
    notice: Option<(&str, &str)>,
    submitted: Option<&admin::DeepSettingsForm>,
) -> String {
    let notice_html = notice.map_or_else(String::new, |(kind, message)| {
        format!(
            r#"<div id="settings-error">{}</div>"#,
            render::notice(kind, message)
        )
    });
    let defaults = admin::DeepSettingsValues::from_settings(&Settings::default());
    let mut fields = String::new();
    let mut navigation = String::new();
    let mut active_section = "";
    for field in admin::DeepSettingsField::ALL {
        if field.section() != active_section {
            if !active_section.is_empty() {
                fields.push_str("</fieldset>");
            }
            active_section = field.section();
            let _ = write!(
                fields,
                r#"<fieldset id="settings-{}" class="deep-settings-group"><legend>{}</legend>"#,
                field.toml_section(),
                html_escape::encode_text(active_section)
            );
            let _ = write!(
                navigation,
                r##"<a href="#settings-{}">{}</a>"##,
                field.toml_section(),
                html_escape::encode_text(active_section)
            );
        }
        fields.push_str(&render_deep_settings_field(
            field,
            values,
            submitted,
            notice
                .filter(|(kind, _)| *kind == "error")
                .map(|(_, message)| message),
            &defaults,
        ));
    }
    fields.push_str("</fieldset>");
    let exclusions = crate::config::admin_fields::NON_WEB_SETTINGS.iter().fold(
        String::new(),
        |mut items, (key, reason)| {
            let _ = write!(
                items,
                "<li><strong>{}</strong>: {}</li>",
                html_escape::encode_text(key),
                html_escape::encode_text(reason)
            );
            items
        },
    );
    format!(
        r#"{notice_html}<section class="panel admin-card deep-settings-panel" data-testid="admin-card"><div class="settings-editor-bar"><div><h1>Deep server settings</h1><p class="muted">View configured values from settings.toml, then review changes before saving. Each setting shows when it takes effect.</p></div><button class="primary" type="submit" form="deep-settings-form">Save</button></div><nav class="settings-category-nav" aria-label="Settings categories">{navigation}</nav><div class="settings-search" hidden><label for="settings-search">Find a setting</label><input id="settings-search" type="search" placeholder="Registration, upload size, rate limit…"><p id="settings-search-status" role="status"></p></div><form id="deep-settings-form" method="post" action="/admin/deep-settings" class="deep-settings-form"><input type="hidden" name="csrf" value="{}">{fields}<input type="hidden" name="intent" value="preview"><div class="settings-form-actions"><button class="primary" type="submit">Review changes</button></div></form><details class="deployment-settings"><summary>Deployment-managed settings</summary><p>These four settings remain in the configuration file for the following reasons.</p><ul>{exclusions}</ul><p>Database credentials and master keys are not TOML options. Onion private keys and password hashes are never returned by this editor.</p></details></section>"#,
        html_escape::encode_double_quoted_attribute(csrf),
    )
}

fn render_deep_settings_field(
    field: admin::DeepSettingsField,
    values: &admin::DeepSettingsValues,
    submitted: Option<&admin::DeepSettingsForm>,
    error: Option<&str>,
    defaults: &admin::DeepSettingsValues,
) -> String {
    let name = field.form_name();
    let id = format!("deep-{name}");
    let value = submitted.map_or_else(
        || values.form_value(field),
        |form| form.submitted_value(field).to_owned(),
    );
    let escaped = html_escape::encode_double_quoted_attribute(&value);
    let described = format!(
        r#"aria-describedby="{id}-help{}""#,
        if submitted.is_some() {
            " settings-error"
        } else {
            ""
        }
    );
    let described = if error.is_some_and(|message| message.starts_with(field.label())) {
        format!("{described} aria-invalid=\"true\"")
    } else {
        described
    };
    let control = match field.input_kind() {
        admin::DeepSettingsInputKind::Text | admin::DeepSettingsInputKind::Url => {
            let kind = if field.input_kind() == admin::DeepSettingsInputKind::Url {
                "url"
            } else {
                "text"
            };
            format!(
                r#"<input id="{id}" name="{name}" type="{kind}" {described} value="{escaped}">"#
            )
        }
        admin::DeepSettingsInputKind::Number | admin::DeepSettingsInputKind::Size => {
            let step = if field.input_kind() == admin::DeepSettingsInputKind::Size {
                "any"
            } else {
                "1"
            };
            // Signed rate-limit values <= 0 deliberately block the action.
            let min = if field.toml_section() == "moderation" {
                ""
            } else {
                "min=\"0\""
            };
            format!(
                r#"<input id="{id}" name="{name}" type="number" {described} inputmode="decimal" {min} step="{step}" value="{escaped}">"#
            )
        }
        admin::DeepSettingsInputKind::Boolean => {
            let checked = if value == "true" { " checked" } else { "" };
            format!(
                r#"<input id="{id}" name="{name}" type="checkbox" {described} value="true"{checked}>"#
            )
        }
        admin::DeepSettingsInputKind::List => format!(
            r#"<textarea id="{id}" name="{name}" {described} rows="4">{}</textarea>"#,
            html_escape::encode_text(&value)
        ),
        admin::DeepSettingsInputKind::Encoding => {
            let options = crate::config::VideoEncodingSpeed::ALL.into_iter().fold(
                String::new(),
                |mut options, speed| {
                    let option = speed.as_str();
                    let selected = if value == option { " selected" } else { "" };
                    let _ = write!(
                        options,
                        r#"<option value="{option}"{selected}>{option}</option>"#
                    );
                    options
                },
            );
            format!(r#"<select id="{id}" name="{name}" {described}>{options}</select>"#)
        }
    };
    let helper = field.helper().unwrap_or_default();
    let default = defaults.form_value(field);
    let timing = if field.applies_live() {
        if field.toml_section() == "backup" {
            "Next backup check"
        } else {
            "Applies immediately"
        }
    } else {
        "Restart required"
    };
    format!(
        r#"<div class="deep-settings-field"><label for="{id}">{}</label>{control}<p id="{id}-help" class="muted field-help">{} <span class="setting-default">Default: {}.</span> <span class="setting-timing">{timing}</span></p></div>"#,
        html_escape::encode_text(field.label()),
        html_escape::encode_text(helper),
        html_escape::encode_text(&default)
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
        r#"{notice_html}<section class="panel admin-card deep-settings-confirm" data-testid="admin-card"><h1>These settings are about to be changed</h1><p class="muted">Review the changed values before writing settings.toml. Restart required for startup settings. Blur applies immediately; backup policy takes effect on the next check.</p><ul class="settings-item-list">{rows}</ul><div class="actions"><form method="post" action="/admin/deep-settings"><input type="hidden" name="csrf" value="{}"><input type="hidden" name="intent" value="confirm">{hidden}<button class="primary" type="submit">Confirm/Save</button></form><form method="post" action="/admin/deep-settings"><input type="hidden" name="csrf" value="{}"><input type="hidden" name="intent" value="discard">{hidden}<button type="submit">Discard Changes</button></form></div></section>"#,
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
    backups_page(&state, &user, &csrf, None).await
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
    let csrf = form_csrf(&state, &headers).await.unwrap_or_default();
    let settings = load_deep_settings(&state)?;
    if !settings.backup.enabled {
        return backups_page(
            &state,
            &user,
            &csrf,
            Some(("error", "Backups are disabled in settings.toml.")),
        )
        .await;
    }
    let include_tor = form.include_tor_keys.is_some();
    let paths = state.paths.clone();
    let created = tokio::task::spawn_blocking(move || backup::create_backup(&paths, include_tor))
        .await
        .map_err(|err| AppError::BadRequest(format!("backup task failed: {err}")))?;
    let archive = match created {
        Ok(archive) => archive,
        Err(err) => {
            tracing::warn!(error = %err, "manual backup failed");
            let message = public_backup_error("Backup failed", &err);
            return backups_page(&state, &user, &csrf, Some(("error", &message))).await;
        }
    };
    admin::audit(
        &state.pool,
        user.id,
        "create_backup",
        archive
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("backup"),
    )
    .await?;
    let message = format!(
        "Backup created: {}",
        archive
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("archive")
    );
    backups_page(&state, &user, &csrf, Some(("success", &message))).await
}

async fn admin_download_backup(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(filename): Path<String>,
) -> AppResult<Response> {
    require_admin(&state, &headers).await?;
    let path = backup::backup_path_for_download(&state.paths, &filename)
        .map_err(|_err| AppError::NotFound)?;
    let metadata = tokio::fs::metadata(&path).await?;
    let file = tokio::fs::File::open(&path).await?;
    let stream = futures_util::stream::unfold(file, |mut file| async move {
        let mut buffer = vec![0u8; 64 * 1024];
        match file.read(&mut buffer).await {
            Ok(0) => None,
            Ok(read) => {
                buffer.truncate(read);
                Some((Ok::<Bytes, io::Error>(Bytes::from(buffer)), file))
            }
            Err(error) => Some((Err(error), file)),
        }
    });
    let disposition = format!("attachment; filename=\"{}\"", filename.replace('"', ""));
    let mut response = Body::from_stream(stream).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-tar"),
    );
    response.headers_mut().insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&metadata.len().to_string())
            .map_err(|err| AppError::BadRequest(err.to_string()))?,
    );
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&disposition).map_err(|err| AppError::BadRequest(err.to_string()))?,
    );
    Ok(response)
}

async fn admin_backup_settings_update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<admin::BackupSettingsForm>,
) -> AppResult<Html<String>> {
    let user = require_admin(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    let csrf = form_csrf(&state, &headers).await.unwrap_or_default();
    let _configuration_guard = state.configuration_write_lock.lock().await;
    let current = load_deep_settings(&state)?;
    let values = match admin::parse_backup_settings_form(&form, &current) {
        Ok(values) => values,
        Err(err) => {
            let message = err.to_string();
            return backups_page_submission(
                &state,
                &user,
                &csrf,
                Some(("error", &message)),
                Some(&form),
            )
            .await;
        }
    };
    let updated = values.apply_to(&current);
    if let Err(err) = admin::write_backup_settings(&state.paths.settings_path, &updated) {
        tracing::error!(error = %err, "failed to save backup settings");
        return backups_page(
            &state,
            &user,
            &csrf,
            Some((
                "error",
                "Backup settings could not be saved. Check the server logs.",
            )),
        )
        .await;
    }
    admin::audit(
        &state.pool,
        user.id,
        "update_backup_settings",
        "settings.toml",
    )
    .await?;
    backups_page(
        &state,
        &user,
        &csrf,
        Some((
            "success",
            "Backup settings saved. The scheduler reads settings.toml on its next check.",
        )),
    )
    .await
}

async fn admin_restore_backup(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    multipart: Multipart,
) -> AppResult<Html<String>> {
    let user = require_admin(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers).await.unwrap_or_default();
    // Staged restore uploads live under the runtime temp directory; keep
    // cleanup from removing them while the restore is in flight.
    let _operation = crate::runtime::begin_temp_operation();
    let upload = match parse_restore_upload(&state, &headers, multipart).await {
        Ok(upload) => upload,
        Err(err) => {
            return backups_page(&state, &user, &csrf, Some(("error", &err.to_string()))).await;
        }
    };
    if upload.confirmation != "RESTORE" {
        let _remove_result = tokio::fs::remove_file(&upload.archive_path).await;
        return backups_page(
            &state,
            &user,
            &csrf,
            Some((
                "error",
                "Type RESTORE to confirm restoring from an uploaded backup.",
            )),
        )
        .await;
    }
    admin::audit(
        &state.pool,
        user.id,
        "restore_backup_upload",
        &upload.filename,
    )
    .await?;
    let paths = state.paths.clone();
    let archive_path = upload.archive_path.clone();
    let include_tor_keys = upload.include_tor_keys;
    let restored = tokio::task::spawn_blocking(move || {
        backup::restore_backup(&paths, &archive_path, include_tor_keys)
    })
    .await
    .map_err(|err| AppError::BadRequest(format!("restore task failed: {err}")))?;
    let _remove_result = tokio::fs::remove_file(&upload.archive_path).await;
    match restored {
        Ok(report) => {
            state
                .restart_required
                .store(true, std::sync::atomic::Ordering::Relaxed);
            let safety = report
                .pre_restore_backup
                .as_ref()
                .and_then(|path| path.file_name())
                .and_then(|name| name.to_str())
                .unwrap_or("pre-restore backup");
            let message = format!(
                "Restore completed. Restart RustPost so the running process reopens the restored database. Safety backup: {safety}."
            );
            backups_page(&state, &user, &csrf, Some(("success", &message))).await
        }
        Err(err) => {
            tracing::warn!(error = %err, "restore upload failed");
            let message = public_backup_error("Restore failed", &err);
            backups_page(&state, &user, &csrf, Some(("error", &message))).await
        }
    }
}

struct ParsedRestoreUpload {
    archive_path: PathBuf,
    filename: String,
    include_tor_keys: bool,
    confirmation: String,
}

async fn parse_restore_upload(
    state: &AppState,
    headers: &HeaderMap,
    mut multipart: Multipart,
) -> AppResult<ParsedRestoreUpload> {
    let mut csrf_validated = false;
    let mut include_tor_keys = false;
    let mut confirmation = String::new();
    let mut archive_path = None;
    let mut filename = "uploaded-backup.tar".to_owned();
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
            "include_tor_keys" => {
                include_tor_keys = field.text().await.is_ok_and(|value| value == "true");
            }
            "restore_confirm" => {
                confirmation = field
                    .text()
                    .await
                    .map_err(|err| AppError::BadRequest(err.to_string()))?;
            }
            "backup" if field.file_name().is_some() => {
                if !csrf_validated {
                    return Err(AppError::Forbidden);
                }
                field
                    .file_name()
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or("uploaded-backup.tar")
                    .clone_into(&mut filename);
                if !std::path::Path::new(&filename)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("tar"))
                {
                    return Err(AppError::BadRequest(
                        "Choose a .tar backup archive.".to_owned(),
                    ));
                }
                let path = state
                    .paths
                    .tmp_dir
                    .join(format!("restore-upload-{}.tar", Uuid::new_v4().simple()));
                write_multipart_field_to_file(field, &path, None).await?;
                archive_path = Some(path);
            }
            _ => {}
        }
    }
    if !csrf_validated {
        return Err(AppError::Forbidden);
    }
    Ok(ParsedRestoreUpload {
        archive_path: archive_path.ok_or_else(|| {
            AppError::BadRequest("Choose a backup archive to restore.".to_owned())
        })?,
        filename,
        include_tor_keys,
        confirmation,
    })
}

/// Streams one multipart field to `path` and returns the bytes written.
///
/// When `max_bytes` is set the limit is enforced for every chunk as it
/// arrives, so an oversized upload is rejected before the whole body is
/// buffered or stored. Uploads must never be buffered in memory.
async fn write_multipart_field_to_file(
    mut field: axum::extract::multipart::Field<'_>,
    path: &std::path::Path,
    max_bytes: Option<u64>,
) -> AppResult<u64> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut output = tokio::fs::File::create(path).await?;
    let mut written = 0u64;
    while let Some(chunk) = field
        .chunk()
        .await
        .map_err(|err| AppError::BadRequest(err.to_string()))?
    {
        written = written.saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
        if let Some(limit) = max_bytes
            && written > limit
        {
            return Err(upload_limit_error(limit));
        }
        output.write_all(&chunk).await?;
    }
    output.flush().await?;
    Ok(written)
}

fn upload_limit_error(limit: u64) -> AppError {
    AppError::PayloadTooLarge(format!(
        "the uploaded archive is larger than the configured {} MiB limit",
        limit / (1024 * 1024)
    ))
}

async fn backups_page(
    state: &AppState,
    user: &CurrentUser,
    csrf: &str,
    notice: Option<(&str, &str)>,
) -> AppResult<Html<String>> {
    backups_page_submission(state, user, csrf, notice, None).await
}

async fn backups_page_submission(
    state: &AppState,
    user: &CurrentUser,
    csrf: &str,
    notice: Option<(&str, &str)>,
    submitted: Option<&admin::BackupSettingsForm>,
) -> AppResult<Html<String>> {
    let settings = load_deep_settings(state)?;
    let archives = backup::list_backups(&state.paths)?;
    let notice_html =
        notice.map_or_else(String::new, |(kind, message)| render::notice(kind, message));
    let body = format!(
        "{}{}{}{}{}{}",
        notice_html,
        render::page_header(
            "Backups",
            "Create, download, schedule, and restore full-site runtime backups."
        ),
        render_manual_backup_panel(csrf, &settings),
        render_automatic_backup_panel(csrf, &settings, submitted),
        render_backup_history(&archives, backup::operation_in_progress(&state.paths)),
        render_restore_panel(csrf),
    );
    Ok(Html(
        page_layout(state, Some(user), Some(csrf), "Backups", &body).await?,
    ))
}

fn render_manual_backup_panel(csrf: &str, settings: &Settings) -> String {
    let disabled = if settings.backup.enabled {
        ""
    } else {
        " disabled"
    };
    format!(
        r#"<section class="panel admin-card" data-testid="admin-card"><h2>Manual backup</h2><p class="muted">Archives are written under the private backup directory and are available only to admins.</p><form method="post" action="/admin/backups/create"><input type="hidden" name="csrf" value="{}"><label><input type="checkbox" name="include_tor_keys" value="true"> Include Tor onion-service private keys</label><p class="muted">Leave Tor keys unchecked unless the archive storage is encrypted and access-controlled.</p><button class="primary" type="submit"{disabled}>Create backup</button></form></section>"#,
        html_escape::encode_double_quoted_attribute(csrf),
    )
}

fn render_automatic_backup_panel(
    csrf: &str,
    settings: &Settings,
    submitted: Option<&admin::BackupSettingsForm>,
) -> String {
    let shared = admin::DeepSettingsForm::from_settings(settings);
    let fallback = admin::BackupSettingsForm {
        csrf: csrf.to_owned(),
        enabled: shared.backup_enabled,
        automatic_enabled: shared.automatic_enabled,
        automatic_interval_minutes: shared.automatic_interval_minutes,
        retention_keep_last: shared.retention_keep_last,
        retention_max_age_days: shared.retention_max_age_days,
        automatic_include_tor_keys: shared.automatic_include_tor_keys,
    };
    let values = submitted.unwrap_or(&fallback);
    format!(
        r#"<section class="panel admin-card" data-testid="admin-card"><h2>Automatic backups</h2><form method="post" action="/admin/backups/settings" class="deep-settings-form"><input type="hidden" name="csrf" value="{}"><div class="deep-settings-field"><label for="backup-enabled">Backups enabled</label>{}</div><div class="deep-settings-field"><label for="backup-auto-enabled">Scheduled backups</label>{}</div><div class="deep-settings-field"><label for="backup-interval">Interval minutes</label><input id="backup-interval" name="automatic_interval_minutes" type="number" inputmode="numeric" min="0" step="1" value="{}"></div><div class="deep-settings-field"><label for="backup-keep">Keep newest automatic backups</label><input id="backup-keep" name="retention_keep_last" type="number" inputmode="numeric" min="0" step="1" value="{}"></div><div class="deep-settings-field"><label for="backup-age">Delete automatic backups older than days</label><input id="backup-age" name="retention_max_age_days" type="number" inputmode="numeric" min="0" step="1" value="{}"><p class="muted field-help">Set 0 to disable age cleanup. Manual and pre-restore backups are not pruned.</p></div><div class="deep-settings-field"><label for="backup-auto-tor">Automatic backups include Tor keys</label>{}</div><button class="primary" type="submit">Save backup settings</button></form></section>"#,
        html_escape::encode_double_quoted_attribute(csrf),
        bool_checkbox("backup-enabled", "enabled", values.enabled == "true"),
        bool_checkbox(
            "backup-auto-enabled",
            "automatic_enabled",
            values.automatic_enabled == "true"
        ),
        html_escape::encode_double_quoted_attribute(&values.automatic_interval_minutes),
        html_escape::encode_double_quoted_attribute(&values.retention_keep_last),
        html_escape::encode_double_quoted_attribute(&values.retention_max_age_days),
        bool_checkbox(
            "backup-auto-tor",
            "automatic_include_tor_keys",
            values.automatic_include_tor_keys == "true"
        ),
    )
}

fn bool_checkbox(id: &str, name: &str, value: bool) -> String {
    let checked = if value { " checked" } else { "" };
    format!(
        r#"<input id="{}" name="{}" type="checkbox" value="true"{checked}>"#,
        html_escape::encode_double_quoted_attribute(id),
        html_escape::encode_double_quoted_attribute(name)
    )
}

fn render_backup_history(
    archives: &[backup::BackupArchiveInfo],
    operation_running: bool,
) -> String {
    let status = if archives.is_empty() {
        r#"<p class="muted">No backups have been created yet.</p>"#.to_owned()
    } else {
        let rows = archives
            .iter()
            .take(20)
            .map(|archive| {
                let created = archive.created_at.as_deref().unwrap_or("unknown");
                let tor_keys = archive
                    .tor_keys_included
                    .map_or("unknown", |included| if included { "included" } else { "excluded" });
                let kind = if archive.automatic { "automatic" } else { "manual" };
                let manifest = if archive.manifest_valid { "valid" } else { "unreadable" };
                format!(
                    r#"<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td><a class="button-link" href="/admin/backups/download/{}">Download</a></td></tr>"#,
                    html_escape::encode_text(&archive.filename),
                    html_escape::encode_text(kind),
                    html_escape::encode_text(created),
                    format_bytes(archive.size),
                    html_escape::encode_text(tor_keys),
                    html_escape::encode_text(manifest),
                    html_escape::encode_double_quoted_attribute(&archive.filename),
                )
            })
            .collect::<Vec<_>>()
            .join("");
        format!(
            r#"<table><thead><tr><th>Archive</th><th>Kind</th><th>Created</th><th>Size</th><th>Tor keys</th><th>Manifest</th><th>Action</th></tr></thead><tbody>{rows}</tbody></table>"#
        )
    };
    let running = if operation_running {
        render::notice(
            "info",
            "A backup or restore operation is currently running.",
        )
    } else {
        String::new()
    };
    format!(
        r#"<section class="panel admin-card" data-testid="admin-card"><h2>Recent backups</h2>{running}{status}</section>"#
    )
}

fn render_restore_panel(csrf: &str) -> String {
    format!(
        r#"<section class="panel admin-card danger-zone" data-testid="admin-card"><h2>Danger area</h2><h3>Restore from upload</h3><p class="muted">Restore validates the manifest, hashes, paths, settings, and SQLite integrity in a staging directory before replacing live runtime files. A pre-restore safety backup is created first.</p><form method="post" action="/admin/backups/restore" enctype="multipart/form-data"><input type="hidden" name="csrf" value="{}"><label for="backup-upload">Backup archive</label><input id="backup-upload" name="backup" type="file" accept=".tar" required><label><input type="checkbox" name="include_tor_keys" value="true"> Restore Tor onion-service private keys if present</label><label for="restore-confirm">Type RESTORE to confirm</label><input id="restore-confirm" name="restore_confirm" autocomplete="off" required><button class="danger" type="submit">Restore backup</button></form></section>"#,
        html_escape::encode_double_quoted_attribute(csrf),
    )
}

fn public_backup_error(prefix: &str, error: &anyhow::Error) -> String {
    let detail = error.to_string();
    if detail.contains('/') || detail.contains('\\') {
        format!("{prefix}. Check the server logs for details.")
    } else {
        format!("{prefix}: {detail}")
    }
}

fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    if bytes >= GIB {
        return format_scaled_bytes(bytes, GIB, "GiB");
    }
    if bytes >= MIB {
        return format_scaled_bytes(bytes, MIB, "MiB");
    }
    if bytes >= KIB {
        return format_scaled_bytes(bytes, KIB, "KiB");
    }
    format!("{bytes} B")
}

fn format_scaled_bytes(bytes: u64, unit: u64, suffix: &str) -> String {
    let tenths = u128::from(bytes) * 10 / u128::from(unit);
    format!("{}.{:01} {suffix}", tenths / 10, tenths % 10)
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
    let user = require_active_user(state, headers).await?;
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

/// Reads the session's current CSRF token and rotates it for the next form.
///
/// Two concurrent page loads for one session can both read the same stored
/// hash; only one optimistic update wins. Retrying lets the loser read the new
/// hash and rotate again instead of rendering a form with no token.
async fn form_csrf(state: &AppState, headers: &HeaderMap) -> Option<String> {
    const ROTATION_ATTEMPTS: usize = 3;
    let token = auth::session_cookie(headers)?;
    let token_hash = auth::hash_token(&token);
    for _attempt in 0..ROTATION_ATTEMPTS {
        let (stored_hash, previous_hashes): (String, Option<String>) = state
            .pool
            .call({
                let token_hash = token_hash.clone();
                move |conn| {
                    conn.query_row(
                        "SELECT csrf_token_hash, previous_csrf_token_hash FROM sessions WHERE token_hash = ? AND revoked_at IS NULL AND datetime(expires_at) > CURRENT_TIMESTAMP",
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
        let session_token_hash = token_hash.clone();
        let updated = state
            .pool
            .call(move |conn| {
                let changed = conn.execute(
                    "UPDATE sessions SET csrf_token_hash = ?, previous_csrf_token_hash = ? WHERE token_hash = ? AND csrf_token_hash = ?",
                    params![new_hash, previous_hashes, session_token_hash, stored_hash],
                )?;
                Ok(changed == 1)
            })
            .await
            .ok()?;
        if updated {
            return Some(plain);
        }
    }
    None
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
    fn upload_body_limit_covers_configured_media_mix() {
        let settings = Settings::default();
        let remaining_media =
            u64::try_from(settings.posts.max_media_per_post.saturating_sub(1)).unwrap_or(u64::MAX);
        let expected_payload = settings.media.max_video_size.saturating_add(
            settings
                .media
                .max_image_size
                .saturating_mul(remaining_media),
        );

        assert!(u64::try_from(upload_body_limit(&settings)).unwrap_or(0) > expected_payload);
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
    async fn cross_site_login_and_registration_posts_are_rejected() {
        let server = spawn_test_server_with_admin().await;

        let login = request(
            &server.base_url,
            "POST",
            "/login",
            &[
                ("content-type", "application/x-www-form-urlencoded"),
                ("origin", "https://attacker.example"),
                ("sec-fetch-site", "cross-site"),
            ],
            b"username=siteowner&password=very%20secure%20password".to_vec(),
        )
        .await;
        let registration = request(
            &server.base_url,
            "POST",
            "/register",
            &[
                ("content-type", "application/x-www-form-urlencoded"),
                ("origin", "https://attacker.example"),
                ("sec-fetch-site", "cross-site"),
            ],
            b"username=forced&password=very%20secure%20password&confirm_password=very%20secure%20password".to_vec(),
        )
        .await;

        assert_eq!(login.status, 403);
        assert_eq!(registration.status, 403);
        let forced_accounts: i64 = server
            .pool
            .call(|conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM users WHERE normalized_username = 'forced'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await
            .expect("forced account count");
        assert_eq!(forced_accounts, 0);
    }

    #[tokio::test]
    async fn suspended_admin_session_cannot_access_admin_routes() {
        let server = spawn_test_server_with_admin().await;
        let admin_cookie = admin_session_cookie(&server).await;
        server
            .pool
            .call(|conn| {
                conn.execute(
                    "UPDATE users SET is_suspended = 1 WHERE normalized_username = 'siteowner'",
                    [],
                )?;
                Ok(())
            })
            .await
            .expect("suspend admin");

        let response = get_with_cookie(&server, "/admin/users", &admin_cookie).await;

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
    async fn public_layout_does_not_render_configured_onion_fallback() {
        let mut settings = Settings::default();
        settings.tor.enabled = true;
        settings.tor.display_onion_address =
            "abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx.onion".to_owned();
        let server = spawn_test_server_with_settings(settings).await;

        let response = request(&server.base_url, "GET", "/", &[], Vec::new()).await;

        assert_eq!(response.status, 200);
        assert!(!response.body.contains("tor-header-indicator"));
        assert!(!response.body.contains(".onion"));
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
    async fn login_allows_existing_password_after_policy_increase() {
        let server = spawn_test_server().await;
        let mut permissive_settings = Settings::default();
        permissive_settings.accounts.min_password_length = 0;
        auth::register_user(&server.pool, &permissive_settings, "shorty", "short", false)
            .await
            .expect("register short password user");

        let login = request(
            &server.base_url,
            "POST",
            "/login",
            &[("content-type", "application/x-www-form-urlencoded")],
            b"username=shorty&password=short".to_vec(),
        )
        .await;

        assert_eq!(login.status, 303);
        assert!(header_value(&login, "set-cookie").is_some());
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
    async fn registration_redirects_to_onboarding_with_session() {
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
        assert_eq!(location(&registered), "/onboarding");
        let cookie = session_cookie(&registered);
        let onboarding = get_with_cookie(&server, "/onboarding", &cookie).await;
        assert_eq!(onboarding.status, 200);
        assert!(onboarding.body.contains("<h1>Set up your account</h1>"));
    }

    #[tokio::test]
    async fn onboarding_saves_profile_avatar_follows_and_marks_complete() {
        let server = spawn_test_server().await;
        let _bob_cookie = register_test_user(&server, "bob").await;
        let alice_cookie = register_test_user(&server, "alice").await;
        let onboarding = get_with_cookie(&server, "/onboarding", &alice_cookie).await;
        assert_eq!(onboarding.status, 200);
        assert!(onboarding.body.contains("@bob"));
        let csrf = csrf_token(&onboarding.body);

        let body = multipart_body_with_file(
            "onboarding-boundary",
            &[
                ("csrf", csrf.as_str()),
                ("intent", "save"),
                ("display_name", "Alice Display"),
                ("bio", "building a local RustPost"),
                ("follow_user_id", "1"),
            ],
            "profile_picture",
            "avatar.png",
            "image/png",
            &tiny_png_bytes(),
        );
        let saved = request(
            &server.base_url,
            "POST",
            "/onboarding",
            &[
                ("cookie", &alice_cookie),
                (
                    "content-type",
                    "multipart/form-data; boundary=onboarding-boundary",
                ),
            ],
            body,
        )
        .await;

        assert_eq!(saved.status, 303);
        assert_eq!(location(&saved), "/home");
        let (display_name, bio, completed, has_avatar, follows_bob): (
            String,
            String,
            i64,
            i64,
            i64,
        ) = server
            .pool
            .call(|conn| {
                let profile = conn.query_row(
                    r#"
                    SELECT display_name, bio, onboarding_completed_at IS NOT NULL,
                      profile_picture_media_id IS NOT NULL
                    FROM users WHERE normalized_username = 'alice'
                    "#,
                    [],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, i64>(3)?,
                        ))
                    },
                )?;
                let follows_bob = conn.query_row(
                    "SELECT COUNT(*) FROM follows WHERE follower_id = 2 AND followed_id = 1",
                    [],
                    |row| row.get::<_, i64>(0),
                )?;
                Ok((profile.0, profile.1, profile.2, profile.3, follows_bob))
            })
            .await
            .expect("profile state");
        assert_eq!(display_name, "Alice Display");
        assert_eq!(bio, "building a local RustPost");
        assert_eq!(completed, 1);
        assert_eq!(has_avatar, 1);
        assert_eq!(follows_bob, 1);
    }

    #[tokio::test]
    async fn onboarding_can_skip_optional_steps() {
        let server = spawn_test_server().await;
        let cookie = register_test_user(&server, "charlie").await;
        let onboarding = get_with_cookie(&server, "/onboarding", &cookie).await;
        let csrf = csrf_token(&onboarding.body);

        let skipped = request(
            &server.base_url,
            "POST",
            "/onboarding",
            &[
                ("cookie", &cookie),
                (
                    "content-type",
                    "multipart/form-data; boundary=onboarding-boundary",
                ),
            ],
            multipart_body(
                "onboarding-boundary",
                &[("csrf", csrf.as_str()), ("intent", "skip")],
                false,
            ),
        )
        .await;

        assert_eq!(skipped.status, 303);
        assert_eq!(location(&skipped), "/home");
        let (display_name, completed): (String, i64) = server
            .pool
            .call(|conn| {
                Ok(conn.query_row(
                    "SELECT display_name, onboarding_completed_at IS NOT NULL FROM users WHERE normalized_username = 'charlie'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?)
            })
            .await
            .expect("onboarding completion");
        assert_eq!(display_name, "charlie");
        assert_eq!(completed, 1);

        let onboarding = get_with_cookie(&server, "/onboarding", &cookie).await;
        assert_eq!(onboarding.status, 303);
        assert_eq!(location(&onboarding), "/home");
    }

    #[tokio::test]
    async fn onboarding_suggestions_exclude_current_user() {
        let server = spawn_test_server().await;
        let _bob_cookie = register_test_user(&server, "bob").await;
        let alice_cookie = register_test_user(&server, "alice").await;

        let onboarding = get_with_cookie(&server, "/onboarding", &alice_cookie).await;

        assert_eq!(onboarding.status, 200);
        assert!(onboarding.body.contains("@bob"));
        let suggestions = onboarding
            .body
            .split_once(r#"<fieldset class="onboarding-suggestions">"#)
            .and_then(|(_, rest)| rest.split_once("</fieldset>"))
            .map(|(section, _)| section)
            .expect("suggestions section");
        assert!(!suggestions.contains(r#"value="2""#));
        assert!(!suggestions.contains("@alice"));
    }

    #[tokio::test]
    async fn onboarding_ignores_unavailable_suggested_follow_ids() {
        let server = spawn_test_server().await;
        let _bob_cookie = register_test_user(&server, "bob").await;
        let _carol_cookie = register_test_user(&server, "carol").await;
        let alice_cookie = register_test_user(&server, "alice").await;
        let (bob_id, carol_id, alice_id): (i64, i64, i64) = server
            .pool
            .call(|conn| {
                conn.execute(
                    "UPDATE users SET is_suspended = 1 WHERE normalized_username = 'bob'",
                    [],
                )?;
                conn.execute(
                    "UPDATE users SET is_deleted = 1 WHERE normalized_username = 'carol'",
                    [],
                )?;
                let bob_id = conn.query_row(
                    "SELECT id FROM users WHERE normalized_username = 'bob'",
                    [],
                    |row| row.get(0),
                )?;
                let carol_id = conn.query_row(
                    "SELECT id FROM users WHERE normalized_username = 'carol'",
                    [],
                    |row| row.get(0),
                )?;
                let alice_id = conn.query_row(
                    "SELECT id FROM users WHERE normalized_username = 'alice'",
                    [],
                    |row| row.get(0),
                )?;
                Ok((bob_id, carol_id, alice_id))
            })
            .await
            .expect("users");

        let onboarding = get_with_cookie(&server, "/onboarding", &alice_cookie).await;

        assert_eq!(onboarding.status, 200);
        assert!(!onboarding.body.contains("@bob"));
        assert!(!onboarding.body.contains("@carol"));
        assert!(
            onboarding
                .body
                .contains("No local accounts to suggest yet.")
        );
        let csrf = csrf_token(&onboarding.body);
        let bob_id = bob_id.to_string();
        let carol_id = carol_id.to_string();
        let saved = request(
            &server.base_url,
            "POST",
            "/onboarding",
            &[
                ("cookie", &alice_cookie),
                (
                    "content-type",
                    "multipart/form-data; boundary=onboarding-boundary",
                ),
            ],
            multipart_body(
                "onboarding-boundary",
                &[
                    ("csrf", csrf.as_str()),
                    ("intent", "save"),
                    ("display_name", "Alice"),
                    ("bio", ""),
                    ("follow_user_id", bob_id.as_str()),
                    ("follow_user_id", carol_id.as_str()),
                ],
                false,
            ),
        )
        .await;

        assert_eq!(saved.status, 303);
        let follows: i64 = server
            .pool
            .call(move |conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM follows WHERE follower_id = ?",
                    [alice_id],
                    |row| row.get(0),
                )?)
            })
            .await
            .expect("follow count");
        assert_eq!(follows, 0);
    }

    #[tokio::test]
    async fn onboarding_rejects_non_image_avatar_without_media_row() {
        let server = spawn_test_server().await;
        let cookie = register_test_user(&server, "alice").await;
        let onboarding = get_with_cookie(&server, "/onboarding", &cookie).await;
        let csrf = csrf_token(&onboarding.body);
        let body = multipart_body_with_file(
            "onboarding-boundary",
            &[
                ("csrf", csrf.as_str()),
                ("intent", "save"),
                ("display_name", "Alice"),
                ("bio", ""),
            ],
            "profile_picture",
            "avatar.mp4",
            "video/mp4",
            &tiny_mp4_bytes(),
        );

        let saved = request(
            &server.base_url,
            "POST",
            "/onboarding",
            &[
                ("cookie", &cookie),
                (
                    "content-type",
                    "multipart/form-data; boundary=onboarding-boundary",
                ),
            ],
            body,
        )
        .await;

        assert_eq!(saved.status, 400);
        assert_eq!(media_row_count(&server).await, 0);
    }

    #[tokio::test]
    async fn onboarding_cleans_uploaded_avatar_when_csrf_fails() {
        let server = spawn_test_server().await;
        let cookie = register_test_user(&server, "alice").await;
        let body = multipart_body_with_file(
            "onboarding-boundary",
            &[
                ("csrf", "invalid"),
                ("intent", "save"),
                ("display_name", "Alice"),
                ("bio", ""),
            ],
            "profile_picture",
            "avatar.png",
            "image/png",
            &tiny_png_bytes(),
        );

        let saved = request(
            &server.base_url,
            "POST",
            "/onboarding",
            &[
                ("cookie", &cookie),
                (
                    "content-type",
                    "multipart/form-data; boundary=onboarding-boundary",
                ),
            ],
            body,
        )
        .await;

        assert_eq!(saved.status, 403);
        assert_eq!(media_row_count(&server).await, 0);
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
    async fn post_edit_succeeds_within_default_window_and_marks_edited() {
        let server = spawn_test_server().await;
        let cookie = register_test_user(&server, "alice").await;
        create_text_post(&server, &cookie, "original text").await;
        set_post_created_seconds_ago(&server, 1, 0).await;

        let home = get_with_cookie(&server, "/home", &cookie).await;
        assert!(home.body.contains(r#"href="/posts/1/edit""#));
        let edit = get_with_cookie(&server, "/posts/1/edit", &cookie).await;
        assert_eq!(edit.status, 200);
        assert!(
            edit.body
                .contains("<h1 id=\"edit-post-title\">Edit post</h1>")
        );
        assert!(edit.body.contains("original text"));
        let csrf = csrf_token(&edit.body);
        let edited = post_form_with_cookie(
            &server,
            "/posts/1/edit",
            &cookie,
            &format!(
                "csrf={}&text={}&return_to={}",
                form_encode(&csrf),
                form_encode("edited text"),
                form_encode("/home#post-1")
            ),
        )
        .await;

        assert_eq!(edited.status, 303);
        assert_eq!(location(&edited), "/home#post-1");
        let home = get_with_cookie(&server, "/home", &cookie).await;
        assert!(home.body.contains("edited text"));
        assert!(!home.body.contains("original text"));
        assert!(home.body.contains(r#"<span class="edited-marker""#));
        let edited_at: Option<String> = server
            .pool
            .call(|conn| {
                conn.query_row("SELECT edited_at FROM posts WHERE id = 1", [], |row| {
                    row.get(0)
                })
                .map_err(Into::into)
            })
            .await
            .expect("edited_at");
        assert!(edited_at.is_some());
    }

    #[tokio::test]
    async fn post_edit_is_rejected_after_default_window_expires() {
        let server = spawn_test_server().await;
        let cookie = register_test_user(&server, "alice").await;
        create_text_post(&server, &cookie, "too old").await;
        set_post_created_seconds_ago(&server, 1, 20).await;

        let home = get_with_cookie(&server, "/home", &cookie).await;
        assert!(!home.body.contains(r#"href="/posts/1/edit""#));
        let edit = get_with_cookie(&server, "/posts/1/edit", &cookie).await;
        assert_eq!(edit.status, 400);

        let home = get_with_cookie(&server, "/home", &cookie).await;
        let csrf = csrf_token(&home.body);
        let edited = post_form_with_cookie(
            &server,
            "/posts/1/edit",
            &cookie,
            &format!("csrf={}&text=late", form_encode(&csrf)),
        )
        .await;
        assert_eq!(edited.status, 400);
        assert!(edited.body.contains("edit window"));
        let text: String = server
            .pool
            .call(|conn| {
                conn.query_row("SELECT text FROM posts WHERE id = 1", [], |row| row.get(0))
                    .map_err(Into::into)
            })
            .await
            .expect("post text");
        assert_eq!(text, "too old");
    }

    #[tokio::test]
    async fn edit_controls_do_not_appear_for_other_users() {
        let server = spawn_test_server().await;
        let alice_cookie = register_test_user(&server, "alice").await;
        create_text_post(&server, &alice_cookie, "alice post").await;
        let bob_cookie = register_test_user(&server, "bob").await;

        let bob_home = get_with_cookie(&server, "/home", &bob_cookie).await;

        assert_eq!(bob_home.status, 200);
        assert!(bob_home.body.contains("alice post"));
        assert!(!bob_home.body.contains(r#"href="/posts/1/edit""#));
        let edit = get_with_cookie(&server, "/posts/1/edit", &bob_cookie).await;
        assert_eq!(edit.status, 403);
    }

    #[tokio::test]
    async fn no_js_edit_form_is_still_server_window_enforced() {
        let server = spawn_test_server().await;
        let cookie = register_test_user(&server, "alice").await;
        create_text_post(&server, &cookie, "fallback edit").await;
        let edit = get_with_cookie(&server, "/posts/1/edit", &cookie).await;
        assert_eq!(edit.status, 200);
        assert!(
            edit.body
                .contains(r#"method="post" action="/posts/1/edit""#)
        );
        assert!(!edit.body.contains(r#"data-enhance"#));
        let csrf = csrf_token(&edit.body);
        set_post_created_seconds_ago(&server, 1, 20).await;

        let expired = post_form_with_cookie(
            &server,
            "/posts/1/edit",
            &cookie,
            &format!(
                "csrf={}&text={}",
                form_encode(&csrf),
                form_encode("late fallback")
            ),
        )
        .await;

        assert_eq!(expired.status, 400);
        assert!(expired.body.contains("edit window"));
        let thread = get_with_cookie(&server, "/posts/1", &cookie).await;
        assert!(thread.body.contains("fallback edit"));
        assert!(!thread.body.contains("late fallback"));
    }

    #[tokio::test]
    async fn configured_post_edit_window_is_respected() {
        let mut settings = Settings::default();
        settings.posts.post_edit_window_seconds = 30;
        let server = spawn_test_server_with_settings(settings).await;
        let cookie = register_test_user(&server, "alice").await;
        create_text_post(&server, &cookie, "still editable").await;
        set_post_created_seconds_ago(&server, 1, 20).await;

        let home = get_with_cookie(&server, "/home", &cookie).await;
        assert!(home.body.contains(r#"href="/posts/1/edit""#));
        let csrf = csrf_token(&home.body);
        let edited = post_form_with_cookie(
            &server,
            "/posts/1/edit",
            &cookie,
            &format!(
                "csrf={}&text={}",
                form_encode(&csrf),
                form_encode("edited in 30")
            ),
        )
        .await;

        assert_eq!(edited.status, 303);
        let thread = get_with_cookie(&server, "/posts/1", &cookie).await;
        assert!(thread.body.contains("edited in 30"));
    }

    #[tokio::test]
    async fn zero_post_edit_window_disables_get_post_and_controls() {
        let mut settings = Settings::default();
        settings.posts.post_edit_window_seconds = 0;
        let server = spawn_test_server_with_settings(settings).await;
        let cookie = register_test_user(&server, "alice").await;
        create_text_post(&server, &cookie, "not editable").await;
        set_post_created_seconds_ago(&server, 1, 0).await;

        let home = get_with_cookie(&server, "/home", &cookie).await;
        assert!(!home.body.contains(r#"href="/posts/1/edit""#));
        let edit = get_with_cookie(&server, "/posts/1/edit", &cookie).await;
        assert_eq!(edit.status, 400);
        let csrf = csrf_token(&home.body);
        let edited = post_form_with_cookie(
            &server,
            "/posts/1/edit",
            &cookie,
            &format!("csrf={}&text=changed", form_encode(&csrf)),
        )
        .await;

        assert_eq!(edited.status, 400);
        let text: String = server
            .pool
            .call(|conn| {
                conn.query_row("SELECT text FROM posts WHERE id = 1", [], |row| row.get(0))
                    .map_err(Into::into)
            })
            .await
            .expect("post text");
        assert_eq!(text, "not editable");
    }

    #[tokio::test]
    async fn editing_to_same_clean_text_does_not_mark_post_edited() {
        let server = spawn_test_server().await;
        let cookie = register_test_user(&server, "alice").await;
        create_text_post(&server, &cookie, "same text").await;
        set_post_created_seconds_ago(&server, 1, 0).await;
        let edit = get_with_cookie(&server, "/posts/1/edit", &cookie).await;
        let csrf = csrf_token(&edit.body);

        let edited = post_form_with_cookie(
            &server,
            "/posts/1/edit",
            &cookie,
            &format!(
                "csrf={}&text={}",
                form_encode(&csrf),
                form_encode("same text")
            ),
        )
        .await;

        assert_eq!(edited.status, 303);
        let edited_at: Option<String> = server
            .pool
            .call(|conn| {
                conn.query_row("SELECT edited_at FROM posts WHERE id = 1", [], |row| {
                    row.get(0)
                })
                .map_err(Into::into)
            })
            .await
            .expect("edited_at");
        assert!(edited_at.is_none());
        let home = get_with_cookie(&server, "/home", &cookie).await;
        assert!(!home.body.contains(r#"<span class="edited-marker""#));
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
                .contains(r#"data-profile-followers="1">1 follower"#)
        );
        assert!(!profile.body.contains(">Unfollow</button>"));
    }

    #[tokio::test]
    async fn profile_tabs_render_labels_active_state_and_plain_links() {
        let server = spawn_test_server().await;
        let cookie = register_test_user(&server, "alice").await;

        let profile = get_with_cookie(&server, "/users/alice", &cookie).await;
        assert_eq!(profile.status, 200);
        assert!(profile.body.contains(r#"data-testid="profile-tabs""#));
        assert!(
            profile
                .body
                .contains(r#"<a href="/users/alice" class="active" aria-current="page">Posts</a>"#)
        );
        assert!(
            profile
                .body
                .contains(r#"<a href="/users/alice?tab=replies">Replies</a>"#)
        );
        assert!(
            profile
                .body
                .contains(r#"<a href="/users/alice?tab=media">Media</a>"#)
        );
        assert!(
            profile
                .body
                .contains(r#"<a href="/users/alice?tab=likes">Likes</a>"#)
        );
        assert_eq!(profile.body.matches(r#"aria-current="page""#).count(), 1);

        let replies = request(
            &server.base_url,
            "GET",
            "/users/alice?tab=replies",
            &[
                ("cookie", &cookie),
                ("user-agent", "Mozilla/5.0 Firefox/115.0"),
            ],
            Vec::new(),
        )
        .await;
        assert_eq!(replies.status, 200);
        assert!(replies.body.contains(
            r#"<a href="/users/alice?tab=replies" class="active" aria-current="page">Replies</a>"#
        ));
        assert_eq!(replies.body.matches(r#"aria-current="page""#).count(), 1);
    }

    #[tokio::test]
    async fn profile_likes_tab_respects_public_and_private_visibility() {
        let (server, alice_cookie, bob_cookie) = liked_profile_fixture().await;

        let public_likes = get_with_cookie(&server, "/users/bob?tab=likes", &alice_cookie).await;
        assert_eq!(public_likes.status, 200);
        assert!(public_likes.body.contains("privacy target post"));
        assert!(!public_likes.body.contains("This user’s likes are private"));

        let settings = get_with_cookie(&server, "/settings", &bob_cookie).await;
        assert_eq!(settings.status, 200);
        assert!(settings.body.contains(
            r#"id="liked_posts_public" name="liked_posts_public" type="checkbox" role="switch" value="true" checked aria-describedby="liked_posts_public-help""#
        ));
        let saved = save_profile_settings(
            &server,
            &bob_cookie,
            &[
                ("display_name", "bob"),
                ("bio", ""),
                ("location", ""),
                ("website", ""),
            ],
        )
        .await;
        assert_eq!(saved.status, 303);

        let private_likes = get_with_cookie(&server, "/users/bob?tab=likes", &alice_cookie).await;
        assert_eq!(private_likes.status, 200);
        assert_empty_state(&private_likes.body, "This user’s likes are private", "");
        assert!(!private_likes.body.contains("privacy target post"));
        assert!(!private_likes.body.contains("liked by bob"));
        assert!(!private_likes.body.contains("bob liked"));

        let owner_likes = get_with_cookie(&server, "/users/bob?tab=likes", &bob_cookie).await;
        assert_eq!(owner_likes.status, 200);
        assert!(owner_likes.body.contains("privacy target post"));
        assert!(!owner_likes.body.contains("This user’s likes are private"));

        let anonymous_likes = request(
            &server.base_url,
            "GET",
            "/users/bob?tab=likes&offset=999",
            &[],
            Vec::new(),
        )
        .await;
        assert_eq!(anonymous_likes.status, 200);
        assert_empty_state(&anonymous_likes.body, "This user’s likes are private", "");
        assert!(!anonymous_likes.body.contains("privacy target post"));
    }

    #[test]
    fn settings_profile_media_mounts_controls_on_preview_frames() {
        let body = settings_profile_media(
            Some("/uploads/profile.webp"),
            Some("/uploads/banner.webp"),
            true,
            true,
        );

        assert!(body.contains(
            r#"class="settings-banner-wrap settings-media-frame" data-profile-media-frame"#
        ));
        assert!(body.contains(r#"class="settings-picture-wrap settings-media-frame""#));
        assert!(
            body.contains(r#"class="settings-media-input" id="banner" name="banner" type="file""#)
        );
        assert!(body.contains(
            r#"class="settings-media-delete-input" id="delete_banner" name="delete_banner" type="checkbox" value="true""#
        ));
        assert!(body.contains(
            r#"class="settings-media-input" id="profile_picture" name="profile_picture" type="file""#
        ));
        assert!(body.contains(
            r#"class="settings-media-delete-input" id="delete_profile_picture" name="delete_profile_picture" type="checkbox" value="true""#
        ));
        assert!(body.contains(r#"for="banner" title="Change banner""#));
        assert!(body.contains(r#"for="delete_banner" title="Remove banner""#));
        assert!(body.contains(r#"for="profile_picture" title="Change profile picture""#));
        assert!(body.contains(r#"for="delete_profile_picture" title="Remove profile picture""#));
        assert!(!body.contains("media-control-row"));
        assert!(!body.contains("file-control"));
        assert!(!body.contains("check-row"));
    }

    #[tokio::test]
    async fn settings_page_places_profile_media_before_profile_fields() {
        let server = spawn_test_server().await;
        let cookie = register_test_user(&server, "alice").await;

        let settings = get_with_cookie(&server, "/settings", &cookie).await;
        assert_eq!(settings.status, 200);
        let media = settings
            .body
            .find("settings-section settings-section-media")
            .expect("profile media section");
        let profile = settings
            .body
            .find("settings-section settings-section-profile")
            .expect("profile fields section");
        let form_start = settings
            .body
            .find(r#"<form id="profile-settings-form""#)
            .expect("settings form");
        let form_end = settings.body[form_start..]
            .find("</form>")
            .expect("settings form end");
        let settings_form = &settings.body[form_start..form_start + form_end];

        assert!(media < profile);
        assert!(
            settings
                .body
                .contains(r#"class="settings-media-input" id="banner" name="banner" type="file""#)
        );
        assert!(settings.body.contains(
            r#"class="settings-media-input" id="profile_picture" name="profile_picture" type="file""#
        ));
        assert!(!settings_form.contains("media-control-row"));
        assert!(!settings_form.contains("check-row"));
    }

    #[tokio::test]
    async fn private_likes_reject_bypass_notifications_and_json_count_leaks() {
        let (server, alice_cookie, bob_cookie) = liked_profile_fixture().await;
        let saved = save_profile_settings(
            &server,
            &bob_cookie,
            &[
                ("display_name", "bob"),
                ("bio", ""),
                ("location", ""),
                ("website", ""),
            ],
        )
        .await;
        assert_eq!(saved.status, 303);

        for path in [
            "/users/bob?tab=likes&page=2",
            "/users/bob?tab=likes&cursor=1",
        ] {
            let bypass = get_with_cookie(&server, path, &alice_cookie).await;
            assert_eq!(bypass.status, 200);
            assert_empty_state(&bypass.body, "This user’s likes are private", "");
            assert!(!bypass.body.contains("privacy target post"));
        }

        let invalid_tab = get_with_cookie(&server, "/users/bob?tab=LIKES", &alice_cookie).await;
        assert_eq!(invalid_tab.status, 200);
        assert!(invalid_tab.body.contains("bob fallback post"));
        assert!(!invalid_tab.body.contains("privacy target post"));
        assert!(
            invalid_tab
                .body
                .contains(r#"<a href="/users/bob" class="active" aria-current="page">Posts</a>"#)
        );

        let notifications = get_with_cookie(&server, "/notifications", &alice_cookie).await;
        assert_eq!(notifications.status, 200);
        assert!(!notifications.body.contains("bob"));
        assert!(!notifications.body.contains("liked your post"));
        assert!(!notifications.body.contains("privacy target post"));

        let alice_home = get_with_cookie(&server, "/home", &alice_cookie).await;
        let csrf = csrf_token(&alice_home.body);
        let alice_json = request(
            &server.base_url,
            "POST",
            "/posts/1/bookmark",
            &[
                ("cookie", &alice_cookie),
                ("content-type", "application/x-www-form-urlencoded"),
                ("accept", "application/json"),
                ("x-rustpost-enhance", "1"),
            ],
            format!("csrf={csrf}").into_bytes(),
        )
        .await;
        assert_eq!(alice_json.status, 200);
        assert!(alice_json.body.contains(r#""likes":0"#));

        let bob_home = get_with_cookie(&server, "/home", &bob_cookie).await;
        let csrf = csrf_token(&bob_home.body);
        let bob_json = request(
            &server.base_url,
            "POST",
            "/posts/1/bookmark",
            &[
                ("cookie", &bob_cookie),
                ("content-type", "application/x-www-form-urlencoded"),
                ("accept", "application/json"),
                ("x-rustpost-enhance", "1"),
            ],
            format!("csrf={csrf}").into_bytes(),
        )
        .await;
        assert_eq!(bob_json.status, 200);
        assert!(bob_json.body.contains(r#""likes":1"#));
    }

    #[tokio::test]
    async fn liked_posts_public_setting_persists_without_changing_other_preferences() {
        let server = spawn_test_server().await;
        let cookie = register_test_user(&server, "alice").await;

        let saved_private = save_profile_settings(
            &server,
            &cookie,
            &[
                ("dark_mode", "true"),
                ("nsfw_blur_enabled", "true"),
                ("display_name", "Alice"),
                ("bio", "same bio"),
                ("location", "same place"),
                ("website", "https://example.test"),
            ],
        )
        .await;
        assert_eq!(saved_private.status, 303);

        let private_state = user_settings_state(&server, "alice").await;
        assert_eq!(
            private_state,
            (
                "dark".to_owned(),
                1,
                0,
                "Alice".to_owned(),
                "same bio".to_owned(),
                "same place".to_owned(),
                "https://example.test".to_owned(),
            )
        );

        let settings = get_with_cookie(&server, "/settings", &cookie).await;
        assert_eq!(settings.status, 200);
        assert!(
            settings.body.contains(
                r#"id="dark_mode" name="dark_mode" type="checkbox" role="switch" value="true" checked aria-describedby="dark_mode-help""#
            )
        );
        assert!(settings.body.contains(
            r#"id="nsfw_blur_enabled" name="nsfw_blur_enabled" type="checkbox" role="switch" value="true" checked aria-describedby="nsfw_blur_enabled-help""#
        ));
        assert!(!settings.body.contains(
            r#"id="liked_posts_public" name="liked_posts_public" type="checkbox" role="switch" value="true" checked"#
        ));

        let saved_public = save_profile_settings(
            &server,
            &cookie,
            &[
                ("dark_mode", "true"),
                ("nsfw_blur_enabled", "true"),
                ("liked_posts_public", "true"),
                ("display_name", "Alice"),
                ("bio", "same bio"),
                ("location", "same place"),
                ("website", "https://example.test"),
            ],
        )
        .await;
        assert_eq!(saved_public.status, 303);

        let (theme, nsfw_blur_enabled, liked_posts_public, ..) =
            user_settings_state(&server, "alice").await;
        assert_eq!(
            (theme, nsfw_blur_enabled, liked_posts_public),
            ("dark".to_owned(), 1, 1)
        );
    }

    #[tokio::test]
    async fn profile_website_rejects_unsafe_schemes_and_hides_legacy_values() {
        let server = spawn_test_server().await;
        let cookie = register_test_user(&server, "alice").await;

        let unsafe_saved = save_profile_settings(
            &server,
            &cookie,
            &[
                ("display_name", "Alice"),
                ("bio", ""),
                ("location", ""),
                ("website", "javascript:alert(1)"),
            ],
        )
        .await;
        assert_eq!(unsafe_saved.status, 400);
        assert!(
            unsafe_saved
                .body
                .contains("website URL must start with http:// or https://")
        );
        let (_, _, _, _, _, _, website) = user_settings_state(&server, "alice").await;
        assert_eq!(website, "");

        server
            .pool
            .call(|conn| {
                conn.execute(
                    "UPDATE users SET website = 'javascript:alert(1)' WHERE normalized_username = 'alice'",
                    [],
                )?;
                Ok(())
            })
            .await
            .expect("write legacy unsafe website");

        let profile = get_with_cookie(&server, "/users/alice", &cookie).await;
        assert_eq!(profile.status, 200);
        assert!(!profile.body.contains("javascript:alert"));
        assert!(!profile.body.contains(r#"href="javascript:"#));

        let safe_saved = save_profile_settings(
            &server,
            &cookie,
            &[
                ("display_name", "Alice"),
                ("bio", ""),
                ("location", ""),
                ("website", "https://example.test/profile"),
            ],
        )
        .await;
        assert_eq!(safe_saved.status, 303);
        let profile = get_with_cookie(&server, "/users/alice", &cookie).await;
        assert!(profile.body.contains(
            r#"<a href="https://example.test/profile" rel="noopener noreferrer nofollow">https://example.test/profile</a>"#
        ));
    }

    #[tokio::test]
    async fn profile_tabs_apply_relationship_and_suspended_visibility() {
        let server = spawn_test_server().await;
        let alice_cookie = register_test_user(&server, "alice").await;
        create_text_post(&server, &alice_cookie, "relationship liked target").await;
        let bob_cookie = register_test_user(&server, "bob").await;
        create_text_post(&server, &bob_cookie, "relationship bob post").await;

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

        let visible_likes = get_with_cookie(&server, "/users/bob?tab=likes", &alice_cookie).await;
        assert_eq!(visible_likes.status, 200);
        assert!(visible_likes.body.contains("relationship liked target"));

        let bob_id = server
            .pool
            .call(|conn| {
                conn.query_row(
                    "SELECT id FROM users WHERE normalized_username = 'bob'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(Into::into)
            })
            .await
            .expect("bob id");
        let alice_id = server
            .pool
            .call(|conn| {
                conn.query_row(
                    "SELECT id FROM users WHERE normalized_username = 'alice'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(Into::into)
            })
            .await
            .expect("alice id");
        social::block(&server.pool, bob_id, alice_id)
            .await
            .expect("bob blocks alice");

        let blocked_likes = get_with_cookie(&server, "/users/bob?tab=likes", &alice_cookie).await;
        assert_eq!(blocked_likes.status, 200);
        assert!(!blocked_likes.body.contains("relationship liked target"));
        assert!(!blocked_likes.body.contains("This user’s likes are private"));
        let blocked_posts = get_with_cookie(&server, "/users/bob", &alice_cookie).await;
        assert_eq!(blocked_posts.status, 200);
        assert!(!blocked_posts.body.contains("relationship bob post"));

        server
            .pool
            .call(move |conn| {
                conn.execute(
                    "DELETE FROM blocks WHERE blocker_id = ? AND blocked_id = ?",
                    params![bob_id, alice_id],
                )?;
                conn.execute("UPDATE users SET is_suspended = 1 WHERE id = ?", [bob_id])?;
                Ok(())
            })
            .await
            .expect("suspend bob");

        let suspended_likes = request(
            &server.base_url,
            "GET",
            "/users/bob?tab=likes",
            &[],
            Vec::new(),
        )
        .await;
        assert_eq!(suspended_likes.status, 200);
        assert!(!suspended_likes.body.contains("relationship liked target"));
        assert!(
            !suspended_likes
                .body
                .contains("This user’s likes are private")
        );
        let suspended_posts = request(&server.base_url, "GET", "/users/bob", &[], Vec::new()).await;
        assert_eq!(suspended_posts.status, 200);
        assert!(!suspended_posts.body.contains("relationship bob post"));
    }

    #[tokio::test]
    async fn profile_owner_can_pin_and_replace_one_profile_post() {
        let server = spawn_test_server().await;
        let cookie = register_test_user(&server, "alice").await;
        create_text_post(&server, &cookie, "first profile post").await;
        create_text_post(&server, &cookie, "second profile post").await;

        let profile = get_with_cookie(&server, "/users/alice", &cookie).await;
        assert_eq!(profile.status, 200);
        assert!(profile.body.contains(r#"action="/posts/1/pin""#));
        assert!(profile.body.contains(r#"aria-label="Pin to profile""#));
        let csrf = csrf_token(&profile.body);
        let pinned = request(
            &server.base_url,
            "POST",
            "/posts/1/pin",
            &[
                ("cookie", &cookie),
                ("referer", "/users/alice"),
                ("content-type", "application/x-www-form-urlencoded"),
            ],
            format!("csrf={}", form_encode(&csrf)).into_bytes(),
        )
        .await;

        assert_eq!(pinned.status, 303);
        assert_eq!(location(&pinned), "/users/alice#post-1");
        let profile = get_with_cookie(&server, "/users/alice", &cookie).await;
        assert!(profile.body.contains(r#"aria-label="Pinned post""#));
        assert!(
            profile
                .body
                .contains(r#"<h2 class="section-title">Pinned post</h2>"#)
        );
        assert!(profile.body.contains(r#"aria-label="Unpin from profile""#));
        assert_eq!(profile.body.matches(r#"id="post-1""#).count(), 1);
        let first_index = profile.body.find("first profile post").expect("first post");
        let second_index = profile
            .body
            .find("second profile post")
            .expect("second post");
        assert!(first_index < second_index);

        let csrf = csrf_token(&profile.body);
        let replaced = request(
            &server.base_url,
            "POST",
            "/posts/2/pin",
            &[
                ("cookie", &cookie),
                ("referer", "/users/alice"),
                ("content-type", "application/x-www-form-urlencoded"),
            ],
            format!("csrf={}", form_encode(&csrf)).into_bytes(),
        )
        .await;

        assert_eq!(replaced.status, 303);
        assert_eq!(location(&replaced), "/users/alice#post-2");
        let profile = get_with_cookie(&server, "/users/alice", &cookie).await;
        assert_eq!(profile.body.matches(r#"id="post-2""#).count(), 1);
        let first_index = profile.body.find("first profile post").expect("first post");
        let second_index = profile
            .body
            .find("second profile post")
            .expect("second post");
        assert!(second_index < first_index);
        assert_eq!(pinned_post_id_for_user(&server, "alice").await, Some(2));
    }

    #[tokio::test]
    async fn users_cannot_pin_someone_elses_post() {
        let server = spawn_test_server().await;
        let alice_cookie = register_test_user(&server, "alice").await;
        create_text_post(&server, &alice_cookie, "alice post").await;
        let bob_cookie = register_test_user(&server, "bob").await;
        let bob_home = get_with_cookie(&server, "/home", &bob_cookie).await;
        let csrf = csrf_token(&bob_home.body);

        let pinned = post_form_with_cookie(
            &server,
            "/posts/1/pin",
            &bob_cookie,
            &format!("csrf={}", form_encode(&csrf)),
        )
        .await;

        assert_eq!(pinned.status, 403);
        assert_eq!(pinned_post_id_for_user(&server, "alice").await, None);
        assert_eq!(pinned_post_id_for_user(&server, "bob").await, None);
    }

    #[tokio::test]
    async fn deleting_pinned_post_clears_profile_pin() {
        let server = spawn_test_server().await;
        let cookie = register_test_user(&server, "alice").await;
        create_text_post(&server, &cookie, "temporary pinned post").await;
        let profile = get_with_cookie(&server, "/users/alice", &cookie).await;
        let csrf = csrf_token(&profile.body);
        let pinned = post_form_with_cookie(
            &server,
            "/posts/1/pin",
            &cookie,
            &format!("csrf={}", form_encode(&csrf)),
        )
        .await;
        assert_eq!(pinned.status, 303);
        assert_eq!(pinned_post_id_for_user(&server, "alice").await, Some(1));

        let profile = get_with_cookie(&server, "/users/alice", &cookie).await;
        let csrf = csrf_token(&profile.body);
        let deleted = post_form_with_cookie(
            &server,
            "/posts/1/delete",
            &cookie,
            &format!(
                "csrf={}&return_to={}",
                form_encode(&csrf),
                form_encode("/users/alice")
            ),
        )
        .await;

        assert_eq!(deleted.status, 303);
        assert_eq!(location(&deleted), "/users/alice");
        assert_eq!(pinned_post_id_for_user(&server, "alice").await, None);
        let profile = get_with_cookie(&server, "/users/alice", &cookie).await;
        assert!(!profile.body.contains(r#"aria-label="Pinned post""#));
        assert!(!profile.body.contains("temporary pinned post"));
    }

    #[tokio::test]
    async fn empty_states_render_for_empty_feeds_lists_and_search() {
        let server = spawn_test_server().await;

        let public_home = request(&server.base_url, "GET", "/home", &[], Vec::new()).await;
        assert_eq!(public_home.status, 200);
        assert_empty_state(
            &public_home.body,
            "No posts yet.",
            "The timeline will fill in once people start posting.",
        );

        let alice_cookie = register_test_user(&server, "alice").await;

        let home = get_with_cookie(&server, "/home", &alice_cookie).await;
        assert_eq!(home.status, 200);
        assert_empty_state(
            &home.body,
            "No posts yet.",
            "The timeline will fill in once people start posting.",
        );

        let profile = get_with_cookie(&server, "/users/alice", &alice_cookie).await;
        assert_eq!(profile.status, 200);
        assert_empty_state(
            &profile.body,
            "No posts yet.",
            "This profile has not posted yet.",
        );

        let following = get_with_cookie(&server, "/following", &alice_cookie).await;
        assert_eq!(following.status, 200);
        assert_empty_state(
            &following.body,
            "Follow people to build your feed.",
            "Accounts you follow will appear here.",
        );

        let followers = request(
            &server.base_url,
            "GET",
            "/users/alice/followers",
            &[],
            Vec::new(),
        )
        .await;
        assert_eq!(followers.status, 200);
        assert_empty_state(
            &followers.body,
            "No followers yet.",
            "Followers will appear here.",
        );

        let profile_following = request(
            &server.base_url,
            "GET",
            "/users/alice/following",
            &[],
            Vec::new(),
        )
        .await;
        assert_eq!(profile_following.status, 200);
        assert_empty_state(
            &profile_following.body,
            "Not following anyone yet.",
            "Followed accounts will appear here.",
        );

        let bookmarks = get_with_cookie(&server, "/bookmarks", &alice_cookie).await;
        assert_eq!(bookmarks.status, 200);
        assert_empty_state(
            &bookmarks.body,
            "No bookmarks yet.",
            "Saved posts will appear here.",
        );

        let notifications = get_with_cookie(&server, "/notifications", &alice_cookie).await;
        assert_eq!(notifications.status, 200);
        assert_empty_state(
            &notifications.body,
            "No notifications.",
            "New activity will appear here.",
        );

        let search = request(
            &server.base_url,
            "GET",
            "/search?q=missing",
            &[],
            Vec::new(),
        )
        .await;
        assert_eq!(search.status, 200);
        assert_empty_state(
            &search.body,
            "No matching posts or users found.",
            "Try another search.",
        );
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
    async fn mention_suggestions_endpoint_returns_bounded_json_for_visible_users() {
        let server = spawn_test_server().await;
        let alice_cookie = register_test_user(&server, "alice").await;
        crate::auth::register_user(
            &server.pool,
            &Settings::default(),
            "bob",
            "very secure password",
            false,
        )
        .await
        .expect("bob");
        crate::auth::register_user(
            &server.pool,
            &Settings::default(),
            "bobby",
            "very secure password",
            false,
        )
        .await
        .expect("bobby");
        let suspended = crate::auth::register_user(
            &server.pool,
            &Settings::default(),
            "bot_suspended",
            "very secure password",
            false,
        )
        .await
        .expect("suspended");
        server
            .pool
            .call(move |conn| {
                conn.execute(
                    "UPDATE users SET display_name = '<b>Bob</b>' WHERE normalized_username = 'bob'",
                    [],
                )?;
                conn.execute("UPDATE users SET is_suspended = 1 WHERE id = ?", [suspended])?;
                Ok(())
            })
            .await
            .expect("update users");

        let response = get_with_cookie(&server, "/mentions?q=BO", &alice_cookie).await;

        assert_eq!(response.status, 200);
        assert_header(&response, "content-type", "application/json");
        assert!(response.body.starts_with('['));
        assert!(response.body.contains(r#""username":"bob""#));
        assert!(response.body.contains(r#""display_name":"<b>Bob</b>""#));
        assert!(response.body.contains(r#""username":"bobby""#));
        assert!(!response.body.contains("bot_suspended"));
    }

    #[tokio::test]
    async fn grouped_notification_open_marks_group_read() {
        let server = spawn_test_server().await;
        let alice_cookie = register_test_user(&server, "alice").await;
        create_text_post(&server, &alice_cookie, "grouped notification target").await;
        let bob_cookie = register_test_user(&server, "bob").await;
        let carol_cookie = register_test_user(&server, "carol").await;
        let bob_home = get_with_cookie(&server, "/home", &bob_cookie).await;
        let bob_csrf = csrf_token(&bob_home.body);
        let carol_home = get_with_cookie(&server, "/home", &carol_cookie).await;
        let carol_csrf = csrf_token(&carol_home.body);

        let bob_liked = post_form_with_cookie(
            &server,
            "/posts/1/like",
            &bob_cookie,
            &format!("csrf={}", form_encode(&bob_csrf)),
        )
        .await;
        assert_eq!(bob_liked.status, 303);
        let carol_liked = post_form_with_cookie(
            &server,
            "/posts/1/like",
            &carol_cookie,
            &format!("csrf={}", form_encode(&carol_csrf)),
        )
        .await;
        assert_eq!(carol_liked.status, 303);

        let notifications = get_with_cookie(&server, "/notifications", &alice_cookie).await;
        assert_eq!(notifications.status, 200);
        assert!(notifications.body.contains("2 unread notifications"));
        assert!(notifications.body.contains("2 people"));
        assert!(notifications.body.contains("liked your post"));
        assert!(notifications.body.contains("View people"));
        assert_eq!(
            notifications
                .body
                .matches(r#"class="notification-row unread""#)
                .count(),
            1
        );
        let csrf = csrf_token(&notifications.body);
        let notification_ids = hidden_value(&notifications.body, "notification_ids");

        let opened = post_form_with_cookie(
            &server,
            "/notifications/open",
            &alice_cookie,
            &format!(
                "csrf={}&notification_ids={}&return_to={}",
                form_encode(&csrf),
                form_encode(&notification_ids),
                form_encode("/posts/1")
            ),
        )
        .await;

        assert_eq!(opened.status, 303);
        assert_eq!(location(&opened), "/posts/1");
        let notifications = get_with_cookie(&server, "/notifications", &alice_cookie).await;
        assert!(notifications.body.contains("No unread notifications"));
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
        assert!(notifications.body.contains("No notifications."));
        assert!(
            notifications
                .body
                .contains("New activity will appear here.")
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
        assert!(!home.body.contains(r#"class="nsfw-open""#));
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

        // The running server uses the setting it loaded at startup. Editing
        // settings.toml directly takes effect on restart (documented behavior),
        // and page rendering no longer re-reads and re-parses the file.
        let mut settings = Settings::load(&server.data_dir.join("settings.toml")).expect("load");
        settings.media.nsfw_blur_enabled = false;
        std::fs::write(
            server.data_dir.join("settings.toml"),
            toml::to_string(&settings).expect("settings toml"),
        )
        .expect("write settings");
        let still_blurred = request(&server.base_url, "GET", "/home", &[], Vec::new()).await;
        assert!(still_blurred.body.contains(r#"data-testid="nsfw-media""#));
    }

    #[tokio::test]
    async fn global_nsfw_blur_default_applies_from_loaded_settings() {
        let mut settings = Settings::default();
        settings.media.nsfw_blur_enabled = false;
        let server = spawn_test_server_with_settings(settings).await;
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
                    ("text", "unblurred by default"),
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
        assert!(!logged_out.body.contains(r#"data-testid="nsfw-media""#));
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
    async fn configuration_requests_require_admin_csrf_and_well_formed_fields() {
        let server = spawn_test_server_with_admin().await;
        let cookie = admin_session_cookie(&server).await;
        let page = get_with_cookie(&server, "/admin/deep-settings", &cookie).await;
        let csrf = csrf_token(&page.body);
        let before = std::fs::read(server.data_dir.join("settings.toml")).expect("settings");
        let body = deep_settings_form_body(&server, &csrf, "confirm", &[("max_bio_len", "301")]);
        let unauthorized = request(
            &server.base_url,
            "POST",
            "/admin/deep-settings",
            &[("content-type", "application/x-www-form-urlencoded")],
            body.clone(),
        )
        .await;
        assert_eq!(unauthorized.status, 401);
        let wrong_csrf = request(
            &server.base_url,
            "POST",
            "/admin/deep-settings",
            &[
                ("cookie", &cookie),
                ("content-type", "application/x-www-form-urlencoded"),
            ],
            deep_settings_form_body(&server, "incorrect", "confirm", &[("max_bio_len", "301")]),
        )
        .await;
        assert_eq!(wrong_csrf.status, 403);
        for suffix in [
            "&unexpected=1",
            "&max_bio_len=999",
            "&media.ffmpeg_path=unsafe",
        ] {
            let mut malformed = body.clone();
            malformed.extend_from_slice(suffix.as_bytes());
            let response = request(
                &server.base_url,
                "POST",
                "/admin/deep-settings",
                &[
                    ("cookie", &cookie),
                    ("content-type", "application/x-www-form-urlencoded"),
                ],
                malformed,
            )
            .await;
            assert_eq!(response.status, 422);
        }
        let incomplete = request(
            &server.base_url,
            "POST",
            "/admin/deep-settings",
            &[
                ("cookie", &cookie),
                ("content-type", "application/x-www-form-urlencoded"),
            ],
            format!("csrf={csrf}&intent=confirm").into_bytes(),
        )
        .await;
        assert_eq!(incomplete.status, 422);
        let bad_intent = request(
            &server.base_url,
            "POST",
            "/admin/deep-settings",
            &[
                ("cookie", &cookie),
                ("content-type", "application/x-www-form-urlencoded"),
            ],
            deep_settings_form_body(&server, &csrf, "unexpected", &[]),
        )
        .await;
        assert_eq!(bad_intent.status, 400);
        assert_eq!(
            std::fs::read(server.data_dir.join("settings.toml")).expect("settings"),
            before
        );
    }

    #[tokio::test]
    async fn stale_configuration_confirmation_cannot_overwrite_another_save() {
        let server = spawn_test_server_with_admin().await;
        let cookie = admin_session_cookie(&server).await;
        let page = get_with_cookie(&server, "/admin/deep-settings", &cookie).await;
        let csrf = csrf_token(&page.body);
        let stale = deep_settings_form_body(&server, &csrf, "confirm", &[("max_bio_len", "999")]);
        let path = server.data_dir.join("settings.toml");
        let mut settings = Settings::load(&path).expect("load");
        settings.accounts.max_bio_len = 302;
        admin::write_deep_settings(&path, &settings).expect("concurrent save");
        let response = request(
            &server.base_url,
            "POST",
            "/admin/deep-settings",
            &[
                ("cookie", &cookie),
                ("content-type", "application/x-www-form-urlencoded"),
            ],
            stale,
        )
        .await;
        assert_eq!(response.status, 200);
        assert!(
            response
                .body
                .contains("Settings changed since this form was opened")
        );
        assert_eq!(
            Settings::load(&path).expect("load").accounts.max_bio_len,
            302
        );
    }

    #[tokio::test]
    async fn invalid_configuration_preserves_other_values_and_escapes_submitted_text() {
        let server = spawn_test_server_with_admin().await;
        let cookie = admin_session_cookie(&server).await;
        let page = get_with_cookie(&server, "/admin/deep-settings", &cookie).await;
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
                &server,
                &csrf,
                "preview",
                &[
                    ("max_bio_len", "bad"),
                    ("site_name", "<script>alert(1)</script>"),
                ],
            ),
        )
        .await;
        assert_eq!(response.status, 200);
        assert!(
            response
                .body
                .contains("&lt;script&gt;alert(1)&lt;/script&gt;")
        );
        assert!(!response.body.contains("<script>alert(1)</script>"));
        assert!(response.body.contains(r#"value="bad""#));
        assert!(response.body.contains("settings-error"));
    }

    #[tokio::test]
    async fn backup_settings_share_validation_and_preserve_submitted_values() {
        let server = spawn_test_server_with_admin().await;
        let cookie = admin_session_cookie(&server).await;
        let page = get_with_cookie(&server, "/admin/backups", &cookie).await;
        let csrf = csrf_token(&page.body);
        let before = std::fs::read(server.data_dir.join("settings.toml")).expect("settings");
        let response = request(&server.base_url, "POST", "/admin/backups/settings",
            &[("cookie", &cookie), ("content-type", "application/x-www-form-urlencoded")],
            format!("csrf={csrf}&enabled=true&automatic_enabled=false&automatic_interval_minutes=9&retention_keep_last=0&retention_max_age_days=45&automatic_include_tor_keys=false").into_bytes()).await;
        assert_eq!(response.status, 200);
        assert!(
            response
                .body
                .contains("Automatic backups to keep must be at least 1")
        );
        assert!(response.body.contains(r#"id="backup-interval" name="automatic_interval_minutes" type="number" inputmode="numeric" min="0" step="1" value="9""#));
        assert!(response.body.contains(r#"id="backup-age" name="retention_max_age_days" type="number" inputmode="numeric" min="0" step="1" value="45""#));
        assert_eq!(
            std::fs::read(server.data_dir.join("settings.toml")).expect("settings"),
            before
        );
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
        assert!(response.body.contains(r#"name="post_edit_window_seconds""#));
        assert!(
            response
                .body
                .contains(r#"name="registration_captcha_enabled""#)
        );
        assert!(response.body.contains("<select"));
        assert!(
            response
                .body
                .contains(r#"name="max_bio_len" type="number""#)
        );
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
                &server,
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
            deep_settings_form_body(&server, &csrf, "discard", &[("max_bio_len", "300")]),
        )
        .await;
        let after =
            std::fs::read_to_string(server.data_dir.join("settings.toml")).expect("settings");

        assert_eq!(response.status, 200);
        assert_eq!(before, after);
        assert!(response.body.contains("Changes discarded."));
        assert!(response.body.contains(
            r#"name="max_bio_len" type="number" aria-describedby="deep-max_bio_len-help" inputmode="decimal" min="0" step="1" value="240""#
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
                &server,
                &csrf,
                "confirm",
                &[
                    ("max_bio_len", "300"),
                    ("post_edit_window_seconds", "20"),
                    ("allow_profile_pictures", "false"),
                    ("nsfw_blur_enabled", "false"),
                    ("registration_captcha_enabled", "true"),
                ],
            ),
        )
        .await;
        let saved = Settings::load(&server.data_dir.join("settings.toml")).expect("settings");

        assert_eq!(response.status, 200);
        assert!(response.body.contains("Settings saved successfully. Restart required for startup settings; blur applies immediately and backup policy takes effect on the next check."));
        assert_eq!(saved.accounts.max_bio_len, 300);
        assert_eq!(saved.posts.post_edit_window_seconds, 20);
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
            r#"name="max_bio_len" type="number" aria-describedby="deep-max_bio_len-help" inputmode="decimal" min="0" step="1" value="300""#
        ));
        assert!(fresh.body.contains(
            r#"name="post_edit_window_seconds" type="number" aria-describedby="deep-post_edit_window_seconds-help" inputmode="decimal" min="0" step="1" value="20""#
        ));
        assert!(
            fresh
                .body
                .contains(r#"id="deep-allow_profile_pictures" name="allow_profile_pictures" type="checkbox" aria-describedby="deep-allow_profile_pictures-help" value="true">"#)
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
            deep_settings_form_body(&server, &csrf, "preview", &[("min_password_length", "-1")]),
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
    async fn invalid_post_edit_window_deep_setting_is_rejected_without_writing() {
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
                &server,
                &csrf,
                "preview",
                &[("post_edit_window_seconds", "999")],
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
                .contains("Post edit window must be 300 seconds or less")
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
        // Immediate deletion keeps this test focused on intent and password control.
        let server = spawn_test_server_without_deletion_grace().await;
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
        let server = spawn_test_server_without_deletion_grace().await;
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

    async fn user_id_for(server: &TestServer, username: &str) -> i64 {
        let username = username.to_owned();
        server
            .pool
            .call(move |conn| {
                conn.query_row(
                    "SELECT id FROM users WHERE normalized_username = ?",
                    [username],
                    |row| row.get(0),
                )
                .map_err(Into::into)
            })
            .await
            .expect("user id")
    }

    /// End-to-end backup/restore: creates representative data through the HTTP
    /// API, snapshots it, restores into a fresh data directory, and verifies
    /// the restored database and media files.
    #[tokio::test]
    #[expect(
        clippy::too_many_lines,
        reason = "one end-to-end backup/restore scenario keeps setup, restore, and assertions in order"
    )]
    async fn backup_restore_round_trips_accounts_graph_muted_words_and_media() {
        let server = spawn_test_server().await;
        let alice = register_test_user(&server, "alice").await;
        let bob = register_test_user(&server, "bob").await;
        let _carol = register_test_user(&server, "carol").await;

        create_text_post(&server, &alice, "alice first post").await;
        create_text_post(&server, &bob, "bob first post").await;

        // Media upload from bob.
        let bob_csrf = csrf_token(&get_with_cookie(&server, "/home", &bob).await.body);
        let upload = request(
            &server.base_url,
            "POST",
            "/posts",
            &[
                ("cookie", bob.as_str()),
                (
                    "content-type",
                    "multipart/form-data; boundary=media-boundary",
                ),
            ],
            multipart_body_with_file(
                "media-boundary",
                &[("csrf", bob_csrf.as_str()), ("text", "bob photo post")],
                "media",
                "holiday.png",
                "image/png",
                &tiny_png_bytes(),
            ),
        )
        .await;
        assert_eq!(upload.status, 303);

        // Alice follows bob; bob blocks carol; alice mutes a word.
        let bob_id = user_id_for(&server, "bob").await;
        let carol_id = user_id_for(&server, "carol").await;
        let alice_csrf = csrf_token(&get_with_cookie(&server, "/home", &alice).await.body);
        let followed = post_form_with_cookie(
            &server,
            &format!("/users/{bob_id}/follow"),
            &alice,
            &format!("csrf={alice_csrf}"),
        )
        .await;
        assert_eq!(followed.status, 303);
        let bob_settings_csrf = csrf_token(&get_with_cookie(&server, "/settings", &bob).await.body);
        let blocked = post_form_with_cookie(
            &server,
            &format!("/users/{carol_id}/block"),
            &bob,
            &format!("csrf={bob_settings_csrf}"),
        )
        .await;
        assert_eq!(blocked.status, 303);
        let alice_settings_csrf =
            csrf_token(&get_with_cookie(&server, "/settings", &alice).await.body);
        let muted = post_form_with_cookie(
            &server,
            "/settings/muted-words",
            &alice,
            &format!("csrf={alice_settings_csrf}&term=spoilers"),
        )
        .await;
        assert_eq!(muted.status, 303);

        let source_paths = RuntimePaths::from_data_dir(server.data_dir.clone());
        let archive = backup::create_backup(&source_paths, false).expect("create backup");
        assert!(archive.is_file());

        let target_temp = tempfile::tempdir().expect("target temp dir");
        let target_paths = RuntimePaths::from_data_dir(target_temp.path().to_path_buf());
        target_paths.ensure().expect("target paths");
        let report =
            backup::restore_backup(&target_paths, &archive, false).expect("restore backup");
        assert!(report.pre_restore_backup.is_some());

        let restored =
            rusqlite::Connection::open(&target_paths.database_path).expect("restored database");
        let count = |table: &str| -> i64 {
            restored
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .expect("count")
        };
        assert_eq!(count("users"), 3);
        assert_eq!(count("posts"), 3);
        assert_eq!(count("follows"), 1);
        assert_eq!(count("blocks"), 1);
        assert_eq!(count("muted_words"), 1);
        assert_eq!(count("media"), 1);
        let (stored_path, public_path): (String, String) = restored
            .query_row(
                "SELECT stored_path, public_path FROM media LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("media row");
        // Restore rewrites absolute media paths for the destination data dir.
        let target_prefix = target_paths.data_dir.to_string_lossy().to_string();
        assert!(
            stored_path.starts_with(&target_prefix),
            "restored media path {stored_path} still points outside {target_prefix}"
        );
        let file_name = std::path::Path::new(&stored_path)
            .file_name()
            .expect("media file name");
        assert!(
            target_paths.uploads_originals.join(file_name).is_file(),
            "restored media file is missing for {public_path}"
        );
        assert!(target_paths.settings_path.is_file());
        crate::db::validate_schema(&restored).expect("restored schema validates");
        let restored_post_id: i64 = restored
            .query_row("SELECT post_id FROM post_media LIMIT 1", [], |row| {
                row.get(0)
            })
            .expect("post media");
        drop(restored);

        // Media cleanup must work against the restored paths.
        let restored_pool = crate::db::connect(&target_paths.database_path)
            .await
            .expect("restored pool");
        crate::media::delete_post_media(&restored_pool, &target_paths, restored_post_id)
            .await
            .expect("delete restored media");
        assert!(!target_paths.uploads_originals.join(file_name).exists());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_page_loads_each_render_a_csrf_token() {
        let server = spawn_test_server().await;
        let cookie = register_test_user(&server, "alice").await;
        let mut handles = Vec::new();
        for _ in 0..8 {
            let base_url = server.base_url.clone();
            let cookie = cookie.clone();
            handles.push(tokio::spawn(async move {
                request(
                    &base_url,
                    "GET",
                    "/home",
                    &[("cookie", &cookie)],
                    Vec::new(),
                )
                .await
            }));
        }
        for handle in handles {
            let response = handle.await.expect("task");
            assert_eq!(response.status, 200);
            assert!(
                response.body.contains(r#"name="csrf""#),
                "concurrent page load rendered a form without a CSRF token"
            );
        }
    }

    #[tokio::test]
    async fn oversized_image_headers_are_rejected_before_conversion() {
        let server = spawn_test_server().await;
        let cookie = register_test_user(&server, "alice").await;
        let home = get_with_cookie(&server, "/home", &cookie).await;
        let csrf = csrf_token(&home.body);

        let mut oversized = tiny_png_bytes();
        oversized[16..20].copy_from_slice(&40_000_u32.to_be_bytes());
        oversized[20..24].copy_from_slice(&40_000_u32.to_be_bytes());
        let body = multipart_body_with_file(
            "post-boundary",
            &[("csrf", csrf.as_str()), ("text", "huge image")],
            "media",
            "huge.png",
            "image/png",
            &oversized,
        );
        let response = request(
            &server.base_url,
            "POST",
            "/posts",
            &[
                ("cookie", cookie.as_str()),
                (
                    "content-type",
                    "multipart/form-data; boundary=post-boundary",
                ),
            ],
            body,
        )
        .await;
        assert_eq!(response.status, 400);
        assert!(response.body.contains("dimensions"));
        let media = media_row_count(&server).await;
        assert_eq!(media, 0, "rejected image must not leave a media row");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_post_submissions_each_create_exactly_one_post() {
        let mut settings = Settings::default();
        settings.moderation.posts_per_minute = 50;
        let server = spawn_test_server_with_settings(settings).await;
        let cookie = register_test_user(&server, "alice").await;
        let home = get_with_cookie(&server, "/home", &cookie).await;
        let csrf = csrf_token(&home.body);

        let mut handles = Vec::new();
        for index in 0..10 {
            let base_url = server.base_url.clone();
            let cookie = cookie.clone();
            let csrf = csrf.clone();
            handles.push(tokio::spawn(async move {
                request(
                    &base_url,
                    "POST",
                    "/posts",
                    &[
                        ("cookie", cookie.as_str()),
                        (
                            "content-type",
                            "multipart/form-data; boundary=post-boundary",
                        ),
                    ],
                    multipart_body(
                        "post-boundary",
                        &[
                            ("csrf", csrf.as_str()),
                            ("text", &format!("concurrent submission {index}")),
                        ],
                        false,
                    ),
                )
                .await
            }));
        }
        for handle in handles {
            assert_eq!(handle.await.expect("task").status, 303);
        }

        let (count, distinct): (i64, i64) = server
            .pool
            .call(|conn| {
                Ok((
                    conn.query_row(
                        "SELECT COUNT(*) FROM posts WHERE is_deleted = 0",
                        [],
                        |row| row.get(0),
                    )?,
                    conn.query_row(
                        "SELECT COUNT(DISTINCT text) FROM posts WHERE is_deleted = 0",
                        [],
                        |row| row.get(0),
                    )?,
                ))
            })
            .await
            .expect("post stats");
        assert_eq!(count, 10);
        assert_eq!(distinct, 10);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_identical_media_uploads_keep_files_intact() {
        let mut settings = Settings::default();
        settings.moderation.posts_per_minute = 50;
        let server = spawn_test_server_with_settings(settings).await;
        let cookie = register_test_user(&server, "alice").await;
        let home = get_with_cookie(&server, "/home", &cookie).await;
        let csrf = csrf_token(&home.body);

        let mut handles = Vec::new();
        for index in 0..6 {
            let base_url = server.base_url.clone();
            let cookie = cookie.clone();
            let csrf = csrf.clone();
            handles.push(tokio::spawn(async move {
                let body = multipart_body_with_file(
                    "media-boundary",
                    &[
                        ("csrf", csrf.as_str()),
                        ("text", &format!("concurrent photo {index}")),
                    ],
                    "media",
                    "same.png",
                    "image/png",
                    &tiny_png_bytes(),
                );
                request(
                    &base_url,
                    "POST",
                    "/posts",
                    &[
                        ("cookie", cookie.as_str()),
                        (
                            "content-type",
                            "multipart/form-data; boundary=media-boundary",
                        ),
                    ],
                    body,
                )
                .await
            }));
        }
        for handle in handles {
            let response = handle.await.expect("task");
            assert_eq!(response.status, 303);
        }

        let rows: Vec<(String, Option<i64>)> = server
            .pool
            .call(|conn| {
                let mut stmt = conn.prepare("SELECT stored_path, canonical_media_id FROM media")?;
                let rows = stmt
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
            .expect("media rows");
        assert_eq!(rows.len(), 6);
        assert_eq!(
            rows.iter()
                .filter(|(_, canonical)| canonical.is_none())
                .count(),
            1,
            "identical uploads must share one canonical row"
        );
        let mut distinct_paths = std::collections::BTreeSet::new();
        for (stored_path, _canonical) in &rows {
            let path = std::path::Path::new(stored_path);
            assert!(path.is_file(), "missing media file {stored_path}");
            assert_eq!(
                std::fs::read(path).expect("read media"),
                tiny_png_bytes(),
                "media file content changed under concurrent uploads"
            );
            distinct_paths.insert(stored_path.clone());
        }
        assert_eq!(distinct_paths.len(), 1);
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

    fn tiny_mp4_bytes() -> Vec<u8> {
        let mut bytes = vec![
            0x00, 0x00, 0x00, 0x18, b'f', b't', b'y', b'p', b'i', b's', b'o', b'm', 0x00, 0x00,
            0x00, 0x00, b'i', b's', b'o', b'm', b'm', b'p', b'4', b'2',
        ];
        bytes.extend_from_slice(b"rustpost-test-video");
        bytes
    }

    async fn media_row_count(server: &TestServer) -> i64 {
        server
            .pool
            .call(|conn| Ok(conn.query_row("SELECT COUNT(*) FROM media", [], |row| row.get(0))?))
            .await
            .expect("media count")
    }

    async fn pinned_post_id_for_user(server: &TestServer, username: &str) -> Option<i64> {
        let username = username.to_owned();
        server
            .pool
            .call(move |conn| {
                conn.query_row(
                    "SELECT pinned_post_id FROM users WHERE normalized_username = ?",
                    [username],
                    |row| row.get(0),
                )
                .map_err(Into::into)
            })
            .await
            .expect("pinned post")
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

    async fn liked_profile_fixture() -> (TestServer, String, String) {
        let server = spawn_test_server().await;
        let alice_cookie = register_test_user(&server, "alice").await;
        create_text_post(&server, &alice_cookie, "privacy target post").await;
        let bob_cookie = register_test_user(&server, "bob").await;
        create_text_post(&server, &bob_cookie, "bob fallback post").await;
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
        (server, alice_cookie, bob_cookie)
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

    async fn save_profile_settings(
        server: &TestServer,
        cookie: &str,
        fields: &[(&str, &str)],
    ) -> TestResponse {
        let settings = get_with_cookie(server, "/settings", cookie).await;
        let csrf = csrf_token(&settings.body);
        let mut all_fields = Vec::with_capacity(fields.len() + 1);
        all_fields.push(("csrf", csrf.as_str()));
        all_fields.extend_from_slice(fields);
        request(
            &server.base_url,
            "POST",
            "/settings",
            &[
                ("cookie", cookie),
                (
                    "content-type",
                    "multipart/form-data; boundary=settings-boundary",
                ),
            ],
            multipart_body("settings-boundary", &all_fields, false),
        )
        .await
    }

    async fn user_settings_state(
        server: &TestServer,
        username: &str,
    ) -> (String, i64, i64, String, String, String, String) {
        let username = username.to_owned();
        server
            .pool
            .call(move |conn| {
                Ok(conn.query_row(
                    r#"
                    SELECT theme, nsfw_blur_enabled, liked_posts_public,
                      display_name, bio, location, website
                    FROM users WHERE normalized_username = ?
                    "#,
                    [username],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                            row.get(6)?,
                        ))
                    },
                )?)
            })
            .await
            .expect("user settings state")
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

    async fn set_post_created_seconds_ago(server: &TestServer, post_id: i64, seconds: i64) {
        let modifier = format!("-{seconds} seconds");
        server
            .pool
            .call(move |conn| {
                conn.execute(
                    "UPDATE posts SET created_at = datetime('now', ?), edited_at = NULL WHERE id = ?",
                    params![modifier, post_id],
                )?;
                Ok(())
            })
            .await
            .expect("set post age");
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
        assert!(body.contains(r#"data-card-form="notification-open-"#));
        assert!(body.contains(r#"name="return_to" value="/posts/1""#));
        assert!(body.contains(r#"name="return_to" value="/posts/2""#));
        assert!(body.contains(r#"name="return_to" value="/users/bob""#));
    }

    fn assert_empty_state(body: &str, title: &str, message: &str) {
        assert!(body.contains(r#"class="empty-state" data-testid="empty-state""#));
        assert!(body.contains(&format!("<h2>{title}</h2>")));
        if message.is_empty() {
            assert!(!body.contains(r#"<section class="empty-state" data-testid="empty-state"><h2></h2><p></p></section>"#));
        } else {
            assert!(body.contains(&format!("<p>{message}</p>")));
        }
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

    /// A server configured for immediate account deletion after password
    /// confirmation, matching the pre-grace-period behavior.
    async fn spawn_test_server_without_deletion_grace() -> TestServer {
        let mut settings = Settings::default();
        settings.accounts.deletion_grace_period_days = 0;
        spawn_test_server_with_settings(settings).await
    }

    async fn spawn_test_server_inner(create_admin: bool, settings: Settings) -> TestServer {
        let temp = tempfile::tempdir().expect("temp dir");
        let paths = RuntimePaths::from_data_dir(temp.path().to_path_buf())
            .with_tor_data_dir(&settings.tor.data_dir)
            .with_backup_dir(&settings.backup.backup_dir);
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

    fn deep_settings_form_body(
        server: &TestServer,
        csrf: &str,
        intent: &str,
        overrides: &[(&str, &str)],
    ) -> Vec<u8> {
        use sha2::{Digest as _, Sha256};
        let raw = std::fs::read(server.data_dir.join("settings.toml")).expect("settings");
        let revision = Sha256::digest(&raw)
            .iter()
            .fold(String::new(), |mut output, byte| {
                let _ = write!(output, "{byte:02x}");
                output
            });
        let values = crate::admin::DeepSettingsValues::from_settings(&Settings::default());
        let mut pairs = vec![
            ("csrf".to_owned(), csrf.to_owned()),
            ("intent".to_owned(), intent.to_owned()),
            ("revision".to_owned(), revision),
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

    async fn protected_account_fixture() -> (TestServer, String, String, i64) {
        let server = spawn_test_server().await;
        let alice_cookie = register_test_user(&server, "alice").await;
        let bob_cookie = register_test_user(&server, "bob").await;
        let alice_id = user_id_for(&server, "alice").await;
        let saved = save_profile_settings(
            &server,
            &alice_cookie,
            &[("follow_approval_required", "true")],
        )
        .await;
        assert_eq!(saved.status, 303);
        (server, alice_cookie, bob_cookie, alice_id)
    }

    async fn post_multipart_with_cookie(
        server: &TestServer,
        path: &str,
        cookie: &str,
        boundary: &str,
        fields: &[(&str, &str)],
    ) -> TestResponse {
        let content_type = format!("multipart/form-data; boundary={boundary}");
        request(
            &server.base_url,
            "POST",
            path,
            &[("cookie", cookie), ("content-type", &content_type)],
            multipart_body(boundary, fields, false),
        )
        .await
    }

    async fn count_rows(server: &TestServer, sql: &'static str) -> i64 {
        server
            .pool
            .call(move |conn| Ok(conn.query_row(sql, [], |row| row.get::<_, i64>(0))?))
            .await
            .expect("row count")
    }

    #[tokio::test]
    async fn protected_account_follow_requests_need_approval_before_following() {
        let (server, alice_cookie, bob_cookie, alice_id) = protected_account_fixture().await;
        let bob_id = user_id_for(&server, "bob").await;

        let before = get_with_cookie(&server, "/users/alice", &bob_cookie).await;
        let csrf = csrf_token(&before.body);
        let requested = post_form_with_cookie(
            &server,
            &format!("/users/{alice_id}/follow"),
            &bob_cookie,
            &format!("csrf={csrf}"),
        )
        .await;
        assert_eq!(requested.status, 303);

        assert_eq!(count_rows(&server, "SELECT COUNT(*) FROM follows").await, 0);
        assert_eq!(
            count_rows(&server, "SELECT COUNT(*) FROM follow_requests").await,
            1
        );
        let profile = get_with_cookie(&server, "/users/alice", &bob_cookie).await;
        assert!(profile.body.contains(">Requested</button>"));
        assert!(profile.body.contains(&format!(
            r#"data-profile-followers="{alice_id}">0 followers"#
        )));

        let requests = get_with_cookie(&server, "/follow-requests", &alice_cookie).await;
        assert_eq!(requests.status, 200);
        assert!(requests.body.contains("@bob"));
        assert!(
            requests
                .body
                .contains(&format!("/users/{bob_id}/follow/approve"))
        );
        assert!(
            requests
                .body
                .contains(&format!("/users/{bob_id}/follow/reject"))
        );
        let notifications = get_with_cookie(&server, "/notifications", &alice_cookie).await;
        assert!(notifications.body.contains("requested to follow you"));

        let csrf = csrf_token(&requests.body);
        let approved = post_form_with_cookie(
            &server,
            &format!("/users/{bob_id}/follow/approve"),
            &alice_cookie,
            &format!("csrf={csrf}"),
        )
        .await;
        assert_eq!(approved.status, 303);
        assert_eq!(count_rows(&server, "SELECT COUNT(*) FROM follows").await, 1);
        assert_eq!(
            count_rows(&server, "SELECT COUNT(*) FROM follow_requests").await,
            0
        );
        let bob_notifications = get_with_cookie(&server, "/notifications", &bob_cookie).await;
        assert!(
            bob_notifications
                .body
                .contains("approved your follow request")
        );
        let profile = get_with_cookie(&server, "/users/alice", &bob_cookie).await;
        assert!(profile.body.contains(">Following</button>"));
    }

    #[tokio::test]
    async fn follow_requests_can_be_rejected_cancelled_and_blocks_clear_them() {
        let (server, alice_cookie, bob_cookie, alice_id) = protected_account_fixture().await;
        let bob_id = user_id_for(&server, "bob").await;

        // Cancel a pending request from the requester's own view.
        let before = get_with_cookie(&server, "/users/alice", &bob_cookie).await;
        let csrf = csrf_token(&before.body);
        post_form_with_cookie(
            &server,
            &format!("/users/{alice_id}/follow"),
            &bob_cookie,
            &format!("csrf={csrf}"),
        )
        .await;
        let sent = get_with_cookie(&server, "/follow-requests", &bob_cookie).await;
        assert!(sent.body.contains("@alice"));
        let csrf = csrf_token(&sent.body);
        let cancelled = post_form_with_cookie(
            &server,
            &format!("/users/{alice_id}/follow/cancel"),
            &bob_cookie,
            &format!("csrf={csrf}"),
        )
        .await;
        assert_eq!(cancelled.status, 303);
        assert_eq!(
            count_rows(&server, "SELECT COUNT(*) FROM follow_requests").await,
            0
        );

        // Reject a fresh request.
        let before = get_with_cookie(&server, "/users/alice", &bob_cookie).await;
        let csrf = csrf_token(&before.body);
        post_form_with_cookie(
            &server,
            &format!("/users/{alice_id}/follow"),
            &bob_cookie,
            &format!("csrf={csrf}"),
        )
        .await;
        let requests = get_with_cookie(&server, "/follow-requests", &alice_cookie).await;
        let csrf = csrf_token(&requests.body);
        let rejected = post_form_with_cookie(
            &server,
            &format!("/users/{bob_id}/follow/reject"),
            &alice_cookie,
            &format!("csrf={csrf}"),
        )
        .await;
        assert_eq!(rejected.status, 303);
        assert_eq!(
            count_rows(&server, "SELECT COUNT(*) FROM follow_requests").await,
            0
        );
        assert_eq!(count_rows(&server, "SELECT COUNT(*) FROM follows").await, 0);

        // A pending request disappears when the target blocks the requester.
        let before = get_with_cookie(&server, "/users/alice", &bob_cookie).await;
        let csrf = csrf_token(&before.body);
        post_form_with_cookie(
            &server,
            &format!("/users/{alice_id}/follow"),
            &bob_cookie,
            &format!("csrf={csrf}"),
        )
        .await;
        let profile = get_with_cookie(&server, "/users/bob", &alice_cookie).await;
        let csrf = csrf_token(&profile.body);
        let blocked = post_form_with_cookie(
            &server,
            &format!("/users/{bob_id}/block"),
            &alice_cookie,
            &format!("csrf={csrf}"),
        )
        .await;
        assert_eq!(blocked.status, 303);
        assert_eq!(
            count_rows(&server, "SELECT COUNT(*) FROM follow_requests").await,
            0
        );
        let blocked_request = get_with_cookie(&server, "/users/alice", &bob_cookie).await;
        let csrf = csrf_token(&blocked_request.body);
        let refused = post_form_with_cookie(
            &server,
            &format!("/users/{alice_id}/follow"),
            &bob_cookie,
            &format!("csrf={csrf}"),
        )
        .await;
        assert_eq!(refused.status, 400);
    }

    #[tokio::test]
    async fn disabling_follow_approval_keeps_pending_requests_pending() {
        let (server, alice_cookie, bob_cookie, alice_id) = protected_account_fixture().await;
        let before = get_with_cookie(&server, "/users/alice", &bob_cookie).await;
        let csrf = csrf_token(&before.body);
        post_form_with_cookie(
            &server,
            &format!("/users/{alice_id}/follow"),
            &bob_cookie,
            &format!("csrf={csrf}"),
        )
        .await;

        let saved =
            save_profile_settings(&server, &alice_cookie, &[("display_name", "Alice")]).await;
        assert_eq!(saved.status, 303);
        assert_eq!(
            count_rows(&server, "SELECT COUNT(*) FROM follow_requests").await,
            1
        );

        let carol_cookie = register_test_user(&server, "carol").await;
        let carol_view = get_with_cookie(&server, "/users/alice", &carol_cookie).await;
        let csrf = csrf_token(&carol_view.body);
        let followed = post_form_with_cookie(
            &server,
            &format!("/users/{alice_id}/follow"),
            &carol_cookie,
            &format!("csrf={csrf}"),
        )
        .await;
        assert_eq!(followed.status, 303);
        assert_eq!(count_rows(&server, "SELECT COUNT(*) FROM follows").await, 1);
        assert_eq!(
            count_rows(&server, "SELECT COUNT(*) FROM follow_requests").await,
            1
        );
    }

    #[tokio::test]
    async fn admin_announcement_renders_next_to_site_name_and_escapes_text() {
        let server = spawn_test_server_with_admin().await;
        let admin_cookie = admin_session_cookie(&server).await;
        let dashboard = get_with_cookie(&server, "/admin", &admin_cookie).await;
        let csrf = csrf_token(&dashboard.body);
        let updated = post_form_with_cookie(
            &server,
            "/admin/announcement",
            &admin_cookie,
            &format!(
                "csrf={}&announcement={}&enabled=true&intent=save",
                form_encode(&csrf),
                form_encode("<b>Window</b> at 20:00")
            ),
        )
        .await;
        assert_eq!(updated.status, 303);

        let home = request(&server.base_url, "GET", "/home", &[], Vec::new()).await;
        assert_eq!(home.status, 200);
        assert!(home.body.contains(r#"data-testid="announcement""#));
        assert!(home.body.contains("&lt;b&gt;Window&lt;/b&gt; at 20:00"));
        assert!(!home.body.contains("<b>Window</b> at 20:00"));

        let member_cookie = register_test_user(&server, "member").await;
        let member_home = get_with_cookie(&server, "/home", &member_cookie).await;
        let member_csrf = csrf_token(&member_home.body);
        let forbidden = post_form_with_cookie(
            &server,
            "/admin/announcement",
            &member_cookie,
            &format!(
                "csrf={}&announcement=hi&enabled=true&intent=save",
                form_encode(&member_csrf)
            ),
        )
        .await;
        assert_eq!(forbidden.status, 403);

        let dashboard = get_with_cookie(&server, "/admin", &admin_cookie).await;
        let csrf = csrf_token(&dashboard.body);
        let cleared = post_form_with_cookie(
            &server,
            "/admin/announcement",
            &admin_cookie,
            &format!("csrf={}&intent=clear", form_encode(&csrf)),
        )
        .await;
        assert_eq!(cleared.status, 303);
        let home = request(&server.base_url, "GET", "/home", &[], Vec::new()).await;
        assert!(!home.body.contains(r#"data-testid="announcement""#));

        let dashboard = get_with_cookie(&server, "/admin", &admin_cookie).await;
        let csrf = csrf_token(&dashboard.body);
        let too_long = post_form_with_cookie(
            &server,
            "/admin/announcement",
            &admin_cookie,
            &format!(
                "csrf={}&announcement={}&enabled=true&intent=save",
                form_encode(&csrf),
                form_encode(&"x".repeat(281))
            ),
        )
        .await;
        assert_eq!(too_long.status, 400);
        assert!(too_long.body.contains("announcement is too long"));
    }

    #[tokio::test]
    async fn maintenance_mode_blocks_registration_and_posting_but_stays_readable() {
        let server = spawn_test_server_with_admin().await;
        let member_cookie = register_test_user(&server, "member").await;
        create_text_post(&server, &member_cookie, "before maintenance").await;
        let admin_cookie = admin_session_cookie(&server).await;
        let dashboard = get_with_cookie(&server, "/admin", &admin_cookie).await;
        let csrf = csrf_token(&dashboard.body);
        let enabled = post_form_with_cookie(
            &server,
            "/admin/maintenance",
            &admin_cookie,
            &format!(
                "csrf={}&enabled=true&message={}",
                form_encode(&csrf),
                form_encode("Back at noon")
            ),
        )
        .await;
        assert_eq!(enabled.status, 303);

        let home = get_with_cookie(&server, "/home", &member_cookie).await;
        assert_eq!(home.status, 200);
        assert!(home.body.contains(r#"data-testid="maintenance-notice""#));
        assert!(home.body.contains("Back at noon"));
        assert!(!home.body.contains(r#"id="post-text""#));

        let csrf = csrf_token(&home.body);
        let blocked = post_multipart_with_cookie(
            &server,
            "/posts",
            &member_cookie,
            "maintenance-post",
            &[("csrf", csrf.as_str()), ("text", "should not publish")],
        )
        .await;
        assert_eq!(blocked.status, 503);
        assert_eq!(
            count_rows(
                &server,
                "SELECT COUNT(*) FROM posts WHERE text = 'should not publish'"
            )
            .await,
            0
        );

        let register = request(
            &server.base_url,
            "POST",
            "/register",
            &[("content-type", "application/x-www-form-urlencoded")],
            b"username=late&password=very%20secure%20password&confirm_password=very%20secure%20password"
                .to_vec(),
        )
        .await;
        assert_eq!(register.status, 503);
        let register_page = request(&server.base_url, "GET", "/register", &[], Vec::new()).await;
        assert_eq!(register_page.status, 200);
        assert!(
            register_page
                .body
                .contains("Registration is currently closed.")
        );

        // Administrators keep posting so they can verify the instance.
        let admin_home = get_with_cookie(&server, "/home", &admin_cookie).await;
        assert!(admin_home.body.contains(r#"id="post-text""#));
        let admin_csrf = csrf_token(&admin_home.body);
        let admin_post = post_multipart_with_cookie(
            &server,
            "/posts",
            &admin_cookie,
            "maintenance-admin-post",
            &[
                ("csrf", admin_csrf.as_str()),
                ("text", "admin verification post"),
            ],
        )
        .await;
        assert_eq!(admin_post.status, 303);

        let dashboard = get_with_cookie(&server, "/admin", &admin_cookie).await;
        let csrf = csrf_token(&dashboard.body);
        let disabled = post_form_with_cookie(
            &server,
            "/admin/maintenance",
            &admin_cookie,
            &format!("csrf={}&intent=save", form_encode(&csrf)),
        )
        .await;
        assert_eq!(disabled.status, 303);
        create_text_post(&server, &member_cookie, "after maintenance").await;
    }

    #[tokio::test]
    async fn admin_forced_password_reset_restricts_account_until_password_changes() {
        let server = spawn_test_server_with_admin().await;
        let member_cookie = register_test_user(&server, "member").await;
        create_text_post(&server, &member_cookie, "before forced reset").await;
        let member_id = user_id_for(&server, "member").await;
        let admin_cookie = admin_session_cookie(&server).await;
        let users_page = get_with_cookie(&server, "/admin/users", &admin_cookie).await;
        let csrf = csrf_token(&users_page.body);
        let reset = post_form_with_cookie(
            &server,
            &format!("/admin/users/{member_id}/require-password-reset"),
            &admin_cookie,
            &format!("csrf={csrf}"),
        )
        .await;
        assert_eq!(reset.status, 303);
        assert_eq!(
            count_rows(
                &server,
                "SELECT COUNT(*) FROM admin_audit_log WHERE action = 'require_password_reset'"
            )
            .await,
            1
        );

        let home = get_with_cookie(&server, "/home", &member_cookie).await;
        assert_eq!(home.status, 303);
        assert!(location(&home).starts_with("/settings/password"));

        let page = get_with_cookie(&server, "/settings/password?required=1", &member_cookie).await;
        assert_eq!(page.status, 200);
        assert!(page.body.contains("You must change your password"));
        assert!(!page.body.contains(r#"id="profile-settings-form""#));

        let csrf = csrf_token(&page.body);
        let blocked_post = post_multipart_with_cookie(
            &server,
            "/posts",
            &member_cookie,
            "reset-post",
            &[("csrf", csrf.as_str()), ("text", "blocked by reset")],
        )
        .await;
        assert_eq!(blocked_post.status, 303);
        assert_eq!(
            count_rows(
                &server,
                "SELECT COUNT(*) FROM posts WHERE text = 'blocked by reset'"
            )
            .await,
            0
        );

        let changed = post_form_with_cookie(
            &server,
            "/settings/password",
            &member_cookie,
            &format!(
                "csrf={}&current_password=very%20secure%20password&new_password=brand%20new%20password&confirm_new_password=brand%20new%20password",
                form_encode(&csrf)
            ),
        )
        .await;
        assert_eq!(changed.status, 303);
        let home = get_with_cookie(&server, "/home", &member_cookie).await;
        assert_eq!(home.status, 200);
        create_text_post(&server, &member_cookie, "after password reset").await;

        let old_login = request(
            &server.base_url,
            "POST",
            "/login",
            &[("content-type", "application/x-www-form-urlencoded")],
            b"username=member&password=very%20secure%20password".to_vec(),
        )
        .await;
        assert_eq!(old_login.status, 401);
        let new_login = request(
            &server.base_url,
            "POST",
            "/login",
            &[("content-type", "application/x-www-form-urlencoded")],
            b"username=member&password=brand%20new%20password".to_vec(),
        )
        .await;
        assert_eq!(new_login.status, 303);
    }

    #[tokio::test]
    async fn admin_forced_logout_revokes_only_that_accounts_sessions() {
        let server = spawn_test_server_with_admin().await;
        let member_cookie = register_test_user(&server, "member").await;
        let second_login = request(
            &server.base_url,
            "POST",
            "/login",
            &[("content-type", "application/x-www-form-urlencoded")],
            b"username=member&password=very%20secure%20password".to_vec(),
        )
        .await;
        assert_eq!(second_login.status, 303);
        let second_cookie = session_cookie(&second_login);
        let bystander_cookie = register_test_user(&server, "bystander").await;
        let member_id = user_id_for(&server, "member").await;
        let admin_cookie = admin_session_cookie(&server).await;

        for _ in 0..2 {
            let users_page = get_with_cookie(&server, "/admin/users", &admin_cookie).await;
            let csrf = csrf_token(&users_page.body);
            let revoked = post_form_with_cookie(
                &server,
                &format!("/admin/users/{member_id}/revoke-sessions"),
                &admin_cookie,
                &format!("csrf={csrf}"),
            )
            .await;
            assert_eq!(revoked.status, 303);
        }

        for cookie in [&member_cookie, &second_cookie] {
            let settings = get_with_cookie(&server, "/settings", cookie).await;
            assert_eq!(settings.status, 401);
        }
        let bystander = get_with_cookie(&server, "/settings", &bystander_cookie).await;
        assert_eq!(bystander.status, 200);
        let login_again = request(
            &server.base_url,
            "POST",
            "/login",
            &[("content-type", "application/x-www-form-urlencoded")],
            b"username=member&password=very%20secure%20password".to_vec(),
        )
        .await;
        assert_eq!(login_again.status, 303);
    }

    #[tokio::test]
    async fn forced_logout_does_not_clear_a_forced_password_reset() {
        let server = spawn_test_server_with_admin().await;
        let cookie = register_test_user(&server, "member").await;
        let member_id = user_id_for(&server, "member").await;
        let admin_cookie = admin_session_cookie(&server).await;

        let users_page = get_with_cookie(&server, "/admin/users", &admin_cookie).await;
        let csrf = csrf_token(&users_page.body);
        let reset = post_form_with_cookie(
            &server,
            &format!("/admin/users/{member_id}/require-password-reset"),
            &admin_cookie,
            &format!("csrf={csrf}"),
        )
        .await;
        assert_eq!(reset.status, 303);

        let users_page = get_with_cookie(&server, "/admin/users", &admin_cookie).await;
        let csrf = csrf_token(&users_page.body);
        let revoked = post_form_with_cookie(
            &server,
            &format!("/admin/users/{member_id}/revoke-sessions"),
            &admin_cookie,
            &format!("csrf={csrf}"),
        )
        .await;
        assert_eq!(revoked.status, 303);
        let old_session = get_with_cookie(&server, "/settings", &cookie).await;
        assert_eq!(old_session.status, 401);

        let login = request(
            &server.base_url,
            "POST",
            "/login",
            &[("content-type", "application/x-www-form-urlencoded")],
            b"username=member&password=very%20secure%20password".to_vec(),
        )
        .await;
        assert_eq!(login.status, 303);
        let fresh_cookie = session_cookie(&login);
        let home = get_with_cookie(&server, "/home", &fresh_cookie).await;
        assert_eq!(home.status, 303);
        assert!(location(&home).starts_with("/settings/password"));
    }

    #[tokio::test]
    async fn deletion_grace_period_blocks_writes_and_can_be_cancelled() {
        let server = spawn_test_server().await;
        let cookie = register_test_user(&server, "deleter").await;
        create_text_post(&server, &cookie, "post before deletion").await;

        let confirm_page = get_with_cookie(&server, "/settings/delete/confirm", &cookie).await;
        let csrf = csrf_token(&confirm_page.body);
        let intent = hidden_value(&confirm_page.body, "delete_intent");
        let requested = post_form_with_cookie(
            &server,
            "/settings/delete/confirm",
            &cookie,
            &format!("csrf={csrf}&delete_intent={intent}&password=very%20secure%20password"),
        )
        .await;
        assert_eq!(requested.status, 303);
        assert_eq!(location(&requested), "/settings?saved=delete-requested");
        assert_eq!(
            count_rows(
                &server,
                "SELECT COUNT(*) FROM users WHERE deletion_scheduled_at IS NOT NULL"
            )
            .await,
            1
        );

        let home = get_with_cookie(&server, "/home", &cookie).await;
        assert_eq!(home.status, 200);
        assert!(home.body.contains(r#"data-testid="account-notice""#));

        let csrf = csrf_token(&home.body);
        let blocked = post_multipart_with_cookie(
            &server,
            "/posts",
            &cookie,
            "deletion-post",
            &[("csrf", csrf.as_str()), ("text", "should not publish")],
        )
        .await;
        assert_eq!(blocked.status, 403);

        let settings = get_with_cookie(&server, "/settings", &cookie).await;
        assert!(settings.body.contains(r#"data-testid="deletion-deadline""#));
        let csrf = csrf_token(&settings.body);
        let cancelled = post_form_with_cookie(
            &server,
            "/settings/delete/cancel",
            &cookie,
            &format!("csrf={csrf}"),
        )
        .await;
        assert_eq!(cancelled.status, 303);
        assert_eq!(
            count_rows(
                &server,
                "SELECT COUNT(*) FROM users WHERE deletion_scheduled_at IS NULL"
            )
            .await,
            1
        );
        create_text_post(&server, &cookie, "post after cancellation").await;
    }

    #[tokio::test]
    async fn matched_deletion_deadlines_finalize_idempotently() {
        let server = spawn_test_server().await;
        let cookie = register_test_user(&server, "expiring").await;
        create_text_post(&server, &cookie, "expiring post").await;
        let user_id = user_id_for(&server, "expiring").await;
        let paths = RuntimePaths::from_data_dir(server.data_dir.clone());
        server
            .pool
            .call(move |conn| {
                conn.execute(
                    "UPDATE users SET deletion_requested_at = datetime('now','-31 days'), deletion_scheduled_at = datetime('now','-1 day') WHERE id = ?",
                    [user_id],
                )?;
                Ok(())
            })
            .await
            .expect("schedule deletion");

        let removed = crate::account::finalize_due_deletions(&server.pool, &paths)
            .await
            .expect("finalize");
        assert_eq!(removed, 1);
        assert_eq!(
            count_rows(&server, "SELECT COUNT(*) FROM users WHERE id = 1").await,
            0
        );
        assert_eq!(count_rows(&server, "SELECT COUNT(*) FROM posts").await, 0);
        let session = get_with_cookie(&server, "/home", &cookie).await;
        assert_eq!(session.status, 200);
        assert!(!session.body.contains(r#"data-testid="account-notice""#));
        let removed_again = crate::account::finalize_due_deletions(&server.pool, &paths)
            .await
            .expect("finalize again");
        assert_eq!(removed_again, 0);
    }

    #[tokio::test]
    async fn zero_grace_period_deletes_immediately_after_confirmation() {
        let mut settings = Settings::default();
        settings.accounts.deletion_grace_period_days = 0;
        let server = spawn_test_server_with_settings(settings).await;
        let cookie = register_test_user(&server, "instant").await;
        create_text_post(&server, &cookie, "instant post").await;

        let confirm_page = get_with_cookie(&server, "/settings/delete/confirm", &cookie).await;
        let csrf = csrf_token(&confirm_page.body);
        let intent = hidden_value(&confirm_page.body, "delete_intent");
        let deleted = post_form_with_cookie(
            &server,
            "/settings/delete/confirm",
            &cookie,
            &format!("csrf={csrf}&delete_intent={intent}&password=very%20secure%20password"),
        )
        .await;
        assert_eq!(deleted.status, 303);
        assert_eq!(location(&deleted), "/account-deleted");
        assert_eq!(count_rows(&server, "SELECT COUNT(*) FROM users").await, 0);
    }

    #[tokio::test]
    async fn username_changes_keep_identity_and_reserve_previous_handles() {
        let server = spawn_test_server().await;
        let cookie = register_test_user(&server, "original").await;
        create_text_post(&server, &cookie, "post from original").await;

        let settings = get_with_cookie(&server, "/settings", &cookie).await;
        let csrf = csrf_token(&settings.body);
        let wrong_password = post_form_with_cookie(
            &server,
            "/settings/username",
            &cookie,
            &format!("csrf={csrf}&new_username=renamed&password=not%20the%20password"),
        )
        .await;
        assert_eq!(wrong_password.status, 401);

        let changed = post_form_with_cookie(
            &server,
            "/settings/username",
            &cookie,
            &format!(
                "csrf={csrf}&new_username={}&password=very%20secure%20password",
                form_encode("ReNamed")
            ),
        )
        .await;
        assert_eq!(changed.status, 303);
        assert_eq!(location(&changed), "/settings?saved=username");
        assert_eq!(
            count_rows(
                &server,
                "SELECT COUNT(*) FROM username_history WHERE username = 'original'"
            )
            .await,
            1
        );

        let profile = get_with_cookie(&server, "/users/renamed", &cookie).await;
        assert_eq!(profile.status, 200);
        assert!(profile.body.contains("Previously known as @original"));
        assert!(profile.body.contains("post from original"));

        let old_url = request(&server.base_url, "GET", "/users/original", &[], Vec::new()).await;
        assert_eq!(old_url.status, 200);
        assert!(old_url.body.contains("No account uses @original"));
        assert!(old_url.body.contains(r#"href="/users/ReNamed""#));

        let duplicate = request(
            &server.base_url,
            "POST",
            "/register",
            &[("content-type", "application/x-www-form-urlencoded")],
            b"username=original&password=very%20secure%20password&confirm_password=very%20secure%20password".to_vec(),
        )
        .await;
        assert_eq!(duplicate.status, 400);
        assert!(duplicate.body.contains("That username is already taken."));

        let taken = post_form_with_cookie(
            &server,
            "/settings/username",
            &cookie,
            &format!("csrf={csrf}&new_username=settings&password=very%20secure%20password"),
        )
        .await;
        assert_eq!(taken.status, 400);

        let revert = post_form_with_cookie(
            &server,
            "/settings/username",
            &cookie,
            &format!("csrf={csrf}&new_username=original&password=very%20secure%20password"),
        )
        .await;
        assert_eq!(revert.status, 303);
        let profile = get_with_cookie(&server, "/users/original", &cookie).await;
        assert!(profile.body.contains("Previously known as @ReNamed"));
        assert_eq!(
            count_rows(
                &server,
                "SELECT COUNT(*) FROM username_history WHERE normalized_username = 'original'"
            )
            .await,
            0
        );
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one end-to-end scenario: export from one disposable instance and import into another"
    )]
    #[tokio::test]
    async fn account_archives_round_trip_over_http_and_respect_protected_follows() {
        let source = spawn_test_server().await;
        let source_cookie = register_test_user(&source, "mover").await;
        create_text_post(&source, &source_cookie, "portable post body").await;
        let _protector_cookie = register_test_user(&source, "protector").await;
        let protector_id = user_id_for(&source, "protector").await;
        let protector_view = get_with_cookie(&source, "/users/protector", &source_cookie).await;
        let csrf = csrf_token(&protector_view.body);
        let followed = post_form_with_cookie(
            &source,
            &format!("/users/{protector_id}/follow"),
            &source_cookie,
            &format!("csrf={csrf}"),
        )
        .await;
        assert_eq!(followed.status, 303);

        let export = get_with_cookie(&source, "/settings/export", &source_cookie).await;
        assert_eq!(export.status, 200);
        assert!(
            header_value(&export, "content-disposition")
                .expect("disposition")
                .contains("attachment")
        );
        assert_eq!(&export.body_bytes[..2], &[0x1f, 0x8b]);

        let destination = spawn_test_server().await;
        let receiver_cookie = register_test_user(&destination, "receiver").await;
        let protector_cookie = register_test_user(&destination, "protector").await;
        let saved = save_profile_settings(
            &destination,
            &protector_cookie,
            &[("follow_approval_required", "true")],
        )
        .await;
        assert_eq!(saved.status, 303);

        let import_page = get_with_cookie(&destination, "/settings/import", &receiver_cookie).await;
        assert!(import_page.body.contains(r#"name="archive""#));
        let csrf = csrf_token(&import_page.body);
        let body = multipart_body_with_file(
            "archive-boundary",
            &[("csrf", csrf.as_str())],
            "archive",
            "account.tar.gz",
            "application/gzip",
            &export.body_bytes,
        );
        let imported = request(
            &destination.base_url,
            "POST",
            "/settings/import",
            &[
                ("cookie", receiver_cookie.as_str()),
                (
                    "content-type",
                    "multipart/form-data; boundary=archive-boundary",
                ),
            ],
            body,
        )
        .await;
        assert_eq!(imported.status, 200);
        assert!(imported.body.contains("Import complete"));
        assert_eq!(
            count_rows(
                &destination,
                "SELECT COUNT(*) FROM posts WHERE text = 'portable post body'"
            )
            .await,
            1
        );
        assert_eq!(
            count_rows(&destination, "SELECT COUNT(*) FROM follow_requests").await,
            1,
            "protected accounts must receive a pending request instead of a follow"
        );
        assert_eq!(
            count_rows(&destination, "SELECT COUNT(*) FROM follows").await,
            0
        );

        let import_page = get_with_cookie(&destination, "/settings/import", &receiver_cookie).await;
        let csrf = csrf_token(&import_page.body);
        let body = multipart_body_with_file(
            "archive-boundary",
            &[("csrf", csrf.as_str())],
            "archive",
            "account.tar.gz",
            "application/gzip",
            &export.body_bytes,
        );
        let repeated = request(
            &destination.base_url,
            "POST",
            "/settings/import",
            &[
                ("cookie", receiver_cookie.as_str()),
                (
                    "content-type",
                    "multipart/form-data; boundary=archive-boundary",
                ),
            ],
            body,
        )
        .await;
        assert_eq!(repeated.status, 400);
        assert!(repeated.body.contains("already been imported"));
    }

    #[tokio::test]
    async fn renames_and_deletions_keep_follow_requests_consistent() {
        let (server, alice_cookie, bob_cookie, alice_id) = protected_account_fixture().await;
        let bob_id = user_id_for(&server, "bob").await;

        let before = get_with_cookie(&server, "/users/alice", &bob_cookie).await;
        let csrf = csrf_token(&before.body);
        post_form_with_cookie(
            &server,
            &format!("/users/{alice_id}/follow"),
            &bob_cookie,
            &format!("csrf={csrf}"),
        )
        .await;

        // Renaming the requester keeps the request addressed to the same account.
        let settings = get_with_cookie(&server, "/settings", &bob_cookie).await;
        let csrf = csrf_token(&settings.body);
        let renamed = post_form_with_cookie(
            &server,
            "/settings/username",
            &bob_cookie,
            &format!("csrf={csrf}&new_username=robert&password=very%20secure%20password"),
        )
        .await;
        assert_eq!(renamed.status, 303);
        let requests = get_with_cookie(&server, "/follow-requests", &alice_cookie).await;
        assert!(requests.body.contains("@robert"));
        assert!(!requests.body.contains("@bob"));
        let csrf = csrf_token(&requests.body);
        let approved = post_form_with_cookie(
            &server,
            &format!("/users/{bob_id}/follow/approve"),
            &alice_cookie,
            &format!("csrf={csrf}"),
        )
        .await;
        assert_eq!(approved.status, 303);
        assert_eq!(count_rows(&server, "SELECT COUNT(*) FROM follows").await, 1);

        // Deleting the requester removes their pending requests and history.
        server
            .pool
            .call(move |conn| {
                conn.execute(
                    "UPDATE users SET deletion_requested_at = datetime('now','-31 days'), deletion_scheduled_at = datetime('now','-1 day') WHERE id = ?",
                    [bob_id],
                )?;
                conn.execute(
                    "INSERT INTO follow_requests (requester_id, target_id) VALUES (?, ?)",
                    params![bob_id, alice_id],
                )?;
                Ok(())
            })
            .await
            .expect("schedule requester deletion");
        let paths = RuntimePaths::from_data_dir(server.data_dir.clone());
        let removed = crate::account::finalize_due_deletions(&server.pool, &paths)
            .await
            .expect("finalize");
        assert_eq!(removed, 1);
        assert_eq!(
            count_rows(&server, "SELECT COUNT(*) FROM follow_requests").await,
            0
        );
        assert_eq!(
            count_rows(
                &server,
                "SELECT COUNT(*) FROM username_history WHERE normalized_username IN ('bob', 'robert')"
            )
            .await,
            0
        );
        let reused = request(
            &server.base_url,
            "POST",
            "/register",
            &[("content-type", "application/x-www-form-urlencoded")],
            b"username=robert&password=very%20secure%20password&confirm_password=very%20secure%20password".to_vec(),
        )
        .await;
        assert_eq!(reused.status, 303);
    }

    #[tokio::test]
    async fn maintenance_mode_blocks_account_imports() {
        let server = spawn_test_server_with_admin().await;
        let member_cookie = register_test_user(&server, "member").await;
        let admin_cookie = admin_session_cookie(&server).await;
        let dashboard = get_with_cookie(&server, "/admin", &admin_cookie).await;
        let csrf = csrf_token(&dashboard.body);
        post_form_with_cookie(
            &server,
            "/admin/maintenance",
            &admin_cookie,
            &format!("csrf={}&enabled=true", form_encode(&csrf)),
        )
        .await;

        let page = get_with_cookie(&server, "/settings/import", &member_cookie).await;
        assert_eq!(page.status, 200);
        assert!(page.body.contains("maintenance mode"));

        let csrf = csrf_token(&page.body);
        let body = multipart_body_with_file(
            "import-blocked",
            &[("csrf", csrf.as_str())],
            "archive",
            "account.tar.gz",
            "application/gzip",
            &[0x1f, 0x8b, 0x08, 0x00],
        );
        let blocked = request(
            &server.base_url,
            "POST",
            "/settings/import",
            &[
                ("cookie", member_cookie.as_str()),
                (
                    "content-type",
                    "multipart/form-data; boundary=import-blocked",
                ),
            ],
            body,
        )
        .await;
        assert_eq!(blocked.status, 503);
    }

    async fn enable_maintenance(server: &TestServer, admin_cookie: &str, message: &str) {
        let dashboard = get_with_cookie(server, "/admin", admin_cookie).await;
        assert_eq!(dashboard.status, 200);
        let csrf = csrf_token(&dashboard.body);
        let body = format!(
            "csrf={}&enabled=true&message={}",
            form_encode(&csrf),
            form_encode(message)
        );
        let enabled =
            post_form_with_cookie(server, "/admin/maintenance", admin_cookie, &body).await;
        assert_eq!(enabled.status, 303);
    }

    async fn disable_maintenance(server: &TestServer, admin_cookie: &str) {
        let dashboard = get_with_cookie(server, "/admin", admin_cookie).await;
        let csrf = csrf_token(&dashboard.body);
        let body = format!("csrf={}&intent=save", form_encode(&csrf));
        let disabled =
            post_form_with_cookie(server, "/admin/maintenance", admin_cookie, &body).await;
        assert_eq!(disabled.status, 303);
    }

    async fn login_test_user(server: &TestServer, username: &str, password: &str) -> TestResponse {
        request(
            &server.base_url,
            "POST",
            "/login",
            &[("content-type", "application/x-www-form-urlencoded")],
            format!(
                "username={}&password={}",
                form_encode(username),
                form_encode(password)
            )
            .into_bytes(),
        )
        .await
    }

    async fn flag_password_reset(server: &TestServer, admin_cookie: &str, user_id: i64) {
        let users = get_with_cookie(server, "/admin/users", admin_cookie).await;
        let csrf = csrf_token(&users.body);
        let reset = post_form_with_cookie(
            server,
            &format!("/admin/users/{user_id}/require-password-reset"),
            admin_cookie,
            &format!("csrf={csrf}"),
        )
        .await;
        assert_eq!(reset.status, 303);
    }

    #[test]
    fn maintenance_policy_covers_every_publish_route() {
        use axum::http::Method;

        let cases = [
            (Method::GET, "/home", MaintenancePolicy::Allowed),
            (Method::GET, "/posts/1", MaintenancePolicy::Allowed),
            (Method::GET, "/settings/import", MaintenancePolicy::Allowed),
            (Method::POST, "/register", MaintenancePolicy::Blocked),
            (
                Method::POST,
                "/posts",
                MaintenancePolicy::BlockedUnlessAdmin,
            ),
            (
                Method::POST,
                "/settings/import",
                MaintenancePolicy::BlockedUnlessAdmin,
            ),
            (
                Method::POST,
                "/posts/1/quote",
                MaintenancePolicy::BlockedUnlessAdmin,
            ),
            (
                Method::POST,
                "/posts/1/repost",
                MaintenancePolicy::BlockedUnlessAdmin,
            ),
            (
                Method::POST,
                "/posts/1/edit",
                MaintenancePolicy::BlockedUnlessAdmin,
            ),
            (Method::POST, "/posts/1/delete", MaintenancePolicy::Allowed),
            (Method::POST, "/posts/1/like", MaintenancePolicy::Allowed),
            (
                Method::POST,
                "/posts/1/bookmark",
                MaintenancePolicy::Allowed,
            ),
            (Method::POST, "/posts/1/pin", MaintenancePolicy::Allowed),
            (Method::POST, "/posts/1/reply", MaintenancePolicy::Allowed),
            (Method::POST, "/users/1/follow", MaintenancePolicy::Allowed),
            (Method::POST, "/users/1/block", MaintenancePolicy::Allowed),
            (Method::POST, "/users/1/mute", MaintenancePolicy::Allowed),
            (Method::POST, "/settings", MaintenancePolicy::Allowed),
            (
                Method::POST,
                "/settings/password",
                MaintenancePolicy::Allowed,
            ),
            (
                Method::POST,
                "/settings/username",
                MaintenancePolicy::Allowed,
            ),
            (
                Method::POST,
                "/settings/delete/confirm",
                MaintenancePolicy::Allowed,
            ),
            (
                Method::POST,
                "/settings/delete/cancel",
                MaintenancePolicy::Allowed,
            ),
            (Method::POST, "/logout", MaintenancePolicy::Allowed),
            (
                Method::POST,
                "/notifications/read",
                MaintenancePolicy::Allowed,
            ),
            (
                Method::POST,
                "/notifications/open",
                MaintenancePolicy::Allowed,
            ),
            (
                Method::POST,
                "/admin/maintenance",
                MaintenancePolicy::Allowed,
            ),
            (
                Method::POST,
                "/admin/users/1/revoke-sessions",
                MaintenancePolicy::Allowed,
            ),
            (Method::POST, "/onboarding", MaintenancePolicy::Allowed),
        ];
        for (method, path, expected) in cases {
            assert_eq!(
                maintenance_policy(&method, path),
                expected,
                "unexpected policy for {method} {path}"
            );
        }
    }

    #[tokio::test]
    async fn maintenance_mode_matrix_blocks_publishing_and_keeps_account_operations() {
        let server = spawn_test_server_with_admin().await;
        let member_cookie = register_test_user(&server, "member").await;
        create_text_post(&server, &member_cookie, "member original post").await;
        let admin_cookie = admin_session_cookie(&server).await;
        create_text_post(&server, &admin_cookie, "admin original post").await;

        enable_maintenance(&server, &admin_cookie, "Matrix maintenance").await;

        // Publish mutations are refused for members and never touch the data.
        let home = get_with_cookie(&server, "/home", &member_cookie).await;
        let csrf = csrf_token(&home.body);
        for path in ["/posts/1/edit", "/posts/1/quote", "/posts/1/repost"] {
            let blocked = post_multipart_with_cookie(
                &server,
                path,
                &member_cookie,
                "maintenance-matrix",
                &[
                    ("csrf", csrf.as_str()),
                    ("text", "blocked during maintenance"),
                ],
            )
            .await;
            assert_eq!(blocked.status, 503, "{path} should be blocked");
            assert!(blocked.body.contains("Matrix maintenance"));
        }
        assert_eq!(
            count_rows(
                &server,
                "SELECT COUNT(*) FROM posts WHERE text = 'blocked during maintenance'"
            )
            .await,
            0
        );
        assert_eq!(
            count_rows(
                &server,
                "SELECT COUNT(*) FROM posts WHERE id = 1 AND text = 'member original post'"
            )
            .await,
            1,
            "the edit must not have changed the stored post"
        );

        // Account and security operations keep working for members.
        let saved =
            save_profile_settings(&server, &member_cookie, &[("display_name", "Member")]).await;
        assert_eq!(saved.status, 303);
        let settings = get_with_cookie(&server, "/settings", &member_cookie).await;
        let csrf = csrf_token(&settings.body);
        let renamed = post_form_with_cookie(
            &server,
            "/settings/username",
            &member_cookie,
            &format!("csrf={csrf}&new_username=member_renamed&password=very%20secure%20password"),
        )
        .await;
        assert_eq!(renamed.status, 303);
        let export = get_with_cookie(&server, "/settings/export", &member_cookie).await;
        assert_eq!(export.status, 200);
        let settings = get_with_cookie(&server, "/settings", &member_cookie).await;
        let csrf = csrf_token(&settings.body);
        let cancelled = post_form_with_cookie(
            &server,
            "/settings/delete/cancel",
            &member_cookie,
            &format!("csrf={csrf}"),
        )
        .await;
        assert_eq!(cancelled.status, 303);
        for path in ["/posts/1/like", "/posts/1/bookmark"] {
            let response =
                post_form_with_cookie(&server, path, &member_cookie, &format!("csrf={csrf}")).await;
            assert_eq!(response.status, 303, "{path} should stay available");
        }
        let reply_redirect = post_form_with_cookie(
            &server,
            "/posts/1/reply",
            &member_cookie,
            &format!("csrf={csrf}"),
        )
        .await;
        assert_eq!(reply_redirect.status, 303);

        // Administrators keep publishing so they can verify the instance.
        let admin_csrf = csrf_token(&get_with_cookie(&server, "/home", &admin_cookie).await.body);
        let admin_edit = post_form_with_cookie(
            &server,
            "/posts/2/edit",
            &admin_cookie,
            &format!(
                "csrf={admin_csrf}&text={}",
                form_encode("admin edit during maintenance")
            ),
        )
        .await;
        assert_eq!(admin_edit.status, 303);
        let admin_repost = post_form_with_cookie(
            &server,
            "/posts/1/repost",
            &admin_cookie,
            &format!("csrf={admin_csrf}"),
        )
        .await;
        assert_eq!(admin_repost.status, 303);

        disable_maintenance(&server, &admin_cookie).await;
        create_text_post(&server, &member_cookie, "member post after maintenance").await;
    }

    #[tokio::test]
    #[expect(
        clippy::too_many_lines,
        reason = "one table-driven forced-reset route matrix keeps every protected and exempt route visible"
    )]
    async fn forced_password_reset_blocks_protected_routes_and_keeps_exempt_flows() {
        let server = spawn_test_server_with_admin().await;
        let member_cookie = register_test_user(&server, "member").await;
        create_text_post(&server, &member_cookie, "before forced reset").await;
        let member_id = user_id_for(&server, "member").await;
        let second_cookie =
            session_cookie(&login_test_user(&server, "member", "very secure password").await);
        let stale_cookie =
            session_cookie(&login_test_user(&server, "member", "very secure password").await);
        let admin_cookie = admin_session_cookie(&server).await;
        flag_password_reset(&server, &admin_cookie, member_id).await;

        // Every protected GET route redirects to the password page.
        for path in [
            "/home",
            "/settings",
            "/notifications",
            "/following",
            "/bookmarks",
            "/mentions",
            "/search?q=reset",
            "/tags/reset",
            "/posts/1",
            "/users/member",
            "/follow-requests",
            "/settings/import",
            "/onboarding",
            "/admin",
            "/admin/users",
        ] {
            for cookie in [&member_cookie, &second_cookie] {
                let response = get_with_cookie(&server, path, cookie).await;
                assert_eq!(response.status, 303, "GET {path} should redirect");
                assert!(
                    location(&response).starts_with("/settings/password?required=1"),
                    "GET {path} redirected to {}",
                    location(&response)
                );
            }
        }

        // Protected state-changing routes are intercepted before their handlers.
        for path in [
            "/settings",
            "/settings/username",
            "/settings/muted-words",
            "/posts/1/edit",
            "/posts/1/delete",
            "/posts/1/like",
            "/posts/1/bookmark",
            "/posts/1/pin",
            "/posts/1/repost",
            "/posts/1/quote",
            "/users/1/follow",
            "/users/1/block",
            "/users/1/mute",
            "/notifications/read",
            "/notifications/open",
            "/settings/import",
        ] {
            let response = post_form_with_cookie(&server, path, &member_cookie, "csrf=bogus").await;
            assert_eq!(response.status, 303, "POST {path} should redirect");
            assert!(location(&response).starts_with("/settings/password?required=1"));
        }

        // Exempt account flows stay reachable.
        let password_page = get_with_cookie(&server, "/settings/password", &member_cookie).await;
        assert_eq!(password_page.status, 200);
        assert!(password_page.body.contains(r#"id="current_password""#));
        assert_eq!(
            get_with_cookie(&server, "/settings/delete", &member_cookie)
                .await
                .status,
            200
        );
        assert_eq!(
            get_with_cookie(&server, "/settings/delete/confirm", &member_cookie)
                .await
                .status,
            200
        );
        assert_eq!(
            get_with_cookie(&server, "/account-deleted", &member_cookie)
                .await
                .status,
            200
        );
        assert_eq!(
            get_with_cookie(&server, "/settings/export", &second_cookie)
                .await
                .status,
            200
        );
        assert_eq!(
            request(
                &server.base_url,
                "GET",
                "/assets/rustpost.js",
                &[],
                Vec::new()
            )
            .await
            .status,
            200
        );
        assert_eq!(
            request(&server.base_url, "GET", "/favicon.ico", &[], Vec::new())
                .await
                .status,
            200
        );

        // A stale cookie is treated as anonymous, not as a bypass.
        let stale = get_with_cookie(&server, "/home", "rustpost_session=bogus").await;
        assert_eq!(stale.status, 200);

        // Logout stays reachable; it revokes only the session it is sent with.
        let member_password_page =
            get_with_cookie(&server, "/settings/password", &member_cookie).await;
        let member_csrf = csrf_token(&member_password_page.body);
        let logout = post_form_with_cookie(
            &server,
            "/logout",
            &member_cookie,
            &format!("csrf={member_csrf}"),
        )
        .await;
        assert_eq!(logout.status, 303);

        // Changing the password clears the flag atomically and revokes the
        // other sessions; the session that changed the password survives.
        let page = get_with_cookie(&server, "/settings/password", &second_cookie).await;
        let csrf = csrf_token(&page.body);
        let changed = post_form_with_cookie(
            &server,
            "/settings/password",
            &second_cookie,
            &format!(
                "csrf={csrf}&current_password=very%20secure%20password&new_password=brand%20new%20password&confirm_new_password=brand%20new%20password"
            ),
        )
        .await;
        assert_eq!(changed.status, 303);
        assert_eq!(location(&changed), "/settings?saved=password");
        assert_eq!(
            count_rows(
                &server,
                "SELECT COUNT(*) FROM users WHERE must_change_password = 1"
            )
            .await,
            0
        );
        assert_eq!(
            get_with_cookie(&server, "/home", &second_cookie)
                .await
                .status,
            200
        );
        let third_cookie =
            session_cookie(&login_test_user(&server, "member", "brand new password").await);
        assert_eq!(
            get_with_cookie(&server, "/settings", &third_cookie)
                .await
                .status,
            200
        );
        // Sessions created before the password change are gone.
        let revoked = get_with_cookie(&server, "/settings", &stale_cookie).await;
        assert_eq!(revoked.status, 401);
    }

    #[tokio::test]
    async fn forced_logout_revokes_all_sessions_and_allows_relogin() {
        let server = spawn_test_server_with_admin().await;
        let first_cookie = register_test_user(&server, "victim").await;
        let second_cookie =
            session_cookie(&login_test_user(&server, "victim", "very secure password").await);
        let victim_id = user_id_for(&server, "victim").await;
        let other_cookie = register_test_user(&server, "bystander").await;

        assert_eq!(
            get_with_cookie(&server, "/settings", &second_cookie)
                .await
                .status,
            200
        );

        let admin_cookie = admin_session_cookie(&server).await;
        let users = get_with_cookie(&server, "/admin/users", &admin_cookie).await;
        let csrf = csrf_token(&users.body);
        let revoked = post_form_with_cookie(
            &server,
            &format!("/admin/users/{victim_id}/revoke-sessions"),
            &admin_cookie,
            &format!("csrf={csrf}"),
        )
        .await;
        assert_eq!(revoked.status, 303);

        for cookie in [&first_cookie, &second_cookie] {
            let response = get_with_cookie(&server, "/settings", cookie).await;
            assert_eq!(
                response.status, 401,
                "revoked session must not authenticate"
            );
        }
        assert_eq!(
            get_with_cookie(&server, "/settings", &other_cookie)
                .await
                .status,
            200,
            "other accounts keep their sessions"
        );
        let victim_sessions = {
            server
                .pool
                .call(move |conn| {
                    Ok(conn.query_row(
                        "SELECT COUNT(*) FROM sessions WHERE user_id = ? AND revoked_at IS NULL",
                        [victim_id],
                        |row| row.get::<_, i64>(0),
                    )?)
                })
                .await
                .expect("victim sessions")
        };
        assert_eq!(victim_sessions, 0);

        // The account can log in again and a repeated forced logout is safe.
        let third_cookie =
            session_cookie(&login_test_user(&server, "victim", "very secure password").await);
        assert_eq!(
            get_with_cookie(&server, "/settings", &third_cookie)
                .await
                .status,
            200
        );
        let users = get_with_cookie(&server, "/admin/users", &admin_cookie).await;
        let csrf = csrf_token(&users.body);
        let repeated = post_form_with_cookie(
            &server,
            &format!("/admin/users/{victim_id}/revoke-sessions"),
            &admin_cookie,
            &format!("csrf={csrf}"),
        )
        .await;
        assert_eq!(repeated.status, 303);
        assert_eq!(
            get_with_cookie(&server, "/settings", &third_cookie)
                .await
                .status,
            401
        );

        // Missing accounts are rejected without touching sessions.
        let missing = post_form_with_cookie(
            &server,
            "/admin/users/9999/revoke-sessions",
            &admin_cookie,
            &format!("csrf={csrf}"),
        )
        .await;
        assert_eq!(missing.status, 400);
    }

    #[tokio::test]
    #[expect(
        clippy::too_many_lines,
        reason = "one transition scenario walks public/protected toggles, pending requests, and follower counts"
    )]
    async fn protected_account_toggles_never_auto_approve_pending_requests() {
        let mut settings = Settings::default();
        settings.moderation.account_creations_per_ip_per_day = 20;
        let server = spawn_test_server_with_settings(settings).await;
        let alice_cookie = register_test_user(&server, "alice").await;
        let alice_id = user_id_for(&server, "alice").await;
        let saved = save_profile_settings(
            &server,
            &alice_cookie,
            &[("follow_approval_required", "true")],
        )
        .await;
        assert_eq!(saved.status, 303);
        let bob_cookie = register_test_user(&server, "bob").await;
        let bob_id = user_id_for(&server, "bob").await;
        let carol_cookie = register_test_user(&server, "carol").await;

        // Bob requests; the pending request is not a follower and shows a badge.
        let profile = get_with_cookie(&server, "/users/alice", &bob_cookie).await;
        let csrf = csrf_token(&profile.body);
        let requested = post_form_with_cookie(
            &server,
            &format!("/users/{alice_id}/follow"),
            &bob_cookie,
            &format!("csrf={csrf}"),
        )
        .await;
        assert_eq!(requested.status, 303);
        assert_eq!(count_rows(&server, "SELECT COUNT(*) FROM follows").await, 0);
        let alice_home = get_with_cookie(&server, "/home", &alice_cookie).await;
        assert!(
            alice_home
                .body
                .contains(r#"aria-label="1 pending follow requests""#),
            "the pending request must show in the navigation badge"
        );
        let alice_profile = get_with_cookie(&server, "/users/alice", &carol_cookie).await;
        assert!(alice_profile.body.contains("0 followers"));

        // Disabling protection keeps the pending request pending and lets new
        // follows through immediately. Nothing is auto-approved.
        let saved = save_profile_settings(
            &server,
            &alice_cookie,
            &[("display_name", "Alice Unprotected")],
        )
        .await;
        assert_eq!(saved.status, 303);
        assert_eq!(
            count_rows(&server, "SELECT COUNT(*) FROM follow_requests").await,
            1
        );
        assert_eq!(count_rows(&server, "SELECT COUNT(*) FROM follows").await, 0);
        let carol_view = get_with_cookie(&server, "/users/alice", &carol_cookie).await;
        let csrf = csrf_token(&carol_view.body);
        let carol_follow = post_form_with_cookie(
            &server,
            &format!("/users/{alice_id}/follow"),
            &carol_cookie,
            &format!("csrf={csrf}"),
        )
        .await;
        assert_eq!(carol_follow.status, 303);
        assert_eq!(count_rows(&server, "SELECT COUNT(*) FROM follows").await, 1);
        assert_eq!(
            count_rows(&server, "SELECT COUNT(*) FROM follow_requests").await,
            1,
            "the pending request must survive the privacy change"
        );

        // Re-enabling protection keeps the existing follower and the pending
        // request; a new account still needs approval.
        let saved = save_profile_settings(
            &server,
            &alice_cookie,
            &[
                ("display_name", "Alice Protected"),
                ("follow_approval_required", "true"),
            ],
        )
        .await;
        assert_eq!(saved.status, 303);
        assert_eq!(count_rows(&server, "SELECT COUNT(*) FROM follows").await, 1);
        assert_eq!(
            count_rows(&server, "SELECT COUNT(*) FROM follow_requests").await,
            1
        );
        let dave_cookie = register_test_user(&server, "dave").await;
        let dave_view = get_with_cookie(&server, "/users/alice", &dave_cookie).await;
        let csrf = csrf_token(&dave_view.body);
        let dave_follow = post_form_with_cookie(
            &server,
            &format!("/users/{alice_id}/follow"),
            &dave_cookie,
            &format!("csrf={csrf}"),
        )
        .await;
        assert_eq!(dave_follow.status, 303);
        assert_eq!(count_rows(&server, "SELECT COUNT(*) FROM follows").await, 1);
        assert_eq!(
            count_rows(&server, "SELECT COUNT(*) FROM follow_requests").await,
            2
        );

        // Approving Bob after all toggles creates exactly one follow.
        let requests = get_with_cookie(&server, "/follow-requests", &alice_cookie).await;
        let csrf = csrf_token(&requests.body);
        let approved = post_form_with_cookie(
            &server,
            &format!("/users/{bob_id}/follow/approve"),
            &alice_cookie,
            &format!("csrf={csrf}"),
        )
        .await;
        assert_eq!(approved.status, 303);
        assert_eq!(count_rows(&server, "SELECT COUNT(*) FROM follows").await, 2);
        assert_eq!(
            count_rows(&server, "SELECT COUNT(*) FROM follow_requests").await,
            1
        );
        let alice_profile = get_with_cookie(&server, "/users/alice", &carol_cookie).await;
        assert!(alice_profile.body.contains("2 followers"));
    }

    #[tokio::test]
    async fn released_username_tombstones_cover_old_urls_before_and_after_reuse() {
        let server = spawn_test_server_without_deletion_grace().await;
        let alice_cookie = register_test_user(&server, "tombstone_alice").await;
        create_text_post(&server, &alice_cookie, "alice tombstone post").await;

        let settings = get_with_cookie(&server, "/settings", &alice_cookie).await;
        let csrf = csrf_token(&settings.body);
        let renamed = post_form_with_cookie(
            &server,
            "/settings/username",
            &alice_cookie,
            &format!(
                "csrf={csrf}&new_username=tombstone_alice_new&password=very%20secure%20password"
            ),
        )
        .await;
        assert_eq!(renamed.status, 303);

        // While the account is alive the old URL is a history page.
        let old_url = request(
            &server.base_url,
            "GET",
            "/users/tombstone_alice",
            &[],
            Vec::new(),
        )
        .await;
        assert_eq!(old_url.status, 200);
        assert!(old_url.body.contains("No account uses @tombstone_alice"));
        assert!(
            old_url
                .body
                .contains(r#"href="/users/tombstone_alice_new""#)
        );

        // Immediate deletion releases the handles and records tombstones.
        let confirm = get_with_cookie(&server, "/settings/delete/confirm", &alice_cookie).await;
        let csrf = csrf_token(&confirm.body);
        let intent = hidden_value(&confirm.body, "delete_intent");
        let deleted = post_form_with_cookie(
            &server,
            "/settings/delete/confirm",
            &alice_cookie,
            &format!("csrf={csrf}&delete_intent={intent}&password=very%20secure%20password"),
        )
        .await;
        assert_eq!(deleted.status, 303);
        assert_eq!(location(&deleted), "/account-deleted");

        for handle in ["tombstone_alice", "tombstone_alice_new"] {
            let page = request(
                &server.base_url,
                "GET",
                &format!("/users/{handle}"),
                &[],
                Vec::new(),
            )
            .await;
            assert_eq!(page.status, 200, "{handle} should render a tombstone");
            assert!(
                page.body.contains(r#"data-testid="released-username""#),
                "{handle} should not silently disappear"
            );
            assert!(!page.body.contains("alice tombstone post"));
        }

        // A new account can claim the released handle, and the old URL then
        // clearly marks it as a different account.
        let bob_cookie = register_test_user(&server, "tombstone_alice_new").await;
        let profile = get_with_cookie(&server, "/users/tombstone_alice_new", &bob_cookie).await;
        assert_eq!(profile.status, 200);
        assert!(
            profile
                .body
                .contains(r#"data-testid="released-username-note""#),
            "a reclaimed handle must disclose the earlier deleted account"
        );
        assert!(!profile.body.contains("alice tombstone post"));
        let other_handle = request(
            &server.base_url,
            "GET",
            "/users/tombstone_alice",
            &[],
            Vec::new(),
        )
        .await;
        assert!(
            other_handle
                .body
                .contains(r#"data-testid="released-username""#),
            "the unclaimed old handle stays a tombstone"
        );
    }

    #[tokio::test]
    async fn oversized_archive_uploads_are_rejected_cleanly_and_valid_ones_still_import() {
        let mut settings = Settings::default();
        settings.accounts.max_archive_upload_bytes = 64 * 1024;
        let server = spawn_test_server_with_settings(settings).await;
        let uploader_cookie = register_test_user(&server, "uploader").await;
        create_text_post(&server, &uploader_cookie, "uploader archive post").await;
        let paths = RuntimePaths::from_data_dir(server.data_dir.clone());

        let import_page = get_with_cookie(&server, "/settings/import", &uploader_cookie).await;
        let csrf = csrf_token(&import_page.body);
        let mut oversized = vec![0u8; 64 * 1024 + 1];
        oversized[..4].copy_from_slice(&[0x1f, 0x8b, 0x08, 0x00]);
        let body = multipart_body_with_file(
            "oversized-archive",
            &[("csrf", csrf.as_str())],
            "archive",
            "oversized.tar.gz",
            "application/gzip",
            &oversized,
        );
        let rejected = request(
            &server.base_url,
            "POST",
            "/settings/import",
            &[
                ("cookie", uploader_cookie.as_str()),
                (
                    "content-type",
                    "multipart/form-data; boundary=oversized-archive",
                ),
            ],
            body,
        )
        .await;
        assert_eq!(rejected.status, 413);
        assert!(
            rejected.body.contains("larger than the configured"),
            "oversized uploads need a clear error: {}",
            rejected.body
        );
        assert_eq!(count_rows(&server, "SELECT COUNT(*) FROM posts").await, 1);
        assert_eq!(
            count_rows(&server, "SELECT COUNT(*) FROM account_imports").await,
            0
        );
        let leftovers = std::fs::read_dir(&paths.tmp_dir)
            .expect("tmp dir")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(crate::portability::IMPORT_TMP_PREFIX)
            })
            .count();
        assert_eq!(
            leftovers, 0,
            "rejected uploads must not leave staging files"
        );

        // A small, valid archive still imports under the same limit.
        let export = get_with_cookie(&server, "/settings/export", &uploader_cookie).await;
        assert_eq!(export.status, 200);
        assert!(export.body_bytes.len() < 64 * 1024);
        let receiver_cookie = register_test_user(&server, "receiver").await;
        let import_page = get_with_cookie(&server, "/settings/import", &receiver_cookie).await;
        let csrf = csrf_token(&import_page.body);
        let body = multipart_body_with_file(
            "valid-archive",
            &[("csrf", csrf.as_str())],
            "archive",
            "account.tar.gz",
            "application/gzip",
            &export.body_bytes,
        );
        let imported = request(
            &server.base_url,
            "POST",
            "/settings/import",
            &[
                ("cookie", receiver_cookie.as_str()),
                (
                    "content-type",
                    "multipart/form-data; boundary=valid-archive",
                ),
            ],
            body,
        )
        .await;
        assert_eq!(imported.status, 200);
        assert!(imported.body.contains("Import complete"));
        assert_eq!(
            count_rows(
                &server,
                "SELECT COUNT(*) FROM posts WHERE text = 'uploader archive post'"
            )
            .await,
            2,
            "the original post plus the imported copy"
        );
    }

    #[tokio::test]
    #[expect(
        clippy::too_many_lines,
        reason = "one scenario proves exclusions, tamper resistance, and non-mutation together"
    )]
    async fn account_archives_exclude_nonportable_state_and_reject_privilege_fields() {
        let server = spawn_test_server().await;
        let mover_cookie = register_test_user(&server, "archive_mover").await;
        create_text_post(&server, &mover_cookie, "portable content only").await;
        let onlooker_cookie = register_test_user(&server, "archive_onlooker").await;
        create_text_post(&server, &onlooker_cookie, "other account content").await;
        let mover_id = user_id_for(&server, "archive_mover").await;

        let onlooker_home = get_with_cookie(&server, "/home", &onlooker_cookie).await;
        let csrf = csrf_token(&onlooker_home.body);
        for path in ["/posts/1/like", "/posts/1/bookmark", "/posts/1/repost"] {
            let response =
                post_form_with_cookie(&server, path, &onlooker_cookie, &format!("csrf={csrf}"))
                    .await;
            assert_eq!(response.status, 303, "{path}");
        }
        let follow = post_form_with_cookie(
            &server,
            &format!("/users/{mover_id}/follow"),
            &onlooker_cookie,
            &format!("csrf={csrf}"),
        )
        .await;
        assert_eq!(follow.status, 303);

        let export = get_with_cookie(&server, "/settings/export", &mover_cookie).await;
        assert_eq!(export.status, 200);
        let entries = read_tar_gz_entries(&export.body_bytes);
        let names = entries
            .iter()
            .map(|(name, _bytes)| name.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "manifest.json",
                "profile.json",
                "posts.json",
                "media.json",
                "follows.json",
                "settings.json"
            ]
            .map(str::to_owned)
        );
        let posts_json = String::from_utf8(
            entries
                .iter()
                .find(|(name, _)| name == "posts.json")
                .expect("posts document")
                .1
                .clone(),
        )
        .expect("posts utf8");
        assert!(posts_json.contains("portable content only"));
        assert!(!posts_json.contains("other account content"));

        let all_text = entries
            .iter()
            .map(|(_name, bytes)| String::from_utf8_lossy(bytes).into_owned())
            .collect::<String>();
        for secret in [
            "password_hash",
            "token_hash",
            "csrf",
            "is_admin",
            "is_suspended",
            "must_change_password",
            "deletion_scheduled_at",
            "notification",
            "bookmark",
            "repost",
        ] {
            assert!(
                !all_text.contains(secret),
                "archives must not carry {secret}"
            );
        }

        // Tampering with the profile document cannot synthesize privileged or
        // restricted account state on the destination.
        let tampered = entries
            .iter()
            .map(|(name, bytes)| {
                if name != "profile.json" {
                    return (name.clone(), bytes.clone());
                }
                let mut profile: serde_json::Value =
                    serde_json::from_slice(bytes).expect("profile json");
                let object = profile.as_object_mut().expect("profile object");
                for (key, value) in [
                    ("is_admin", serde_json::Value::Bool(true)),
                    ("is_suspended", serde_json::Value::Bool(true)),
                    ("must_change_password", serde_json::Value::Bool(true)),
                    (
                        "deletion_scheduled_at",
                        serde_json::Value::String("2000-01-01 00:00:00".to_owned()),
                    ),
                    (
                        "password_hash",
                        serde_json::Value::String("$argon2id$forged".to_owned()),
                    ),
                ] {
                    object.insert(key.to_owned(), value);
                }
                (
                    name.clone(),
                    serde_json::to_vec(&profile).expect("tampered profile"),
                )
            })
            .collect::<Vec<_>>();
        let tampered_bytes = write_tar_gz_entries(&tampered);

        let receiver_cookie = register_test_user(&server, "archive_receiver").await;
        let import_page = get_with_cookie(&server, "/settings/import", &receiver_cookie).await;
        let csrf = csrf_token(&import_page.body);
        let body = multipart_body_with_file(
            "tampered-archive",
            &[("csrf", csrf.as_str())],
            "archive",
            "tampered.tar.gz",
            "application/gzip",
            &tampered_bytes,
        );
        let imported = request(
            &server.base_url,
            "POST",
            "/settings/import",
            &[
                ("cookie", receiver_cookie.as_str()),
                (
                    "content-type",
                    "multipart/form-data; boundary=tampered-archive",
                ),
            ],
            body,
        )
        .await;
        assert_eq!(imported.status, 200, "{}", imported.body);
        assert!(imported.body.contains("Import complete"));
        assert_eq!(
            count_rows(
                &server,
                "SELECT COUNT(*) FROM posts WHERE text = 'portable content only'"
            )
            .await,
            2,
            "the original post plus the imported copy"
        );
        let receiver_id = user_id_for(&server, "archive_receiver").await;
        let state = {
            server
                .pool
                .call(move |conn| {
                    Ok(conn.query_row(
                        "SELECT is_admin, is_suspended, must_change_password, deletion_scheduled_at FROM users WHERE id = ?",
                        [receiver_id],
                        |row| {
                            Ok((
                                row.get::<_, i64>(0)?,
                                row.get::<_, i64>(1)?,
                                row.get::<_, i64>(2)?,
                                row.get::<_, Option<String>>(3)?,
                            ))
                        },
                    )?)
                })
                .await
                .expect("receiver state")
        };
        assert_eq!(state, (0, 0, 0, None));
        let relogin = login_test_user(&server, "archive_receiver", "very secure password").await;
        assert_eq!(relogin.status, 303, "the destination password is untouched");
    }

    #[tokio::test]
    async fn deletion_finalization_invalidates_sessions_and_releases_with_tombstones() {
        let server = spawn_test_server().await;
        let cookie = register_test_user(&server, "scheduler_victim").await;
        create_text_post(&server, &cookie, "scheduled deletion post").await;
        let user_id = user_id_for(&server, "scheduler_victim").await;
        let second_cookie = session_cookie(
            &login_test_user(&server, "scheduler_victim", "very secure password").await,
        );
        server
            .pool
            .call(move |conn| {
                conn.execute(
                    "UPDATE users SET deletion_requested_at = datetime('now','-31 days'), deletion_scheduled_at = datetime('now','-1 day') WHERE id = ?",
                    [user_id],
                )?;
                Ok(())
            })
            .await
            .expect("schedule deletion");

        // The scheduler can race a request from the account it is finalizing.
        let paths = RuntimePaths::from_data_dir(server.data_dir.clone());
        let (removed, racing_request) = tokio::join!(
            crate::account::finalize_due_deletions(&server.pool, &paths),
            get_with_cookie(&server, "/settings", &second_cookie)
        );
        assert_eq!(removed.expect("finalize"), 1);
        assert!(
            matches!(racing_request.status, 200 | 401),
            "a racing request must see either the account or a cleanly signed-out state, got {}",
            racing_request.status
        );
        assert_eq!(
            get_with_cookie(&server, "/settings", &second_cookie)
                .await
                .status,
            401,
            "sessions must be invalidated by finalization"
        );
        let live_sessions = {
            server
                .pool
                .call(move |conn| {
                    Ok(conn.query_row(
                        "SELECT COUNT(*) FROM sessions WHERE user_id = ?",
                        [user_id],
                        |row| row.get::<_, i64>(0),
                    )?)
                })
                .await
                .expect("sessions")
        };
        assert_eq!(live_sessions, 0);

        // The handle is released and tombstoned rather than silently 404ing.
        let page = request(
            &server.base_url,
            "GET",
            "/users/scheduler_victim",
            &[],
            Vec::new(),
        )
        .await;
        assert_eq!(page.status, 200);
        assert!(page.body.contains(r#"data-testid="released-username""#));
        let reused_cookie = register_test_user(&server, "scheduler_victim").await;
        let reused = get_with_cookie(&server, "/users/scheduler_victim", &reused_cookie).await;
        assert!(
            reused
                .body
                .contains(r#"data-testid="released-username-note""#)
        );
        assert!(!reused.body.contains("scheduled deletion post"));
    }

    fn read_tar_gz_entries(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
        use flate2::read::GzDecoder;
        use std::io::Read as _;

        let decoder = GzDecoder::new(bytes);
        let mut archive = tar::Archive::new(decoder);
        let mut entries = Vec::new();
        for entry in archive.entries().expect("tar entries") {
            let mut entry = entry.expect("tar entry");
            let name = entry
                .path()
                .expect("entry path")
                .to_string_lossy()
                .to_string();
            let mut buffer = Vec::new();
            entry.read_to_end(&mut buffer).expect("entry bytes");
            entries.push((name, buffer));
        }
        entries
    }

    fn write_tar_gz_entries(entries: &[(String, Vec<u8>)]) -> Vec<u8> {
        use flate2::write::GzEncoder;

        let encoder = GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        for (name, bytes) in entries {
            let mut header = tar::Header::new_ustar();
            header.set_path(name).expect("entry path");
            header.set_entry_type(tar::EntryType::Regular);
            header.set_size(u64::try_from(bytes.len()).expect("entry size"));
            header.set_mode(0o600);
            header.set_cksum();
            builder.append(&header, bytes.as_slice()).expect("append");
        }
        let encoder = builder.into_inner().expect("tar finish");
        encoder.finish().expect("gzip finish")
    }
}
