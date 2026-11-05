//! Notifications for the render subsystem.

use super::{DateTime, NaiveDateTime, NotificationGroupView, Utc, empty_state};

pub fn notifications_page(
    notifications: &[NotificationGroupView],
    unread_count: i64,
    csrf: &str,
) -> String {
    let status = if unread_count == 0 {
        "No unread notifications".to_owned()
    } else if unread_count == 1 {
        "1 unread notification".to_owned()
    } else {
        format!("{unread_count} unread notifications")
    };
    let mark_read = if unread_count > 0 {
        format!(
            r#"<form method="post" action="/notifications/read"><input type="hidden" name="csrf" value="{}"><button type="submit">Mark all as read</button></form>"#,
            html_escape::encode_double_quoted_attribute(csrf)
        )
    } else {
        r#"<span class="caught-up-pill">All caught up</span>"#.to_owned()
    };
    let header = format!(
        r#"<section class="notifications-hero"><div><p class="eyebrow">Inbox</p><h1>Notifications</h1><p>{}</p></div>{}</section>"#,
        html_escape::encode_text(&status),
        mark_read
    );
    if notifications.is_empty() {
        return format!(
            "{}{}",
            header,
            empty_state("No notifications.", "New activity will appear here.")
        );
    }
    let caught_up = if unread_count == 0 {
        r#"<section class="notice success caught-up"><p>All caught up. Everything here has been read.</p></section>"#
    } else {
        ""
    };
    format!(
        "{}{}<section class=\"notifications-list\" aria-label=\"Notifications\">{}</section>",
        header,
        caught_up,
        grouped_notification_rows(notifications, csrf)
    )
}

pub(super) fn grouped_notification_rows(
    notifications: &[NotificationGroupView],
    csrf: &str,
) -> String {
    let mut current_group = "";
    let mut html = String::new();
    for notification in notifications {
        let group = notification_group(notification);
        if group != current_group {
            current_group = group;
            html.push_str(&format!(
                r#"<h2 class="notification-group">{}</h2>"#,
                html_escape::encode_text(group)
            ));
        }
        html.push_str(&notification_row(notification, csrf));
    }
    html
}

pub(super) fn notification_group(notification: &NotificationGroupView) -> &'static str {
    if notification.unread_count > 0 {
        "New"
    } else if is_today(&notification.created_at) {
        "Today"
    } else {
        "Earlier"
    }
}

pub(super) fn notification_row(notification: &NotificationGroupView, csrf: &str) -> String {
    let unread = notification.unread_count > 0;
    let target = notification_target(notification);
    let form_id = format!("notification-open-{}", notification.id);
    let target_attrs = target.as_ref().map_or_else(String::new, |_href| {
        format!(
            r#" data-card-form="{}" tabindex="0""#,
            html_escape::encode_double_quoted_attribute(&form_id)
        )
    });
    let preview = notification_preview(notification, target.as_deref());
    let unread_marker = if unread {
        format!(
            r#"<span class="unread-dot" aria-label="{}"></span>"#,
            html_escape::encode_double_quoted_attribute(&notification_unread_label(
                notification.unread_count
            ))
        )
    } else {
        String::new()
    };
    let details = notification_actor_details(notification);
    let open_control = notification_open_control(notification, target.as_deref(), &form_id, csrf);
    format!(
        r#"<article class="notification-row{}"{}><div class="notification-kind" aria-hidden="true">{}</div><div class="notification-body"><p class="notification-line">{}</p>{}{}<p class="notification-meta"><time datetime="{}">{}</time>{}</p></div>{}{}</article>"#,
        if unread { " unread" } else { "" },
        target_attrs,
        html_escape::encode_text(notification_kind_label(&notification.kind)),
        notification_line(notification),
        preview,
        details,
        html_escape::encode_double_quoted_attribute(&notification.created_at),
        html_escape::encode_text(&relative_time(&notification.created_at)),
        notification_count_meta(notification),
        open_control,
        unread_marker
    )
}

pub(super) fn notification_line(notification: &NotificationGroupView) -> String {
    if notification.total_count == 1 {
        return format!(
            "{} <span>{}</span>",
            notification_actor(
                notification
                    .actors
                    .first()
                    .and_then(|actor| actor.username.as_deref()),
                notification
                    .actors
                    .first()
                    .and_then(|actor| actor.display_name.as_deref()),
                notification.actors.first().and_then(|actor| actor.user_id)
            ),
            html_escape::encode_text(notification_action_text(&notification.kind))
        );
    }
    format!(
        r#"<strong>{}</strong> <span>{}</span>"#,
        html_escape::encode_text(&notification_people_label(notification.total_count)),
        html_escape::encode_text(notification_group_action_text(&notification.kind))
    )
}

pub(super) fn notification_actor(
    username: Option<&str>,
    display_name: Option<&str>,
    user_id: Option<i64>,
) -> String {
    match (username, display_name) {
        (Some(username), display_name) => format!(
            r#"<a class="author-name" href="/users/{}">{}</a> <span class="username">@{}</span>"#,
            html_escape::encode_double_quoted_attribute(username),
            html_escape::encode_text(display_name.unwrap_or(username)),
            html_escape::encode_text(username)
        ),
        (None, _) if user_id.is_some() => "Deleted account".to_owned(),
        _ => "Someone".to_owned(),
    }
}

pub(super) fn notification_actor_details(notification: &NotificationGroupView) -> String {
    if notification.total_count <= 1 {
        return String::new();
    }
    let actors = notification
        .actors
        .iter()
        .map(|actor| {
            format!(
                r#"<li>{}</li>"#,
                notification_actor(
                    actor.username.as_deref(),
                    actor.display_name.as_deref(),
                    actor.user_id
                )
            )
        })
        .collect::<Vec<_>>()
        .join("");
    format!(
        r#"<details class="notification-actors"><summary>View people</summary><ul>{actors}</ul></details>"#
    )
}

pub(super) fn notification_open_control(
    notification: &NotificationGroupView,
    target: Option<&str>,
    form_id: &str,
    csrf: &str,
) -> String {
    let Some(target) = target else {
        return String::new();
    };
    let notification_ids = notification
        .notification_ids
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let group_target = notification
        .group_target_post_id
        .map_or_else(String::new, |id| {
            format!(r#"<input type="hidden" name="group_target_post_id" value="{id}">"#)
        });
    // Group kinds without a post target (for example follow requests) are
    // marked read through their explicit notification ids.
    let group_kind = if notification.kind == "follow" || notification.group_target_post_id.is_some()
    {
        format!(
            r#"<input type="hidden" name="group_kind" value="{}">"#,
            html_escape::encode_double_quoted_attribute(&notification.kind)
        )
    } else {
        String::new()
    };
    format!(
        r#"<form id="{}" class="notification-open-form" method="post" action="/notifications/open"><input type="hidden" name="csrf" value="{}"><input type="hidden" name="notification_ids" value="{}">{group_kind}{group_target}<input type="hidden" name="return_to" value="{}"><button class="button-link notification-open" type="submit">Open</button></form>"#,
        html_escape::encode_double_quoted_attribute(form_id),
        html_escape::encode_double_quoted_attribute(csrf),
        html_escape::encode_double_quoted_attribute(&notification_ids),
        html_escape::encode_double_quoted_attribute(target)
    )
}

pub(super) fn notification_preview(
    notification: &NotificationGroupView,
    target: Option<&str>,
) -> String {
    if is_account_notification(&notification.kind) {
        return String::new();
    }
    let text = if notification.post_available {
        notification
            .post_text
            .as_deref()
            .map_or_else(|| "Post preview unavailable".to_owned(), snippet)
    } else {
        "Post is no longer available.".to_owned()
    };
    if notification.post_available
        && let Some(target) = target
    {
        format!(
            r#"<a class="notification-preview" href="{}">{}</a>"#,
            html_escape::encode_double_quoted_attribute(target),
            html_escape::encode_text(&text)
        )
    } else {
        format!(
            r#"<p class="notification-preview unavailable">{}</p>"#,
            html_escape::encode_text(&text)
        )
    }
}

/// Notification kinds that point at an account rather than a post.
pub(super) fn is_account_notification(kind: &str) -> bool {
    matches!(
        kind,
        "follow" | "follow_request" | "follow_request_approved"
    )
}

pub(super) fn notification_target(notification: &NotificationGroupView) -> Option<String> {
    if notification.kind == "follow_request" {
        return Some("/follow-requests".to_owned());
    }
    if is_account_notification(&notification.kind) {
        return notification
            .actors
            .first()
            .and_then(|actor| actor.username.as_ref())
            .as_ref()
            .map(|username| format!("/users/{username}"));
    }
    notification
        .post_available
        .then_some(notification.post_id)
        .flatten()
        .map(|post_id| format!("/posts/{post_id}"))
}

pub(super) fn notification_count_meta(notification: &NotificationGroupView) -> String {
    if notification.total_count <= 1 && notification.unread_count == 0 {
        return String::new();
    }
    let mut parts = Vec::new();
    if notification.total_count > 1 {
        parts.push(notification_items_label(notification.total_count));
    }
    if notification.unread_count > 0 {
        parts.push(notification_unread_label(notification.unread_count));
    }
    format!(
        r#" <span class="notification-counts">{}</span>"#,
        html_escape::encode_text(&parts.join(" / "))
    )
}

pub(super) fn notification_people_label(count: usize) -> String {
    if count == 1 {
        "1 person".to_owned()
    } else {
        format!("{count} people")
    }
}

pub(super) fn notification_items_label(count: usize) -> String {
    if count == 1 {
        "1 notification".to_owned()
    } else {
        format!("{count} notifications")
    }
}

pub(super) fn notification_unread_label(count: i64) -> String {
    if count == 1 {
        "1 unread".to_owned()
    } else {
        format!("{count} unread")
    }
}

pub(super) fn notification_action_text(kind: &str) -> &'static str {
    match kind {
        "reply" => "replied to your post",
        "like" => "liked your post",
        "repost" => "reposted your post",
        "quote" => "quoted your post",
        "follow" => "followed you",
        "mention" => "mentioned you in a post",
        "follow_request" => "requested to follow you",
        "follow_request_approved" => "approved your follow request",
        _ => "sent you a notification",
    }
}

pub(super) fn notification_group_action_text(kind: &str) -> &'static str {
    match kind {
        "reply" => "replied to your post",
        "like" => "liked your post",
        "repost" => "reposted your post",
        "quote" => "quoted your post",
        "follow" => "followed you",
        "mention" => "mentioned you in a post",
        "follow_request" => "requested to follow you",
        "follow_request_approved" => "approved your follow request",
        _ => "sent you notifications",
    }
}

pub(super) fn notification_kind_label(kind: &str) -> &'static str {
    match kind {
        "reply" => "R",
        "like" => "L",
        "repost" => "Re",
        "quote" => "Q",
        "follow" | "follow_request" | "follow_request_approved" => "F",
        "mention" => "@",
        _ => "N",
    }
}

pub(super) fn snippet(text: &str) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let limit = 160;
    if compact.chars().count() <= limit {
        compact
    } else {
        let mut value = compact.chars().take(limit).collect::<String>();
        value.push_str("...");
        value
    }
}

pub(super) fn is_today(created_at: &str) -> bool {
    parse_timestamp(created_at)
        .is_some_and(|created| created.date_naive() == Utc::now().date_naive())
}

pub(super) fn relative_time(created_at: &str) -> String {
    let Some(created) = parse_timestamp(created_at) else {
        return created_at.to_owned();
    };
    let elapsed = Utc::now().signed_duration_since(created);
    if elapsed.num_minutes() < 1 {
        "just now".to_owned()
    } else if elapsed.num_hours() < 1 {
        let minutes = elapsed.num_minutes();
        format!("{minutes}m ago")
    } else if elapsed.num_days() < 1 {
        let hours = elapsed.num_hours();
        format!("{hours}h ago")
    } else {
        let days = elapsed.num_days();
        format!("{days}d ago")
    }
}

pub(super) fn parse_timestamp(value: &str) -> Option<DateTime<Utc>> {
    NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S")
        .ok()
        .map(|timestamp| DateTime::from_naive_utc_and_offset(timestamp, Utc))
}
