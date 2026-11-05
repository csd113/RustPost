//! Guards for the server subsystem.

use super::*;

/// Refuse new requests while an exclusive backup snapshot is active. Existing
/// requests keep their lease until their database/filesystem work is complete.
pub(super) async fn snapshot_guard(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
    next: middleware::Next,
) -> Response {
    if request.method() == axum::http::Method::POST
        && request.uri().path() == "/admin/backups/create"
    {
        return next.run(request).await;
    }
    let lease = if backup::operation_in_progress(&state.paths) {
        Err(anyhow::anyhow!("backup is waiting for admitted requests"))
    } else {
        crate::runtime::snapshot_lease(&state.paths, false)
    };
    let _lease = match lease {
        Ok(lease) => lease,
        Err(error) => {
            tracing::debug!(error = %error, "request delayed by backup snapshot");
            let mut response = (
                StatusCode::SERVICE_UNAVAILABLE,
                "A backup snapshot is in progress. Please retry shortly.",
            )
                .into_response();
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
            return response;
        }
    };
    next.run(request).await
}

/// Paths that stay reachable while an account is restricted to a forced
/// password change. Static assets, logout, the password form, account
/// deletion, and data export must remain available.
pub(super) fn password_change_exempt(path: &str) -> bool {
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
            | "/settings/delete/prepare"
            | "/settings/delete/confirm"
            | "/settings/delete/cancel"
            | "/settings/export"
            | "/account-deleted"
    )
}

/// State-changing paths still allowed while an account is pending deletion.
/// Everything else is read-only until the deletion is cancelled.
pub(super) fn pending_deletion_exempt(path: &str) -> bool {
    matches!(
        path,
        "/logout"
            | "/settings/password"
            | "/settings/delete/cancel"
            | "/settings/delete/prepare"
            | "/settings/delete/confirm"
    )
}

pub(super) fn is_state_changing(method: &axum::http::Method) -> bool {
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
pub(super) async fn account_state_guard(
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

pub(super) fn maintenance_policy(method: &axum::http::Method, path: &str) -> MaintenancePolicy {
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

pub(super) async fn maintenance_guard(
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
