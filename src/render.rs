use crate::auth::{CurrentUser, Theme};
use crate::social::{
    AccountView, MediaView, NotificationView, PostView, QuotePreview, TimelineEventKind,
};
use axum::http::StatusCode;
use chrono::{DateTime, NaiveDateTime, Utc};

#[derive(Debug, Clone)]
pub struct LayoutContext {
    pub anonymous_mode_enabled: bool,
    pub tor_onion_address: Option<String>,
    pub follower_count: Option<i64>,
    pub following_count: Option<i64>,
    pub notification_unread_count: Option<i64>,
    pub favicon_content_type: &'static str,
}

impl Default for LayoutContext {
    fn default() -> Self {
        Self {
            anonymous_mode_enabled: false,
            tor_onion_address: None,
            follower_count: None,
            following_count: None,
            notification_unread_count: None,
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
            nav_link("/admin", "Admin", "admin")
        } else {
            String::new()
        };
        let notifications = notification_nav_link(context.notification_unread_count.unwrap_or(0));
        let logout = csrf.map_or_else(String::new, |token| {
            format!(
                r#"<form method="post" action="/logout"><input type="hidden" name="csrf" value="{}"><button>{}<span>Log out</span></button></form>"#,
                html_escape::encode_double_quoted_attribute(token),
                icon_svg("log-out")
            )
        });
        let profile = nav_link(&format!("/users/{}", user.username), "Profile", "profile");
        format!(
            "{}{}{}{notifications}{}{}{admin}{logout}",
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
<html lang="en" data-theme="{}">
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
<header class="site-header"><div class="header-inner"><a class="brand" href="/home"><span class="brand-mark">{}</span><span>{}</span></a><nav class="mobile-nav" aria-label="Primary">{}</nav></div></header>
<section class="noscript-banner" role="status"><strong>JavaScript is disabled.</strong> RustPost will use standard links and forms.</section>
<main><div class="app-shell" data-testid="app-shell">{}<section class="primary-column" data-testid="primary-column">{} </section>{}</div></main>
<footer class="site-footer">{} alpha{}</footer>
</body>
</html>"#,
        html_escape::encode_double_quoted_attribute(theme),
        html_escape::encode_double_quoted_attribute(context.favicon_content_type),
        html_escape::encode_text(title),
        html_escape::encode_text(site_name),
        CSS,
        html_escape::encode_text(&brand_mark.to_string()),
        html_escape::encode_text(site_name),
        auth_nav,
        left_rail,
        body,
        side_panel,
        html_escape::encode_text(site_name),
        footer_onion
    )
}

fn nav_link(href: &str, label: &str, icon: &str) -> String {
    format!(
        r#"<a href="{}">{}<span>{}</span></a>"#,
        html_escape::encode_double_quoted_attribute(href),
        icon_svg(icon),
        html_escape::encode_text(label)
    )
}

fn left_rail(_user: Option<&CurrentUser>, nav: &str) -> String {
    format!(
        r#"<aside class="left-rail" data-testid="left-rail"><nav class="rail-nav" aria-label="Primary">{nav}</nav></aside>"#
    )
}

fn notification_nav_link(unread_count: i64) -> String {
    if unread_count > 0 {
        format!(
            r#"<a href="/notifications">{}<span>Notifications</span> <span class="nav-badge" aria-label="{unread_count} unread notifications">{unread_count}</span></a>"#,
            icon_svg("bell")
        )
    } else {
        nav_link("/notifications", "Notifications", "bell")
    }
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
            user.map_or_else(String::new, |user| {
                let username = html_escape::encode_double_quoted_attribute(&user.username);
                let display_name = html_escape::encode_double_quoted_attribute(&user.display_name);
                format!(
                    r#"<dt>Social</dt><dd><a data-testid="dashboard-followers-link" href="/users/{username}/followers" aria-label="View followers for {display_name}">{followers} followers</a><br><a data-testid="dashboard-following-link" href="/users/{username}/following" aria-label="View users {display_name} follows">{following} following</a></dd>"#
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

pub fn login_form(message: Option<&str>, min_password_length: usize) -> String {
    let notice = message.map_or_else(String::new, |message| notice("error", message));
    let hint = password_requirement_hint(min_password_length);
    let attrs = password_length_attrs(min_password_length, "password-requirement");
    format!(
        r#"<section class="panel form-card auth-panel" data-testid="form-card"><h1>Log in</h1>{notice}<form method="post" class="auth-form"><label for="username">Username</label><input id="username" name="username" autocomplete="username" required><label for="password">Password</label><p class="field-help" id="password-requirement">{hint}</p><div class="password-control"><input id="password" name="password" type="password" autocomplete="current-password"{attrs}><button type="button" class="password-toggle" data-password-toggle="password" aria-label="Show password">Show</button></div><button class="auth-submit" type="submit">Log in</button></form></section>"#
    )
}

pub fn register_form(
    message: Option<&str>,
    min_password_length: usize,
    captcha: Option<&crate::registration_captcha::RegistrationCaptchaChallenge>,
) -> String {
    let notice = message.map_or_else(String::new, |message| notice("error", message));
    let hint = password_requirement_hint(min_password_length);
    let password_attrs = password_length_attrs(min_password_length, "password-requirement");
    let confirm_attrs = password_length_attrs(min_password_length, "confirm-password-requirement");
    let captcha_html = captcha.map_or_else(String::new, register_captcha_fields);
    format!(
        r#"<section class="panel form-card auth-panel" data-testid="form-card"><h1>Create account</h1>{notice}<form method="post" class="auth-form"><label for="username">Username</label><input id="username" name="username" autocomplete="username" required><label for="password">Password</label><p class="field-help" id="password-requirement">{hint}</p><div class="password-control"><input id="password" name="password" type="password" autocomplete="new-password"{password_attrs}><button type="button" class="password-toggle" data-password-toggle="password" aria-label="Show password">Show</button></div><label for="confirm_password">Confirm password</label><p class="field-help" id="confirm-password-requirement">{hint}</p><div class="password-control"><input id="confirm_password" name="confirm_password" type="password" autocomplete="new-password"{confirm_attrs}><button type="button" class="password-toggle" data-password-toggle="confirm_password" aria-label="Show password confirmation">Show</button></div>{captcha_html}<button class="auth-submit" type="submit">Create account</button></form></section>"#
    )
}

fn register_captcha_fields(
    captcha: &crate::registration_captcha::RegistrationCaptchaChallenge,
) -> String {
    format!(
        r#"<fieldset class="captcha-challenge"><legend>Registration CAPTCHA</legend><input type="hidden" name="captcha_token" value="{}"><img class="captcha-image" src="{}" alt="CAPTCHA challenge image"><label for="captcha_answer">CAPTCHA answer</label><p class="field-help" id="captcha-help">Enter the characters shown in the image. The challenge expires in {} minutes. If it is hard to read, reload this page for a new challenge.</p><input id="captcha_answer" name="captcha_answer" autocomplete="off" autocapitalize="characters" spellcheck="false" required aria-describedby="captcha-help"></fieldset>"#,
        html_escape::encode_double_quoted_attribute(&captcha.token),
        html_escape::encode_double_quoted_attribute(&captcha.image_data_uri),
        captcha.expires_minutes,
    )
}

pub fn password_length_attrs(min_password_length: usize, described_by: &str) -> String {
    let described_by = html_escape::encode_double_quoted_attribute(described_by);
    if min_password_length == 0 {
        format!(r#" aria-describedby="{described_by}""#)
    } else {
        format!(r#" minlength="{min_password_length}" required aria-describedby="{described_by}""#)
    }
}

fn password_requirement_hint(min_password_length: usize) -> String {
    if min_password_length == 0 {
        "No minimum password length is currently required.".to_owned()
    } else if min_password_length == 1 {
        "Password must be at least 1 character.".to_owned()
    } else {
        format!("Password must be at least {min_password_length} characters.")
    }
}

const CLIENT_SCRIPT: &str = r#"document.documentElement.classList.add("js-enabled");

function cardInteractiveTarget(target) {
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

function closeRepostMenus(except) {
  document.querySelectorAll("[data-repost-menu]").forEach((menu) => {
    if (menu === except) {
      return;
    }
    menu.hidden = true;
    const button = document.querySelector(`[aria-controls="${menu.id}"]`);
    if (button) {
      button.setAttribute("aria-expanded", "false");
    }
  });
}

function openRepostMenu(button) {
  const menu = document.getElementById(button.getAttribute("aria-controls"));
  if (!menu) {
    return;
  }
  closeRepostMenus(menu);
  menu.hidden = false;
  button.setAttribute("aria-expanded", "true");
  const firstItem = menu.querySelector('[role="menuitem"]');
  if (firstItem) {
    firstItem.focus();
  }
}

document.addEventListener("pointerdown", (event) => {
  const button = event.target.closest("[data-repost-menu-button]");
  if (!button) {
    return;
  }
  const timer = window.setTimeout(() => {
    button.dataset.longPressOpen = "true";
    openRepostMenu(button);
  }, 450);
  button.dataset.longPressTimer = String(timer);
});

document.addEventListener("pointerup", (event) => {
  const button = event.target.closest("[data-repost-menu-button]");
  if (!button || !button.dataset.longPressTimer) {
    return;
  }
  window.clearTimeout(Number.parseInt(button.dataset.longPressTimer, 10));
  delete button.dataset.longPressTimer;
});

document.addEventListener("click", (event) => {
  const button = event.target.closest("[data-repost-menu-button]");
  if (button && button.dataset.longPressOpen === "true") {
    event.preventDefault();
    event.stopPropagation();
    delete button.dataset.longPressOpen;
    return;
  }
  if (!event.target.closest("[data-repost-control]")) {
    closeRepostMenus(null);
  }
});

document.addEventListener("keydown", (event) => {
  const button = event.target.closest("[data-repost-menu-button]");
  if (button && (event.key === "ArrowDown" || event.key === "F10")) {
    event.preventDefault();
    openRepostMenu(button);
    return;
  }
  if (event.key === "Escape") {
    closeRepostMenus(null);
  }
});

function updateComposerCount(textarea) {
  const counter = document.querySelector(`[data-character-counter="${textarea.id}"]`);
  if (!counter) {
    return;
  }
  const max = Number.parseInt(textarea.getAttribute("data-character-limit") || "0", 10);
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

pub fn composer(csrf: Option<&str>, parent: Option<i64>, max_text_chars: usize) -> String {
    let parent_input = parent.map_or_else(String::new, |id| {
        format!(r#"<input type="hidden" name="parent_post_id" value="{id}">"#)
    });
    let csrf = csrf.unwrap_or_default();
    let input_id = parent.map_or_else(|| "post-text".to_owned(), |id| format!("reply-text-{id}"));
    let placeholder = if parent.is_some() {
        "Write a reply..."
    } else {
        "What's happening?"
    };
    format!(
        r#"<section class="composer" id="reply" aria-labelledby="composer-title"><div class="section-heading"><h1 id="composer-title">{}</h1><span class="muted" data-character-counter="{}">{} remaining</span></div><form method="post" action="/posts" enctype="multipart/form-data" data-enhance="post-create">
<input type="hidden" name="csrf" value="{}">{}
<label class="sr-only" for="{}">What is happening?</label>
<textarea id="{}" name="text" maxlength="{}" rows="4" data-character-limit="{}" placeholder="{}"></textarea>
<div class="composer-tools"><label class="file-control" for="media">Attach media<input id="media" name="media" type="file" multiple accept="image/*,video/mp4,video/webm,video/quicktime"></label><button class="primary" type="submit">Post</button></div>
</form></section>"#,
        if parent.is_some() {
            "Reply"
        } else {
            "New post"
        },
        html_escape::encode_double_quoted_attribute(&input_id),
        max_text_chars,
        html_escape::encode_double_quoted_attribute(csrf),
        parent_input,
        html_escape::encode_double_quoted_attribute(&input_id),
        html_escape::encode_double_quoted_attribute(&input_id),
        max_text_chars,
        max_text_chars,
        html_escape::encode_double_quoted_attribute(placeholder)
    )
}

pub fn quote_composer(csrf: &str, quote: &QuotePreview, max_text_chars: usize) -> String {
    let preview = quote_preview_card(quote);
    format!(
        r#"<section class="composer quote-composer" aria-labelledby="composer-title"><div class="section-heading"><h1 id="composer-title">Quote post</h1><span class="muted" data-character-counter="quote-text">{max_text_chars} remaining</span></div>{preview}<form method="post" action="/posts/{}/quote" class="quote-form">
<input type="hidden" name="csrf" value="{}">
<label class="sr-only" for="quote-text">Add your comment</label>
<textarea id="quote-text" name="text" maxlength="{}" rows="4" data-character-limit="{}" placeholder="Add your thoughts..." required></textarea>
<div class="composer-tools"><span></span><button class="primary" type="submit">Post quote</button></div>
</form></section>"#,
        quote.id,
        html_escape::encode_double_quoted_attribute(csrf),
        max_text_chars,
        max_text_chars
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

pub fn account_links(accounts: &[AccountView], empty_message: &str) -> String {
    if accounts.is_empty() {
        return empty_state(empty_message, "");
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
            format!(
                r#"<article class="account-row" data-testid="account-row">{}<div><a class="author-name" href="/users/{}">{}</a> <span class="username">@{}</span></div></article>"#,
                avatar,
                html_escape::encode_double_quoted_attribute(&account.username),
                html_escape::encode_text(&account.display_name),
                html_escape::encode_text(&account.username)
            )
        })
        .collect::<Vec<_>>()
        .join("");
    format!(r#"<section class="account-list" data-testid="account-list">{rows}</section>"#)
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
            r#"<article class="post unavailable" data-testid="post-card" id="post-{}" data-event-id="{}">{}<div class="text">This post is no longer available.</div></article>"#,
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
            r#"<div class="actions" data-testid="post-actions">{}{}{}{}{}</div>"#,
            icon_action_form(
                &format!("/posts/{}/like", post.id),
                csrf,
                if post.viewer_liked { "Unlike" } else { "Like" },
                "like",
                "heart",
                post.viewer_liked
            ),
            if post.viewer_can_repost {
                repost_action_with_quote(
                    &format!("/posts/{}/repost", post.id),
                    &format!("/posts/{}/quote", post.id),
                    csrf,
                    if post.viewer_reposted {
                        "Unrepost"
                    } else {
                        "Repost"
                    },
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
        format!(r#" data-card-href="/posts/{}""#, post.id)
    } else {
        String::new()
    };
    let permalink = if options.clickable_card {
        format!(
            r#"<a class="post-permalink" href="/posts/{}">Open post</a>"#,
            post.id
        )
    } else {
        String::new()
    };
    let timestamp = if options.show_timestamp {
        format!(
            r#"<span class="post-time">{}</span>"#,
            html_escape::encode_text(&post.created_at)
        )
    } else {
        String::new()
    };
    let quote = post
        .quote
        .as_ref()
        .map_or_else(String::new, quote_preview_card);
    format!(
        r#"<article class="{}" data-testid="post-card" id="post-{}" data-post-id="{}" data-event-id="{}"{}>{}{}<header class="post-header"><div class="author-block">{}<div>{}</div></div>{}</header><div class="text">{}</div>{}{}<div class="counts"><span data-count="likes">{} likes</span><span data-count="reposts">{} reposts</span><span data-count="replies">{} replies</span>{}</div>{}</article>"#,
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
        quote,
        post.like_count,
        post.repost_count,
        post.reply_count,
        permalink,
        controls
    )
}

fn repost_action_with_quote(
    action: &str,
    quote_href: &str,
    csrf: &str,
    label: &str,
    active: bool,
) -> String {
    let menu_id = format!("quote-menu-{}", action.trim_matches('/').replace('/', "-"));
    format!(
        r#"<div class="repost-control" data-repost-control><form method="post" action="{}" data-enhance="post-action"><input type="hidden" name="csrf" value="{}"><button class="icon-button{}" type="submit" data-action-kind="repost" data-repost-menu-button aria-haspopup="menu" aria-expanded="false" aria-controls="{}" aria-pressed="{}" aria-label="{}" title="{}">{}<span class="sr-only" data-button-label>{}</span></button></form><div class="repost-menu" id="{}" role="menu" data-repost-menu hidden><a role="menuitem" href="{}">{}<span>Quote post</span></a></div><a class="icon-button quote-fallback" href="{}" aria-label="Quote post" title="Quote post">{}<span class="sr-only">Quote post</span></a></div>"#,
        html_escape::encode_double_quoted_attribute(action),
        html_escape::encode_double_quoted_attribute(csrf),
        if active { " active" } else { "" },
        html_escape::encode_double_quoted_attribute(&menu_id),
        if active { "true" } else { "false" },
        html_escape::encode_double_quoted_attribute(label),
        html_escape::encode_double_quoted_attribute(label),
        icon_svg("repost"),
        html_escape::encode_text(label),
        html_escape::encode_double_quoted_attribute(&menu_id),
        html_escape::encode_double_quoted_attribute(quote_href),
        icon_svg("quote-post"),
        html_escape::encode_double_quoted_attribute(quote_href),
        icon_svg("quote-post")
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

fn quote_preview_card(quote: &QuotePreview) -> String {
    if quote.unavailable {
        return r#"<aside class="quote-preview unavailable" aria-label="Quoted post"><p>Quoted post is no longer available.</p></aside>"#
            .to_owned();
    }
    let author = match (&quote.username, &quote.anonymous_label) {
        (Some(username), _) => format!(
            r#"<a class="author-name" href="/users/{}">{}</a> <span class="username">@{}</span>"#,
            html_escape::encode_double_quoted_attribute(username),
            html_escape::encode_text(quote.display_name.as_deref().unwrap_or(username)),
            html_escape::encode_text(username)
        ),
        (None, Some(label)) => html_escape::encode_text(label).to_string(),
        _ => "Deleted user".to_owned(),
    };
    format!(
        r#"<aside class="quote-preview" aria-label="Quoted post"><a class="quote-link" href="/posts/{}"><span class="quote-author">{}</span><span class="quote-text">{}</span><span class="quote-time">{}</span></a></aside>"#,
        quote.id,
        author,
        linkify(&quote.text),
        html_escape::encode_text(&quote.created_at)
    )
}

fn icon_svg(icon: &str) -> &'static str {
    match icon {
        "home" => {
            r#"<svg aria-hidden="true" viewBox="0 0 24 24"><path d="M3 11.3 12 3l9 8.3-2 2.1-1.1-1V20h-4.6v-5.2h-2.6V20H6.1v-7.6l-1.1 1-2-2.1z"/></svg>"#
        }
        "users" => {
            r#"<svg aria-hidden="true" viewBox="0 0 24 24"><path d="M9 11.5a4 4 0 1 1 0-8 4 4 0 0 1 0 8zm-7 8.2c.4-4.3 3.1-6.5 7-6.5s6.6 2.2 7 6.5V21H2v-1.3zm14.5-7.3a3.4 3.4 0 1 0 0-6.8 3.4 3.4 0 0 0-.9.1 5.8 5.8 0 0 1-1.3 6.4c.7.1 1.5.2 2.2.3zm.2 1.7c2.9.4 4.9 2.3 5.3 5.6V21h-3.9v-1.6a8.7 8.7 0 0 0-1.4-5.3z"/></svg>"#
        }
        "search" => {
            r#"<svg aria-hidden="true" viewBox="0 0 24 24"><path d="M10.5 4a6.5 6.5 0 0 1 5.1 10.5l4.4 4.4-2.1 2.1-4.4-4.4A6.5 6.5 0 1 1 10.5 4zm0 3a3.5 3.5 0 1 0 0 7 3.5 3.5 0 0 0 0-7z"/></svg>"#
        }
        "bell" => {
            r#"<svg aria-hidden="true" viewBox="0 0 24 24"><path d="M12 22a2.8 2.8 0 0 0 2.7-2h-5.4A2.8 2.8 0 0 0 12 22zm7-6.5-1.8-2.1V9a5.2 5.2 0 0 0-3.7-5V2h-3v2A5.2 5.2 0 0 0 6.8 9v4.4L5 15.5V18h14v-2.5z"/></svg>"#
        }
        "profile" => {
            r#"<svg aria-hidden="true" viewBox="0 0 24 24"><path d="M12 12a4.5 4.5 0 1 0 0-9 4.5 4.5 0 0 0 0 9zm-8 8.2c.5-4.6 3.6-7 8-7s7.5 2.4 8 7V22H4v-1.8z"/></svg>"#
        }
        "admin" => {
            r#"<svg aria-hidden="true" viewBox="0 0 24 24"><path d="M12 2 20 5v6c0 5.1-3.2 8.7-8 11-4.8-2.3-8-5.9-8-11V5l8-3zm0 4-4 1.5V11c0 3.1 1.5 5.4 4 7 2.5-1.6 4-3.9 4-7V7.5L12 6z"/></svg>"#
        }
        "log-in" => {
            r#"<svg aria-hidden="true" viewBox="0 0 24 24"><path d="M4 4h8v3H7v10h5v3H4V4zm10.5 3.2L20.3 12l-5.8 4.8-1.9-2.3 1.8-1.5H9v-3h5.4l-1.8-1.5 1.9-2.3z"/></svg>"#
        }
        "log-out" => {
            r#"<svg aria-hidden="true" viewBox="0 0 24 24"><path d="M4 4h8v3H7v10h5v3H4V4zm12.5 3.2 1.9 2.3-1.8 1.5H11v3h5.6l-1.8 1.5 1.9 2.3 5.8-4.8-6-4.8z"/></svg>"#
        }
        "user-plus" => {
            r#"<svg aria-hidden="true" viewBox="0 0 24 24"><path d="M9.5 12a4.5 4.5 0 1 0 0-9 4.5 4.5 0 0 0 0 9zM2 20.5c.5-4.4 3.3-6.7 7.5-6.7 1.7 0 3.1.4 4.2 1.1A6.8 6.8 0 0 0 12.5 21H2v-.5zM18 14v3h3v3h-3v3h-3v-3h-3v-3h3v-3h3z"/></svg>"#
        }
        "heart" => {
            r#"<svg aria-hidden="true" viewBox="0 0 24 24"><path d="M12 21s-7-4.4-9.4-8.8C.6 8.5 2.7 4.5 6.7 4.5c2 0 3.5 1.1 4.3 2.4.8-1.3 2.3-2.4 4.3-2.4 4 0 6.1 4 4.1 7.7C19 16.6 12 21 12 21z"/></svg>"#
        }
        "reply" => {
            r#"<svg aria-hidden="true" viewBox="0 0 24 24"><path d="M20 18.5c-1.9-4.7-5.6-6.2-10.5-6.2V17L3 10.5 9.5 4v4.4c6.2 0 10 3.4 10.5 10.1z"/></svg>"#
        }
        "repost" => {
            r#"<svg aria-hidden="true" viewBox="0 0 24 24"><path d="M7 7h9.2l-2-2L16 3l5.5 5.5L16 14l-1.8-2 2-2H8v3H5V9c0-1.1.9-2 2-2zm10 10H7.8l2 2L8 21l-5.5-5.5L8 10l1.8 2-2 2H16v-3h3v4c0 1.1-.9 2-2 2z"/></svg>"#
        }
        "quote-post" => {
            r#"<svg aria-hidden="true" viewBox="0 0 24 24"><path d="M5 4h10c1.1 0 2 .9 2 2v6.2l3 2.8-3 2.8V18c0 1.1-.9 2-2 2H5c-1.1 0-2-.9-2-2V6c0-1.1.9-2 2-2zm1 3v10h8v-4.6l1.9 1.8.9-.9-2.8-2.7-2.8 2.7.9.9L14 12.4V7H6zm2 3.2c0-1.3.8-2.2 2.2-2.2v1.4c-.5 0-.8.2-.8.8h1.2V13H8v-2.8zm4 0c0-1.3.8-2.2 2.2-2.2v1.4c-.5 0-.8.2-.8.8h1.2V13H12v-2.8z"/></svg>"#
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

pub fn notifications_page(
    notifications: &[NotificationView],
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
            empty_state(
                "No notifications yet",
                "Replies, likes, reposts, follows, and mentions will appear here."
            )
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
        grouped_notification_rows(notifications)
    )
}

fn grouped_notification_rows(notifications: &[NotificationView]) -> String {
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
        html.push_str(&notification_row(notification));
    }
    html
}

fn notification_group(notification: &NotificationView) -> &'static str {
    if notification.read_at.is_none() {
        "New"
    } else if is_today(&notification.created_at) {
        "Today"
    } else {
        "Earlier"
    }
}

fn notification_row(notification: &NotificationView) -> String {
    let unread = notification.read_at.is_none();
    let target = notification_target(notification);
    let target_attrs = target.as_ref().map_or_else(String::new, |href| {
        format!(
            r#" data-card-href="{}""#,
            html_escape::encode_double_quoted_attribute(href)
        )
    });
    let preview = notification_preview(notification, target.as_deref());
    let unread_marker = if unread {
        r#"<span class="unread-dot" aria-label="Unread"></span>"#
    } else {
        ""
    };
    format!(
        r#"<article class="notification-row{}"{}><div class="notification-kind" aria-hidden="true">{}</div><div class="notification-body"><p class="notification-line">{} <span>{}</span></p>{}<p class="notification-meta"><time datetime="{}">{}</time></p></div>{}</article>"#,
        if unread { " unread" } else { "" },
        target_attrs,
        html_escape::encode_text(notification_kind_label(&notification.kind)),
        notification_actor(notification),
        html_escape::encode_text(notification_action_text(&notification.kind)),
        preview,
        html_escape::encode_double_quoted_attribute(&notification.created_at),
        html_escape::encode_text(&relative_time(&notification.created_at)),
        unread_marker
    )
}

fn notification_actor(notification: &NotificationView) -> String {
    match (
        notification.actor_username.as_deref(),
        notification.actor_display_name.as_deref(),
    ) {
        (Some(username), display_name) => format!(
            r#"<a class="author-name" href="/users/{}">{}</a> <span class="username">@{}</span>"#,
            html_escape::encode_double_quoted_attribute(username),
            html_escape::encode_text(display_name.unwrap_or(username)),
            html_escape::encode_text(username)
        ),
        (None, _) if notification.actor_user_id.is_some() => "Deleted account".to_owned(),
        _ => "Someone".to_owned(),
    }
}

fn notification_preview(notification: &NotificationView, target: Option<&str>) -> String {
    if notification.kind == "follow" {
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

fn notification_target(notification: &NotificationView) -> Option<String> {
    if notification.kind == "follow" {
        return notification
            .actor_username
            .as_ref()
            .map(|username| format!("/users/{username}"));
    }
    notification
        .post_available
        .then_some(notification.post_id)
        .flatten()
        .map(|post_id| format!("/posts/{post_id}"))
}

fn notification_action_text(kind: &str) -> &'static str {
    match kind {
        "reply" => "replied to your post",
        "like" => "liked your post",
        "repost" => "reposted your post",
        "quote" => "quoted your post",
        "follow" => "followed you",
        "mention" => "mentioned you in a post",
        _ => "sent you a notification",
    }
}

fn notification_kind_label(kind: &str) -> &'static str {
    match kind {
        "reply" => "R",
        "like" => "L",
        "repost" => "Re",
        "quote" => "Q",
        "follow" => "F",
        "mention" => "@",
        _ => "N",
    }
}

fn snippet(text: &str) -> String {
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

fn is_today(created_at: &str) -> bool {
    parse_timestamp(created_at)
        .is_some_and(|created| created.date_naive() == Utc::now().date_naive())
}

fn relative_time(created_at: &str) -> String {
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

fn parse_timestamp(value: &str) -> Option<DateTime<Utc>> {
    NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S")
        .ok()
        .map(|timestamp| DateTime::from_naive_utc_and_offset(timestamp, Utc))
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
:root{font-family:Inter,ui-sans-serif,system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;color-scheme:light;line-height:1.5;--bg:#f5f6f1;--surface:#fff;--surface-subtle:#fbfcfa;--surface-muted:#f4f5f2;--header-bg:rgba(255,255,255,.96);--text:#202124;--text-strong:#172017;--muted:#667064;--muted-strong:#59625a;--border:#dfe4dc;--border-strong:#b9c2b8;--link:#1f5f8b;--link-strong:#24445f;--brand:#163b2f;--brand-hover:#235544;--brand-text:#fff;--hover:#eef3f0;--focus:#93c5fd;--shadow:rgba(20,35,30,.04);--reply-border:#c8d8d0;--avatar-bg:#eef3f0;--danger:#8a3d2d;--danger-strong:#6f2f22;--danger-bg:#fff8f5;--danger-border:#e6b8a8;--success-bg:#f4fbf5;--success-border:#add7b4;--media-bg:#f6f7f4;--shell-side:240px;--shell-primary:640px;--shell-gap:1.25rem;--shell-max:1160px;--header-padding-y:.8rem;--header-brand-size:2rem;--hairline:1px;--rail-sticky-top:calc(var(--header-brand-size) + var(--header-padding-y) + var(--header-padding-y) + var(--shell-gap) + var(--hairline))}
:root[data-theme="dark"]{color-scheme:dark;--bg:#111827;--surface:#182231;--surface-subtle:#1d2939;--surface-muted:#233044;--header-bg:rgba(17,24,39,.96);--text:#eef4fb;--text-strong:#f8fafc;--muted:#c3cfdd;--muted-strong:#d4deea;--border:#344256;--border-strong:#596b83;--link:#8fc7ff;--link-strong:#badcff;--brand:#4f8fc7;--brand-hover:#6aa8df;--brand-text:#06111f;--hover:#243349;--focus:#fbbf24;--shadow:rgba(0,0,0,.26);--reply-border:#4f6680;--avatar-bg:#243349;--danger:#ffb4a2;--danger-strong:#ffd2c7;--danger-bg:#3a2020;--danger-border:#8f4d43;--success-bg:#163321;--success-border:#4c8a61;--media-bg:#0f172a}
*{box-sizing:border-box}body{margin:0;min-width:320px;color:var(--text);background:var(--bg)}a{color:var(--link);text-decoration:none}a:hover{text-decoration:underline}
.site-header{position:sticky;top:0;z-index:10;background:var(--header-bg);border-bottom:1px solid var(--border);backdrop-filter:blur(8px)}
.header-inner{max-width:var(--shell-max);margin:0 auto;padding:var(--header-padding-y) 1rem;display:flex;align-items:center;justify-content:space-between;gap:1rem}
.brand{display:flex;align-items:center;gap:.55rem;font-weight:800;color:var(--text-strong)}.brand-mark{display:inline-grid;place-items:center;width:var(--header-brand-size);height:var(--header-brand-size);border-radius:7px;background:var(--brand);color:var(--brand-text)}
nav{display:flex;gap:.35rem;align-items:center;flex-wrap:wrap;justify-content:flex-end}nav a,nav button,.button-link{display:inline-flex;align-items:center;gap:.35rem;min-height:2.15rem;border-radius:7px;padding:.42rem .65rem;color:var(--link-strong);border:1px solid transparent;background:transparent}
nav a:hover,nav button:hover,.button-link:hover{background:var(--hover);text-decoration:none}nav form,.actions form{display:inline}
nav svg{width:1.05rem;height:1.05rem;fill:currentColor;flex:0 0 auto}
.nav-badge{display:inline-grid;place-items:center;min-width:1.25rem;height:1.25rem;border-radius:999px;padding:0 .35rem;background:var(--brand);color:var(--brand-text);font-size:.78rem;font-weight:800;line-height:1}
main{padding:var(--shell-gap)}.app-shell{width:min(100%,var(--shell-max));margin:0 auto;display:grid;grid-template-columns:var(--shell-side) minmax(0,var(--shell-primary)) var(--shell-side);gap:var(--shell-gap);align-items:start;justify-content:center}.primary-column{min-width:0;width:100%}.left-rail,.right-rail{min-width:0;position:sticky;top:var(--rail-sticky-top);display:grid;gap:.75rem;align-items:start}.side-rail-card,.rail-nav{background:var(--surface);border:1px solid var(--border);border-radius:8px;color:var(--muted-strong);box-shadow:0 1px 2px var(--shadow)}.side-rail-card{padding:.85rem}.side-rail-card h2{margin:.1rem 0 .6rem;font-size:1rem;color:var(--text)}.rail-nav{display:grid;grid-template-columns:minmax(0,1fr);gap:.2rem;width:100%;padding:.35rem;justify-content:stretch}.rail-nav a,.rail-nav button{width:100%;min-height:2.35rem;justify-content:flex-start;padding:.5rem .65rem}.rail-nav form{display:block}.mobile-nav{display:none}.dashboard-list{display:grid;grid-template-columns:auto minmax(0,1fr);gap:.45rem .75rem;margin:.25rem 0 .85rem}.dashboard-list dt{font-weight:800;color:var(--text)}.dashboard-list dd{margin:0;overflow-wrap:anywhere}.dashboard-account{color:var(--text)}.dashboard-account:hover{text-decoration:none}.dashboard-actions{display:flex;flex-wrap:wrap;gap:.4rem}.site-footer{max-width:var(--shell-max);margin:0 auto;padding:1rem;color:var(--muted);font-size:.9rem}.footer-onion{display:block;margin-top:.25rem;overflow-wrap:anywhere}
.page-header,.post,.composer,.panel,.empty-state,.notice{background:var(--surface);border:1px solid var(--border);border-radius:8px;margin:0 0 .7rem;padding:.85rem;box-shadow:0 1px 2px var(--shadow)}
.page-header h1,.section-heading h1,.panel h1{margin:0;font-size:1.45rem;line-height:1.2}.panel h1+table,.panel h1+form,.panel h1+p,.panel h1+dl{margin-top:.85rem}.page-header p,.muted,.empty-state p{color:var(--muted);margin:.35rem 0 0}.section-heading{display:flex;justify-content:space-between;gap:1rem;align-items:baseline;margin-bottom:.8rem}
.notifications-hero{background:var(--surface);border:1px solid var(--border);border-radius:8px;margin:0 0 .7rem;padding:1rem;box-shadow:0 1px 2px var(--shadow);display:flex;align-items:center;justify-content:space-between;gap:1rem}.notifications-hero h1{margin:0;font-size:1.55rem;line-height:1.15}.notifications-hero p:not(.eyebrow){margin:.35rem 0 0;color:var(--muted-strong)}.caught-up-pill{display:inline-flex;align-items:center;min-height:2rem;border:1px solid var(--success-border);border-radius:999px;background:var(--success-bg);color:var(--text-strong);padding:.32rem .75rem;font-weight:800}.caught-up{padding:.75rem .85rem}.caught-up p{margin:0}.notifications-list{display:grid;gap:.5rem}.notification-group{margin:.8rem .15rem .2rem;color:var(--muted);font-size:.82rem;text-transform:uppercase;letter-spacing:.08em}.notification-row{position:relative;display:grid;grid-template-columns:auto minmax(0,1fr) auto;gap:.75rem;align-items:start;background:var(--surface);border:1px solid var(--border);border-radius:8px;padding:.8rem;box-shadow:0 1px 2px var(--shadow)}.notification-row.unread{border-color:var(--border-strong);background:var(--surface-subtle)}.js-enabled .notification-row[data-card-href]{cursor:pointer}.js-enabled .notification-row[data-card-href]:hover{border-color:var(--border-strong);background:var(--hover)}.notification-kind{display:grid;place-items:center;width:2rem;height:2rem;border-radius:7px;background:var(--surface-muted);color:var(--link-strong);font-weight:900;font-size:.8rem}.notification-row.unread .notification-kind{background:var(--brand);color:var(--brand-text)}.notification-body{min-width:0}.notification-line{margin:0;overflow-wrap:anywhere}.notification-meta{margin:.35rem 0 0;color:var(--muted);font-size:.88rem}.notification-preview{display:block;margin:.5rem 0 0;border:1px solid var(--border);border-radius:7px;padding:.55rem .65rem;background:var(--surface-subtle);color:var(--muted-strong);overflow-wrap:anywhere}.notification-preview:hover{background:var(--surface);text-decoration:none}.notification-preview.unavailable{border-style:dashed}.unread-dot{width:.65rem;height:.65rem;border-radius:999px;background:var(--brand);margin-top:.7rem}
label{display:block;font-weight:700;margin:.85rem 0 .35rem}input,textarea,button,select{font:inherit}input[type=text],input[type=search],input[type=password],input[type=url],input:not([type]),textarea,select{width:100%;padding:.72rem .8rem;border:1px solid var(--border-strong);border-radius:7px;background:var(--surface);color:var(--text)}textarea{resize:vertical;min-height:7rem}::placeholder{color:var(--muted)}
input[type=checkbox]{accent-color:var(--brand)}.check-row,.theme-toggle{display:flex;align-items:center;gap:.55rem;font-weight:700;color:var(--text)}.theme-toggle{padding:.65rem .75rem;border:1px solid var(--border);border-radius:8px;background:var(--surface-subtle)}.theme-toggle input{width:auto}
input[type=text].password-visible{padding-right:.8rem}.password-control{display:grid;grid-template-columns:minmax(0,1fr) auto;gap:.45rem;align-items:center}.password-control input{min-width:0}.password-toggle{display:none;background:var(--surface);color:var(--link-strong);border-color:var(--border);min-width:4.5rem}.js-enabled .password-toggle{display:inline-block}.auth-submit{margin-top:1.15rem}.auth-form{margin-top:.35rem}.auth-form .field-help{margin:.15rem 0 .4rem;color:var(--muted-strong)}
.search-panel h1{margin-bottom:.75rem}.search-form{display:grid;grid-template-columns:minmax(0,1fr) auto;gap:.55rem;align-items:center}.search-form input{min-width:0}.search-results{display:grid;gap:.65rem}.section-title{margin:.2rem 0 .65rem;font-size:1.05rem;color:var(--text)}.search-users{margin-bottom:0}.search-users .section-title{margin-top:0}.search-account{grid-template-columns:auto minmax(0,1fr)}
input:focus,textarea:focus,select:focus,button:focus-visible,a:focus-visible{outline:3px solid var(--focus);outline-offset:2px}button,.primary{border:1px solid var(--brand);background:var(--brand);color:var(--brand-text);border-radius:7px;padding:.5rem .8rem;cursor:pointer;font-weight:700}button:hover,.primary:hover{background:var(--brand-hover);text-decoration:none}
nav button{border-color:transparent;background:transparent;color:var(--link-strong);padding:.42rem .65rem}.rail-nav button{border-color:transparent;background:transparent;color:var(--link-strong);padding:.5rem .65rem}.rail-nav button:hover,.mobile-nav button:hover{background:var(--hover);color:var(--link-strong)}
.composer-tools{display:flex;align-items:center;justify-content:space-between;gap:.75rem;margin-top:.85rem}.file-control{display:inline-flex;align-items:center;gap:.6rem;margin:0;color:var(--link-strong);font-weight:700}.file-control input{max-width:15rem}
.thread-nav{display:flex;margin:0 0 .45rem .85rem}.thread-back{width:2rem;height:2rem;display:inline-flex;align-items:center;justify-content:center;border-radius:999px;color:var(--link-strong)}.thread-back svg{width:1.2rem;height:1.2rem;fill:currentColor}.thread-back:hover{background:var(--hover);text-decoration:none}
.timeline{display:grid;gap:.65rem}.post{overflow:hidden;position:relative}.js-enabled .post[data-card-href]{cursor:pointer}.js-enabled .post[data-card-href]:hover{border-color:var(--border-strong)}.reply-post{margin-left:1.1rem;border-left:4px solid var(--reply-border);background:var(--surface-subtle)}.reply-post::before{content:"";position:absolute;left:-1.1rem;top:1.25rem;width:1.1rem;border-top:2px solid var(--reply-border)}.anchor-target{position:absolute;top:-5rem}.post-header{display:flex;justify-content:space-between;gap:.65rem;align-items:flex-start}.author-block{display:flex;gap:.55rem;align-items:center;min-width:0}.post-avatar{width:2rem;height:2rem;object-fit:cover;border-radius:999px;border:1px solid var(--border);background:var(--avatar-bg);flex:0 0 auto;margin:0}.post-avatar.placeholder{display:inline-grid;place-items:center;color:var(--muted-strong);font-weight:800}.author-name{font-weight:800;color:var(--text-strong)}.username,.post-time,.counts{color:var(--muted);font-size:.92rem}.text{white-space:pre-wrap;margin:.55rem 0;line-height:1.5;overflow-wrap:anywhere}.post img,.post video{display:block;max-width:100%;border-radius:8px;border:1px solid var(--border);margin-top:.5rem;background:var(--media-bg)}.post img.post-avatar{display:block;margin:0;border-radius:999px}
.counts{display:flex;gap:.5rem;flex-wrap:wrap;margin-top:.3rem;min-height:1.4rem}.post-permalink{font-weight:700;color:var(--link-strong)}.actions{display:inline-flex;gap:.25rem;flex-wrap:wrap;align-items:center;margin-top:.5rem;max-width:100%}.icon-button{width:2.2rem;height:2.2rem;display:inline-flex;align-items:center;justify-content:center;border:1px solid var(--border);border-radius:7px;background:var(--surface);color:var(--link-strong);padding:0}.icon-button svg{width:1.05rem;height:1.05rem;fill:currentColor}.icon-button:hover,.icon-button.active{background:var(--hover);color:var(--text-strong);text-decoration:none}.icon-button.disabled,.icon-button:disabled{color:var(--muted);background:var(--surface-muted);border-color:var(--border);cursor:not-allowed;opacity:.75}.icon-button.disabled:hover,.icon-button:disabled:hover{background:var(--surface-muted);color:var(--muted)}.repost-control{position:relative;display:inline-flex;align-items:center;gap:.25rem}.repost-menu{position:absolute;z-index:8;left:0;top:calc(100% + .25rem);min-width:8.5rem;padding:.3rem;border:1px solid var(--border-strong);border-radius:7px;background:var(--surface);box-shadow:0 6px 18px var(--shadow)}.repost-menu a{display:inline-flex;align-items:center;gap:.35rem;width:100%;min-height:2rem;border-radius:6px;padding:.32rem .55rem;color:var(--link-strong);font-weight:700}.repost-menu a svg{width:1rem;height:1rem;fill:currentColor;flex:0 0 auto}.repost-menu a:hover,.quote-fallback:hover{background:var(--hover);text-decoration:none}.quote-preview{display:block;margin:.6rem 0 .25rem;border:1px solid var(--border);border-radius:7px;background:var(--surface-subtle);overflow:hidden}.quote-preview p{margin:.65rem;color:var(--muted-strong)}.quote-link{display:grid;gap:.2rem;padding:.6rem;color:var(--text)}.quote-link:hover{background:var(--hover);text-decoration:none}.quote-author{font-weight:800}.quote-text{white-space:pre-wrap;overflow-wrap:anywhere}.quote-time{color:var(--muted);font-size:.86rem}.follow-button{min-width:6.6rem}.follow-button.active{background:var(--hover);color:var(--text-strong);border-color:var(--border-strong)}.profile-actions{margin-top:0}.profile-secondary button{background:var(--surface);color:var(--danger);border-color:var(--danger-border);padding:.32rem .5rem;min-height:1.85rem;font-size:.86rem}.profile-secondary button:hover{background:var(--danger-bg);color:var(--danger-strong)}.profile-title-row{display:flex;align-items:flex-start;justify-content:space-between;gap:.75rem}.sr-only{position:absolute;width:1px;height:1px;padding:0;margin:-1px;overflow:hidden;clip:rect(0,0,0,0);white-space:nowrap;border:0}.repost-banner{color:var(--muted-strong);font-size:.9rem;font-weight:800;margin-bottom:.35rem}.unavailable{color:var(--muted)}.empty-state{text-align:center;padding:2rem 1rem}.empty-state h2{margin:0;font-size:1.2rem}.notice.error,.error-panel{border-color:var(--danger-border);background:var(--danger-bg)}.notice.success{border-color:var(--success-border);background:var(--success-bg)}.eyebrow{text-transform:uppercase;letter-spacing:.08em;font-weight:800;color:var(--muted);font-size:.78rem}.noscript-banner{max-width:1100px;margin:.7rem auto 0;padding:.65rem .85rem;border:1px solid var(--border);border-radius:8px;background:var(--surface-subtle);color:var(--muted-strong)}.js-enabled .noscript-banner{display:none}
.profile-banner{width:100%;max-height:220px;object-fit:cover;border-radius:8px;border:1px solid var(--border);background:var(--surface-muted)}.profile-heading{display:flex;gap:1rem;align-items:flex-start;margin-top:.85rem}.profile-main{min-width:0;flex:1}.profile-picture{width:88px;height:88px;object-fit:cover;border-radius:999px;border:3px solid var(--surface);background:var(--avatar-bg);flex:0 0 auto}.profile-meta{color:var(--muted-strong);margin:.45rem 0 0}.settings-profile-editor{padding:0;overflow:hidden}.settings-editor-bar{display:flex;justify-content:space-between;align-items:center;gap:1rem;padding:.85rem;border-bottom:1px solid var(--border)}.settings-editor-bar h1{margin:0}.settings-editor-bar .primary{flex:0 0 auto}.settings-profile-form{padding:0 .85rem .85rem}.settings-profile-media{margin:0 -.85rem .85rem}.settings-banner-wrap{background:var(--media-bg)}.settings-banner-preview{display:block;width:100%;height:220px;object-fit:cover;background:linear-gradient(135deg,var(--surface-muted),var(--hover));border:0;border-radius:0}.settings-banner-preview.placeholder::before{content:"";display:block;width:100%;height:100%}.settings-picture-row{display:grid;grid-template-columns:auto minmax(0,1fr);gap:1rem;align-items:end;padding:0 .85rem .85rem;margin-top:-48px}.settings-picture-preview{width:112px;height:112px;object-fit:cover;border-radius:999px;border:5px solid var(--surface);background:var(--avatar-bg);box-shadow:0 1px 4px var(--shadow)}.settings-picture-preview.placeholder{display:block}.settings-media-controls{display:grid;gap:.5rem;align-content:end;padding-top:3.25rem}.media-control-row{display:flex;align-items:center;gap:.75rem;flex-wrap:wrap}.settings-fields{display:grid;gap:.25rem}.settings-grid{display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:.7rem}.deep-settings-panel{padding:0;overflow:hidden}.deep-settings-form{padding:.85rem;display:grid;gap:.85rem}.deep-settings-group{border:1px solid var(--border);border-radius:8px;padding:.8rem;display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:.7rem}.deep-settings-group legend{font-weight:800;padding:0 .35rem}.deep-settings-field{display:grid;gap:.25rem;align-content:start}.deep-settings-field label{font-weight:800}.deep-settings-field input,.deep-settings-field select{min-width:0}.field-help{font-size:.88rem}.deep-settings-confirm .settings-item-list li{display:block}.compact-panel h2,.danger-panel h2{margin:0 0 .65rem;font-size:1.1rem}.inline-settings-form{display:grid;grid-template-columns:minmax(0,1fr) auto;gap:.55rem;align-items:center;margin:.2rem 0 .75rem}.inline-settings-form input{min-width:0}.settings-password-form button[type=submit]{margin-top:.9rem}.settings-item-list{list-style:none;margin:.25rem 0 0;padding:0;display:grid;gap:.45rem}.settings-item-list li{display:flex;justify-content:space-between;align-items:center;gap:.75rem;border:1px solid var(--border);border-radius:7px;padding:.55rem .65rem;background:var(--surface-subtle)}.settings-item-list form{flex:0 0 auto}.settings-item-list button{padding:.32rem .55rem;background:var(--surface);color:var(--link-strong);border-color:var(--border)}.compact-empty{border:1px dashed var(--border);border-radius:7px;padding:.75rem;background:var(--surface-subtle);color:var(--muted-strong)}.compact-empty p{margin:.25rem 0 0}.danger-panel{border-color:var(--danger-border);background:var(--danger-bg)}.danger,.danger-link{border-color:var(--danger-border);background:var(--danger);color:var(--brand-text)}.danger:hover,.danger-link:hover{background:var(--danger-strong);color:var(--brand-text);text-decoration:none}.delete-account-panel p{max-width:62ch}.favicon-preview{width:32px;height:32px;object-fit:contain;border:1px solid var(--border);border-radius:6px;background:var(--surface)}.admin-user-search{display:grid;grid-template-columns:minmax(0,1fr) minmax(0,1fr) auto;gap:.65rem;align-items:end;margin-top:.75rem}.admin-user-search label{margin-top:0}.admin-user-search-actions{display:flex;gap:.4rem;align-items:center;margin-bottom:.05rem}.admin-user-row{display:grid;grid-template-columns:minmax(0,1fr) auto;gap:.75rem;border:1px solid var(--border);border-radius:8px;padding:.75rem;margin-top:.65rem;background:var(--surface-subtle)}.admin-user-heading{overflow-wrap:anywhere}.admin-user-statuses,.admin-user-matches{display:flex;flex-wrap:wrap;gap:.35rem;margin-top:.45rem}.admin-user-pill,.admin-user-match{display:inline-flex;align-items:center;min-height:1.55rem;border:1px solid var(--border);border-radius:999px;padding:.15rem .5rem;background:var(--surface);font-size:.82rem;font-weight:800;color:var(--muted-strong)}.admin-user-match{border-color:var(--success-border);background:var(--success-bg);color:var(--text)}.admin-user-meta{margin:.6rem 0 0}.admin-post-preview{margin:.55rem 0 0;color:var(--muted-strong);overflow-wrap:anywhere}.admin-user-actions{display:flex;align-items:flex-start}.admin-users-empty{margin-top:.75rem}.account-list{display:grid;gap:.65rem}.account-row{display:grid;grid-template-columns:auto minmax(0,1fr) auto;gap:.75rem;align-items:center;background:var(--surface);border:1px solid var(--border);border-radius:8px;padding:.85rem}.account-row p{margin:.3rem 0 0;color:var(--muted-strong);overflow-wrap:anywhere}.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(180px,1fr));gap:.85rem}.item-list{margin:.75rem 0 0;padding-left:1.2rem}.item-list li{margin:.45rem 0}.panel dl:not(.dashboard-list){display:grid;grid-template-columns:max-content minmax(0,1fr);gap:.45rem .85rem}.panel dl:not(.dashboard-list) dt{font-weight:800}.panel dl:not(.dashboard-list) dd{margin:0;overflow-wrap:anywhere}table{width:100%;border-collapse:collapse}td,th{border-bottom:1px solid var(--border);text-align:left;padding:.55rem;vertical-align:top}pre{white-space:pre-wrap;overflow:auto;max-width:100%}
@media (max-width:1100px){.app-shell{--shell-side:220px;--shell-max:880px;grid-template-columns:var(--shell-side) minmax(0,var(--shell-primary))}.right-rail{display:none}}
@media (max-width:820px){.app-shell{grid-template-columns:minmax(0,680px)}.left-rail,.right-rail{display:none}.mobile-nav{display:flex}}
@media (max-width:600px){main{padding:.75rem}.header-inner{align-items:flex-start;flex-direction:column}.site-header{position:static}nav{justify-content:flex-start}.mobile-nav{width:100%}.search-form,.inline-settings-form,.settings-grid,.deep-settings-group,.admin-user-search,.admin-user-row{grid-template-columns:1fr}.search-form button,.inline-settings-form button{width:100%}.composer-tools,.post-header,.profile-heading,.profile-title-row,.account-row,.settings-editor-bar,.notifications-hero{align-items:stretch;grid-template-columns:1fr;flex-direction:column}.settings-banner-preview{height:150px}.settings-picture-row{grid-template-columns:1fr;margin-top:-38px;gap:.5rem}.settings-picture-preview{width:92px;height:92px}.settings-media-controls{padding-top:0}.media-control-row{align-items:flex-start}.settings-item-list li{align-items:stretch;flex-direction:column}.admin-user-search-actions,.admin-user-actions{align-items:stretch;flex-direction:column}.admin-user-search-actions button,.admin-user-search-actions .button-link,.admin-user-actions button{width:100%;justify-content:center}.panel dl:not(.dashboard-list){grid-template-columns:1fr}table{display:block;max-width:100%;overflow-x:auto}.author-block{align-items:flex-start}.file-control{display:block}.file-control input{display:block;max-width:100%;margin-top:.35rem}.reply-post{margin-left:.65rem;padding-left:.8rem}.reply-post::before{left:-.65rem;width:.65rem}.button-link{padding:.42rem .55rem}.counts{gap:.45rem}.page-header h1,.section-heading h1,.panel h1,.notifications-hero h1{font-size:1.25rem}.notification-row{grid-template-columns:auto minmax(0,1fr);gap:.6rem}.unread-dot{position:absolute;right:.75rem;top:.75rem;margin:0}.notification-preview{padding:.5rem}}
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
    fn layout_uses_user_theme_marker() {
        let user = CurrentUser {
            id: 1,
            username: "ada".to_owned(),
            display_name: "Ada Lovelace".to_owned(),
            is_admin: false,
            is_suspended: false,
            theme: Theme::Dark,
        };
        let body = layout(Some(&user), "Home Feed", "<p>body</p>", "My Microblog");

        assert!(body.contains(r#"<html lang="en" data-theme="dark">"#));
    }

    #[test]
    fn layout_includes_no_javascript_status_and_styles() {
        let body = layout(None, "Home Feed", "<p>body</p>", "My Microblog");

        assert!(body.contains(r#"class="noscript-banner" role="status""#));
        assert!(body.contains(".js-enabled .noscript-banner"));
        assert!(body.contains("display:none"));
        assert!(
            client_script().contains(r#"document.documentElement.classList.add("js-enabled")"#)
        );
        assert!(body.contains("JavaScript is disabled."));
        assert!(body.contains("RustPost will use standard links and forms."));
    }

    #[test]
    fn register_form_uses_configured_password_length() {
        let body = register_form(None, 5, None);

        assert!(body.contains(r#"minlength="5" required"#));
        assert!(body.contains("Password must be at least 5 characters."));
        assert!(body.contains(r#"aria-describedby="password-requirement""#));
        assert!(body.contains(r#"aria-describedby="confirm-password-requirement""#));
        assert!(!body.contains(r#"minlength="10""#));
    }

    #[test]
    fn login_form_shows_configured_password_requirement() {
        let body = login_form(None, 12);

        assert!(body.contains("Password must be at least 12 characters."));
        assert!(body.contains(r#"minlength="12" required"#));
        assert!(body.contains(r#"aria-describedby="password-requirement""#));
    }

    #[test]
    fn password_fields_allow_empty_when_minimum_is_zero() {
        let body = register_form(None, 0, None);

        assert!(!body.contains("minlength="));
        assert!(!body.contains(r#"autocomplete="new-password" required"#));
        assert!(body.contains("No minimum password length is currently required."));
    }

    #[test]
    fn register_form_renders_optional_captcha_fields() {
        let captcha = crate::registration_captcha::RegistrationCaptchaChallenge {
            token: "token-1".to_owned(),
            image_data_uri: "data:image/png;base64,abc".to_owned(),
            expires_minutes: 10,
            answer: "ABCDE".to_owned(),
        };

        let body = register_form(None, 10, Some(&captcha));

        assert!(body.contains("<legend>Registration CAPTCHA</legend>"));
        assert!(body.contains(r#"name="captcha_token" value="token-1""#));
        assert!(body.contains(r#"id="captcha_answer" name="captcha_answer""#));
        assert!(body.contains("The challenge expires in 10 minutes."));
    }

    #[test]
    fn signed_in_left_rail_contains_navigation_only() {
        let user = CurrentUser {
            id: 1,
            username: "ada".to_owned(),
            display_name: "Ada Lovelace".to_owned(),
            is_admin: false,
            is_suspended: false,
            theme: Theme::Light,
        };
        let body = layout_with_csrf(
            Some(&user),
            Some("csrf"),
            "Home Feed",
            "<p>body</p>",
            "RustPost",
        );
        let left_rail = body
            .split_once(r#"<aside class="left-rail" data-testid="left-rail">"#)
            .and_then(|(_, rest)| rest.split_once("</aside>"))
            .map(|(panel, _)| panel)
            .expect("left rail");

        assert!(left_rail.contains("Home Feed"));
        assert!(left_rail.contains("Following"));
        assert!(left_rail.contains("Search"));
        assert!(left_rail.contains("Notifications"));
        assert!(left_rail.contains("Bookmarks"));
        assert!(left_rail.contains("Profile"));
        assert!(left_rail.contains("Log out"));
        assert!(!left_rail.contains("Signed in"));
        assert!(!left_rail.contains("Ada Lovelace"));
        assert!(!left_rail.contains("rail-account"));
    }

    #[test]
    fn dashboard_uses_account_link_without_duplicate_feed_links_or_onion() {
        let user = CurrentUser {
            id: 1,
            username: "ada".to_owned(),
            display_name: "Ada Lovelace".to_owned(),
            is_admin: false,
            is_suspended: false,
            theme: Theme::Light,
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
            .split_once(r#"<aside class="right-rail" data-testid="right-rail">"#)
            .and_then(|(_, rest)| rest.split_once("</aside>"))
            .map(|(panel, _)| panel)
            .expect("dashboard panel");
        assert!(dashboard.contains(r#"href="/users/ada""#));
        assert!(dashboard.contains("Ada Lovelace"));
        assert!(dashboard.contains(r#"href="/users/ada/followers""#));
        assert!(dashboard.contains(r#"aria-label="View followers for Ada Lovelace""#));
        assert!(dashboard.contains(">2 followers</a>"));
        assert!(dashboard.contains(r#"href="/users/ada/following""#));
        assert!(dashboard.contains(r#"aria-label="View users Ada Lovelace follows""#));
        assert!(dashboard.contains(">3 following</a>"));
        assert!(dashboard.contains(r#"<a class="button-link" href="/settings">Settings</a>"#));
        assert!(!dashboard.contains(r#"href="/home">Home Feed"#));
        assert!(!dashboard.contains(r#"href="/following">Following"#));
        assert!(!dashboard.contains("quick-links"));
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
    fn composer_has_live_remaining_counter_and_contextual_placeholder() {
        let post = composer(Some("csrf"), None, 512);
        assert!(post.contains("512 remaining"));
        assert!(post.contains("data-character-limit=\"512\""));
        assert!(post.contains("maxlength=\"512\""));
        assert!(post.contains("What is happening?"));
        assert!(post.contains(r#"placeholder="What's happening?""#));

        let reply = composer(Some("csrf"), Some(10), 512);
        assert!(reply.contains(r#"id="reply-text-10""#));
        assert!(reply.contains(r#"placeholder="Write a reply...""#));
    }

    #[test]
    fn quote_composer_has_contextual_placeholder() {
        let quote = QuotePreview {
            id: 42,
            username: Some("ada".to_owned()),
            display_name: Some("Ada".to_owned()),
            anonymous_label: None,
            text: "quoted post".to_owned(),
            created_at: "2026-05-18 10:30".to_owned(),
            unavailable: false,
        };
        let body = quote_composer("csrf", &quote, 512);

        assert!(body.contains(r#"id="quote-text""#));
        assert!(body.contains(r#"placeholder="Add your thoughts...""#));
        assert!(body.contains("data-character-limit=\"512\""));
        assert!(body.contains("maxlength=\"512\""));
    }

    #[test]
    fn timeline_cards_are_clickable_without_exposing_timestamps() {
        let post = test_post();
        let body = post_card(&post, None, None);

        assert!(body.contains(r#"data-card-href="/posts/42""#));
        assert!(!body.contains(r#"tabindex="0""#));
        assert!(body.contains(r#"<a class="post-permalink" href="/posts/42">Open post</a>"#));
        assert!(!body.contains("Open thread"));
        assert!(!body.contains("2026-05-18 10:30"));
        assert!(!body.contains(r#"class="post-time""#));
    }

    #[test]
    fn thread_cards_show_timestamps() {
        let post = test_post();
        let body = thread_post_card(&post, None, None);

        assert!(body.contains(r#"<span class="post-time">2026-05-18 10:30</span>"#));
        assert!(!body.contains(r#"class="post-time" href="/posts/42""#));
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
        assert!(!root_card.contains(r#"href="/posts/42">Open post</a>"#));
        assert!(root_card.contains(r#"<span class="post-time">2026-05-18 10:30</span>"#));
        assert!(!root_card.contains(r#"class="post-time" href="/posts/42""#));
        assert!(body.contains(r#"data-card-href="/posts/43""#));
        assert!(body.contains(r#"href="/posts/43">Open post</a>"#));
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
            theme: Theme::Light,
        };
        let mut post = test_post();
        post.user_id = Some(2);
        post.viewer_can_repost = true;
        let body = post_card(&post, Some(&user), Some("csrf"));

        assert!(body.contains(
            r#"<div class="actions" data-testid="post-actions"><form method="post" action="/posts/42/like""#
        ));
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

    #[test]
    fn repost_action_exposes_quote_menu_and_fallback() {
        let user = CurrentUser {
            id: 1,
            username: "ada".to_owned(),
            display_name: "Ada".to_owned(),
            is_admin: false,
            is_suspended: false,
            theme: Theme::Light,
        };
        let mut post = test_post();
        post.user_id = Some(2);
        post.viewer_can_repost = true;

        let body = post_card(&post, Some(&user), Some("csrf"));

        assert!(body.contains("data-repost-menu-button"));
        assert!(body.contains(r#"aria-haspopup="menu""#));
        assert!(body.contains(r#"role="menu""#));
        assert!(body.contains(r#"role="menuitem" href="/posts/42/quote">"#));
        assert!(body.contains(r#"<span>Quote post</span></a>"#));
        assert!(body.contains(
            r#"class="icon-button quote-fallback" href="/posts/42/quote" aria-label="Quote post" title="Quote post""#
        ));
        assert!(body.contains(r#"<span class="sr-only">Quote post</span></a>"#));
        assert!(!body.contains(r#">Quote</a>"#));
    }

    #[test]
    fn quote_repost_renders_embedded_original_preview() {
        let mut post = test_post();
        post.text = "my quote".to_owned();
        post.quote = Some(QuotePreview {
            id: 7,
            username: Some("bob".to_owned()),
            display_name: Some("Bob".to_owned()),
            anonymous_label: None,
            text: "original post".to_owned(),
            created_at: "2026-05-18 10:00".to_owned(),
            unavailable: false,
        });

        let body = post_card(&post, None, None);

        assert!(body.contains(r#"class="quote-preview""#));
        assert!(body.contains(r#"href="/posts/7""#));
        assert!(body.contains("original post"));
        assert!(body.contains("@bob"));
    }

    #[test]
    fn quote_repost_renders_unavailable_original_preview() {
        let mut post = test_post();
        post.quote = Some(QuotePreview {
            id: 7,
            username: None,
            display_name: None,
            anonymous_label: None,
            text: String::new(),
            created_at: String::new(),
            unavailable: true,
        });

        let body = post_card(&post, None, None);

        assert!(body.contains("Quoted post is no longer available."));
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
            quote: None,
            media: Vec::new(),
        }
    }
}
