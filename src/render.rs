use crate::auth::CurrentUser;
use crate::social::{AccountView, MediaView, PostView, TimelineEventKind};
use axum::http::StatusCode;

#[derive(Debug, Clone)]
pub struct LayoutContext {
    pub anonymous_mode_enabled: bool,
    pub tor_onion_address: Option<String>,
    pub follower_count: Option<i64>,
    pub following_count: Option<i64>,
    pub favicon_content_type: &'static str,
}

impl Default for LayoutContext {
    fn default() -> Self {
        Self {
            anonymous_mode_enabled: false,
            tor_onion_address: None,
            follower_count: None,
            following_count: None,
            favicon_content_type: "image/x-icon",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PostRenderOptions {
    pub show_timestamp: bool,
    pub clickable_card: bool,
}

impl PostRenderOptions {
    const fn timeline() -> Self {
        Self {
            show_timestamp: false,
            clickable_card: true,
        }
    }

    const fn thread() -> Self {
        Self {
            show_timestamp: true,
            clickable_card: true,
        }
    }
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
<meta http-equiv="Content-Security-Policy" content="default-src 'self'; img-src 'self' data:; media-src 'self'; style-src 'self' 'unsafe-inline'; form-action 'self'">
<meta http-equiv="X-Content-Type-Options" content="nosniff">
<meta name="referrer" content="same-origin">
<link rel="icon" href="/favicon.ico" type="{}">
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
        html_escape::encode_double_quoted_attribute(context.favicon_content_type),
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

const CLIENT_SCRIPT: &str = r#"function cardInteractiveTarget(target) {
  if (!(target instanceof Element)) {
    return null;
  }
  return target.closest('a,button,input,textarea,select,label,form,[role="button"]');
}

document.addEventListener("click", (event) => {
  if (!(event.target instanceof Element)) {
    return;
  }
  const card = event.target.closest("[data-card-href]");
  if (!card || cardInteractiveTarget(event.target)) {
    return;
  }
  window.location.assign(card.getAttribute("data-card-href"));
});

document.addEventListener("keydown", (event) => {
  if (!(event.target instanceof Element)) {
    return;
  }
  if (event.key !== "Enter" || event.target !== event.target.closest("[data-card-href]")) {
    return;
  }
  window.location.assign(event.target.getAttribute("data-card-href"));
});

document.addEventListener("click", (event) => {
  if (!(event.target instanceof Element)) {
    return;
  }
  const backLink = event.target.closest("[data-history-back]");
  if (!backLink || window.history.length <= 1) {
    return;
  }
  event.preventDefault();
  window.history.back();
});

document.addEventListener("click", (event) => {
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
});

function resetSubmittingForms() {
  document.querySelectorAll('form[data-submitting="true"]').forEach((form) => {
    delete form.dataset.submitting;
    form.querySelectorAll("button:disabled").forEach((button) => {
      button.disabled = false;
    });
  });
}

window.addEventListener("pageshow", (event) => {
  const nav = performance.getEntriesByType("navigation")[0];
  const restored = event.persisted || (nav && nav.type === "back_forward");
  if (restored) {
    resetSubmittingForms();
  }
  if (restored && document.querySelector('form[method="post"] input[name="csrf"]')) {
    window.location.reload();
  }
});

function setButtonState(button, active, label) {
  button.classList.toggle("active", active);
  button.setAttribute("aria-pressed", active ? "true" : "false");
  button.setAttribute("aria-label", label);
  button.setAttribute("title", label);
  const text = button.querySelector("[data-button-label]");
  if (text) {
    text.textContent = label;
  }
}

document.addEventListener("submit", async (event) => {
  const form = event.target.closest("form[data-enhance]");
  if (!form || !window.fetch) {
    return;
  }
  event.preventDefault();
  if (form.dataset.submitting === "true") {
    return;
  }
  form.dataset.submitting = "true";
  const submitter = event.submitter || form.querySelector("button[type=submit]");
  if (submitter) {
    submitter.disabled = true;
  }
  try {
    const formData = new FormData(form);
    const body = form.enctype === "multipart/form-data" ? formData : new URLSearchParams(formData);
    const response = await fetch(form.action, {
      method: form.method || "POST",
      body,
      headers: { "Accept": "application/json", "X-RustPost-Enhance": "1" },
      credentials: "same-origin"
    });
    if (!response.ok) {
      HTMLFormElement.prototype.submit.call(form);
      return;
    }
    const data = await response.json();
    if (data.kind === "follow") {
      const followForm = document.querySelector(`[data-follow-user="${data.user_id}"]`);
      if (followForm) {
        followForm.action = data.action;
        const button = followForm.querySelector("button");
        if (button) {
          button.classList.toggle("active", data.following);
          button.textContent = data.following ? "Following" : "Follow";
          button.setAttribute("aria-pressed", data.following ? "true" : "false");
          button.setAttribute("aria-label", data.following ? "Unfollow this account" : "Follow this account");
          button.setAttribute("title", data.following ? "Unfollow this account" : "Follow this account");
        }
      }
      document.querySelectorAll(`[data-profile-followers="${data.user_id}"]`).forEach((node) => {
        node.textContent = `${data.followers} followers`;
      });
      document.querySelectorAll(`[data-profile-following="${data.user_id}"]`).forEach((node) => {
        node.textContent = `${data.following_count} following`;
      });
    } else if (data.kind === "post-action") {
      document.querySelectorAll(`[data-post-id="${data.post_id}"]`).forEach((post) => {
        const likes = post.querySelector('[data-count="likes"]');
        const reposts = post.querySelector('[data-count="reposts"]');
        if (likes) {
          likes.textContent = `${data.likes} likes`;
        }
        if (reposts) {
          reposts.textContent = `${data.reposts} reposts`;
        }
        const liked = post.querySelector('[data-action-kind="like"]');
        const bookmarked = post.querySelector('[data-action-kind="bookmark"]');
        const reposted = post.querySelector('[data-action-kind="repost"]');
        if (liked) {
          setButtonState(liked, data.liked, data.liked ? "Unlike" : "Like");
        }
        if (bookmarked) {
          setButtonState(bookmarked, data.bookmarked, data.bookmarked ? "Unbookmark" : "Bookmark");
        }
        if (reposted) {
          setButtonState(reposted, data.reposted, data.reposted ? "Unrepost" : "Repost");
        }
      });
    } else if (data.kind === "post-created") {
      let timeline = document.querySelector(".timeline");
      const empty = document.querySelector(".empty-state");
      if (!timeline && empty) {
        empty.outerHTML = '<section class="timeline" aria-label="Posts"></section>';
        timeline = document.querySelector(".timeline");
      }
      if (!timeline) {
        window.location.assign(data.redirect);
        return;
      }
      timeline.insertAdjacentHTML(data.parent_post_id === null ? "afterbegin" : "beforeend", data.html);
      if (data.parent_post_id !== null) {
        document.querySelectorAll(`[data-post-id="${data.parent_post_id}"] [data-count="replies"]`).forEach((node) => {
          const current = Number.parseInt(node.textContent || "0", 10);
          const next = Number.isFinite(current) ? current + 1 : 1;
          node.textContent = `${next} replies`;
        });
      }
      const created = document.getElementById(`post-${data.post_id}`);
      if (created) {
        created.setAttribute("tabindex", "-1");
        created.focus({ preventScroll: true });
      }
      if (window.history && data.redirect) {
        window.history.pushState(null, "", data.redirect);
      }
      form.reset();
      form.querySelectorAll("textarea[data-character-limit]").forEach(updateComposerCount);
    }
  } catch (_err) {
    form.submit();
  } finally {
    delete form.dataset.submitting;
    if (submitter) {
      submitter.disabled = false;
    }
  }
});

document.addEventListener("submit", (event) => {
  const form = event.target.closest("form");
  if (!form || form.dataset.enhance) {
    return;
  }
  if ((form.method || "GET").toUpperCase() !== "POST") {
    return;
  }
  if (form.dataset.submitting === "true") {
    event.preventDefault();
    return;
  }
  form.dataset.submitting = "true";
  const submitter = event.submitter || form.querySelector("button[type=submit]");
  if (submitter) {
    submitter.disabled = true;
  }
});"#;

pub fn client_script() -> &'static str {
    CLIENT_SCRIPT
}

pub fn composer(csrf: Option<&str>, parent: Option<i64>) -> String {
    let parent_input = parent.map_or_else(String::new, |id| {
        format!(r#"<input type="hidden" name="parent_post_id" value="{id}">"#)
    });
    let csrf = csrf.unwrap_or_default();
    let input_id = parent.map_or_else(|| "post-text".to_owned(), |id| format!("reply-text-{id}"));
    format!(
        r#"<section class="composer" id="reply" aria-labelledby="composer-title"><div class="section-heading"><h1 id="composer-title">{}</h1><span class="muted" data-character-counter="{}">280 remaining</span></div><form method="post" action="/posts" enctype="multipart/form-data" data-enhance="post-create">
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
                follow_form(account.id, csrf, true)
            } else {
                follow_form(account.id, csrf, false)
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

pub fn search_page(
    site_name: &str,
    query: &str,
    users: &[AccountView],
    posts: &[PostView],
    user: Option<&CurrentUser>,
    csrf: Option<&str>,
) -> String {
    let form = search_form(site_name, query);
    let state = if query.is_empty() {
        empty_state(
            "Find posts and people",
            "Search for posts, usernames, mentions, or hashtags.",
        )
    } else if users.is_empty() && posts.is_empty() {
        empty_state("No results found", &format!(r#"No matches for "{query}"."#))
    } else {
        search_results(query, users, posts, user, csrf)
    };
    format!("{form}{state}")
}

fn search_form(site_name: &str, query: &str) -> String {
    format!(
        r#"<section class="panel search-panel"><h1>Search</h1><form method="get" action="/search" class="search-form"><label class="sr-only" for="q">Search {}</label><input id="q" name="q" type="search" value="{}" placeholder="Search posts, @users, or #tags" autocomplete="off"><button class="primary" type="submit">Search</button></form></section>"#,
        html_escape::encode_text(site_name),
        html_escape::encode_double_quoted_attribute(query)
    )
}

fn search_results(
    query: &str,
    users: &[AccountView],
    posts: &[PostView],
    user: Option<&CurrentUser>,
    csrf: Option<&str>,
) -> String {
    let total = users.len() + posts.len();
    let plural = if total == 1 { "result" } else { "results" };
    let users_section = search_user_results(users);
    let posts_heading = if users.is_empty() || posts.is_empty() {
        String::new()
    } else {
        r#"<h3 class="section-title">Posts</h3>"#.to_owned()
    };
    let posts_section = if posts.is_empty() {
        String::new()
    } else {
        format!(
            "{posts_heading}{}",
            posts_with_options(posts, user, csrf, PostRenderOptions::timeline())
        )
    };
    format!(
        r#"<section class="search-results" aria-labelledby="search-results-title"><h2 class="section-title" id="search-results-title">{} {} for "{}"</h2>{}{}</section>"#,
        total,
        plural,
        html_escape::encode_text(query),
        users_section,
        posts_section
    )
}

fn search_user_results(users: &[AccountView]) -> String {
    if users.is_empty() {
        return String::new();
    }
    let rows = users
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
            let bio = if account.bio.trim().is_empty() {
                String::new()
            } else {
                format!("<p>{}</p>", html_escape::encode_text(&account.bio))
            };
            format!(
                r#"<article class="account-row search-account">{}<div><a class="author-name" href="/users/{}">{}</a> <span class="username">@{}</span>{}</div></article>"#,
                avatar,
                html_escape::encode_double_quoted_attribute(&account.username),
                html_escape::encode_text(&account.display_name),
                html_escape::encode_text(&account.username),
                bio
            )
        })
        .collect::<Vec<_>>()
        .join("");
    format!(
        r#"<section class="panel search-users" aria-labelledby="search-users-title"><h3 class="section-title" id="search-users-title">People</h3><div class="account-list">{rows}</div></section>"#
    )
}

pub fn follow_form(user_id: i64, csrf: &str, following: bool) -> String {
    let (action, label, aria_label) = if following {
        (
            format!("/users/{user_id}/unfollow"),
            "Following",
            "Unfollow this account",
        )
    } else {
        (
            format!("/users/{user_id}/follow"),
            "Follow",
            "Follow this account",
        )
    };
    format!(
        r#"<form method="post" action="{}" data-enhance="follow" data-follow-user="{}"><input type="hidden" name="csrf" value="{}"><button class="follow-button{}" type="submit" aria-pressed="{}" aria-label="{}" title="{}">{}</button></form>"#,
        html_escape::encode_double_quoted_attribute(&action),
        user_id,
        html_escape::encode_double_quoted_attribute(csrf),
        if following { " active" } else { "" },
        if following { "true" } else { "false" },
        html_escape::encode_double_quoted_attribute(aria_label),
        html_escape::encode_double_quoted_attribute(aria_label),
        html_escape::encode_text(label)
    )
}

pub fn posts(posts: &[PostView], user: Option<&CurrentUser>, csrf: Option<&str>) -> String {
    posts_with_options(posts, user, csrf, PostRenderOptions::timeline())
}

pub fn thread_posts(posts: &[PostView], user: Option<&CurrentUser>, csrf: Option<&str>) -> String {
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
            .enumerate()
            .map(|(index, post)| {
                let mut options = PostRenderOptions::thread();
                if index == 0 {
                    options.clickable_card = false;
                }
                post_card_with_options(post, user, csrf, options)
            })
            .collect::<Vec<_>>()
            .join("")
    )
}

fn posts_with_options(
    posts: &[PostView],
    user: Option<&CurrentUser>,
    csrf: Option<&str>,
    options: PostRenderOptions,
) -> String {
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
            .map(|post| post_card_with_options(post, user, csrf, options))
            .collect::<Vec<_>>()
            .join("")
    )
}

pub fn post_card(post: &PostView, user: Option<&CurrentUser>, csrf: Option<&str>) -> String {
    post_card_with_options(post, user, csrf, PostRenderOptions::timeline())
}

pub fn thread_post_card(post: &PostView, user: Option<&CurrentUser>, csrf: Option<&str>) -> String {
    post_card_with_options(post, user, csrf, PostRenderOptions::thread())
}

// Rendering a post card stays centralized because the markup, counts, media,
// and action controls must remain consistent between timelines and threads.
#[expect(
    clippy::too_many_lines,
    reason = "post card markup is centralized to keep timeline and thread rendering identical"
)]
fn post_card_with_options(
    post: &PostView,
    user: Option<&CurrentUser>,
    csrf: Option<&str>,
    options: PostRenderOptions,
) -> String {
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
        format!(
            r#"<div class="actions">{}{}{}{}{}</div>"#,
            icon_action_form(
                &format!("/posts/{}/like", post.id),
                csrf,
                if post.viewer_liked { "Unlike" } else { "Like" },
                "like",
                "heart",
                post.viewer_liked
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
                    "repost",
                    post.viewer_reposted,
                )
            } else {
                disabled_icon_button("Repost unavailable for your own post", "repost")
            },
            reply_link,
            icon_action_form(
                &format!("/posts/{}/bookmark", post.id),
                csrf,
                if post.viewer_bookmarked {
                    "Unbookmark"
                } else {
                    "Bookmark"
                },
                "bookmark",
                "bookmark",
                post.viewer_bookmarked
            ),
            delete,
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
    let card_attrs = if options.clickable_card {
        format!(
            r#" data-card-href="/posts/{}" tabindex="0" aria-label="Open post {}""#,
            post.id, post.id
        )
    } else {
        String::new()
    };
    let timestamp = if options.show_timestamp {
        format!(
            r#"<a class="post-time" href="/posts/{}">{}</a>"#,
            post.id,
            html_escape::encode_text(&post.created_at)
        )
    } else {
        String::new()
    };
    format!(
        r#"<article class="{}" id="post-{}" data-post-id="{}" data-event-id="{}"{}>{}{}<header class="post-header"><div class="author-block">{}<div>{}</div></div>{}</header><div class="text">{}</div>{}<div class="counts"><span data-count="likes">{} likes</span><span data-count="reposts">{} reposts</span><span data-count="replies">{} replies</span></div>{}</article>"#,
        post_class,
        post.id,
        post.id,
        html_escape::encode_double_quoted_attribute(&post.event_id),
        card_attrs,
        reply_anchor,
        repost_banner,
        avatar,
        author,
        timestamp,
        text,
        media,
        post.like_count,
        post.repost_count,
        post.reply_count,
        controls
    )
}

fn icon_action_form(
    action: &str,
    csrf: &str,
    label: &str,
    kind: &str,
    icon: &str,
    active: bool,
) -> String {
    format!(
        r#"<form method="post" action="{}" data-enhance="post-action"><input type="hidden" name="csrf" value="{}"><button class="icon-button{}" type="submit" data-action-kind="{}" aria-pressed="{}" aria-label="{}" title="{}">{}<span class="sr-only" data-button-label>{}</span></button></form>"#,
        html_escape::encode_double_quoted_attribute(action),
        html_escape::encode_double_quoted_attribute(csrf),
        if active { " active" } else { "" },
        html_escape::encode_double_quoted_attribute(kind),
        if active { "true" } else { "false" },
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

pub fn thread_back_control() -> String {
    r##"<div class="thread-nav"><a class="thread-back" href="/home" data-history-back aria-label="Back" title="Back"><svg aria-hidden="true" viewBox="0 0 24 24"><path d="M11 5 4 12l7 7 1.8-1.8L8.9 13H20v-2H8.9l3.9-4.2L11 5z"/></svg><span class="sr-only">Back</span></a></div>"##
        .to_owned()
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
.page-header,.post,.composer,.panel,.empty-state,.notice{background:#fff;border:1px solid #dfe4dc;border-radius:8px;margin:0 0 .7rem;padding:.85rem;box-shadow:0 1px 2px rgba(20,35,30,.04)}
.page-header h1,.section-heading h1,.panel h1{margin:0;font-size:1.45rem;line-height:1.2}.panel h1+table,.panel h1+form,.panel h1+p,.panel h1+dl{margin-top:.85rem}.page-header p,.muted,.empty-state p{color:#667064;margin:.35rem 0 0}.section-heading{display:flex;justify-content:space-between;gap:1rem;align-items:baseline;margin-bottom:.8rem}
label{display:block;font-weight:700;margin:.85rem 0 .35rem}input,textarea,button{font:inherit}input[type=text],input[type=search],input[type=password],input[type=url],input:not([type]),textarea{width:100%;padding:.72rem .8rem;border:1px solid #b9c2b8;border-radius:7px;background:#fff}textarea{resize:vertical;min-height:7rem}
input[type=text].password-visible{padding-right:.8rem}.password-control{display:grid;grid-template-columns:minmax(0,1fr) auto;gap:.45rem;align-items:center}.password-control input{min-width:0}.password-toggle{background:#fff;color:#24445f;border-color:#cdd7d0;min-width:4.5rem}.auth-submit{margin-top:1.15rem}.auth-form{margin-top:.35rem}
.search-panel h1{margin-bottom:.75rem}.search-form{display:grid;grid-template-columns:minmax(0,1fr) auto;gap:.55rem;align-items:center}.search-form input{min-width:0}.search-results{display:grid;gap:.65rem}.section-title{margin:.2rem 0 .65rem;font-size:1.05rem;color:#202124}.search-users{margin-bottom:0}.search-users .section-title{margin-top:0}.search-account{grid-template-columns:auto minmax(0,1fr)}
input:focus,textarea:focus,button:focus-visible,a:focus-visible{outline:3px solid #93c5fd;outline-offset:2px}button,.primary{border:1px solid #163b2f;background:#163b2f;color:#fff;border-radius:7px;padding:.5rem .8rem;cursor:pointer;font-weight:700}button:hover,.primary:hover{background:#235544;text-decoration:none}
.composer-tools{display:flex;align-items:center;justify-content:space-between;gap:.75rem;margin-top:.85rem}.file-control{display:inline-flex;align-items:center;gap:.6rem;margin:0;color:#24445f;font-weight:700}.file-control input{max-width:15rem}
.thread-nav{display:flex;margin:0 0 .45rem .85rem}.thread-back{width:2rem;height:2rem;display:inline-flex;align-items:center;justify-content:center;border-radius:999px;color:#24445f}.thread-back svg{width:1.2rem;height:1.2rem;fill:currentColor}.thread-back:hover{background:#eef3f0;text-decoration:none}
.timeline{display:grid;gap:.65rem}.post{overflow:hidden;position:relative}.post[data-card-href]{cursor:pointer}.post[data-card-href]:hover{border-color:#c7d2ca}.post[data-card-href]:focus-visible{outline:3px solid #93c5fd;outline-offset:2px}.reply-post{margin-left:1.1rem;border-left:4px solid #c8d8d0;background:#fbfcfa}.reply-post::before{content:"";position:absolute;left:-1.1rem;top:1.25rem;width:1.1rem;border-top:2px solid #c8d8d0}.anchor-target{position:absolute;top:-5rem}.post-header{display:flex;justify-content:space-between;gap:.65rem;align-items:flex-start}.author-block{display:flex;gap:.55rem;align-items:center;min-width:0}.post-avatar{width:2rem;height:2rem;object-fit:cover;border-radius:999px;border:1px solid #d0d8d2;background:#eef3f0;flex:0 0 auto;margin:0}.post-avatar.placeholder{display:inline-grid;place-items:center;color:#526159;font-weight:800}.author-name{font-weight:800;color:#202124}.username,.post-time,.counts{color:#687068;font-size:.92rem}.text{white-space:pre-wrap;margin:.55rem 0;line-height:1.5;overflow-wrap:anywhere}.post img,.post video{display:block;max-width:100%;border-radius:8px;border:1px solid #d9ded6;margin-top:.5rem;background:#f6f7f4}.post img.post-avatar{display:block;margin:0;border-radius:999px}
.counts{display:flex;gap:.5rem;flex-wrap:wrap;margin-top:.3rem;min-height:1.4rem}.actions{display:flex;gap:.25rem;flex-wrap:wrap;align-items:center;margin-top:.5rem}.icon-button{width:2.2rem;height:2.2rem;display:inline-flex;align-items:center;justify-content:center;border:1px solid #cdd7d0;border-radius:7px;background:#fff;color:#24445f;padding:0}.icon-button svg{width:1.05rem;height:1.05rem;fill:currentColor}.icon-button:hover,.icon-button.active{background:#eef3f0;color:#163b2f;text-decoration:none}.icon-button.disabled,.icon-button:disabled{color:#9aa39d;background:#f4f5f2;border-color:#dfe4dc;cursor:not-allowed}.icon-button.disabled:hover,.icon-button:disabled:hover{background:#f4f5f2;color:#9aa39d}.follow-button{min-width:6.6rem}.follow-button.active{background:#eef3f0;color:#163b2f;border-color:#9fb9ad}.profile-actions{margin-top:0}.profile-secondary button{background:#fff;color:#8a3d2d;border-color:#e0c4bb;padding:.32rem .5rem;min-height:1.85rem;font-size:.86rem}.profile-secondary button:hover{background:#fff8f5;color:#6f2f22}.profile-title-row{display:flex;align-items:flex-start;justify-content:space-between;gap:.75rem}.sr-only{position:absolute;width:1px;height:1px;padding:0;margin:-1px;overflow:hidden;clip:rect(0,0,0,0);white-space:nowrap;border:0}.repost-banner{color:#4b655d;font-size:.9rem;font-weight:800;margin-bottom:.35rem}.unavailable{color:#667064}.empty-state{text-align:center;padding:2rem 1rem}.empty-state h2{margin:0;font-size:1.2rem}.notice.error,.error-panel{border-color:#e6b8a8;background:#fff8f5}.notice.success{border-color:#add7b4;background:#f4fbf5}.eyebrow{text-transform:uppercase;letter-spacing:.08em;font-weight:800;color:#6d766e;font-size:.78rem}
.profile-banner{width:100%;max-height:220px;object-fit:cover;border-radius:8px;border:1px solid #d9ded6;background:#dfe9e1}.profile-heading{display:flex;gap:1rem;align-items:flex-start;margin-top:.85rem}.profile-main{min-width:0;flex:1}.profile-picture{width:88px;height:88px;object-fit:cover;border-radius:8px;border:1px solid #d9ded6;background:#eef3f0;flex:0 0 auto}.favicon-preview{width:32px;height:32px;object-fit:contain;border:1px solid #d9ded6;border-radius:6px;background:#fff}.account-list{display:grid;gap:.65rem}.account-row{display:grid;grid-template-columns:auto minmax(0,1fr) auto;gap:.75rem;align-items:center;background:#fff;border:1px solid #dfe4dc;border-radius:8px;padding:.85rem}.account-row p{margin:.3rem 0 0;color:#59625a;overflow-wrap:anywhere}.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(180px,1fr));gap:.85rem}.item-list{margin:.75rem 0 0;padding-left:1.2rem}.item-list li{margin:.45rem 0}.panel dl:not(.dashboard-list){display:grid;grid-template-columns:max-content minmax(0,1fr);gap:.45rem .85rem}.panel dl:not(.dashboard-list) dt{font-weight:800}.panel dl:not(.dashboard-list) dd{margin:0;overflow-wrap:anywhere}table{width:100%;border-collapse:collapse}td,th{border-bottom:1px solid #e3e7e0;text-align:left;padding:.55rem;vertical-align:top}pre{white-space:pre-wrap;overflow:auto;max-width:100%}
@media (max-width:900px){.content-shell{grid-template-columns:1fr}.side-panel{position:static;display:none}}
@media (max-width:600px){main{padding:.75rem}.header-inner{align-items:flex-start;flex-direction:column}.site-header{position:static}nav{justify-content:flex-start}.search-form{grid-template-columns:1fr}.search-form button{width:100%}.composer-tools,.post-header,.profile-heading,.profile-title-row,.account-row{align-items:stretch;grid-template-columns:1fr;flex-direction:column}.panel dl:not(.dashboard-list){grid-template-columns:1fr}table{display:block;max-width:100%;overflow-x:auto}.author-block{align-items:flex-start}.file-control{display:block}.file-control input{display:block;max-width:100%;margin-top:.35rem}.reply-post{margin-left:.65rem;padding-left:.8rem}.reply-post::before{left:-.65rem;width:.65rem}.button-link{padding:.42rem .55rem}.counts{gap:.45rem}.page-header h1,.section-heading h1,.panel h1{font-size:1.25rem}}
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

    #[test]
    fn timeline_cards_are_clickable_without_exposing_timestamps() {
        let post = test_post();
        let body = post_card(&post, None, None);

        assert!(body.contains(r#"data-card-href="/posts/42""#));
        assert!(body.contains(r#"tabindex="0""#));
        assert!(!body.contains(r#">Open post</a>"#));
        assert!(!body.contains("Open thread"));
        assert!(!body.contains("2026-05-18 10:30"));
        assert!(!body.contains(r#"class="post-time""#));
    }

    #[test]
    fn thread_cards_show_timestamps() {
        let post = test_post();
        let body = thread_post_card(&post, None, None);

        assert!(body.contains(r#"class="post-time" href="/posts/42">2026-05-18 10:30</a>"#));
    }

    #[test]
    fn thread_posts_do_not_make_root_card_self_navigating() {
        let root = test_post();
        let mut reply = test_post();
        reply.id = 43;
        reply.parent_post_id = Some(42);
        reply.event_id = "p:43".to_owned();

        let body = thread_posts(&[root, reply], None, None);

        let root_card = body
            .split_once(r#"id="post-42""#)
            .and_then(|(_, rest)| rest.split_once(r#"id="post-43""#))
            .map(|(card, _)| card)
            .expect("root card");
        assert!(!root_card.contains(r#"data-card-href="/posts/42""#));
        assert!(!root_card.contains(r#"tabindex="0""#));
        assert!(body.contains(r#"data-card-href="/posts/43""#));
    }

    #[test]
    fn thread_back_control_has_history_hook_and_home_fallback() {
        let body = thread_back_control();

        assert!(body.contains(r#"aria-label="Back""#));
        assert!(body.contains(r#"href="/home""#));
        assert!(body.contains("data-history-back"));
    }

    #[test]
    fn compact_post_avatar_uses_profile_picture_path() {
        let mut post = test_post();
        post.profile_picture_path = Some("/uploads/thumbs/ada-profile.webp".to_owned());

        let body = post_card(&post, None, None);

        assert!(
            body.contains(r#"<img class="post-avatar" src="/uploads/thumbs/ada-profile.webp""#)
        );
        assert!(!body.contains(r#"post-avatar placeholder"#));
    }

    #[test]
    fn compact_post_avatar_uses_placeholder_without_profile_picture() {
        let post = test_post();
        let body = post_card(&post, None, None);

        assert!(
            body.contains(r#"<span class="post-avatar placeholder" aria-hidden="true">A</span>"#)
        );
    }

    #[test]
    fn search_page_preserves_and_escapes_query() {
        let body = search_page("RustPost", r#"<rust> "query""#, &[], &[], None, None);

        assert!(body.contains(r#"value="&lt;rust&gt; &quot;query&quot;""#));
        assert!(body.contains("No results found"));
        assert!(body.contains("No matches for"));
        assert!(body.contains("&lt;rust&gt;"));
        assert!(body.contains("&quot;query&quot;"));
        assert!(!body.contains(r#"<rust> "query""#));
    }

    #[test]
    fn search_page_uses_post_cards_for_results() {
        let post = test_post();
        let account = AccountView {
            id: 1,
            username: "ada".to_owned(),
            display_name: "Ada".to_owned(),
            bio: "Computing notes".to_owned(),
            profile_picture_path: None,
            viewer_following: false,
        };

        let body = search_page("RustPost", "ada", &[account], &[post], None, None);

        assert!(body.contains(r#"2 results for "ada""#));
        assert!(body.contains(r#"id="search-users-title">People"#));
        assert!(body.contains(r#"class="account-row search-account""#));
        assert!(body.contains(r#"data-card-href="/posts/42""#));
    }

    #[test]
    fn card_actions_stay_inside_compact_action_row() {
        let user = CurrentUser {
            id: 1,
            username: "ada".to_owned(),
            display_name: "Ada".to_owned(),
            is_admin: false,
            is_suspended: false,
        };
        let mut post = test_post();
        post.user_id = Some(2);
        post.viewer_can_repost = true;
        let body = post_card(&post, Some(&user), Some("csrf"));

        assert!(
            body.contains(r#"<div class="actions"><form method="post" action="/posts/42/like""#)
        );
        let like = body
            .find(r#"data-action-kind="like""#)
            .expect("like action");
        let repost = body
            .find(r#"data-action-kind="repost""#)
            .expect("repost action");
        let reply = body.find(r#"aria-label="Reply""#).expect("reply action");
        let bookmark = body
            .find(r#"data-action-kind="bookmark""#)
            .expect("bookmark action");
        assert!(like < repost);
        assert!(repost < reply);
        assert!(reply < bookmark);
    }

    fn test_post() -> PostView {
        PostView {
            event_id: "p:42".to_owned(),
            event_kind: TimelineEventKind::Post,
            id: 42,
            user_id: Some(1),
            username: Some("ada".to_owned()),
            display_name: Some("Ada".to_owned()),
            profile_picture_path: None,
            anonymous_label: None,
            text: "hello".to_owned(),
            parent_post_id: None,
            created_at: "2026-05-18 10:30".to_owned(),
            event_created_at: "2026-05-18 10:30".to_owned(),
            like_count: 1,
            repost_count: 2,
            reply_count: 3,
            viewer_liked: false,
            viewer_bookmarked: false,
            viewer_reposted: false,
            viewer_can_repost: false,
            original_unavailable: false,
            reposted_by_user_id: None,
            reposted_by_username: None,
            reposted_by_display_name: None,
            reposted_at: None,
            media: Vec::new(),
        }
    }
}
