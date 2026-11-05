//! Configuration for the server subsystem.

use super::*;

pub(super) async fn admin_deep_settings(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<DeepSettingsQuery>,
) -> AppResult<Html<String>> {
    let user = require_admin(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers)
        .await?
        .ok_or(AppError::Unauthorized)?;
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

pub(super) async fn admin_deep_settings_update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<admin::DeepSettingsForm>,
) -> AppResult<Html<String>> {
    let user = require_admin(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    let csrf = form_csrf(&state, &headers)
        .await?
        .ok_or(AppError::Unauthorized)?;
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

pub(super) fn configuration_revision(state: &AppState) -> AppResult<String> {
    use sha2::{Digest as _, Sha256};
    let raw = std::fs::read(&state.paths.settings_path)?;
    Ok(Sha256::digest(&raw)
        .iter()
        .fold(String::new(), |mut output, byte| {
            let _ = write!(output, "{byte:02x}");
            output
        }))
}

pub(super) async fn deep_settings_html(
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

pub(super) fn load_deep_settings(state: &AppState) -> AppResult<Settings> {
    let settings = Settings::load(&state.paths.settings_path)?;
    settings.validate()?;
    Ok(settings)
}

pub(super) fn render_deep_settings_form(
    csrf: &str,
    values: &admin::DeepSettingsValues,
    notice: Option<(&str, &str)>,
) -> String {
    render_deep_settings_submission(csrf, values, notice, None)
}

pub(super) fn render_deep_settings_submission(
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

pub(super) fn render_deep_settings_field(
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

pub(super) fn render_deep_settings_confirmation(
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

pub(super) fn render_deep_settings_hidden_fields(values: &admin::DeepSettingsValues) -> String {
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

pub(super) fn render_media_jobs_report(report: &admin::MediaJobsReport) -> String {
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

pub(super) fn compact_text(input: &str, max_chars: usize) -> String {
    let compact = input.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= max_chars {
        return compact;
    }

    let take = max_chars.saturating_sub(3);
    let mut shortened = compact.chars().take(take).collect::<String>();
    shortened.push_str("...");
    shortened
}

pub(super) fn format_age(seconds: i64) -> String {
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
