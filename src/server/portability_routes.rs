//! Portability for the server subsystem.

use super::*;

pub(super) async fn export_account(
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
pub(super) fn safe_filename_component(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        .take(32)
        .collect()
}

pub(super) async fn stream_staged_file(
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

pub(super) async fn import_account_form(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AppResult<Html<String>> {
    let user = require_active_user(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers)
        .await?
        .ok_or(AppError::Unauthorized)?;
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

pub(super) fn render_import_account_form(
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

pub(super) async fn import_account(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    multipart: Multipart,
) -> AppResult<Response> {
    let user = require_active_user(&state, &headers).await?;
    // The guard keeps runtime temp cleanup from removing this operation's
    // staged upload or export archive while it is in flight.
    let _operation = crate::runtime::begin_temp_operation();
    // Reject over-budget uploads early without charging unverified forms.
    rate_limit::ensure_under_limit(
        &state.pool,
        rate_limit::Scope::Post,
        &user_actor(user.id),
        state.settings.moderation.posts_per_minute,
        60,
    )
    .await
    .map_err(|err| AppError::RateLimited(err.to_string()))?;
    let staged = stage_account_import(&state, &headers, multipart).await?;
    // Charge the shared posting budget only after the complete multipart form
    // passes CSRF validation (including duplicate fields). The atomic check
    // still arbitrates concurrent imports/posts; TempPath cleans up a loser.
    rate_limit::check_and_record(
        &state.pool,
        rate_limit::Scope::Post,
        &user_actor(user.id),
        state.settings.moderation.posts_per_minute,
        60,
    )
    .await
    .map_err(|err| AppError::RateLimited(err.to_string()))?;
    let report =
        portability::import_account(&state.pool, &state.paths, &state.settings, user.id, &staged)
            .await;
    let _ = tokio::fs::remove_file(&staged).await;
    match report {
        Ok(report) => {
            let csrf = form_csrf(&state, &headers)
                .await?
                .ok_or(AppError::Unauthorized)?;
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
            let csrf = form_csrf(&state, &headers)
                .await?
                .ok_or(AppError::Unauthorized)?;
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

pub(super) async fn stage_account_import(
    state: &AppState,
    headers: &HeaderMap,
    mut multipart: Multipart,
) -> AppResult<tempfile::TempPath> {
    /// Smallest accepted `csrf` field; larger values are rejected so a
    /// malicious request cannot make the server buffer an arbitrary string.
    const MAX_CSRF_FIELD_BYTES: u64 = 4 * 1024;
    /// Bound for unrelated multipart fields that are drained and ignored.
    const MAX_IGNORED_FIELD_BYTES: u64 = 64 * 1024;

    let mut csrf: Option<String> = None;
    let mut staged: Option<tempfile::TempPath> = None;
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|err| AppError::BadRequest(err.to_string()))?
    {
        let name = field.name().map(ToOwned::to_owned).unwrap_or_default();
        if name == "csrf" {
            if csrf.is_some() {
                return Err(AppError::Csrf);
            }
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
        let path = tempfile::Builder::new()
            .prefix(portability::IMPORT_TMP_PREFIX)
            .suffix(".tar.gz")
            .tempfile_in(&state.paths.tmp_dir)?
            .into_temp_path();
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
    let token = csrf.as_deref().ok_or(AppError::Csrf)?;
    validate_csrf(&state.pool, headers, token).await?;
    staged.ok_or_else(|| AppError::BadRequest("choose a RustPost account archive".to_owned()))
}

/// Reads a text multipart field with a byte cap so an oversized field cannot
/// force unbounded buffering.
pub(super) async fn read_limited_text_field(
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
pub(super) async fn drain_limited_field(
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

pub(super) fn render_import_report(report: &portability::ImportReport, csrf: &str) -> String {
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

pub(super) async fn parse_profile_update(
    state: &AppState,
    headers: &HeaderMap,
    user_id: i64,
    multipart: Multipart,
) -> AppResult<ParsedProfileUpdate> {
    let mut form = ParsedProfileUpdate {
        pending_uploads: media::PendingUploads::new(&state.pool, &state.paths),
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
    let outcome = fill_profile_update_form(state, headers, user_id, multipart, &mut form).await;
    if outcome.is_err() {
        cleanup_profile_uploads(state, &form).await;
    }
    outcome.map(|()| form)
}

pub(super) async fn fill_profile_update_form(
    state: &AppState,
    headers: &HeaderMap,
    user_id: i64,
    mut multipart: Multipart,
    form: &mut ParsedProfileUpdate,
) -> AppResult<()> {
    let mut csrf_seen = false;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|err| AppError::BadRequest(err.to_string()))?
    {
        let Some(name) = field.name().map(ToOwned::to_owned) else {
            continue;
        };
        if name == "csrf" {
            if csrf_seen {
                return Err(AppError::Csrf);
            }
            csrf_seen = true;
        }
        match name.as_str() {
            "csrf" | "display_name" | "bio" | "location" | "website" => {
                fill_profile_text_field(form, &name, field).await?;
                if name == "csrf" {
                    validate_csrf(&state.pool, headers, &form.csrf_token).await?;
                }
            }
            "dark_mode" => form.theme = Theme::Dark,
            "nsfw_blur_enabled" => form.nsfw_blur_enabled = true,
            "liked_posts_public" => form.liked_posts_public = true,
            "follow_approval_required" => form.follow_approval_required = true,
            "delete_profile_picture" => form.delete_profile_picture = true,
            "delete_banner" => form.delete_banner = true,
            "profile_picture" if field.file_name().is_some() => {
                validate_csrf(&state.pool, headers, &form.csrf_token).await?;
                fill_profile_picture_upload(state, user_id, form, field).await?;
            }
            "banner" if field.file_name().is_some() => {
                validate_csrf(&state.pool, headers, &form.csrf_token).await?;
                fill_banner_upload(state, user_id, form, field).await?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Reads one of the plain text profile fields into the parsed form.
pub(super) async fn fill_profile_text_field(
    form: &mut ParsedProfileUpdate,
    name: &str,
    field: axum::extract::multipart::Field<'_>,
) -> AppResult<()> {
    let text = read_form_text(field).await?;
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

pub(super) async fn fill_profile_picture_upload(
    state: &AppState,
    user_id: i64,
    form: &mut ParsedProfileUpdate,
    field: axum::extract::multipart::Field<'_>,
) -> AppResult<()> {
    if form.profile_picture_media_id.is_some() {
        return Err(AppError::BadRequest(
            "choose only one profile picture".to_owned(),
        ));
    }
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
    if let Some(id) = form.profile_picture_media_id {
        form.pending_uploads.track(id);
    }
    Ok(())
}

pub(super) async fn fill_banner_upload(
    state: &AppState,
    user_id: i64,
    form: &mut ParsedProfileUpdate,
    field: axum::extract::multipart::Field<'_>,
) -> AppResult<()> {
    if form.banner_media_id.is_some() {
        return Err(AppError::BadRequest("choose only one banner".to_owned()));
    }
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
    if let Some(id) = form.banner_media_id {
        form.pending_uploads.track(id);
    }
    Ok(())
}

/// Deletes profile media that was uploaded for a settings save that did not
/// complete, so rejected saves do not leave orphaned uploads behind.
pub(super) async fn cleanup_profile_uploads(state: &AppState, form: &ParsedProfileUpdate) {
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
