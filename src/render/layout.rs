//! Layout for the render subsystem.

use super::{CSS, CurrentUser, LayoutContext, Theme, icon_svg};

pub fn layout(user: Option<&CurrentUser>, title: &str, body: &str, site_name: &str) -> String {
    layout_with_csrf(user, None, title, body, site_name)
}

pub fn layout_with_csrf(
    user: Option<&CurrentUser>,
    csrf: Option<&str>,
    title: &str,
    body: &str,
    site_name: &str,
) -> String {
    layout_with_context(
        user,
        csrf,
        title,
        body,
        site_name,
        &LayoutContext::default(),
    )
}

pub fn layout_with_context(
    user: Option<&CurrentUser>,
    csrf: Option<&str>,
    title: &str,
    body: &str,
    site_name: &str,
    context: &LayoutContext,
) -> String {
    let auth_nav = if let Some(user) = user {
        let admin = if user.is_admin {
            nav_link("/admin", "Admin", "admin")
        } else {
            String::new()
        };
        let notifications = notification_nav_link(context.notification_unread_count.unwrap_or(0));
        let requests = follow_requests_nav_link(context.pending_follow_requests.unwrap_or(0));
        let logout = csrf.map_or_else(String::new, |token| {
            format!(
                r#"<form method="post" action="/logout"><input type="hidden" name="csrf" value="{}"><button>{}<span>Log out</span></button></form>"#,
                html_escape::encode_double_quoted_attribute(token),
                icon_svg("log-out")
            )
        });
        let profile = nav_link(&format!("/users/{}", user.username), "Profile", "profile");
        format!(
            "{}{}{}{notifications}{requests}{}{}{admin}{logout}",
            nav_link("/home", "Home Feed", "home"),
            nav_link("/following", "Following", "users"),
            nav_link("/search", "Search", "search"),
            nav_link("/bookmarks", "Bookmarks", "bookmark"),
            profile,
        )
    } else {
        format!(
            "{}{}{}{}",
            nav_link("/home", "Home Feed", "home"),
            nav_link("/search", "Search", "search"),
            nav_link("/login", "Log in", "log-in"),
            nav_link("/register", "Register", "user-plus")
        )
    };
    let brand_mark = site_name.chars().next().unwrap_or('R');
    let left_rail = left_rail(user, &auth_nav);
    let side_panel = dashboard_panel(user, context);
    let theme = user.map_or(Theme::Light, |user| user.theme).as_str();
    let header_tor = tor_header_indicator(context.tor_onion_address.as_deref());
    let announcement = announcement_banner(context.announcement.as_deref());
    let maintenance = maintenance_banner(context.maintenance_notice.as_deref());
    let account_notice = account_notice_banner(context.account_notice.as_deref());
    format!(
        r#"<!doctype html>
<html lang="en" data-theme="{}">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta http-equiv="Content-Security-Policy" content="default-src 'self'; img-src 'self' data: https://i.ytimg.com; media-src 'self'; frame-src https://www.youtube-nocookie.com; style-src 'self' 'unsafe-inline'; form-action 'self'">
<meta http-equiv="X-Content-Type-Options" content="nosniff">
<meta name="referrer" content="same-origin">
<link rel="icon" href="/favicon.ico" type="{}">
<title>{} - {}</title>
<script src="/assets/rustpost-boot.js"></script>
<style>{}</style>
<script src="/assets/rustpost.js" defer></script>
</head>
<body>
<header class="site-header"><div class="header-inner"><div class="header-brand-row"><a class="brand" href="/home"><span class="brand-mark">{}</span><span>{}</span></a>{}{}</div><nav class="mobile-nav" aria-label="Primary">{}</nav></div></header>
{maintenance}{account_notice}
<noscript><section class="noscript-banner" role="status"><strong>JavaScript is disabled.</strong> RustPost will use standard links and forms.</section></noscript>
<main><div class="app-shell" data-testid="app-shell">{}<section class="primary-column" data-testid="primary-column">{} </section>{}</div></main>
<footer class="site-footer">{}</footer>
</body>
</html>"#,
        html_escape::encode_double_quoted_attribute(theme),
        html_escape::encode_double_quoted_attribute(context.favicon_content_type),
        html_escape::encode_text(title),
        html_escape::encode_text(site_name),
        CSS,
        html_escape::encode_text(&brand_mark.to_string()),
        html_escape::encode_text(site_name),
        announcement,
        header_tor,
        auth_nav,
        left_rail,
        body,
        side_panel,
        html_escape::encode_text(site_name),
    )
}

pub(super) fn announcement_banner(announcement: Option<&str>) -> String {
    announcement.map_or_else(String::new, |announcement| {
        format!(
            r#"<span class="announcement" role="status" data-testid="announcement">{}</span>"#,
            html_escape::encode_text(announcement)
        )
    })
}

pub(super) fn maintenance_banner(notice: Option<&str>) -> String {
    notice.map_or_else(String::new, |notice| {
        format!(
            r#"<section class="notice maintenance-notice" role="status" data-testid="maintenance-notice"><p>{}</p></section>"#,
            html_escape::encode_text(notice)
        )
    })
}

pub(super) fn account_notice_banner(notice: Option<&str>) -> String {
    notice.map_or_else(String::new, |notice| {
        format!(
            r#"<section class="notice error account-notice" role="alert" data-testid="account-notice"><p>{}</p><p><a href="/settings/delete">Review account deletion</a></p></section>"#,
            html_escape::encode_text(notice)
        )
    })
}

pub(super) fn tor_header_indicator(onion: Option<&str>) -> String {
    onion.map_or_else(String::new, |onion| {
        let attr_onion = html_escape::encode_double_quoted_attribute(onion);
        let onion_url = format!("http://{onion}");
        let escaped_onion_url = html_escape::encode_text(&onion_url);
        let attr_onion_url = html_escape::encode_double_quoted_attribute(&onion_url);
        let short = short_onion_address(onion);
        format!(
            r#"<div class="tor-indicator" aria-label="Tor onion service" data-testid="tor-header-indicator"><details class="tor-disclosure"><summary class="tor-pill" aria-label="Show Tor onion address" title="Show Tor onion address" data-testid="tor-pill"><span class="tor-pill-label">Tor</span><span class="tor-summary-text">{}</span></summary><div class="tor-details" data-testid="tor-popover"><a class="tor-full-link" href="{}" title="Open Tor mirror: {}" aria-label="Open Tor mirror at {}" data-testid="tor-full-address">{}</a></div></details><button class="tor-copy-button" type="button" data-copy-text="{}" data-copy-label="Copy" data-copied-label="Copied" data-copy-failed-label="Failed" aria-label="Copy Tor onion address" title="Copy Tor onion address" data-testid="tor-copy-button"><span data-copy-feedback aria-live="polite">Copy</span></button></div>"#,
            html_escape::encode_text(&short),
            attr_onion_url,
            attr_onion,
            attr_onion,
            escaped_onion_url,
            attr_onion,
        )
    })
}

pub(super) fn short_onion_address(onion: &str) -> String {
    let (service_id, suffix_label) = onion
        .strip_suffix(".onion")
        .map_or((onion, ""), |service_id| (service_id, ".onion"));
    let prefix: String = service_id.chars().take(6).collect();
    let suffix = service_id
        .chars()
        .rev()
        .take(3)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    if prefix.is_empty() || suffix.is_empty() || service_id.len() <= prefix.len() + suffix.len() {
        onion.to_owned()
    } else {
        format!("{prefix}...{suffix}{suffix_label}")
    }
}

pub(super) fn nav_link(href: &str, label: &str, icon: &str) -> String {
    format!(
        r#"<a href="{}">{}<span>{}</span></a>"#,
        html_escape::encode_double_quoted_attribute(href),
        icon_svg(icon),
        html_escape::encode_text(label)
    )
}

pub(super) fn left_rail(_user: Option<&CurrentUser>, nav: &str) -> String {
    format!(
        r#"<aside class="left-rail" data-testid="left-rail"><nav class="rail-nav" aria-label="Primary">{nav}</nav></aside>"#
    )
}

pub(super) fn notification_nav_link(unread_count: i64) -> String {
    if unread_count > 0 {
        format!(
            r#"<a href="/notifications">{}<span>Notifications</span> <span class="nav-badge" aria-label="{unread_count} unread notifications">{unread_count}</span></a>"#,
            icon_svg("bell")
        )
    } else {
        nav_link("/notifications", "Notifications", "bell")
    }
}

pub(super) fn follow_requests_nav_link(pending_count: i64) -> String {
    if pending_count > 0 {
        format!(
            r#"<a href="/follow-requests">{}<span>Requests</span> <span class="nav-badge" aria-label="{pending_count} pending follow requests">{pending_count}</span></a>"#,
            icon_svg("user-check")
        )
    } else {
        nav_link("/follow-requests", "Requests", "user-check")
    }
}

/// Picks the singular or plural label for a count.
pub(super) fn plural<'a>(count: i64, singular: &'a str, plural: &'a str) -> &'a str {
    if count == 1 { singular } else { plural }
}

pub(super) fn dashboard_panel(user: Option<&CurrentUser>, context: &LayoutContext) -> String {
    let posting = if context.anonymous_mode_enabled {
        "Signed-in and anonymous posting"
    } else if user.is_some() {
        "Signed-in posting"
    } else {
        "Login required"
    };
    let account = user.map_or_else(
        || r#"<dt>Account</dt><dd>Guest</dd>"#.to_owned(),
        |user| {
            format!(
                r#"<dt>Account</dt><dd><a class="dashboard-account" href="/users/{}"><strong>{}</strong><br><span class="muted">@{}</span></a></dd>"#,
                html_escape::encode_double_quoted_attribute(&user.username),
                html_escape::encode_text(&user.display_name),
                html_escape::encode_text(&user.username)
            )
        },
    );
    let social = match (context.follower_count, context.following_count) {
        (Some(followers), Some(following)) => {
            user.map_or_else(String::new, |user| {
                let username = html_escape::encode_double_quoted_attribute(&user.username);
                let display_name = html_escape::encode_double_quoted_attribute(&user.display_name);
                let follower_label = plural(followers, "follower", "followers");
                format!(
                    r#"<dt>Social</dt><dd><a data-testid="dashboard-followers-link" href="/users/{username}/followers" aria-label="View followers for {display_name}">{followers} {follower_label}</a><br><a data-testid="dashboard-following-link" href="/users/{username}/following" aria-label="View users {display_name} follows">{following} following</a></dd>"#
                )
            })
        }
        _ => String::new(),
    };
    let settings = if user.is_some() {
        r#"<a class="button-link" href="/settings">Settings</a>"#
    } else {
        ""
    };
    format!(
        r#"<aside class="right-rail" data-testid="right-rail"><section class="side-rail-card"><h2>Dashboard</h2><dl class="dashboard-list">{}<dt>Posting</dt><dd>{}</dd>{}</dl><div class="dashboard-actions">{settings}</div></section></aside>"#,
        account, posting, social
    )
}
