//! Profiles for the server subsystem.

use super::*;

/// Renders the page for a handle no account currently owns.
///
/// When the handle appears in `username_history`, the page lists the accounts
/// that previously held it instead of silently resolving to a different
/// person. Unknown handles keep the normal 404 response.
pub(super) async fn historical_username_page(
    state: &AppState,
    user: Option<&CurrentUser>,
    headers: &HeaderMap,
    requested: &str,
) -> AppResult<Html<String>> {
    let normalized = requested.trim().to_ascii_lowercase();
    let holders = identity::historical_username_holders(&state.pool, &normalized).await?;
    if holders.is_empty() {
        if instance::username_was_released(&state.pool, &normalized).await? {
            let csrf = form_csrf(state, headers).await?;
            let body = render::released_username_page(&normalized);
            return Ok(Html(
                page_layout(state, user, csrf.as_deref(), "Released username", &body).await?,
            ));
        }
        return Err(AppError::NotFound);
    }
    let csrf = form_csrf(state, headers).await?;
    let body = render::historical_username_page(&normalized, &holders);
    Ok(Html(
        page_layout(state, user, csrf.as_deref(), "Username history", &body).await?,
    ))
}

pub(super) fn profile_tab_from_query(tab: Option<&str>) -> social::ProfileTimelineTab {
    match tab {
        Some("replies") => social::ProfileTimelineTab::Replies,
        Some("media") => social::ProfileTimelineTab::Media,
        Some("likes") => social::ProfileTimelineTab::Likes,
        _ => social::ProfileTimelineTab::Posts,
    }
}

pub(super) fn profile_tab_empty_state(
    tab: social::ProfileTimelineTab,
) -> render::EmptyState<'static> {
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
pub(super) async fn profile(
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
    let csrf = form_csrf(&state, &headers).await?;
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
pub(super) fn profile_state_note(
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
pub(super) fn count_label(count: i64, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("{count} {singular}")
    } else {
        format!("{count} {plural}")
    }
}

pub(super) async fn profile_identity(
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

pub(super) async fn profile_followers(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(username): Path<String>,
    Query(query): Query<AccountListQuery>,
) -> AppResult<Html<String>> {
    let user = current(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers).await?;
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

pub(super) async fn profile_following(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(username): Path<String>,
    Query(query): Query<AccountListQuery>,
) -> AppResult<Html<String>> {
    let user = current(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers).await?;
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
pub(super) fn account_list_next_link(
    base: &str,
    accounts: &[social::AccountView],
    has_more: bool,
) -> String {
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

pub(super) fn profile_controls(
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

pub(super) async fn follow(
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

pub(super) async fn approve_follow_request(
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

pub(super) async fn reject_follow_request(
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

pub(super) async fn cancel_follow_request(
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

pub(super) async fn follow_requests(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AppResult<Html<String>> {
    let user = require_active_user(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers)
        .await?
        .ok_or(AppError::Unauthorized)?;
    let incoming = social::incoming_follow_requests(&state.pool, user.id).await?;
    let outgoing = social::outgoing_follow_requests(&state.pool, user.id).await?;
    let body = render::follow_requests_page(&incoming, &outgoing, &csrf);
    Ok(Html(
        page_layout(&state, Some(&user), Some(&csrf), "Follow requests", &body).await?,
    ))
}

pub(super) async fn unfollow(
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

pub(super) async fn block(
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

pub(super) async fn unblock(
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
