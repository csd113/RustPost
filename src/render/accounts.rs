//! Accounts for the render subsystem.

use super::{FollowRequestDirection, FollowRequestView, compact_empty_state, layout};

pub fn thread_back_control() -> String {
    r##"<div class="thread-nav"><a class="thread-back" href="/home" data-history-back aria-label="Back" title="Back"><svg aria-hidden="true" viewBox="0 0 24 24"><path d="M11 5 4 12l7 7 1.8-1.8L8.9 13H20v-2H8.9l3.9-4.2L11 5z"/></svg><span class="sr-only">Back</span></a></div>"##
        .to_owned()
}

/// "Previously known as" note for profiles that changed handles.
pub fn username_history_note(history: &[crate::identity::UsernameHistoryEntry]) -> String {
    if history.is_empty() {
        return String::new();
    }
    let handles = history
        .iter()
        .map(|entry| format!("@{}", entry.username))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"<p class="username-history" data-testid="username-history">Previously known as {}</p>"#,
        html_escape::encode_text(&handles)
    )
}

/// Renders the page for a handle that no current account owns but that has
/// history on this instance, so old profile links cannot silently resolve to a
/// different person.
pub fn historical_username_page(
    requested: &str,
    holders: &[crate::identity::HistoricalUsernameHolder],
) -> String {
    let rows = holders
        .iter()
        .map(|holder| {
            format!(
                r#"<li><a class="author-name" href="/users/{}">{}</a> <span class="username">@{}</span> <span class="muted">held until {}</span></li>"#,
                html_escape::encode_double_quoted_attribute(&holder.username),
                html_escape::encode_text(&holder.display_name),
                html_escape::encode_text(&holder.username),
                html_escape::encode_text(&holder.changed_at),
            )
        })
        .collect::<Vec<_>>()
        .join("");
    format!(
        r#"<section class="panel" data-testid="historical-username"><h1>No account uses @{requested}</h1><p>This handle is not in use right now, but it was used before. RustPost does not redirect old profile links so they cannot silently point at a different person.</p><h2>Accounts that previously used this handle</h2><ul class="username-history-list">{rows}</ul></section>"#,
        requested = html_escape::encode_text(requested),
    )
}

/// Notice rendered on a profile whose handle was previously held by a
/// permanently deleted account, so an old link cannot silently appear to
/// represent the deleted account.
pub fn released_username_profile_note() -> String {
    r#"<p class="username-history" data-testid="released-username-note">This handle was previously used by an account that has since been permanently deleted. The current profile is a different account.</p>"#
        .to_owned()
}

/// Page rendered for a handle released by a permanently deleted account.
///
/// Used when no current account owns the handle. Old profile URLs remain
/// distinguishable from the deleted account without blocking the handle from
/// being claimed again.
pub fn released_username_page(requested: &str) -> String {
    format!(
        r#"<section class="panel" data-testid="released-username"><h1>No account uses @{requested}</h1><p>This handle was used by an account that has since been permanently deleted. The name is available again, and this page does not represent the deleted account.</p><p><a class="button-link" href="/register">Register this handle</a></p></section>"#,
        requested = html_escape::encode_text(requested),
    )
}

/// Renders the maintenance page shown for state-changing requests that are
/// disabled while maintenance mode is on.
pub fn maintenance_page(site_name: &str, notice: &str) -> String {
    let body = format!(
        r#"<section class="panel error-panel"><p class="eyebrow">503 unavailable</p><h1>Maintenance in progress</h1><p>{}</p><p><a class="button-link" href="/home">Back to Home Feed</a></p></section>"#,
        html_escape::encode_text(notice)
    );
    layout(None, "Maintenance in progress", &body, site_name)
}

/// Pending follow requests, split into incoming requests awaiting the viewer's
/// decision and outgoing requests the viewer can cancel.
pub fn follow_requests_page(
    incoming: &[FollowRequestView],
    outgoing: &[FollowRequestView],
    csrf: &str,
) -> String {
    format!(
        r#"{}{}"#,
        follow_request_section(
            "Follow requests",
            "Accounts that need your approval before they can follow you.",
            incoming,
            csrf,
            FollowRequestDirection::Incoming,
        ),
        follow_request_section(
            "Sent requests",
            "Requests you sent to protected accounts. You can cancel them here.",
            outgoing,
            csrf,
            FollowRequestDirection::Outgoing,
        ),
    )
}

pub(super) fn follow_request_section(
    title: &str,
    help: &str,
    requests: &[FollowRequestView],
    csrf: &str,
    direction: FollowRequestDirection,
) -> String {
    let content = if requests.is_empty() {
        match direction {
            FollowRequestDirection::Incoming => compact_empty_state(
                "No follow requests.",
                "Requests appear here when someone asks to follow a protected account.",
            ),
            FollowRequestDirection::Outgoing => compact_empty_state(
                "No sent requests.",
                "Requests you send to protected accounts appear here.",
            ),
        }
    } else {
        let rows = requests
            .iter()
            .map(|request| follow_request_row(request, csrf, direction))
            .collect::<Vec<_>>()
            .join("");
        format!(r#"<div class="follow-request-list">{rows}</div>"#)
    };
    format!(
        r#"<section class="panel" data-testid="follow-requests-panel"><h2>{}</h2><p class="muted">{}</p>{content}</section>"#,
        html_escape::encode_text(title),
        html_escape::encode_text(help),
    )
}

pub(super) fn follow_request_row(
    request: &FollowRequestView,
    csrf: &str,
    direction: FollowRequestDirection,
) -> String {
    let csrf = html_escape::encode_double_quoted_attribute(csrf);
    let username = html_escape::encode_double_quoted_attribute(&request.username);
    let actions = match direction {
        FollowRequestDirection::Incoming => {
            let approve = format!(
                r#"<form method="post" action="/users/{}/follow/approve"><input type="hidden" name="csrf" value="{csrf}"><button type="submit">Approve</button></form>"#,
                request.user_id
            );
            let reject = format!(
                r#"<form method="post" action="/users/{}/follow/reject"><input type="hidden" name="csrf" value="{csrf}"><button type="submit">Reject</button></form>"#,
                request.user_id
            );
            format!("{approve}{reject}")
        }
        FollowRequestDirection::Outgoing => format!(
            r#"<form method="post" action="/users/{}/follow/cancel"><input type="hidden" name="csrf" value="{csrf}"><button type="submit">Cancel request</button></form>"#,
            request.user_id
        ),
    };
    let avatar = request.profile_picture_path.as_ref().map_or_else(
        || {
            let initial = request.display_name.chars().next().unwrap_or('R');
            format!(
                r#"<span class="post-avatar placeholder" aria-hidden="true">{}</span>"#,
                html_escape::encode_text(&initial.to_string())
            )
        },
        |path| {
            format!(
                r#"<img class="post-avatar" src="{}" alt="" loading="lazy">"#,
                html_escape::encode_double_quoted_attribute(path)
            )
        },
    );
    format!(
        r#"<article class="follow-request-row" data-testid="follow-request-row">{avatar}<div><a class="author-name" href="/users/{username}">{}</a> <span class="username">@{}</span><p>{}</p><p class="muted">Requested {}</p></div><div class="actions">{actions}</div></article>"#,
        html_escape::encode_text(&request.display_name),
        html_escape::encode_text(&request.username),
        html_escape::encode_text(&request.bio),
        html_escape::encode_text(&request.created_at),
    )
}
