use super::{
    AppError, AppResult, AppState, Arc, ConnectInfo, CurrentUser, Deserialize, Form, HeaderMap,
    Html, Json, Redirect, Response, Serialize, SocketAddr, State, StatusCode, admin, auth,
    form_csrf, format_bytes, is_state_changing, middleware, page_layout, rate_limit, render,
    require_admin, validate_csrf,
};
use crate::updates::{self, Discovery, Phase, Request as UpdateRequest, Status as UpdateStatus};
use axum::response::IntoResponse as _;

#[derive(Debug, Serialize, Deserialize)]
pub struct UpdateHealth {
    pub version: String,
    pub schema: i64,
    pub ready: bool,
}

pub(super) async fn updater_script() -> Response {
    (
        [("content-type", "application/javascript; charset=utf-8")],
        include_str!("../../assets/rustpost-updates.js"),
    )
        .into_response()
}

pub(super) async fn update_health(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> AppResult<Json<UpdateHealth>> {
    if !peer.ip().is_loopback() {
        return Err(AppError::Forbidden);
    }
    let report = crate::db::schema_report(&state.pool).await?;
    let directories = [
        state.paths.db_dir.clone(),
        state.paths.uploads_originals.clone(),
        state.paths.uploads_images.clone(),
        state.paths.uploads_videos.clone(),
        state.paths.uploads_thumbs.clone(),
        state.paths.assets_dir.clone(),
    ];
    let accessible = tokio::task::spawn_blocking(move || {
        directories.iter().all(|path| {
            std::fs::read_dir(path).is_ok() && tempfile::NamedTempFile::new_in(path).is_ok()
        })
    })
    .await
    .unwrap_or(false);
    Ok(Json(UpdateHealth {
        version: updates::VERSION.into(),
        schema: report.version().unwrap_or(0),
        ready: report.is_compatible()
            && accessible
            && !state
                .restart_required
                .load(std::sync::atomic::Ordering::Relaxed),
    }))
}

/// During startup migrations/health validation the new service must not accept
/// user writes that rollback would subsequently discard. Daemon loss fails
/// closed for mutations in a managed deployment.
pub(super) async fn managed_update_guard(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
    next: middleware::Next,
) -> Response {
    if let Some(socket) = &state.update_socket
        && is_state_changing(request.method())
    {
        let allowed = updates::mutations_allowed(Some(socket)).await;
        if !allowed {
            return (StatusCode::SERVICE_UNAVAILABLE, Html(render::error_page(StatusCode::SERVICE_UNAVAILABLE, "RustPost is applying or recovering a software update. Your changes were not submitted. Reload after the update completes."))).into_response();
        }
    }
    next.run(request).await
}

async fn status(state: &AppState) -> anyhow::Result<UpdateStatus> {
    if let Some(socket) = &state.update_socket {
        let reply = updates::request_to(socket, &UpdateRequest::Status).await?;
        anyhow::ensure!(reply.error.is_none(), "updater status is unavailable");
        Ok(reply.status)
    } else {
        Ok(state.update_status.lock().await.clone())
    }
}

pub(super) async fn admin_updates(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AppResult<Html<String>> {
    let user = require_admin(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers).await.unwrap_or_default();
    updates_page(&state, &user, &csrf, None).await
}

pub(super) async fn admin_update_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AppResult<Json<UpdateStatus>> {
    require_admin(&state, &headers).await?;
    status(&state)
        .await
        .map(Json)
        .map_err(|_| AppError::BadRequest("Updater status is unavailable.".into()))
}

#[derive(Deserialize)]
pub(super) struct CheckForm {
    csrf: String,
}

pub(super) async fn admin_check_updates(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<CheckForm>,
) -> AppResult<Html<String>> {
    let user = require_admin(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    rate_limit::check_and_record(
        &state.pool,
        rate_limit::Scope::UpdateCheck,
        &user.id.to_string(),
        5,
        900,
    )
    .await
    .map_err(|e| AppError::BadRequest(e.to_string()))?;
    let result = if let Some(socket) = &state.update_socket {
        updates::request_to(socket, &UpdateRequest::Check)
            .await
            .map(|reply| reply.error)
    } else {
        let discovery = tokio::task::spawn_blocking(|| updates::discover(updates::VERSION, None))
            .await
            .map_err(anyhow::Error::from)?;
        let mut current = state.update_status.lock().await;
        current.discovery = Some(discovery);
        drop(current);
        Ok(None)
    };
    admin::audit(&state.pool, user.id, "check_software_updates", "stable").await?;
    let message = match result {
        Ok(None) => None,
        Ok(Some(message)) => Some(message),
        Err(error) => {
            tracing::warn!(error = %error, "software update check failed");
            Some(
                "Unable to contact the updater. Check its service and deployment configuration."
                    .into(),
            )
        }
    };
    let csrf = form_csrf(&state, &headers).await.unwrap_or_default();
    updates_page(&state, &user, &csrf, message.as_deref()).await
}

#[derive(Deserialize)]
pub(super) struct InstallForm {
    csrf: String,
    approval: String,
    password: String,
}

pub(super) async fn admin_install_update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<InstallForm>,
) -> AppResult<Response> {
    let user = require_admin(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    rate_limit::check_and_record(
        &state.pool,
        rate_limit::Scope::UpdateReauthentication,
        &user.id.to_string(),
        5,
        900,
    )
    .await
    .map_err(|e| AppError::BadRequest(e.to_string()))?;
    if !auth::verify_user_password(&state.pool, user.id, &form.password).await? {
        return Err(AppError::Forbidden);
    }
    let Some(socket) = &state.update_socket else {
        return Err(AppError::BadRequest(
            "This deployment must be updated through its deployment mechanism.".into(),
        ));
    };
    // Record initiation before IPC: the updater durably records administrator
    // and release identity too, including if DB rollback removes this audit.
    admin::audit(
        &state.pool,
        user.id,
        "install_software_update",
        "stable approved release",
    )
    .await?;
    let reply = updates::request_to(
        socket,
        &UpdateRequest::Install {
            approval: form.approval,
            administrator: user.id,
        },
    )
    .await
    .map_err(|_| AppError::BadRequest("Unable to start the updater.".into()))?;
    if let Some(error) = reply.error {
        return Err(AppError::BadRequest(error));
    }
    Ok(Redirect::to("/admin/updates").into_response())
}

fn phase_label(phase: Phase) -> &'static str {
    match phase {
        Phase::Idle => "Ready to check",
        Phase::Downloading => "Downloading release",
        Phase::Verifying => "Verifying release",
        Phase::Stopping => "Preparing restart",
        Phase::BackingUp => "Verifying database backup",
        Phase::Staged => "Release staged",
        Phase::Activating => "Activating release",
        Phase::Restarting => "Restarting and migrating",
        Phase::HealthChecking => "Checking application health",
        Phase::RollingBack => "Restoring previous version and database",
        Phase::RestartingPrevious => "Checking restored application",
        Phase::ResumingPrevious => "Restarting previous application",
        Phase::Succeeded => "Update succeeded",
        Phase::RolledBack => "Update rolled back",
        Phase::Failed => "Update stopped safely",
        Phase::FailedManualIntervention => "Operator intervention required",
    }
}

fn update_body(current: &UpdateStatus, csrf: &str, managed: bool, container: bool) -> String {
    let escape = html_escape::encode_text;
    let token = html_escape::encode_double_quoted_attribute(csrf);
    let mut body = render::page_header(
        "Software updates",
        "Check stable RustPost releases and safely update your installation.",
    );
    body.push_str(&format!(r#"<section class="panel admin-card"><h2>Installed version</h2><p>v{}</p><p role="status" id="update-progress">{}</p><p>{}</p><form method="post" action="/admin/updates/check"><input type="hidden" name="csrf" value="{token}"><button type="submit">Check for updates</button></form></section>"#, escape(updates::VERSION), phase_label(current.phase), escape(&current.message)));
    if !managed {
        let detail = if container {
            "This instance is container-managed. Update its image through Docker or your deployment system; keep the persistent data volume and create a verified backup first."
        } else {
            "Panel installation requires the native Linux updater service. Update this installation through its deployment mechanism, or configure the managed Linux installation described in the operator guide."
        };
        body.push_str(&render::notice("info", detail));
    }
    match &current.discovery {
        None => body.push_str(r#"<section class="panel"><h2>Latest stable release</h2><p>Click Check for updates to query the official RustPost GitHub releases.</p></section>"#),
        Some(Discovery::UpToDate) => body.push_str(&render::notice("success", "RustPost is up to date on the stable channel.")),
        Some(Discovery::UnableToCheck(error)) => body.push_str(&render::notice("error", error)),
        Some(Discovery::Available(release)) => {
            let size = release.size.map_or_else(|| "Unavailable".into(), format_bytes);
            body.push_str(&format!(r#"<section class="panel admin-card"><h2>Latest stable: v{}</h2><dl><dt>Released</dt><dd>{}</dd><dt>Download size</dt><dd>{}</dd><dt>Verification and compatibility</dt><dd>{}</dd></dl><h3>Release notes</h3><pre style="white-space:pre-wrap;overflow-wrap:anywhere">{}</pre></section>"#, escape(&release.version), escape(&release.published_at), escape(&size), escape(&release.verification), escape(&release.notes)));
            if managed && release.compatible && let Some(approval) = &current.approval {
                body.push_str(&format!(r#"<section class="panel admin-card"><h2>Install update</h2><p>RustPost will verify the release, automatically create and verify a database backup, preserve and back up configuration, retain the previous executable, and temporarily restart. It checks the new version and database after migration and automatically restores the previous executable and database if the upgrade fails.</p><p>A brief outage is expected. User changes are paused during installation. Reopen this page after reconnecting to see the saved result.</p><form id="update-install" method="post" action="/admin/updates/install"><input type="hidden" name="csrf" value="{token}"><input type="hidden" name="approval" value="{}"><label for="update-password">Current administrator password</label><input id="update-password" name="password" type="password" autocomplete="current-password" required><button type="submit">Install update</button></form></section>"#, html_escape::encode_double_quoted_attribute(approval)));
            }
        }
    }
    if current.job.is_some() {
        body.push_str(&format!(r#"<section class="panel admin-card"><h2>Last update attempt</h2><p>v{} → v{}</p><p>{}</p><p>{}</p></section>"#, escape(current.previous_version.as_deref().unwrap_or("unknown")), escape(current.target_version.as_deref().unwrap_or("unknown")), escape(&current.updated_at), escape(&current.message)));
    }
    body.push_str(&backup_body(&current.backups));
    if current.phase.active() || (managed && current.approval.is_some()) {
        body.push_str(&format!(r#"<p><a href="/admin/updates">Refresh update status</a></p><noscript><p>During the restart this page may be temporarily unavailable. Reload to view the persisted result.</p></noscript><script src="/assets/rustpost-updates.js" data-active="{}" defer></script>"#, current.phase.active()));
    }
    body
}

async fn updates_page(
    state: &AppState,
    user: &CurrentUser,
    csrf: &str,
    error: Option<&str>,
) -> AppResult<Html<String>> {
    let current = match status(state).await {
        Ok(status) => status,
        Err(error) => {
            tracing::warn!(error = %error, "updater status unavailable");
            UpdateStatus {
                installed: updates::VERSION.into(),
                message:
                    "Updater unavailable. Check the updater service; installation is disabled."
                        .into(),
                ..UpdateStatus::default()
            }
        }
    };
    let mut body = update_body(
        &current,
        csrf,
        state.update_socket.is_some(),
        updates::container_managed(),
    );
    if let Some(error) = error {
        body.push_str(&render::notice("error", error));
    }
    Ok(Html(
        page_layout(state, Some(user), Some(csrf), "Software updates", &body).await?,
    ))
}

fn backup_body(backups: &[updates::BackupInfo]) -> String {
    let mut body = String::from(
        r#"<section class="panel admin-card"><h2>Pre-upgrade backups</h2><p>Verified database and configuration snapshots are retained by the updater. Active rollback backups are protected from retention. User media and instance identity remain in place.</p>"#,
    );
    if backups.is_empty() {
        body.push_str("<p>No pre-upgrade backups yet.</p>");
    }
    for backup in backups {
        body.push_str(&format!(
            "<p>{} · v{} → v{} · {} · Pre-upgrade · {}</p>",
            html_escape::encode_text(&backup.created_at),
            html_escape::encode_text(&backup.previous_version),
            html_escape::encode_text(&backup.target_version),
            format_bytes(backup.size),
            if backup.verified {
                "Verified"
            } else {
                "Unverified"
            }
        ));
    }
    body.push_str("</section>");
    body
}

pub(super) async fn update_backup_panel(state: &AppState) -> String {
    if state.update_socket.is_none() {
        return String::new();
    }
    match status(state).await {
        Ok(status) => backup_body(&status.backups),
        Err(_) => render::notice(
            "error",
            "Pre-upgrade backup status is unavailable; check the updater service.",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn release_notes_and_attributes_are_escaped() {
        let status = UpdateStatus {
            discovery: Some(Discovery::Available(Box::new(updates::Release {
                id: 1,
                version: "2.0.0".into(),
                published_at: "<img onerror=alert(1)>".into(),
                notes: "<script>alert(1)</script>".into(),
                manifest: None,
                compatible: true,
                verification: "verified".into(),
                size: None,
            }))),
            approval: Some("\" onclick=\"oops".into()),
            ..UpdateStatus::default()
        };
        let html = update_body(&status, "csrf", true, false);
        assert!(!html.contains("<script>alert"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(!html.contains("value=\"\" onclick="));
        assert!(update_body(&status, "csrf", false, true).contains("container-managed"));
    }
}
