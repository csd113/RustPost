//! Backups for the server subsystem.

use super::*;

pub(super) async fn admin_backups(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AppResult<Html<String>> {
    let user = require_admin(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers)
        .await?
        .ok_or(AppError::Unauthorized)?;
    backups_page(&state, &user, &csrf, None).await
}

pub(super) async fn admin_create_backup(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<BackupForm>,
) -> AppResult<Html<String>> {
    let user = require_admin(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    let csrf = form_csrf(&state, &headers)
        .await?
        .ok_or(AppError::Unauthorized)?;
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

pub(super) async fn admin_download_backup(
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

pub(super) async fn admin_backup_settings_update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<admin::BackupSettingsForm>,
) -> AppResult<Html<String>> {
    let user = require_admin(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    let csrf = form_csrf(&state, &headers)
        .await?
        .ok_or(AppError::Unauthorized)?;
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

/// Runtime file replacement must happen with the server stopped. Keep the
/// existing endpoint fail-closed for clients using an older admin page.
pub(super) async fn admin_restore_backup(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AppResult<Response> {
    require_admin(&state, &headers).await?;
    Err(AppError::BadRequest("Stop RustPost and restore with rustpost-cli --data-dir <directory> restore <archive.tar>, then restart. Live restore is disabled to protect active SQLite connections.".to_owned()))
}

/// Streams one multipart field to `path` and returns the bytes written.
///
/// When `max_bytes` is set the limit is enforced for every chunk as it
/// arrives, so an oversized upload is rejected before the whole body is
/// buffered or stored. Uploads must never be buffered in memory.
pub(super) async fn write_multipart_field_to_file(
    mut field: axum::extract::multipart::Field<'_>,
    path: &std::path::Path,
    max_bytes: Option<u64>,
) -> AppResult<u64> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut output = tokio::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .await?;
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

pub(super) fn upload_limit_error(limit: u64) -> AppError {
    AppError::PayloadTooLarge(format!(
        "the uploaded archive is larger than the configured {} MiB limit",
        limit / (1024 * 1024)
    ))
}

pub(super) async fn backups_page(
    state: &AppState,
    user: &CurrentUser,
    csrf: &str,
    notice: Option<(&str, &str)>,
) -> AppResult<Html<String>> {
    backups_page_submission(state, user, csrf, notice, None).await
}

pub(super) async fn backups_page_submission(
    state: &AppState,
    user: &CurrentUser,
    csrf: &str,
    notice: Option<(&str, &str)>,
    submitted: Option<&admin::BackupSettingsForm>,
) -> AppResult<Html<String>> {
    let settings = load_deep_settings(state)?;
    let paths = state.paths.clone();
    let archives = tokio::task::spawn_blocking(move || backup::list_backups(&paths))
        .await
        .map_err(|error| {
            AppError::BadRequest(format!("backup listing worker failed: {error}"))
        })??;
    let notice_html =
        notice.map_or_else(String::new, |(kind, message)| render::notice(kind, message));
    let body = format!(
        "{}{}{}{}{}{}",
        notice_html,
        render::page_header(
            "Backups",
            "Create, download, schedule, and verify full-site backups. Restore requires the server to be stopped."
        ),
        render_manual_backup_panel(csrf, &settings),
        render_automatic_backup_panel(csrf, &settings, submitted),
        render_backup_history(&archives, backup::operation_in_progress(&state.paths), csrf),
        render_restore_panel(csrf),
    );
    Ok(Html(
        page_layout(state, Some(user), Some(csrf), "Backups", &body).await?,
    ))
}

pub(super) fn render_manual_backup_panel(csrf: &str, settings: &Settings) -> String {
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

pub(super) fn render_automatic_backup_panel(
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

pub(super) fn bool_checkbox(id: &str, name: &str, value: bool) -> String {
    let checked = if value { " checked" } else { "" };
    format!(
        r#"<input id="{}" name="{}" type="checkbox" value="true"{checked}>"#,
        html_escape::encode_double_quoted_attribute(id),
        html_escape::encode_double_quoted_attribute(name)
    )
}

pub(super) fn render_backup_history(
    archives: &[backup::BackupArchiveInfo],
    operation_running: bool,
    csrf: &str,
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
                let manifest = if archive.manifest_valid { "readable" } else { "unreadable" };
                format!(
                    r#"<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td><a class="button-link" href="/admin/backups/download/{}">Download</a><form method="post" action="/admin/backups/verify"><input type="hidden" name="csrf" value="{csrf}"><input type="hidden" name="filename" value="{filename}"><label><input type="checkbox" name="include_tor_keys" value="true"> Allow verification of included Tor keys</label><button>Verify backup</button></form></td></tr>"#,
                    html_escape::encode_text(&archive.filename),
                    html_escape::encode_text(kind),
                    html_escape::encode_text(created),
                    format_bytes(archive.size),
                    html_escape::encode_text(tor_keys),
                    html_escape::encode_text(manifest),
                    html_escape::encode_double_quoted_attribute(&archive.filename),
                    csrf=html_escape::encode_double_quoted_attribute(csrf),
                    filename=html_escape::encode_double_quoted_attribute(&archive.filename),
                )
            })
            .collect::<Vec<_>>()
            .join("");
        format!(
            r#"<table><thead><tr><th>Archive</th><th>Kind</th><th>Created</th><th>Size</th><th>Tor keys</th><th>Manifest metadata</th><th>Action</th></tr></thead><tbody>{rows}</tbody></table>"#
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

pub(super) fn render_restore_panel(_csrf: &str) -> String {
    r#"<section class="panel admin-card danger-zone" data-testid="admin-card"><h2>Restore a backup</h2><p>Stop RustPost before restoring so active connections cannot write to a replaced database. On the server, run <code>rustpost-cli --data-dir &lt;directory&gt; restore &lt;archive.tar&gt;</code>, then restart RustPost.</p><p class="muted">Restore validates paths, hashes, settings and SQLite integrity before changing runtime files, and creates a safety backup first. Add <code>--include-tor-keys</code> only when restoring an archive containing Tor private keys.</p></section>"#.to_owned()
}

pub(super) fn public_backup_error(prefix: &str, error: &anyhow::Error) -> String {
    let detail = error.to_string();
    if detail.contains('/') || detail.contains('\\') {
        format!("{prefix}. Check the server logs for details.")
    } else {
        format!("{prefix}: {detail}")
    }
}

pub(super) fn format_bytes(bytes: u64) -> String {
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

pub(super) fn format_scaled_bytes(bytes: u64, unit: u64, suffix: &str) -> String {
    let tenths = u128::from(bytes) * 10 / u128::from(unit);
    format!("{}.{:01} {suffix}", tenths / 10, tenths % 10)
}

#[derive(Deserialize)]
pub(super) struct VerifyBackupForm {
    csrf: String,
    filename: String,
    include_tor_keys: Option<String>,
}

pub(super) async fn admin_verify_backup(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<VerifyBackupForm>,
) -> AppResult<Html<String>> {
    let user = require_admin(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    let csrf = form_csrf(&state, &headers)
        .await?
        .ok_or(AppError::Unauthorized)?;
    let archive = backup::backup_path_for_download(&state.paths, &form.filename)
        .map_err(|_| AppError::NotFound)?;
    let paths = state.paths.clone();
    let include = form.include_tor_keys.is_some();
    let result =
        tokio::task::spawn_blocking(move || backup::verify_backup(&paths, &archive, include))
            .await
            .map_err(|_| AppError::BadRequest("backup verification task failed".to_owned()))?;
    match result {
        Ok(report) => {
            admin::audit(&state.pool, user.id, "verify_backup", &form.filename).await?;
            let message = format!(
                "Backup verified: {} files, {} bytes; migrations and media paths rehearsed through schema {}. Live data was not replaced.",
                report.files, report.bytes, report.schema_version
            );
            backups_page(&state, &user, &csrf, Some(("success", &message))).await
        }
        Err(error) => {
            tracing::warn!(error=%error,"backup verification failed");
            let message = public_backup_error("Backup verification failed", &error);
            backups_page(&state, &user, &csrf, Some(("error", &message))).await
        }
    }
}
