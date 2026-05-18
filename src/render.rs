use crate::auth::CurrentUser;
use crate::social::{AccountView, MediaView, PostView, TimelineEventKind};
use axum::http::StatusCode;

#[derive(Debug, Clone, Default)]
pub struct LayoutContext {
    pub anonymous_mode_enabled: bool,
    pub tor_onion_address: Option<String>,
    pub follower_count: Option<i64>,
    pub following_count: Option<i64>,
}

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
            r#"<a href="/admin">Admin</a>"#
        } else {
            ""
        };
        let logout = csrf.map_or_else(String::new, |token| {
            format!(
                r#"<form method="post" action="/logout"><input type="hidden" name="csrf" value="{}"><button>Log out</button></form>"#,
                html_escape::encode_double_quoted_attribute(token)
            )
        });
        format!(
            r#"<a href="/home">Home Feed</a><a href="/following">Following</a><a href="/search">Search</a><a href="/notifications">Notifications</a><a href="/bookmarks">Bookmarks</a><a href="/users/{}">Profile</a>{admin}{logout}"#,
            html_escape::encode_double_quoted_attribute(&user.username)
        )
    } else {
        r#"<a href="/home">Home Feed</a><a href="/search">Search</a><a href="/login">Log in</a><a href="/register">Register</a>"#
            .to_owned()
    };
    let brand_mark = site_name.chars().next().unwrap_or('R');
    let side_panel = dashboard_panel(user, context);
    let footer_onion = context
        .tor_onion_address
        .as_deref()
        .map_or_else(String::new, |onion| {
            format!(
                r#" <span class="footer-onion">Onion: <code>{}</code></span>"#,
                html_escape::encode_text(onion)
            )
        });
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta http-equiv="Content-Security-Policy" content="default-src 'self'; img-src 'self' data:; media-src 'self'; style-src 'self' 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'">
<meta http-equiv="X-Content-Type-Options" content="nosniff">
<meta name="referrer" content="same-origin">
<title>{} - {}</title>
<style>{}</style>
<script src="/assets/rustpost.js" defer></script>
</head>
<body>
<header class="site-header"><div class="header-inner"><a class="brand" href="/home"><span class="brand-mark">{}</span><span>{}</span></a><nav>{}</nav></div></header>
<main><div class="content-shell"><section class="content-column">{} </section>{}</div></main>
<footer class="site-footer">{} alpha{}</footer>
</body>
</html>"#,
        html_escape::encode_text(title),
        html_escape::encode_text(site_name),
        CSS,
        html_escape::encode_text(&brand_mark.to_string()),
        html_escape::encode_text(site_name),
        auth_nav,
        body,
        side_panel,
        html_escape::encode_text(site_name),
        footer_onion
    )
}

fn dashboard_panel(user: Option<&CurrentUser>, context: &LayoutContext) -> String {
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
            format!(r#"<dt>Social</dt><dd>{followers} followers<br>{following} following</dd>"#)
        }
        _ => String::new(),
    };
    let admin = if user.is_some_and(|user| user.is_admin) {
        r#"<a class="button-link" href="/admin">Admin</a>"#
    } else {
        ""
    };
    let settings = if user.is_some() {
        r#"<a class="button-link" href="/settings">Settings</a>"#
    } else {
        ""
    };
    format!(
        r#"<aside class="side-panel"><h2>Dashboard</h2><dl class="dashboard-list">{}<dt>Posting</dt><dd>{}</dd>{}</dl><div class="quick-links">{settings}{admin}</div></aside>"#,
        account, posting, social
    )
}

pub fn login_form(message: Option<&str>) -> String {
    let notice = message.map_or_else(String::new, |message| notice("error", message));
    format!(
        r#"<section class="panel auth-panel"><h1>Log in</h1>{notice}<form method="post" class="auth-form"><label for="username">Username</label><input id="username" name="username" autocomplete="username" required><label for="password">Password</label><div class="password-control"><input id="password" name="password" type="password" autocomplete="current-password" required><button type="button" class="password-toggle" data-password-toggle="password" aria-label="Show password">Show</button></div><button class="auth-submit" type="submit">Log in</button></form></section>"#
    )
}

pub fn register_form(message: Option<&str>) -> String {
    let notice = message.map_or_else(String::new, |message| notice("error", message));
    format!(
        r#"<section class="panel auth-panel"><h1>Create account</h1>{notice}<form method="post" class="auth-form"><label for="username">Username</label><input id="username" name="username" autocomplete="username" required><label for="password">Password</label><div class="password-control"><input id="password" name="password" type="password" minlength="10" autocomplete="new-password" required><button type="button" class="password-toggle" data-password-toggle="password" aria-label="Show password">Show</button></div><label for="confirm_password">Confirm password</label><div class="password-control"><input id="confirm_password" name="confirm_password" type="password" minlength="10" autocomplete="new-password" required><button type="button" class="password-toggle" data-password-toggle="confirm_password" aria-label="Show password confirmation">Show</button></div><button class="auth-submit" type="submit">Create account</button></form></section>"#
    )
}

pub fn client_script() -> &'static str {
    r#"document.addEventListener("click", (event) => {
  const button = event.target.closest("[data-password-toggle]");
  if (!button) {
    return;
  }
  const input = document.getElementById(button.getAttribute("data-password-toggle"));
  if (!input) {
    return;
  }
  const show = input.type === "password";
  input.type = show ? "text" : "password";
  button.textContent = show ? "Hide" : "Show";
  button.setAttribute("aria-label", show ? "Hide password" : "Show password");
});

function updateComposerCount(textarea) {
  const counter = document.querySelector(`[data-character-counter="${textarea.id}"]`);
  if (!counter) {
    return;
  }
  const max = Number.parseInt(textarea.getAttribute("maxlength") || "280", 10);
  const length = Array.from(textarea.value).length;
  counter.textContent = `${Math.max(0, max - length)} remaining`;
}

document.querySelectorAll("textarea[data-character-limit]").forEach((textarea) => {
  updateComposerCount(textarea);
  textarea.addEventListener("input", () => updateComposerCount(textarea));
});"#
}

pub fn composer(csrf: Option<&str>, parent: Option<i64>) -> String {
    let parent_input = parent.map_or_else(String::new, |id| {
        format!(r#"<input type="hidden" name="parent_post_id" value="{id}">"#)
    });
    let csrf = csrf.unwrap_or_default();
    let input_id = parent.map_or_else(|| "post-text".to_owned(), |id| format!("reply-text-{id}"));
    format!(
        r#"<section class="composer" id="reply" aria-labelledby="composer-title"><div class="section-heading"><h1 id="composer-title">{}</h1><span class="muted" data-character-counter="{}">280 remaining</span></div><form method="post" action="/posts" enctype="multipart/form-data">
<input type="hidden" name="csrf" value="{}">{}
<label class="sr-only" for="{}">What is happening?</label>
<textarea id="{}" name="text" maxlength="280" rows="4" data-character-limit="280"></textarea>
<div class="composer-tools"><label class="file-control" for="media">Attach media<input id="media" name="media" type="file" multiple accept="image/*,video/mp4,video/webm,video/quicktime"></label><button class="primary" type="submit">Post</button></div>
</form></section>"#,
        if parent.is_some() {
            "Reply"
        } else {
            "New post"
        },
        html_escape::encode_double_quoted_attribute(&input_id),
        html_escape::encode_double_quoted_attribute(csrf),
        parent_input,
        html_escape::encode_double_quoted_attribute(&input_id),
        html_escape::encode_double_quoted_attribute(&input_id)
    )
}

pub fn accounts(accounts: &[AccountView], csrf: &str) -> String {
    if accounts.is_empty() {
        return empty_state(
            "You are not following anyone yet.",
            "Follow accounts to build your home feed.",
        );
    }
    let rows = accounts
        .iter()
        .map(|account| {
            let avatar = account.profile_picture_path.as_ref().map_or_else(
                || {
                    let initial = account.display_name.chars().next().unwrap_or('R');
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
            let action = if account.viewer_following {
                action_form(&format!("/users/{}/unfollow", account.id), csrf, "Unfollow")
            } else {
                action_form(&format!("/users/{}/follow", account.id), csrf, "Follow")
            };
            format!(
                r#"<article class="account-row">{}<div><a class="author-name" href="/users/{}">{}</a> <span class="username">@{}</span><p>{}</p></div><div>{}</div></article>"#,
                avatar,
                html_escape::encode_double_quoted_attribute(&account.username),
                html_escape::encode_text(&account.display_name),
                html_escape::encode_text(&account.username),
                html_escape::encode_text(&account.bio),
                action
            )
        })
        .collect::<Vec<_>>()
        .join("");
    format!(r#"<section class="account-list">{rows}</section>"#)
}

fn action_form(action: &str, csrf: &str, label: &str) -> String {
    format!(
        r#"<form method="post" action="{}"><input type="hidden" name="csrf" value="{}"><button>{}</button></form>"#,
        html_escape::encode_double_quoted_attribute(action),
        html_escape::encode_double_quoted_attribute(csrf),
        html_escape::encode_text(label)
    )
}

pub fn posts(posts: &[PostView], user: Option<&CurrentUser>, csrf: Option<&str>) -> String {
    if posts.is_empty() {
        return empty_state(
            "No posts yet",
            "The timeline will fill in once people start posting.",
        );
    }
    format!(
        r#"<section class="timeline" aria-label="Posts">{}</section>"#,
        posts
            .iter()
            .map(|post| post_card(post, user, csrf))
            .collect::<Vec<_>>()
            .join("")
    )
}

// Rendering a post card stays centralized because the markup, counts, media,
// and action controls must remain consistent between timelines and threads.
#[expect(
    clippy::too_many_lines,
    reason = "post card markup is centralized to keep timeline and thread rendering identical"
)]
pub fn post_card(post: &PostView, user: Option<&CurrentUser>, csrf: Option<&str>) -> String {
    let repost_banner = if post.event_kind == TimelineEventKind::Repost {
        let name = post
            .reposted_by_display_name
            .as_deref()
            .or(post.reposted_by_username.as_deref())
            .unwrap_or("Someone");
        format!(
            r#"<div class="repost-banner">{} reposted</div>"#,
            html_escape::encode_text(name)
        )
    } else {
        String::new()
    };
    if post.original_unavailable {
        return format!(
            r#"<article class="post unavailable" id="post-{}" data-event-id="{}">{}<div class="text">This post is no longer available.</div></article>"#,
            post.id,
            html_escape::encode_double_quoted_attribute(&post.event_id),
            repost_banner
        );
    }
    let author = match (&post.username, &post.anonymous_label) {
        (Some(username), _) => format!(
            r#"<a class="author-name" href="/users/{}">{}</a> <span class="username">@{}</span>"#,
            html_escape::encode_double_quoted_attribute(username),
            html_escape::encode_text(post.display_name.as_deref().unwrap_or(username)),
            html_escape::encode_text(username)
        ),
        (None, Some(label)) => html_escape::encode_text(label).to_string(),
        _ => "Deleted user".to_owned(),
    };
    let avatar = post.profile_picture_path.as_ref().map_or_else(
        || {
            let initial = post
                .display_name
                .as_deref()
                .or(post.username.as_deref())
                .or(post.anonymous_label.as_deref())
                .and_then(|value| value.chars().next())
                .unwrap_or('R');
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
    let text = linkify(&post.text);
    let media = post
        .media
        .iter()
        .map(render_media)
        .collect::<Vec<_>>()
        .join("");
    let controls = if let (Some(user), Some(csrf)) = (user, csrf) {
        let delete = if post.user_id == Some(user.id) || user.is_admin {
            icon_link(&format!("/posts/{}/delete", post.id), "Delete", "trash")
        } else {
            String::new()
        };
        let reply_link = icon_link(&format!("/posts/{}#reply", post.id), "Reply", "reply");
        let thread_link = if post.parent_post_id.is_none() {
            format!(
                r#"<a class="button-link thread-link" href="/posts/{}">Open thread</a>"#,
                post.id
            )
        } else {
            String::new()
        };
        format!(
            r#"<div class="actions">{}{}{}{}{}{}</div>"#,
            icon_action_form(
                &format!("/posts/{}/like", post.id),
                csrf,
                if post.viewer_liked { "Unlike" } else { "Like" },
                "heart",
                post.viewer_liked
            ),
            icon_action_form(
                &format!("/posts/{}/bookmark", post.id),
                csrf,
                if post.viewer_bookmarked {
                    "Unbookmark"
                } else {
                    "Bookmark"
                },
                "bookmark",
                post.viewer_bookmarked
            ),
            if post.viewer_can_repost {
                icon_action_form(
                    &format!("/posts/{}/repost", post.id),
                    csrf,
                    if post.viewer_reposted {
                        "Unrepost"
                    } else {
                        "Repost"
                    },
                    "repost",
                    post.viewer_reposted,
                )
            } else {
                disabled_icon_button("Repost unavailable for your own post", "repost")
            },
            reply_link,
            delete,
            thread_link
        )
    } else if post.parent_post_id.is_none() {
        format!(
            r#"<div class="actions"><a class="button-link" href="/posts/{}">Open thread</a></div>"#,
            post.id
        )
    } else {
        String::new()
    };
    let post_class = if post.parent_post_id.is_some() {
        "post reply-post"
    } else {
        "post"
    };
    let reply_anchor = if post.parent_post_id.is_some() {
        format!(
            r#"<span class="anchor-target" id="reply-{}"></span>"#,
            post.id
        )
    } else {
        String::new()
    };
    format!(
        r#"<article class="{}" id="post-{}" data-event-id="{}">{}{}<header class="post-header"><div class="author-block">{}<div>{}</div></div><a class="post-time" href="/posts/{}">#{}</a></header><div class="text">{}</div>{}<div class="counts"><span>{} likes</span><span>{} reposts</span><span>{} replies</span><span>{}</span></div>{}</article>"#,
        post_class,
        post.id,
        html_escape::encode_double_quoted_attribute(&post.event_id),
        reply_anchor,
        repost_banner,
        avatar,
        author,
        post.id,
        post.id,
        text,
        media,
        post.like_count,
        post.repost_count,
        post.reply_count,
        html_escape::encode_text(&post.created_at),
        controls
    )
}

fn icon_action_form(action: &str, csrf: &str, label: &str, icon: &str, active: bool) -> String {
    format!(
        r#"<form method="post" action="{}"><input type="hidden" name="csrf" value="{}"><button class="icon-button{}" type="submit" aria-label="{}" title="{}">{}<span class="sr-only">{}</span></button></form>"#,
        html_escape::encode_double_quoted_attribute(action),
        html_escape::encode_double_quoted_attribute(csrf),
        if active { " active" } else { "" },
        html_escape::encode_double_quoted_attribute(label),
        html_escape::encode_double_quoted_attribute(label),
        icon_svg(icon),
        html_escape::encode_text(label)
    )
}

fn icon_link(href: &str, label: &str, icon: &str) -> String {
    format!(
        r#"<a class="icon-button" href="{}" aria-label="{}" title="{}">{}<span class="sr-only">{}</span></a>"#,
        html_escape::encode_double_quoted_attribute(href),
        html_escape::encode_double_quoted_attribute(label),
        html_escape::encode_double_quoted_attribute(label),
        icon_svg(icon),
        html_escape::encode_text(label)
    )
}

fn disabled_icon_button(label: &str, icon: &str) -> String {
    format!(
        r#"<button class="icon-button disabled" type="button" aria-label="{}" title="{}" disabled>{}<span class="sr-only">{}</span></button>"#,
        html_escape::encode_double_quoted_attribute(label),
        html_escape::encode_double_quoted_attribute(label),
        icon_svg(icon),
        html_escape::encode_text(label)
    )
}

fn icon_svg(icon: &str) -> &'static str {
    match icon {
        "heart" => {
            r#"<svg aria-hidden="true" viewBox="0 0 24 24"><path d="M12 21s-7-4.4-9.4-8.8C.6 8.5 2.7 4.5 6.7 4.5c2 0 3.5 1.1 4.3 2.4.8-1.3 2.3-2.4 4.3-2.4 4 0 6.1 4 4.1 7.7C19 16.6 12 21 12 21z"/></svg>"#
        }
        "reply" => {
            r#"<svg aria-hidden="true" viewBox="0 0 24 24"><path d="M20 18.5c-1.9-4.7-5.6-6.2-10.5-6.2V17L3 10.5 9.5 4v4.4c6.2 0 10 3.4 10.5 10.1z"/></svg>"#
        }
        "repost" => {
            r#"<svg aria-hidden="true" viewBox="0 0 24 24"><path d="M7 7h9.2l-2-2L16 3l5.5 5.5L16 14l-1.8-2 2-2H8v3H5V9c0-1.1.9-2 2-2zm10 10H7.8l2 2L8 21l-5.5-5.5L8 10l1.8 2-2 2H16v-3h3v4c0 1.1-.9 2-2 2z"/></svg>"#
        }
        "bookmark" => {
            r#"<svg aria-hidden="true" viewBox="0 0 24 24"><path d="M6 3h12c.6 0 1 .4 1 1v17l-7-4-7 4V4c0-.6.4-1 1-1z"/></svg>"#
        }
        "trash" => {
            r#"<svg aria-hidden="true" viewBox="0 0 24 24"><path d="M9 3h6l1 2h4v3H4V5h4l1-2zm-3 7h12l-1 11H7L6 10zm4 2v7h2v-7h-2zm4 0v7h2v-7h-2z"/></svg>"#
        }
        _ => r#"<svg aria-hidden="true" viewBox="0 0 24 24"><circle cx="12" cy="12" r="8"/></svg>"#,
    }
}

fn render_media(media: &MediaView) -> String {
    let path = html_escape::encode_double_quoted_attribute(&media.public_path);
    let alt = html_escape::encode_double_quoted_attribute(&media.alt_text);
    if media.media_kind == "video" {
        format!(r#"<video controls preload="metadata" src="{path}"></video>"#)
    } else {
        format!(r#"<img src="{path}" alt="{alt}" loading="lazy">"#)
    }
}

pub fn linkify(text: &str) -> String {
    html_escape::encode_text(text)
        .split_whitespace()
        .map(|word| {
            if let Some(tag) = word.strip_prefix('#').filter(|value| !value.is_empty()) {
                format!(
                    r##"<a href="/tags/{}">#{}</a>"##,
                    html_escape::encode_double_quoted_attribute(tag),
                    html_escape::encode_text(tag)
                )
            } else if let Some(name) = word.strip_prefix('@').filter(|value| !value.is_empty()) {
                format!(
                    r#"<a href="/users/{}">@{}</a>"#,
                    html_escape::encode_double_quoted_attribute(name),
                    html_escape::encode_text(name)
                )
            } else {
                word.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn empty_state(title: &str, message: &str) -> String {
    format!(
        r#"<section class="empty-state"><h2>{}</h2><p>{}</p></section>"#,
        html_escape::encode_text(title),
        html_escape::encode_text(message)
    )
}

pub fn page_header(title: &str, subtitle: &str) -> String {
    format!(
        r#"<section class="page-header"><h1>{}</h1><p>{}</p></section>"#,
        html_escape::encode_text(title),
        html_escape::encode_text(subtitle)
    )
}

pub fn notice(kind: &str, message: &str) -> String {
    format!(
        r#"<section class="notice {}"><p>{}</p></section>"#,
        html_escape::encode_double_quoted_attribute(kind),
        html_escape::encode_text(message)
    )
}

pub fn error_page(status: StatusCode, message: &str) -> String {
    let title = match status {
        StatusCode::UNAUTHORIZED => "Authentication required",
        StatusCode::FORBIDDEN => "Access denied",
        StatusCode::NOT_FOUND => "Page not found",
        StatusCode::TOO_MANY_REQUESTS => "Slow down",
        StatusCode::BAD_REQUEST => "Check the form",
        _ => "Something went wrong",
    };
    layout(
        None,
        title,
        &format!(
            r#"<section class="panel error-panel"><p class="eyebrow">{} error</p><h1>{}</h1><p>{}</p><p><a class="button-link" href="/home">Back to Home Feed</a></p></section>"#,
            status.as_u16(),
            html_escape::encode_text(title),
            html_escape::encode_text(message)
        ),
        "RustPost",
    )
}

const CSS: &str = r#"
:root{font-family:Inter,ui-sans-serif,system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;color:#202124;background:#f5f6f1;color-scheme:light;line-height:1.5}
*{box-sizing:border-box}body{margin:0;min-width:320px}a{color:#1f5f8b;text-decoration:none}a:hover{text-decoration:underline}
.site-header{position:sticky;top:0;z-index:10;background:rgba(255,255,255,.96);border-bottom:1px solid #d9ded6;backdrop-filter:blur(8px)}
.header-inner{max-width:1120px;margin:0 auto;padding:.8rem 1rem;display:flex;align-items:center;justify-content:space-between;gap:1rem}
.brand{display:flex;align-items:center;gap:.55rem;font-weight:800;color:#172017}.brand-mark{display:inline-grid;place-items:center;width:2rem;height:2rem;border-radius:7px;background:#163b2f;color:#fff}
nav{display:flex;gap:.35rem;align-items:center;flex-wrap:wrap;justify-content:flex-end}nav a,nav button,.button-link{display:inline-flex;align-items:center;min-height:2.15rem;border-radius:7px;padding:.42rem .65rem;color:#24445f;border:1px solid transparent;background:transparent}
nav a:hover,.button-link:hover{background:#eef3f0;text-decoration:none}nav form,.actions form{display:inline}
main{padding:1.25rem}.content-shell{max-width:1120px;margin:0 auto;display:grid;grid-template-columns:minmax(0,720px) 280px;gap:1.25rem;align-items:start}.content-column{min-width:0}.side-panel{position:sticky;top:5rem;background:#fff;border:1px solid #dfe4dc;border-radius:8px;padding:1rem;color:#59625a}.side-panel h2{margin:.1rem 0 .6rem;font-size:1rem;color:#202124}.dashboard-list{display:grid;grid-template-columns:auto minmax(0,1fr);gap:.45rem .75rem;margin:.25rem 0 .85rem}.dashboard-list dt{font-weight:800;color:#202124}.dashboard-list dd{margin:0;overflow-wrap:anywhere}.dashboard-account{color:#202124}.dashboard-account:hover{text-decoration:none}.quick-links{display:flex;flex-wrap:wrap;gap:.4rem}.site-footer{max-width:1120px;margin:0 auto;padding:1rem;color:#687068;font-size:.9rem}.footer-onion{display:block;margin-top:.25rem;overflow-wrap:anywhere}
.page-header,.post,.composer,.panel,.empty-state,.notice{background:#fff;border:1px solid #dfe4dc;border-radius:8px;margin:0 0 .85rem;padding:1rem;box-shadow:0 1px 2px rgba(20,35,30,.04)}
.page-header h1,.section-heading h1,.panel h1{margin:0;font-size:1.45rem;line-height:1.2}.panel h1+table,.panel h1+form,.panel h1+p,.panel h1+dl{margin-top:.85rem}.page-header p,.muted,.empty-state p{color:#667064;margin:.35rem 0 0}.section-heading{display:flex;justify-content:space-between;gap:1rem;align-items:baseline;margin-bottom:.8rem}
label{display:block;font-weight:700;margin:.85rem 0 .35rem}input,textarea,button{font:inherit}input[type=text],input[type=password],input[type=url],input:not([type]),textarea{width:100%;padding:.72rem .8rem;border:1px solid #b9c2b8;border-radius:7px;background:#fff}textarea{resize:vertical;min-height:7rem}
input[type=text].password-visible{padding-right:.8rem}.password-control{display:grid;grid-template-columns:minmax(0,1fr) auto;gap:.45rem;align-items:center}.password-control input{min-width:0}.password-toggle{background:#fff;color:#24445f;border-color:#cdd7d0;min-width:4.5rem}.auth-submit{margin-top:1.15rem}.auth-form{margin-top:.35rem}
input:focus,textarea:focus,button:focus-visible,a:focus-visible{outline:3px solid #93c5fd;outline-offset:2px}button,.primary{border:1px solid #163b2f;background:#163b2f;color:#fff;border-radius:7px;padding:.5rem .8rem;cursor:pointer;font-weight:700}button:hover,.primary:hover{background:#235544;text-decoration:none}
.composer-tools{display:flex;align-items:center;justify-content:space-between;gap:.75rem;margin-top:.85rem}.file-control{display:inline-flex;align-items:center;gap:.6rem;margin:0;color:#24445f;font-weight:700}.file-control input{max-width:15rem}
.timeline{display:grid;gap:.85rem}.post{overflow:hidden;position:relative}.reply-post{margin-left:1.35rem;border-left:4px solid #c8d8d0;background:#fbfcfa}.reply-post::before{content:"";position:absolute;left:-1.35rem;top:1.4rem;width:1.35rem;border-top:2px solid #c8d8d0}.anchor-target{position:absolute;top:-5rem}.post-header{display:flex;justify-content:space-between;gap:.75rem;align-items:flex-start}.author-block{display:flex;gap:.55rem;align-items:center;min-width:0}.post-avatar{width:2rem;height:2rem;object-fit:cover;border-radius:999px;border:1px solid #d0d8d2;background:#eef3f0;flex:0 0 auto;margin:0}.post-avatar.placeholder{display:inline-grid;place-items:center;color:#526159;font-weight:800}.author-name{font-weight:800;color:#202124}.username,.post-time,.counts{color:#687068;font-size:.92rem}.text{white-space:pre-wrap;margin:.75rem 0;line-height:1.55;overflow-wrap:anywhere}.post img,.post video{display:block;max-width:100%;border-radius:8px;border:1px solid #d9ded6;margin-top:.6rem;background:#f6f7f4}.post img.post-avatar{display:block;margin:0;border-radius:999px}
.counts{display:flex;gap:.8rem;flex-wrap:wrap;margin-top:.4rem}.actions{display:flex;gap:.35rem;flex-wrap:wrap;align-items:center;margin-top:.75rem}.icon-button{width:2.2rem;height:2.2rem;display:inline-flex;align-items:center;justify-content:center;border:1px solid #cdd7d0;border-radius:7px;background:#fff;color:#24445f;padding:0}.icon-button svg{width:1.05rem;height:1.05rem;fill:currentColor}.icon-button:hover,.icon-button.active{background:#eef3f0;color:#163b2f;text-decoration:none}.icon-button.disabled,.icon-button:disabled{color:#9aa39d;background:#f4f5f2;border-color:#dfe4dc;cursor:not-allowed}.icon-button.disabled:hover,.icon-button:disabled:hover{background:#f4f5f2;color:#9aa39d}.thread-link{padding:.42rem .65rem}.sr-only{position:absolute;width:1px;height:1px;padding:0;margin:-1px;overflow:hidden;clip:rect(0,0,0,0);white-space:nowrap;border:0}.repost-banner{color:#4b655d;font-size:.9rem;font-weight:800;margin-bottom:.45rem}.unavailable{color:#667064}.empty-state{text-align:center;padding:2rem 1rem}.empty-state h2{margin:0;font-size:1.2rem}.notice.error,.error-panel{border-color:#e6b8a8;background:#fff8f5}.notice.success{border-color:#add7b4;background:#f4fbf5}.eyebrow{text-transform:uppercase;letter-spacing:.08em;font-weight:800;color:#6d766e;font-size:.78rem}
.profile-banner{width:100%;max-height:220px;object-fit:cover;border-radius:8px;border:1px solid #d9ded6;background:#dfe9e1}.profile-heading{display:flex;gap:1rem;align-items:flex-start;margin-top:.85rem}.profile-picture{width:88px;height:88px;object-fit:cover;border-radius:8px;border:1px solid #d9ded6;background:#eef3f0;flex:0 0 auto}.account-list{display:grid;gap:.85rem}.account-row{display:grid;grid-template-columns:auto minmax(0,1fr) auto;gap:.75rem;align-items:center;background:#fff;border:1px solid #dfe4dc;border-radius:8px;padding:1rem}.account-row p{margin:.3rem 0 0;color:#59625a;overflow-wrap:anywhere}.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(180px,1fr));gap:.85rem}.item-list{margin:.75rem 0 0;padding-left:1.2rem}.item-list li{margin:.45rem 0}.panel dl:not(.dashboard-list){display:grid;grid-template-columns:max-content minmax(0,1fr);gap:.45rem .85rem}.panel dl:not(.dashboard-list) dt{font-weight:800}.panel dl:not(.dashboard-list) dd{margin:0;overflow-wrap:anywhere}table{width:100%;border-collapse:collapse}td,th{border-bottom:1px solid #e3e7e0;text-align:left;padding:.55rem;vertical-align:top}pre{white-space:pre-wrap;overflow:auto;max-width:100%}
@media (max-width:900px){.content-shell{grid-template-columns:1fr}.side-panel{position:static;display:none}}
@media (max-width:600px){main{padding:.75rem}.header-inner{align-items:flex-start;flex-direction:column}.site-header{position:static}nav{justify-content:flex-start}.composer-tools,.post-header,.profile-heading,.account-row{align-items:stretch;grid-template-columns:1fr;flex-direction:column}.panel dl:not(.dashboard-list){grid-template-columns:1fr}table{display:block;max-width:100%;overflow-x:auto}.author-block{align-items:flex-start}.file-control{display:block}.file-control input{display:block;max-width:100%;margin-top:.35rem}.reply-post{margin-left:.65rem;padding-left:.8rem}.reply-post::before{left:-.65rem;width:.65rem}.button-link{padding:.42rem .55rem}.counts{gap:.55rem}.page-header h1,.section-heading h1,.panel h1{font-size:1.25rem}}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_uses_configured_site_name() {
        let body = layout(None, "Home Feed", "<p>body</p>", "My Microblog");
        assert!(body.contains("<title>Home Feed - My Microblog</title>"));
        assert!(body.contains("<span>My Microblog</span>"));
        assert!(body.contains("My Microblog alpha"));
        assert!(!body.contains("<span>RustPost</span>"));
    }

    #[test]
    fn layout_replaces_alpha_card_with_dashboard() {
        let body = layout(None, "Home Feed", "<p>body</p>", "My Microblog");
        assert!(body.contains("<h2>Dashboard</h2>"));
        assert!(body.contains("Login required"));
        assert!(!body.contains("Alpha status"));
        assert!(!body.contains("Self-hosted microblog"));
    }

    #[test]
    fn dashboard_uses_account_link_without_duplicate_feed_links_or_onion() {
        let user = CurrentUser {
            id: 1,
            username: "ada".to_owned(),
            display_name: "Ada Lovelace".to_owned(),
            is_admin: false,
            is_suspended: false,
        };
        let body = layout_with_context(
            Some(&user),
            None,
            "Home Feed",
            "<p>body</p>",
            "My Microblog",
            &LayoutContext {
                tor_onion_address: Some("examplehiddenservice.onion".to_owned()),
                follower_count: Some(2),
                following_count: Some(3),
                ..LayoutContext::default()
            },
        );

        let dashboard = body
            .split_once(r#"<aside class="side-panel">"#)
            .and_then(|(_, rest)| rest.split_once("</aside>"))
            .map(|(panel, _)| panel)
            .expect("dashboard panel");
        assert!(dashboard.contains(r#"href="/users/ada""#));
        assert!(dashboard.contains("Ada Lovelace"));
        assert!(!dashboard.contains(r#"href="/home">Home Feed"#));
        assert!(!dashboard.contains(r#"href="/following">Following"#));
        assert!(!dashboard.contains("examplehiddenservice.onion"));
    }

    #[test]
    fn tor_address_renders_only_when_available() {
        let without_tor = layout_with_context(
            None,
            None,
            "Home Feed",
            "<p>body</p>",
            "My Microblog",
            &LayoutContext::default(),
        );
        assert!(!without_tor.contains("examplehiddenservice.onion"));
        assert!(!without_tor.contains("Onion: <code>"));

        let with_tor = layout_with_context(
            None,
            None,
            "Home Feed",
            "<p>body</p>",
            "My Microblog",
            &LayoutContext {
                tor_onion_address: Some("examplehiddenservice.onion".to_owned()),
                ..LayoutContext::default()
            },
        );
        assert!(with_tor.contains("examplehiddenservice.onion"));
        assert!(with_tor.contains("footer-onion"));
    }

    #[test]
    fn composer_has_live_remaining_counter_without_placeholder() {
        let body = composer(Some("csrf"), Some(10));
        assert!(body.contains("280 remaining"));
        assert!(body.contains("data-character-limit=\"280\""));
        assert!(body.contains("What is happening?"));
        assert!(!body.contains("placeholder="));
    }
}
