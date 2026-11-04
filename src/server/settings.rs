//! Settings for the server subsystem.

use super::*;

pub(super) fn settings_query_notice(saved: Option<&str>) -> Option<(&'static str, &'static str)> {
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
pub(super) fn settings_username_panel(
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
pub(super) fn settings_account_data_panel() -> String {
    r#"<section class="panel settings-card" data-testid="settings-card"><h2>Account data</h2><p class="settings-section-help">Export your posts, profile, media, follows, and settings as a portable archive, or import an archive exported from another RustPost instance.</p><p class="actions"><a class="button-link" href="/settings/export">Export account archive</a> <a class="button-link" href="/settings/import">Import account archive</a></p><p class="muted">Archives never include your password, sessions, tokens, or administrator access.</p></section>"#
        .to_owned()
}

/// Danger panel for requesting or cancelling account deletion.
pub(super) fn settings_delete_panel(
    deletion: Option<&account::DeletionRequest>,
    csrf: &str,
) -> String {
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

pub(super) async fn settings_profile(
    pool: &SqlitePool,
    user_id: i64,
) -> AppResult<SettingsProfile> {
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

pub(super) fn settings_profile_media(
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

pub(super) fn settings_media_actions(
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

pub(super) fn settings_media_file_control(input_id: &str, name: &str, label: &str) -> String {
    let input_id = html_escape::encode_double_quoted_attribute(input_id);
    let name = html_escape::encode_double_quoted_attribute(name);
    let label_attr = html_escape::encode_double_quoted_attribute(label);
    let label_text = html_escape::encode_text(label);
    format!(
        r#"<span class="settings-media-control"><input class="settings-media-input" id="{input_id}" name="{name}" type="file" accept="image/*" aria-label="{label_attr}" data-profile-media-file><label class="settings-media-icon-button settings-media-change" for="{input_id}" title="{label_attr}">{}<span class="sr-only">{label_text}</span></label></span>"#,
        render::icon_svg("edit"),
    )
}

pub(super) fn settings_media_delete_control(input_id: &str, name: &str, label: &str) -> String {
    let input_id = html_escape::encode_double_quoted_attribute(input_id);
    let name = html_escape::encode_double_quoted_attribute(name);
    let label_attr = html_escape::encode_double_quoted_attribute(label);
    let label_text = html_escape::encode_text(label);
    format!(
        r#"<span class="settings-media-control"><input class="settings-media-delete-input" id="{input_id}" name="{name}" type="checkbox" value="true" aria-label="{label_attr}" data-profile-media-delete><label class="settings-media-icon-button settings-media-remove" for="{input_id}" title="{label_attr}">{}<span class="sr-only">{label_text}</span></label></span>"#,
        render::icon_svg("trash"),
    )
}

pub(super) fn settings_media_disabled(message: &str) -> String {
    format!(
        r#"<p class="settings-media-disabled">{}</p>"#,
        html_escape::encode_text(message),
    )
}

pub(super) fn settings_user_list(
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

pub(super) fn settings_muted_word_list(words: &[social::MutedWord], csrf: &str) -> String {
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

pub(super) fn validate_profile_location(location: &str) -> AppResult<()> {
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

pub(super) fn validate_profile_website(website: &str) -> AppResult<()> {
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

pub(super) fn is_safe_profile_website_url(website: &str) -> bool {
    website.parse::<Uri>().is_ok_and(|uri| {
        matches!(uri.scheme_str(), Some("http" | "https")) && uri.authority().is_some()
    })
}

pub(super) fn render_profile_website_link(website: &str) -> String {
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

pub(super) async fn mute(
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

pub(super) async fn unmute(
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

pub(super) async fn settings_form(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<SettingsQuery>,
) -> AppResult<Html<String>> {
    let user = require_user(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers)
        .await?
        .ok_or(AppError::Unauthorized)?;
    let notice = settings_query_notice(query.saved.as_deref());
    Ok(Html(settings_page(&state, &user, &csrf, notice).await?))
}

#[expect(
    clippy::too_many_lines,
    reason = "the settings page template is assembled in one place so panels keep a consistent layout"
)]
pub(super) async fn settings_page(
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

pub(super) fn settings_switch(id: &str, checked: bool, label: &str, help: &str) -> String {
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

pub(super) async fn settings_update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    multipart: Multipart,
) -> AppResult<Response> {
    let user = require_user(&state, &headers).await?;

    let mut form = parse_profile_update(&state, &headers, user.id, multipart).await?;
    let outcome = apply_profile_update(&state, &headers, &user, &form).await;
    if outcome.is_err() {
        cleanup_profile_uploads(&state, &form).await;
    }
    if outcome.is_ok() {
        form.pending_uploads.commit();
    }
    outcome
}

pub(super) async fn apply_profile_update(
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
    let change = |uploaded: Option<i64>, deleted: bool| {
        uploaded.map_or_else(
            || {
                if deleted {
                    media::ProfileMediaChange::Clear
                } else {
                    media::ProfileMediaChange::Keep
                }
            },
            media::ProfileMediaChange::Set,
        )
    };
    let picture = change(form.profile_picture_media_id, form.delete_profile_picture);
    let banner = change(form.banner_media_id, form.delete_banner);
    let previous = state
        .pool
        .call(move |conn| {
            let tx = conn.transaction()?;
            let changed = tx.execute(
                "UPDATE users SET display_name = ?, bio = ?, location = ?, website = ?, theme = ?, nsfw_blur_enabled = ?, liked_posts_public = ?, follow_approval_required = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ? AND is_deleted = 0",
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
            anyhow::ensure!(changed == 1, "account not found");
            let previous = media::apply_profile_media_changes(&tx, user_id, &[(media::ProfileMediaSlot::Picture, picture), (media::ProfileMediaSlot::Banner, banner)])?;
            tx.commit()?;
            Ok(previous)
        }).await?;
    media::cleanup_replaced_profile_media(&state.pool, &state.paths, previous).await;
    Ok(Redirect::to("/settings?saved=profile").into_response())
}

pub(super) async fn add_muted_word(
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

pub(super) async fn remove_muted_word(
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

pub(super) async fn change_password(
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
pub(super) async fn password_response(
    state: &AppState,
    user: &CurrentUser,
    headers: &HeaderMap,
    status: StatusCode,
    kind: &'static str,
    message: &str,
) -> AppResult<Response> {
    if user.must_change_password {
        let csrf = form_csrf(state, headers)
            .await?
            .ok_or(AppError::Unauthorized)?;
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

pub(super) async fn settings_response(
    state: &AppState,
    user: &CurrentUser,
    headers: &HeaderMap,
    status: StatusCode,
    kind: &'static str,
    message: &str,
) -> AppResult<Response> {
    let csrf = form_csrf(state, headers)
        .await?
        .ok_or(AppError::Unauthorized)?;
    Ok((
        status,
        Html(settings_page(state, user, &csrf, Some((kind, message))).await?),
    )
        .into_response())
}

pub(super) async fn delete_account_warning(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AppResult<Html<String>> {
    let user = require_active_user(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers)
        .await?
        .ok_or(AppError::Unauthorized)?;
    let deletion = account::deletion_status(&state.pool, user.id).await?;
    let body = deletion.map_or_else(
        || render_delete_account_warning(state.settings.accounts.deletion_grace_period_days),
        |deletion| render_delete_account_pending(&deletion, &csrf),
    );
    Ok(Html(
        page_layout(&state, Some(&user), Some(&csrf), "Delete account", &body).await?,
    ))
}

pub(super) async fn delete_account_final_warning(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AppResult<Html<String>> {
    let user = require_active_user(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers)
        .await?
        .ok_or(AppError::Unauthorized)?;
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

pub(super) async fn delete_account_final(
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

pub(super) async fn delete_account_final_response(
    state: &AppState,
    user: &CurrentUser,
    headers: &HeaderMap,
    status: StatusCode,
    message: &str,
) -> AppResult<Response> {
    let csrf = form_csrf(state, headers)
        .await?
        .ok_or(AppError::Unauthorized)?;
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

pub(super) async fn create_delete_account_intent(
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

pub(super) async fn consume_delete_account_intent(
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

pub(super) async fn account_deleted(State(state): State<Arc<AppState>>) -> AppResult<Html<String>> {
    let body = r#"<section class="panel"><h1>Account deleted</h1><p>Your account and its owned content have been removed.</p><p><a class="button-link" href="/login">Log in</a></p></section>"#;
    Ok(Html(
        page_layout(&state, None, None, "Account deleted", body).await?,
    ))
}

pub(super) fn render_delete_account_warning(grace_days: u64) -> String {
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

pub(super) fn render_delete_account_pending(
    deletion: &account::DeletionRequest,
    csrf: &str,
) -> String {
    format!(
        r#"<section class="panel danger-panel delete-account-panel" data-testid="deletion-pending"><h1>Deletion scheduled</h1><p>This account is scheduled for permanent deletion on <strong>{}</strong>.</p><p>Until then your posts stay visible, but publishing and account changes are disabled. You can cancel the deletion below.</p><form method="post" action="/settings/delete/cancel"><input type="hidden" name="csrf" value="{}"><button type="submit">Cancel deletion</button></form><p class="muted">Requested {}</p></section>"#,
        html_escape::encode_text(&deletion.scheduled_at),
        html_escape::encode_double_quoted_attribute(csrf),
        html_escape::encode_text(&deletion.requested_at),
    )
}

pub(super) fn render_delete_account_final_warning(
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
pub(super) async fn password_change_page(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<SettingsQuery>,
) -> AppResult<Html<String>> {
    let user = require_user(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers)
        .await?
        .ok_or(AppError::Unauthorized)?;
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

pub(super) fn password_change_page_html(
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

pub(super) fn password_hint(state: &AppState) -> String {
    if state.settings.accounts.min_password_length == 0 {
        "No minimum password length is currently required.".to_owned()
    } else {
        format!(
            "Password must be at least {} characters.",
            state.settings.accounts.min_password_length
        )
    }
}

pub(super) fn password_form_html(
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

pub(super) async fn change_username(
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

pub(super) async fn cancel_deletion(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> AppResult<Response> {
    let user = require_active_user(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    account::cancel_deletion(&state.pool, user.id).await?;
    Ok(Redirect::to("/settings?saved=delete-cancelled").into_response())
}
