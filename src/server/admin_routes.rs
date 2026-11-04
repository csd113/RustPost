//! Admin for the server subsystem.

use super::*;

pub(super) async fn admin_dashboard(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<AdminDashboardQuery>,
) -> AppResult<Html<String>> {
    let user = require_admin(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers)
        .await?
        .ok_or(AppError::Unauthorized)?;
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
        r#"<section class="grid admin-nav-grid"><a class="panel admin-card admin-nav-card" data-testid="admin-card" href="/admin/health">Site health</a><a class="panel admin-card admin-nav-card" data-testid="admin-card" href="/admin/users">Users</a><a class="panel admin-card admin-nav-card" data-testid="admin-card" href="/admin/media">Media jobs</a><a class="panel admin-card admin-nav-card" data-testid="admin-card" href="/admin/deep-settings">Deep server settings</a><a class="panel admin-card admin-nav-card" data-testid="admin-card" href="/admin/backups">Backups</a><a class="panel admin-card admin-nav-card" data-testid="admin-card" href="/admin/updates">Software updates</a></section>"#,
        instance_panels,
        favicon_panel
    );
    Ok(Html(
        page_layout(&state, Some(&user), Some(&csrf), "Admin", &body).await?,
    ))
}

/// Announcement and maintenance controls, persisted in `instance_settings`.
pub(super) fn admin_instance_panels(instance: &instance::InstanceSettings, csrf: &str) -> String {
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

pub(super) async fn admin_update_announcement(
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

pub(super) async fn admin_update_maintenance(
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

pub(super) async fn admin_panel_error(
    state: &AppState,
    user: &CurrentUser,
    headers: &HeaderMap,
    message: &str,
) -> AppResult<Response> {
    let csrf = form_csrf(state, headers)
        .await?
        .ok_or(AppError::Unauthorized)?;
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

pub(super) async fn admin_require_password_reset(
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

pub(super) async fn admin_revoke_sessions(
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

pub(super) async fn admin_favicon_upload(
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

pub(super) async fn admin_favicon_remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> AppResult<Response> {
    require_admin(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    favicon::reset(&state.paths).await?;
    Ok(Redirect::to("/admin").into_response())
}

pub(super) async fn parse_favicon_upload(
    state: &AppState,
    headers: &HeaderMap,
    mut multipart: Multipart,
) -> AppResult<ParsedFaviconUpload> {
    let mut csrf_token = None;
    let mut prepared = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|error| AppError::BadRequest(error.to_string()))?
    {
        match field.name() {
            Some("csrf") => {
                if csrf_token.is_some() {
                    return Err(AppError::Csrf);
                }
                let token = read_form_text(field).await?;
                validate_csrf(&state.pool, headers, &token).await?;
                csrf_token = Some(token);
            }
            Some("favicon")
                if field
                    .file_name()
                    .is_some_and(|name| !name.trim().is_empty()) =>
            {
                if prepared.is_some() {
                    return Err(AppError::BadRequest("choose only one favicon".to_owned()));
                }
                let token = csrf_token.as_deref().ok_or(AppError::Csrf)?;
                validate_csrf(&state.pool, headers, token).await?;
                prepared = Some(
                    favicon::prepare_upload(field)
                        .await
                        .map_err(|error| AppError::BadRequest(error.to_string()))?,
                );
            }
            _ => {
                let _ = read_form_text(field).await?;
            }
        }
    }
    let token = csrf_token.as_deref().ok_or(AppError::Csrf)?;
    validate_csrf(&state.pool, headers, token).await?;
    let uploaded = prepared.is_some();
    if let Some(prepared) = prepared {
        favicon::install_prepared(&state.paths, prepared).await?;
    }
    Ok(ParsedFaviconUpload { uploaded })
}

pub(super) async fn admin_health(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AppResult<Html<String>> {
    let user = require_admin(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers)
        .await?
        .ok_or(AppError::Unauthorized)?;
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

pub(super) async fn admin_users(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<AdminUsersQuery>,
) -> AppResult<Html<String>> {
    let user = require_admin(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers)
        .await?
        .ok_or(AppError::Unauthorized)?;
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

pub(super) fn admin_user_rows(
    rows: &[admin::AdminUserInvestigation],
    csrf: &str,
    searched: bool,
) -> String {
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

pub(super) fn admin_user_row(
    row: &admin::AdminUserInvestigation,
    csrf: &str,
    searched: bool,
) -> String {
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

pub(super) fn admin_user_match_labels(
    row: &admin::AdminUserInvestigation,
    searched: bool,
) -> String {
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

pub(super) fn short_preview(text: &str) -> String {
    const LIMIT: usize = 140;
    let trimmed = text.trim();
    let mut preview = trimmed.chars().take(LIMIT).collect::<String>();
    if trimmed.chars().count() > LIMIT {
        preview.push_str("...");
    }
    preview
}

pub(super) async fn admin_suspend(
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

pub(super) async fn admin_delete_post(
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

pub(super) async fn admin_toggle_post_nsfw(
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

pub(super) async fn admin_media(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AppResult<Html<String>> {
    let user = require_admin(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers)
        .await?
        .ok_or(AppError::Unauthorized)?;
    let jobs = admin::media_jobs_report(&state.pool).await?;
    let body = format!(
        r#"<section class="panel admin-card" data-testid="admin-card"><h1>Media jobs</h1>{}</section>"#,
        render_media_jobs_report(&jobs)
    );
    Ok(Html(
        page_layout(&state, Some(&user), Some(&csrf), "Media jobs", &body).await?,
    ))
}
