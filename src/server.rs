use std::fmt::Write as _;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::connect_info::ConnectInfo;
use axum::extract::{DefaultBodyLimit, Form, Multipart, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::middleware;
use axum::response::{Html, IntoResponse as _, Redirect, Response};
use axum::routing::{get, post};
use axum::{Extension, Json};
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

mod update_routes;
use update_routes::{admin_check_updates, admin_updates};

#[cfg(test)]
mod csrf_regressions;

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub update_discovery: Arc<tokio::sync::Mutex<Option<crate::updates::Discovery>>>,
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
            update_discovery: Arc::new(tokio::sync::Mutex::new(None)),
            configuration_write_lock: Arc::new(tokio::sync::Mutex::new(())),
            settings,
            paths,
            ffmpeg,
            tor,
            registration_captcha: RegistrationCaptchaStore::default(),
            nsfw_blur_default: Arc::new(std::sync::atomic::AtomicBool::new(nsfw_blur_default)),
        })
    }
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

/// Periodically prunes expired operational rows and abandoned temp files.
///
/// Every cycle keeps a grace period because requests can arrive while the
/// maintenance worker starts. Active staging operations suppress cleanup.
pub fn spawn_maintenance_scheduler(
    pool: SqlitePool,
    paths: RuntimePaths,
    shutdown_rx: tokio::sync::watch::Receiver<bool>,
) {
    let _task = spawn_maintenance_scheduler_observed(
        pool,
        paths,
        shutdown_rx,
        crate::console::operations::Operations::default(),
    );
}

pub fn spawn_maintenance_scheduler_observed(
    pool: SqlitePool,
    paths: RuntimePaths,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
    operations: crate::console::operations::Operations,
) -> tokio::task::JoinHandle<()> {
    use crate::console::operations::TaskState;
    tokio::spawn(async move {
        // The longest configured rate-limit window is one hour; keep two hours.
        const RATE_LIMIT_RETENTION_SECS: i64 = 2 * 60 * 60;
        const MAINTENANCE_INTERVAL: std::time::Duration = std::time::Duration::from_mins(30);
        loop {
            if *shutdown_rx.borrow() {
                break;
            }
            let mut failed = false;
            operations.set(
                1,
                TaskState::Running,
                "account cleanup, sessions and rate-limit maintenance",
            );
            if !backup::operation_in_progress(&paths)
                && let Ok(_lease) = crate::runtime::snapshot_lease(&paths, false)
            {
                match crate::account::recover_pending_media_deletions(&paths).await {
                    Ok(0) => {}
                    Ok(recovered) => tracing::info!(
                        recovered,
                        "recovered interrupted account deletion media cleanup"
                    ),
                    Err(error) => {
                        failed = true;
                        tracing::warn!(error = %error, "pending media deletion recovery failed");
                    }
                }
                let cleanup_paths = paths.clone();
                match tokio::task::spawn_blocking(move || {
                    cleanup_paths.cleanup_stale_temp_files(std::time::Duration::from_hours(1))
                })
                .await
                {
                    Ok(Ok(0)) => {}
                    Ok(Ok(removed)) => {
                        tracing::info!(removed, "removed stale upload staging files")
                    }
                    Ok(Err(error)) => {
                        failed = true;
                        tracing::warn!(error = %error, "temp file cleanup failed");
                    }
                    Err(error) => {
                        failed = true;
                        tracing::warn!(error = %error, "temp cleanup worker failed");
                    }
                }
                if let Err(error) =
                    crate::rate_limit::prune_old(&pool, RATE_LIMIT_RETENTION_SECS).await
                {
                    failed = true;
                    tracing::warn!(error = %error, "rate limit event pruning failed");
                }
                match crate::auth::prune_stale_sessions(&pool, 7).await {
                    Ok(0) => {}
                    Ok(removed) => tracing::info!(removed, "removed stale sessions"),
                    Err(error) => {
                        failed = true;
                        tracing::warn!(error = %error, "session pruning failed");
                    }
                }
                match crate::account::finalize_due_deletions(&pool, &paths).await {
                    Ok(0) => {}
                    Ok(removed) => tracing::info!(removed, "finalized scheduled account deletions"),
                    Err(error) => {
                        failed = true;
                        tracing::warn!(error = %error, "scheduled account deletion finalization failed");
                    }
                }
                operations.set(
                    1,
                    if failed {
                        TaskState::Failed
                    } else {
                        TaskState::Succeeded
                    },
                    if failed {
                        "one or more maintenance steps failed; inspect web admin / event callsites"
                    } else {
                        "maintenance pass completed"
                    },
                );
            } else {
                operations.set(
                    1,
                    TaskState::Skipped,
                    "backup/restore snapshot coordination busy",
                );
            }
            tokio::select! {
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
                () = tokio::time::sleep(MAINTENANCE_INTERVAL) => {}
            }
        }
    })
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
        .route("/settings/delete/prepare", post(prepare_delete_account))
        .route(
            "/settings/delete/confirm",
            // Old bookmarks and safe navigation show the initial warning;
            // only the token-protected preparation POST may replace an intent.
            get(delete_account_warning).post(delete_account_final),
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
        .route("/admin/updates", get(admin_updates))
        .route("/admin/updates/check", post(admin_check_updates))
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
        .layer(middleware::from_fn(
            crate::compression::response_compression,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            csrf::request_guard,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            snapshot_guard,
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
    pending_uploads: media::PendingUploads,
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
    pending_uploads: media::PendingUploads,
    csrf_token: String,
    text: String,
    parent_post_id: Option<i64>,
    media_ids: Vec<i64>,
    is_nsfw: bool,
}

struct ParsedOnboardingUpdate {
    pending_uploads: media::PendingUploads,
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
    let csrf = form_csrf(&state, &headers).await?;
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
                | "Software updates"
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
            ("/admin/updates", "Updates", "Software updates"),
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

#[derive(Deserialize)]
struct UsernameChangeForm {
    csrf: String,
    new_username: String,
    password: String,
}

// Multipart parsing is kept in one place so uploaded profile media and text
// fields share one validation path before any database updates happen.

#[derive(Deserialize)]
struct AdminDashboardQuery {
    saved: Option<String>,
}

#[derive(Deserialize)]
struct AnnouncementForm {
    csrf: String,
    announcement: Option<String>,
    enabled: Option<String>,
    intent: Option<String>,
}

#[derive(Deserialize)]
struct MaintenanceForm {
    csrf: String,
    message: Option<String>,
    enabled: Option<String>,
}

#[derive(Deserialize)]
struct BackupForm {
    csrf: String,
    include_tor_keys: Option<String>,
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
    csrf::validate(pool, headers, token).await.map_err(|error| {
        if error.downcast_ref::<csrf::Rejected>().is_some() {
            AppError::Csrf
        } else {
            AppError::Anyhow(error)
        }
    })
}

async fn form_csrf(state: &AppState, headers: &HeaderMap) -> AppResult<Option<String>> {
    let Some(token) = auth::session_cookie(headers) else {
        return Ok(None);
    };
    Ok(csrf::issue(&state.pool, &token).await?)
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

mod guards;
use guards::{account_state_guard, maintenance_guard, snapshot_guard};
mod auth_routes;
use auth_routes::{
    login, login_form, logout, onboarding, onboarding_update, register, register_form,
};
mod posting;
use posting::{
    create_post, delete_confirm, delete_post, edit_post, edit_post_form, quote_form, quote_post,
    reply_redirect, repost, thread, toggle_bookmark, toggle_like, toggle_pin_post,
};
mod profiles;
use profiles::{
    account_list_next_link, approve_follow_request, block, cancel_follow_request, follow,
    follow_requests, profile, profile_followers, profile_following, reject_follow_request, unblock,
    unfollow,
};
mod settings;
use settings::{
    account_deleted, add_muted_word, cancel_deletion, change_password, change_username,
    delete_account_final, delete_account_warning, mute, password_change_page,
    prepare_delete_account, remove_muted_word, render_profile_website_link, settings_form,
    settings_profile, settings_update, unmute,
};
mod portability_routes;
use portability_routes::{
    cleanup_profile_uploads, export_account, import_account, import_account_form,
    parse_profile_update,
};
mod post_actions;
use post_actions::{
    account_action_return, delete_preview, delete_return_fallback, edit_preview,
    edit_return_fallback, enhanced_request, ensure_parent_post_exists, follow_action_response,
    parse_notification_group_target, parse_notification_ids, post_action_response, post_is_reply,
    redirect_to_post_anchor, safe_delete_return_target, safe_edit_return_target,
    safe_return_target,
};
mod timelines;
use timelines::{
    bookmarks, following, mark_notifications_read, mention_suggestions, notifications,
    open_notification_group, search, tag,
};
mod admin_routes;
use admin_routes::{
    admin_dashboard, admin_delete_post, admin_favicon_remove, admin_favicon_upload, admin_health,
    admin_media, admin_require_password_reset, admin_revoke_sessions, admin_suspend,
    admin_toggle_post_nsfw, admin_update_announcement, admin_update_maintenance, admin_users,
};
mod configuration;
use configuration::{
    admin_deep_settings, admin_deep_settings_update, load_deep_settings, render_media_jobs_report,
};
mod backups;
use backups::{
    admin_backup_settings_update, admin_backups, admin_create_backup, admin_download_backup,
    admin_restore_backup, format_bytes, write_multipart_field_to_file,
};

/// Bound multipart metadata independently from the much larger file-body cap.
async fn read_form_text(mut field: axum::extract::multipart::Field<'_>) -> AppResult<String> {
    portability_routes::read_limited_text_field(&mut field, 64 * 1024).await
}
