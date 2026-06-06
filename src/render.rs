use crate::auth::{CurrentUser, Theme};
use crate::social::{
    AccountView, MediaView, NotificationGroupView, PostView, ProfileTimelineTab, QuotePreview,
    TimelineEventKind,
};
use crate::youtube::{self, YoutubeEmbed};
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
    pub blur_nsfw_media: bool,
    pub post_edit_window_seconds: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct SearchRenderOptions {
    pub blur_nsfw_media: bool,
    pub post_edit_window_seconds: u64,
}

impl Default for SearchRenderOptions {
    fn default() -> Self {
        Self {
            blur_nsfw_media: true,
            post_edit_window_seconds: crate::config::DEFAULT_POST_EDIT_WINDOW_SECONDS,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct OnboardingPage<'a> {
    pub csrf: &'a str,
    pub display_name: &'a str,
    pub bio: &'a str,
    pub picture_path: Option<&'a str>,
    pub suggestions: &'a [AccountView],
    pub allow_profile_pictures: bool,
    pub max_display_name_len: usize,
    pub max_bio_len: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct EmptyState<'a> {
    pub title: &'a str,
    pub message: &'a str,
}

impl<'a> EmptyState<'a> {
    pub const fn new(title: &'a str, message: &'a str) -> Self {
        Self { title, message }
    }

    const fn default_posts() -> Self {
        Self {
            title: "No posts yet.",
            message: "The timeline will fill in once people start posting.",
        }
    }
}

impl PostRenderOptions {
    const fn timeline() -> Self {
        Self {
            show_timestamp: false,
            clickable_card: true,
            blur_nsfw_media: true,
            post_edit_window_seconds: crate::config::DEFAULT_POST_EDIT_WINDOW_SECONDS,
        }
    }

    const fn thread() -> Self {
        Self {
            show_timestamp: true,
            clickable_card: true,
            blur_nsfw_media: true,
            post_edit_window_seconds: crate::config::DEFAULT_POST_EDIT_WINDOW_SECONDS,
        }
    }

    const fn with_edit_window(mut self, seconds: u64) -> Self {
        self.post_edit_window_seconds = seconds;
        self
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
    let header_tor = tor_header_indicator(context.tor_onion_address.as_deref());
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
<header class="site-header"><div class="header-inner"><div class="header-brand-row"><a class="brand" href="/home"><span class="brand-mark">{}</span><span>{}</span></a>{}</div><nav class="mobile-nav" aria-label="Primary">{}</nav></div></header>
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
        header_tor,
        auth_nav,
        left_rail,
        body,
        side_panel,
        html_escape::encode_text(site_name),
    )
}

fn tor_header_indicator(onion: Option<&str>) -> String {
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

fn short_onion_address(onion: &str) -> String {
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

const CLIENT_BOOT_SCRIPT: &str = r#"document.documentElement.classList.add("js-enabled");"#;

const CLIENT_SCRIPT: &str = r#"document.documentElement.classList.add("js-enabled");

function cardInteractiveTarget(target) {
  if (!(target instanceof Element)) {
    return null;
  }
  return target.closest('a,button,input,textarea,select,label,form,[role="button"],[data-youtube-preview]');
}

document.addEventListener("click", (event) => {
  if (!(event.target instanceof Element)) {
    return;
  }
  const formCard = event.target.closest("[data-card-form]");
  if (formCard && !cardInteractiveTarget(event.target)) {
    const form = document.getElementById(formCard.getAttribute("data-card-form"));
    if (form instanceof HTMLFormElement) {
      event.preventDefault();
      if (form.requestSubmit) {
        form.requestSubmit();
      } else {
        form.submit();
      }
      return;
    }
  }
  const card = event.target.closest("[data-card-href]");
  if (!card || cardInteractiveTarget(event.target)) {
    return;
  }
  window.location.assign(card.getAttribute("data-card-href"));
});

function safeYoutubeEmbedSrc(value) {
  try {
    const url = new URL(value);
    if (url.protocol !== "https:" || url.hostname !== "www.youtube-nocookie.com" || !url.pathname.startsWith("/embed/")) {
      return null;
    }
    url.searchParams.set("autoplay", "1");
    url.searchParams.set("rel", "0");
    return url.toString();
  } catch (_error) {
    return null;
  }
}

function activateYoutubePreview(link) {
  const card = link.closest("[data-youtube-preview]");
  const player = card ? card.querySelector("[data-youtube-player]") : null;
  if (!card || !player || player.querySelector("iframe")) {
    return;
  }
  const src = safeYoutubeEmbedSrc(link.getAttribute("data-youtube-embed-src") || "");
  if (!src) {
    return;
  }
  const iframe = document.createElement("iframe");
  iframe.className = "youtube-iframe";
  iframe.src = src;
  iframe.title = link.getAttribute("data-youtube-title") || "YouTube video";
  iframe.loading = "lazy";
  iframe.referrerPolicy = "no-referrer";
  iframe.allow = "accelerometer; autoplay; clipboard-write; encrypted-media; gyroscope; picture-in-picture; web-share";
  iframe.setAttribute("allowfullscreen", "");
  iframe.setAttribute("sandbox", "allow-scripts allow-same-origin allow-presentation");
  player.hidden = false;
  player.append(iframe);
  card.classList.add("youtube-preview-playing");
}

document.addEventListener("click", (event) => {
  if (!(event.target instanceof Element)) {
    return;
  }
  const link = event.target.closest("[data-youtube-play]");
  if (!link) {
    return;
  }
  event.preventDefault();
  activateYoutubePreview(link);
});

document.addEventListener("keydown", (event) => {
  if (!(event.target instanceof Element)) {
    return;
  }
  if (event.key === "Enter" && event.target === event.target.closest("[data-card-form]")) {
    const form = document.getElementById(event.target.getAttribute("data-card-form"));
    if (form instanceof HTMLFormElement) {
      event.preventDefault();
      if (form.requestSubmit) {
        form.requestSubmit();
      } else {
        form.submit();
      }
      return;
    }
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

const CHARACTER_COUNTER_CLASSES = [
  "character-counter-normal",
  "character-counter-warning",
  "character-counter-danger"
];

function characterCounterFor(textarea) {
  const counter = document.querySelector(`[data-character-counter="${textarea.id}"]`);
  return counter;
}

function characterCounterClass(remaining) {
  if (remaining <= 10) {
    return "character-counter-danger";
  }
  if (remaining <= 50) {
    return "character-counter-warning";
  }
  return "character-counter-normal";
}

function updateComposerSubmit(textarea, remaining) {
  const form = textarea.closest("form");
  const submit = form ? form.querySelector('button[type="submit"]') : null;
  if (!submit) {
    return;
  }
  submit.disabled = remaining < 0 || form.dataset.submitting === "true";
}

function updateComposerCount(textarea) {
  const counter = characterCounterFor(textarea);
  if (!counter) {
    return;
  }
  const max = Number.parseInt(textarea.getAttribute("data-character-limit") || "0", 10);
  if (!Number.isFinite(max)) {
    return;
  }
  const length = Array.from(textarea.value).length;
  const remaining = max - length;
  counter.textContent = `${remaining} remaining`;
  counter.classList.remove(...CHARACTER_COUNTER_CLASSES);
  counter.classList.add(characterCounterClass(remaining));
  updateComposerSubmit(textarea, remaining);
}

function initializeComposerCounters(root) {
  if (root.matches && root.matches("textarea[data-character-limit]")) {
    updateComposerCount(root);
  }
  if (root.querySelectorAll) {
    root.querySelectorAll("textarea[data-character-limit]").forEach(updateComposerCount);
  }
}

const mentionStates = new WeakMap();
const MENTION_DEBOUNCE_MS = 120;

function mentionStateFor(textarea) {
  let state = mentionStates.get(textarea);
  if (state) {
    return state;
  }
  const menu = document.getElementById(textarea.getAttribute("aria-controls"));
  if (!menu) {
    return null;
  }
  state = {
    textarea,
    menu,
    items: [],
    activeIndex: -1,
    atIndex: -1,
    fragment: "",
    requestId: 0,
    timer: 0,
    controller: null
  };
  mentionStates.set(textarea, state);
  return state;
}

function mentionContext(textarea) {
  if (textarea.selectionStart !== textarea.selectionEnd) {
    return null;
  }
  const cursor = textarea.selectionStart;
  const before = textarea.value.slice(0, cursor);
  const atIndex = before.lastIndexOf("@");
  if (atIndex < 0) {
    return null;
  }
  const preceding = atIndex > 0 ? before.charAt(atIndex - 1) : "";
  if (preceding && /[A-Za-z0-9_@.-]/.test(preceding)) {
    return null;
  }
  const fragment = before.slice(atIndex + 1);
  if (!/^[A-Za-z0-9_-]*$/.test(fragment)) {
    return null;
  }
  return { atIndex, fragment };
}

function closeMentionMenu(textarea) {
  const state = mentionStateFor(textarea);
  if (!state) {
    return;
  }
  if (state.timer) {
    window.clearTimeout(state.timer);
    state.timer = 0;
  }
  if (state.controller) {
    state.controller.abort();
    state.controller = null;
  }
  state.items = [];
  state.activeIndex = -1;
  state.atIndex = -1;
  state.fragment = "";
  state.menu.hidden = true;
  state.menu.textContent = "";
  textarea.setAttribute("aria-expanded", "false");
  textarea.removeAttribute("aria-activedescendant");
}

function setActiveMention(state, index) {
  if (state.items.length === 0) {
    state.activeIndex = -1;
    state.textarea.removeAttribute("aria-activedescendant");
    return;
  }
  state.activeIndex = (index + state.items.length) % state.items.length;
  state.menu.querySelectorAll("[role='option']").forEach((option, optionIndex) => {
    const selected = optionIndex === state.activeIndex;
    option.setAttribute("aria-selected", selected ? "true" : "false");
    if (selected) {
      state.textarea.setAttribute("aria-activedescendant", option.id);
      option.scrollIntoView({ block: "nearest" });
    }
  });
}

function selectMention(textarea, index) {
  const state = mentionStateFor(textarea);
  if (!state || index < 0 || index >= state.items.length) {
    return;
  }
  const context = mentionContext(textarea);
  if (!context || context.atIndex !== state.atIndex) {
    closeMentionMenu(textarea);
    return;
  }
  const username = state.items[index].username;
  const start = state.atIndex;
  const end = textarea.selectionStart;
  const suffix = textarea.value.slice(end);
  const needsSpace = suffix.length === 0 || /^[A-Za-z0-9_@#-]/.test(suffix);
  const replacement = `@${username}${needsSpace ? " " : ""}`;
  textarea.value = `${textarea.value.slice(0, start)}${replacement}${suffix}`;
  const cursor = start + replacement.length;
  textarea.setSelectionRange(cursor, cursor);
  closeMentionMenu(textarea);
  textarea.dispatchEvent(new Event("input", { bubbles: true }));
  textarea.focus();
}

function renderMentionSuggestions(state, suggestions) {
  state.items = suggestions;
  state.menu.textContent = "";
  if (suggestions.length === 0) {
    closeMentionMenu(state.textarea);
    return;
  }
  suggestions.forEach((suggestion, index) => {
    const option = document.createElement("button");
    option.type = "button";
    option.className = "mention-option";
    option.id = `${state.menu.id}-option-${index}`;
    option.setAttribute("role", "option");
    option.setAttribute("aria-selected", "false");
    option.dataset.username = suggestion.username;
    const name = document.createElement("span");
    name.className = "mention-name";
    name.textContent = suggestion.display_name || suggestion.username;
    const handle = document.createElement("span");
    handle.className = "mention-handle";
    handle.textContent = `@${suggestion.username}`;
    option.append(name, handle);
    option.addEventListener("pointerdown", (event) => {
      event.preventDefault();
      selectMention(state.textarea, index);
    });
    option.addEventListener("mouseenter", () => setActiveMention(state, index));
    state.menu.append(option);
  });
  state.menu.hidden = false;
  state.textarea.setAttribute("aria-expanded", "true");
  setActiveMention(state, 0);
}

function requestMentionSuggestions(textarea) {
  const state = mentionStateFor(textarea);
  const context = mentionContext(textarea);
  if (!state || !context || !window.fetch) {
    closeMentionMenu(textarea);
    return;
  }
  state.atIndex = context.atIndex;
  state.fragment = context.fragment;
  if (state.timer) {
    window.clearTimeout(state.timer);
  }
  state.timer = window.setTimeout(async () => {
    state.timer = 0;
    if (state.controller) {
      state.controller.abort();
    }
    const requestId = state.requestId + 1;
    state.requestId = requestId;
    state.controller = window.AbortController ? new AbortController() : null;
    try {
      const response = await fetch(`/mentions?q=${encodeURIComponent(context.fragment)}`, {
        headers: { "Accept": "application/json" },
        credentials: "same-origin",
        signal: state.controller ? state.controller.signal : undefined
      });
      if (!response.ok || requestId !== state.requestId) {
        return;
      }
      const suggestions = await response.json();
      const current = mentionContext(textarea);
      if (!current || current.atIndex !== context.atIndex || current.fragment !== context.fragment) {
        return;
      }
      renderMentionSuggestions(state, Array.isArray(suggestions) ? suggestions : []);
    } catch (error) {
      if (!state.controller || error.name !== "AbortError") {
        closeMentionMenu(textarea);
      }
    }
  }, MENTION_DEBOUNCE_MS);
}

function initializeMentionAutocomplete(root) {
  if (root.matches && root.matches("textarea[data-mention-autocomplete]")) {
    mentionStateFor(root);
  }
  if (root.querySelectorAll) {
    root.querySelectorAll("textarea[data-mention-autocomplete]").forEach(mentionStateFor);
  }
}

document.addEventListener("input", (event) => {
  const textarea = event.target.closest("textarea[data-character-limit]");
  if (textarea) {
    updateComposerCount(textarea);
  }
  const mentionTextarea = event.target.closest("textarea[data-mention-autocomplete]");
  if (mentionTextarea) {
    requestMentionSuggestions(mentionTextarea);
  }
});

document.addEventListener("keydown", (event) => {
  if (!(event.target instanceof Element)) {
    return;
  }
  const textarea = event.target.closest("textarea[data-mention-autocomplete]");
  if (!textarea) {
    return;
  }
  const state = mentionStateFor(textarea);
  if (!state || state.menu.hidden) {
    return;
  }
  if (event.key === "ArrowDown") {
    event.preventDefault();
    setActiveMention(state, state.activeIndex + 1);
  } else if (event.key === "ArrowUp") {
    event.preventDefault();
    setActiveMention(state, state.activeIndex - 1);
  } else if (event.key === "Enter" || event.key === "Tab") {
    event.preventDefault();
    selectMention(textarea, state.activeIndex);
  } else if (event.key === "Escape") {
    event.preventDefault();
    closeMentionMenu(textarea);
  }
});

document.addEventListener("click", (event) => {
  if (!(event.target instanceof Element)) {
    return;
  }
  document.querySelectorAll("textarea[data-mention-autocomplete]").forEach((textarea) => {
    const state = mentionStateFor(textarea);
    if (state && event.target !== textarea && !state.menu.contains(event.target)) {
      closeMentionMenu(textarea);
    }
  });
});

initializeComposerCounters(document);
initializeMentionAutocomplete(document);
window.addEventListener("pageshow", () => initializeComposerCounters(document));
window.requestAnimationFrame(() => initializeComposerCounters(document));
if (window.MutationObserver) {
  new MutationObserver((mutations) => {
    mutations.forEach((mutation) => {
      mutation.addedNodes.forEach((node) => {
        initializeComposerCounters(node);
        initializeMentionAutocomplete(node);
      });
    });
  }).observe(document.documentElement, { childList: true, subtree: true });
}

function mediaSummary(input) {
  if (!input.files || input.files.length === 0) {
    return "";
  }
  if (input.files.length === 1) {
    return input.files[0].name || "1 media file selected";
  }
  return `${input.files.length} media files selected`;
}

function updateComposerMedia(input) {
  const form = input.closest("form");
  if (!form) {
    return;
  }
  const hasMedia = !!(input.files && input.files.length > 0);
  const selection = form.querySelector("[data-composer-media-selection]");
  const summary = form.querySelector("[data-composer-media-summary]");
  const nsfw = form.querySelector("[data-composer-nsfw]");
  if (summary) {
    summary.textContent = mediaSummary(input);
  }
  if (selection) {
    selection.hidden = !hasMedia;
  }
  if (nsfw && !hasMedia) {
    nsfw.checked = false;
  }
}

document.querySelectorAll("input[type=file][data-composer-media]").forEach((input) => {
  updateComposerMedia(input);
  input.addEventListener("change", () => updateComposerMedia(input));
});

document.addEventListener("click", (event) => {
  const button = event.target.closest("[data-composer-clear-media]");
  if (!button) {
    return;
  }
  const form = button.closest("form");
  const input = form ? form.querySelector("input[type=file][data-composer-media]") : null;
  if (!input) {
    return;
  }
  input.value = "";
  updateComposerMedia(input);
  input.focus();
});

function mediaFileName(input) {
  if (!input.files || input.files.length === 0) {
    return "";
  }
  return input.files[0].name || "media file";
}

function setProfileMediaStatus(frame, message) {
  const status = frame.querySelector("[data-profile-media-status]");
  if (status) {
    status.textContent = message;
  }
}

function updateProfileMediaFile(input) {
  const frame = input.closest("[data-profile-media-frame]");
  if (!frame) {
    return;
  }
  const fileName = mediaFileName(input);
  frame.classList.toggle("settings-media-has-file", fileName !== "");
  if (fileName !== "") {
    const remove = frame.querySelector("[data-profile-media-delete]");
    if (remove) {
      remove.checked = false;
      frame.classList.remove("settings-media-removing");
    }
    setProfileMediaStatus(frame, `${fileName} selected`);
  } else {
    setProfileMediaStatus(frame, "");
  }
}

function updateProfileMediaDelete(input) {
  const frame = input.closest("[data-profile-media-frame]");
  if (!frame) {
    return;
  }
  frame.classList.toggle("settings-media-removing", input.checked);
  const file = frame.querySelector("[data-profile-media-file]");
  if (input.checked && file) {
    file.value = "";
    updateProfileMediaFile(file);
    frame.classList.add("settings-media-removing");
  }
  const label = input.getAttribute("aria-label") || "Media";
  setProfileMediaStatus(frame, input.checked ? `${label} selected` : `${label} cancelled`);
}

document.querySelectorAll("[data-profile-media-file]").forEach(updateProfileMediaFile);

document.addEventListener("change", (event) => {
  if (!(event.target instanceof Element)) {
    return;
  }
  const file = event.target.closest("[data-profile-media-file]");
  if (file) {
    updateProfileMediaFile(file);
    return;
  }
  const remove = event.target.closest("[data-profile-media-delete]");
  if (remove) {
    updateProfileMediaDelete(remove);
  }
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
      form.querySelectorAll("input[type=file][data-composer-media]").forEach(updateComposerMedia);
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
});

async function copyTextToClipboard(text) {
  if (navigator.clipboard && navigator.clipboard.writeText) {
    try {
      await navigator.clipboard.writeText(text);
      return;
    } catch (_err) {
      // Fall back to the selection-based path below for browsers that expose
      // the Clipboard API but deny writes in this context.
    }
  }
  const textarea = document.createElement("textarea");
  textarea.value = text;
  textarea.setAttribute("readonly", "");
  textarea.style.position = "fixed";
  textarea.style.top = "-1000px";
  document.body.append(textarea);
  textarea.select();
  const copied = document.execCommand("copy");
  textarea.remove();
  if (!copied) {
    throw new Error("copy command failed");
  }
}

function setCopyButtonLabel(button, label) {
  const feedback = button.querySelector("[data-copy-feedback]");
  if (feedback) {
    feedback.textContent = label;
    return;
  }
  button.textContent = label;
}

document.addEventListener("click", async (event) => {
  const button = event.target.closest("[data-copy-text]");
  if (!button) {
    return;
  }
  event.preventDefault();
  event.stopPropagation();
  const original = button.getAttribute("data-copy-label") || button.textContent || "Copy";
  if (button.dataset.copyTimer) {
    window.clearTimeout(Number.parseInt(button.dataset.copyTimer, 10));
    delete button.dataset.copyTimer;
  }
  try {
    await copyTextToClipboard(button.getAttribute("data-copy-text") || "");
    setCopyButtonLabel(button, button.getAttribute("data-copied-label") || "Copied");
  } catch (_err) {
    setCopyButtonLabel(button, button.getAttribute("data-copy-failed-label") || "Failed");
  }
  button.dataset.copyTimer = String(window.setTimeout(() => {
    setCopyButtonLabel(button, original);
    delete button.dataset.copyTimer;
  }, 1600));
});"#;

pub fn client_script() -> &'static str {
    CLIENT_SCRIPT
}

pub fn client_boot_script() -> &'static str {
    CLIENT_BOOT_SCRIPT
}

pub fn composer(csrf: Option<&str>, parent: Option<i64>, max_text_chars: usize) -> String {
    let parent_input = parent.map_or_else(String::new, |id| {
        format!(r#"<input type="hidden" name="parent_post_id" value="{id}">"#)
    });
    let csrf = csrf.unwrap_or_default();
    let input_id = parent.map_or_else(|| "post-text".to_owned(), |id| format!("reply-text-{id}"));
    let mention_menu_id = format!("{input_id}-mention-menu");
    let media_id = parent.map_or_else(|| "post-media".to_owned(), |id| format!("reply-media-{id}"));
    let nsfw_id = parent.map_or_else(|| "post-nsfw".to_owned(), |id| format!("reply-nsfw-{id}"));
    let placeholder = if parent.is_some() {
        "Write a reply..."
    } else {
        "What's happening?"
    };
    format!(
        r#"<section class="composer" id="reply" aria-labelledby="composer-title"><div class="section-heading"><h1 id="composer-title">{}</h1><span class="muted character-counter character-counter-normal" data-character-counter="{}" aria-live="polite">{} remaining</span></div><form method="post" action="/posts" enctype="multipart/form-data" data-enhance="post-create">
<input type="hidden" name="csrf" value="{}">{}
<label class="sr-only" for="{}">What is happening?</label>
<div class="composer-surface">
<textarea id="{}" name="text" rows="4" data-character-limit="{}" data-mention-autocomplete aria-autocomplete="list" aria-haspopup="listbox" aria-expanded="false" aria-controls="{}" placeholder="{}"></textarea>
<div id="{}" class="mention-menu" role="listbox" data-mention-menu hidden></div>
<div class="composer-footer"><label class="composer-file-control" for="{}"><input class="composer-file-input" id="{}" name="media" type="file" multiple accept="image/*,video/mp4,video/webm,video/quicktime" aria-label="Attach media" data-composer-media><span class="composer-file-button" aria-hidden="true">{}<span>Attach media</span></span></label></div>
<div class="composer-media-selection" data-composer-media-selection hidden><span class="composer-media-summary" data-composer-media-summary></span><label class="check-row composer-nsfw" for="{}"><input id="{}" name="nsfw" type="checkbox" value="true" data-composer-nsfw> Mark media as NSFW</label><button class="composer-clear-media" type="button" data-composer-clear-media>Clear</button></div>
</div>
<div class="composer-tools"><span></span><button class="primary" type="submit">Post</button></div>
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
        html_escape::encode_double_quoted_attribute(&mention_menu_id),
        html_escape::encode_double_quoted_attribute(placeholder),
        html_escape::encode_double_quoted_attribute(&mention_menu_id),
        html_escape::encode_double_quoted_attribute(&media_id),
        html_escape::encode_double_quoted_attribute(&media_id),
        icon_svg("paperclip"),
        html_escape::encode_double_quoted_attribute(&nsfw_id),
        html_escape::encode_double_quoted_attribute(&nsfw_id),
    )
}

pub fn quote_composer(csrf: &str, quote: &QuotePreview, max_text_chars: usize) -> String {
    let preview = quote_preview_card(quote);
    format!(
        r#"<section class="composer quote-composer" aria-labelledby="composer-title"><div class="section-heading"><h1 id="composer-title">Quote post</h1><span class="muted character-counter character-counter-normal" data-character-counter="quote-text" aria-live="polite">{max_text_chars} remaining</span></div>{preview}<form method="post" action="/posts/{}/quote" class="quote-form">
<input type="hidden" name="csrf" value="{}">
<label class="sr-only" for="quote-text">Add your comment</label>
<textarea id="quote-text" name="text" rows="4" data-character-limit="{}" placeholder="Add your thoughts..." required></textarea>
<div class="composer-tools"><span></span><button class="primary" type="submit">Post quote</button></div>
</form></section>"#,
        quote.id,
        html_escape::encode_double_quoted_attribute(csrf),
        max_text_chars
    )
}

pub fn accounts(accounts: &[AccountView], csrf: &str) -> String {
    if accounts.is_empty() {
        return empty_state(
            "Follow people to build your feed.",
            "Accounts you follow will appear here.",
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

pub fn onboarding_page(page: OnboardingPage<'_>) -> String {
    let picture = page.picture_path.map_or_else(
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
    let picture_control = if page.allow_profile_pictures {
        r#"<label class="file-control" for="profile_picture">Choose profile picture<input id="profile_picture" name="profile_picture" type="file" accept="image/*"></label>"#
            .to_owned()
    } else {
        r#"<p class="muted">Profile pictures are disabled.</p>"#.to_owned()
    };
    let suggestions = if page.suggestions.is_empty() {
        r#"<p class="muted">No local accounts to suggest yet.</p>"#.to_owned()
    } else {
        page.suggestions
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
                let checked = if account.viewer_following {
                    " checked"
                } else {
                    ""
                };
                let bio = if account.bio.trim().is_empty() {
                    String::new()
                } else {
                    format!(
                        r#"<span class="muted">{}</span>"#,
                        html_escape::encode_text(&account.bio)
                    )
                };
                format!(
                    r#"<label class="onboarding-suggestion">{avatar}<input type="checkbox" name="follow_user_id" value="{}"{checked}><span><strong>{}</strong> <span class="username">@{}</span>{bio}</span></label>"#,
                    account.id,
                    html_escape::encode_text(&account.display_name),
                    html_escape::encode_text(&account.username),
                )
            })
            .collect::<Vec<_>>()
            .join("")
    };
    format!(
        r#"<section class="panel onboarding-panel"><p class="eyebrow">First run</p><h1>Set up your account</h1><form method="post" action="/onboarding" enctype="multipart/form-data" class="onboarding-form"><input type="hidden" name="csrf" value="{}"><div class="onboarding-media-row">{picture}<div>{picture_control}</div></div><div class="settings-fields"><label for="display_name">Display name</label><input id="display_name" name="display_name" maxlength="{}" value="{}"><label for="bio">Bio</label><textarea id="bio" name="bio" maxlength="{}">{}</textarea></div><fieldset class="onboarding-suggestions"><legend>Follow accounts</legend>{suggestions}</fieldset><div class="actions"><button class="primary" type="submit" name="intent" value="save">Finish setup</button><button type="submit" name="intent" value="skip">Skip for now</button></div></form></section>"#,
        html_escape::encode_double_quoted_attribute(page.csrf),
        page.max_display_name_len,
        html_escape::encode_double_quoted_attribute(page.display_name),
        page.max_bio_len,
        html_escape::encode_text(page.bio),
    )
}

pub fn account_links(accounts: &[AccountView], empty_message: &str) -> String {
    account_links_with_empty_state(accounts, EmptyState::new(empty_message, ""))
}

pub fn account_links_with_empty_state(accounts: &[AccountView], empty: EmptyState<'_>) -> String {
    if accounts.is_empty() {
        return empty_state(empty.title, empty.message);
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

pub fn edit_post_form(
    csrf: &str,
    post_id: i64,
    text: &str,
    max_text_chars: usize,
    return_to: &str,
) -> String {
    format!(
        r#"<section class="composer edit-composer" aria-labelledby="edit-post-title"><div class="section-heading"><h1 id="edit-post-title">Edit post</h1><span class="muted character-counter character-counter-normal" data-character-counter="edit-post-text" aria-live="polite">{max_text_chars} remaining</span></div><form method="post" action="/posts/{post_id}/edit" class="edit-post-form"><input type="hidden" name="csrf" value="{}"><input type="hidden" name="return_to" value="{}"><label class="sr-only" for="edit-post-text">Post text</label><textarea id="edit-post-text" name="text" rows="5" data-character-limit="{max_text_chars}">{}</textarea><div class="composer-tools"><a class="button-link" href="{}">Cancel</a><button class="primary" type="submit">Save edit</button></div></form></section>"#,
        html_escape::encode_double_quoted_attribute(csrf),
        html_escape::encode_double_quoted_attribute(return_to),
        html_escape::encode_text(text),
        html_escape::encode_double_quoted_attribute(return_to),
    )
}

pub fn search_page(
    site_name: &str,
    query: &str,
    users: &[AccountView],
    posts: &[PostView],
    user: Option<&CurrentUser>,
    csrf: Option<&str>,
    options: SearchRenderOptions,
) -> String {
    let form = search_form(site_name, query);
    let state = if query.is_empty() {
        empty_state(
            "Find posts and people",
            "Search for posts, usernames, mentions, or hashtags.",
        )
    } else if users.is_empty() && posts.is_empty() {
        empty_state("No matching posts or users found.", "Try another search.")
    } else {
        search_results(query, users, posts, user, csrf, options)
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
    options: SearchRenderOptions,
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
            posts_with_options(
                posts,
                user,
                csrf,
                PostRenderOptions {
                    blur_nsfw_media: options.blur_nsfw_media,
                    ..PostRenderOptions::timeline()
                        .with_edit_window(options.post_edit_window_seconds)
                },
                EmptyState::default_posts(),
            )
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
    posts_with_options(
        posts,
        user,
        csrf,
        PostRenderOptions::timeline(),
        EmptyState::default_posts(),
    )
}

pub fn posts_with_nsfw_blur(
    posts: &[PostView],
    user: Option<&CurrentUser>,
    csrf: Option<&str>,
    blur_nsfw_media: bool,
) -> String {
    posts_with_options(
        posts,
        user,
        csrf,
        PostRenderOptions {
            blur_nsfw_media,
            ..PostRenderOptions::timeline()
        },
        EmptyState::default_posts(),
    )
}

pub fn posts_with_controls(
    posts: &[PostView],
    user: Option<&CurrentUser>,
    csrf: Option<&str>,
    blur_nsfw_media: bool,
    post_edit_window_seconds: u64,
) -> String {
    posts_with_controls_empty_state(
        posts,
        user,
        csrf,
        blur_nsfw_media,
        post_edit_window_seconds,
        EmptyState::default_posts(),
    )
}

pub fn posts_with_controls_empty_state(
    posts: &[PostView],
    user: Option<&CurrentUser>,
    csrf: Option<&str>,
    blur_nsfw_media: bool,
    post_edit_window_seconds: u64,
    empty: EmptyState<'_>,
) -> String {
    posts_with_options(
        posts,
        user,
        csrf,
        PostRenderOptions {
            blur_nsfw_media,
            ..PostRenderOptions::timeline().with_edit_window(post_edit_window_seconds)
        },
        empty,
    )
}

pub fn profile_tabs(username: &str, active: ProfileTimelineTab) -> String {
    let tabs = [
        (ProfileTimelineTab::Posts, "Posts"),
        (ProfileTimelineTab::Replies, "Replies"),
        (ProfileTimelineTab::Media, "Media"),
        (ProfileTimelineTab::Likes, "Likes"),
    ];
    let links = tabs
        .iter()
        .map(|(tab, label)| {
            let href = profile_tab_href(username, *tab);
            let active_attrs = if *tab == active {
                r#" class="active" aria-current="page""#
            } else {
                ""
            };
            format!(
                r#"<a href="{}"{}>{}</a>"#,
                html_escape::encode_double_quoted_attribute(&href),
                active_attrs,
                html_escape::encode_text(label)
            )
        })
        .collect::<Vec<_>>()
        .join("");
    format!(
        r#"<nav class="profile-tabs" data-testid="profile-tabs" aria-label="Profile timelines">{links}</nav>"#
    )
}

fn profile_tab_href(username: &str, tab: ProfileTimelineTab) -> String {
    match tab {
        ProfileTimelineTab::Posts => format!("/users/{username}"),
        ProfileTimelineTab::Replies => format!("/users/{username}?tab=replies"),
        ProfileTimelineTab::Media => format!("/users/{username}?tab=media"),
        ProfileTimelineTab::Likes => format!("/users/{username}?tab=likes"),
    }
}

pub fn pinned_post_with_controls(
    post: &PostView,
    user: Option<&CurrentUser>,
    csrf: Option<&str>,
    blur_nsfw_media: bool,
    post_edit_window_seconds: u64,
) -> String {
    let card = post_card_with_options(
        post,
        user,
        csrf,
        PostRenderOptions {
            blur_nsfw_media,
            ..PostRenderOptions::timeline().with_edit_window(post_edit_window_seconds)
        },
    );
    format!(
        r#"<section class="timeline pinned-timeline" aria-label="Pinned post"><h2 class="section-title">Pinned post</h2>{card}</section>"#
    )
}

pub fn thread_posts(posts: &[PostView], user: Option<&CurrentUser>, csrf: Option<&str>) -> String {
    thread_posts_with_options(posts, user, csrf, PostRenderOptions::thread())
}

pub fn thread_posts_with_nsfw_blur(
    posts: &[PostView],
    user: Option<&CurrentUser>,
    csrf: Option<&str>,
    blur_nsfw_media: bool,
) -> String {
    thread_posts_with_options(
        posts,
        user,
        csrf,
        PostRenderOptions {
            blur_nsfw_media,
            ..PostRenderOptions::thread()
        },
    )
}

pub fn thread_posts_with_controls(
    posts: &[PostView],
    user: Option<&CurrentUser>,
    csrf: Option<&str>,
    blur_nsfw_media: bool,
    post_edit_window_seconds: u64,
) -> String {
    thread_posts_with_options(
        posts,
        user,
        csrf,
        PostRenderOptions {
            blur_nsfw_media,
            ..PostRenderOptions::thread().with_edit_window(post_edit_window_seconds)
        },
    )
}

fn thread_posts_with_options(
    posts: &[PostView],
    user: Option<&CurrentUser>,
    csrf: Option<&str>,
    options: PostRenderOptions,
) -> String {
    if posts.is_empty() {
        let empty = EmptyState::default_posts();
        return empty_state(empty.title, empty.message);
    }
    format!(
        r#"<section class="timeline" aria-label="Posts">{}</section>"#,
        posts
            .iter()
            .enumerate()
            .map(|(index, post)| {
                let mut card_options = options;
                if index == 0 {
                    card_options.clickable_card = false;
                }
                post_card_with_options(post, user, csrf, card_options)
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
    empty: EmptyState<'_>,
) -> String {
    if posts.is_empty() {
        return empty_state(empty.title, empty.message);
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

pub fn post_card_with_nsfw_blur(
    post: &PostView,
    user: Option<&CurrentUser>,
    csrf: Option<&str>,
    blur_nsfw_media: bool,
) -> String {
    post_card_with_options(
        post,
        user,
        csrf,
        PostRenderOptions {
            blur_nsfw_media,
            ..PostRenderOptions::timeline()
        },
    )
}

pub fn post_card_with_controls(
    post: &PostView,
    user: Option<&CurrentUser>,
    csrf: Option<&str>,
    blur_nsfw_media: bool,
    post_edit_window_seconds: u64,
) -> String {
    post_card_with_options(
        post,
        user,
        csrf,
        PostRenderOptions {
            blur_nsfw_media,
            ..PostRenderOptions::timeline().with_edit_window(post_edit_window_seconds)
        },
    )
}

pub fn thread_post_card(post: &PostView, user: Option<&CurrentUser>, csrf: Option<&str>) -> String {
    post_card_with_options(post, user, csrf, PostRenderOptions::thread())
}

pub fn thread_post_card_with_nsfw_blur(
    post: &PostView,
    user: Option<&CurrentUser>,
    csrf: Option<&str>,
    blur_nsfw_media: bool,
) -> String {
    post_card_with_options(
        post,
        user,
        csrf,
        PostRenderOptions {
            blur_nsfw_media,
            ..PostRenderOptions::thread()
        },
    )
}

pub fn thread_post_card_with_controls(
    post: &PostView,
    user: Option<&CurrentUser>,
    csrf: Option<&str>,
    blur_nsfw_media: bool,
    post_edit_window_seconds: u64,
) -> String {
    post_card_with_options(
        post,
        user,
        csrf,
        PostRenderOptions {
            blur_nsfw_media,
            ..PostRenderOptions::thread().with_edit_window(post_edit_window_seconds)
        },
    )
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
    let youtube_previews = render_youtube_previews(post);
    let media = post
        .media
        .iter()
        .enumerate()
        .map(|(index, media)| render_media(media, post.id, index, options.blur_nsfw_media))
        .collect::<Vec<_>>()
        .join("");
    let controls = if let (Some(user), Some(csrf)) = (user, csrf) {
        let delete = if post.user_id == Some(user.id) || user.is_admin {
            icon_link(&format!("/posts/{}/delete", post.id), "Delete", "trash")
        } else {
            String::new()
        };
        let edit = if post.user_id == Some(user.id)
            && post_edit_available(&post.created_at, options.post_edit_window_seconds)
        {
            icon_link(&format!("/posts/{}/edit", post.id), "Edit", "edit")
        } else {
            String::new()
        };
        let pin = if post.user_id == Some(user.id) {
            pin_action_form(
                &format!("/posts/{}/pin", post.id),
                csrf,
                if post.pinned_by_author {
                    "Unpin from profile"
                } else {
                    "Pin to profile"
                },
                post.pinned_by_author,
            )
        } else {
            String::new()
        };
        let nsfw = if user.is_admin && !post.media.is_empty() {
            admin_nsfw_form(post, csrf)
        } else {
            String::new()
        };
        let reply_link = icon_link(&format!("/posts/{}#reply", post.id), "Reply", "reply");
        format!(
            r#"<div class="actions" data-testid="post-actions">{}{}{}{}{}{}{}{}</div>"#,
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
            pin,
            edit,
            delete,
            nsfw,
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
    let edited = post.edited_at.as_ref().map_or_else(String::new, |_| {
        r#"<span class="edited-marker" title="Post was edited">edited</span>"#.to_owned()
    });
    let quote = post
        .quote
        .as_ref()
        .map_or_else(String::new, quote_preview_card);
    format!(
        r#"<article class="{}" data-testid="post-card" id="post-{}" data-post-id="{}" data-event-id="{}"{}>{}{}<header class="post-header"><div class="author-block">{}<div>{}</div></div>{}</header><div class="text">{}</div>{}{}{}<div class="counts"><span data-count="likes">{} likes</span><span data-count="reposts">{} reposts</span><span data-count="replies">{} replies</span>{}{}</div>{}</article>"#,
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
        youtube_previews,
        media,
        quote,
        post.like_count,
        post.repost_count,
        post.reply_count,
        edited,
        permalink,
        controls
    )
}

fn post_edit_available(created_at: &str, window_seconds: u64) -> bool {
    if window_seconds == 0 {
        return false;
    }
    let Some(created) = parse_timestamp(created_at) else {
        return false;
    };
    let Ok(window_seconds) = i64::try_from(window_seconds) else {
        return false;
    };
    let elapsed = Utc::now().signed_duration_since(created);
    elapsed.num_seconds() >= 0 && elapsed.num_seconds() <= window_seconds
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

fn pin_action_form(action: &str, csrf: &str, label: &str, active: bool) -> String {
    format!(
        r#"<form method="post" action="{}"><input type="hidden" name="csrf" value="{}"><button class="icon-button{}" type="submit" aria-pressed="{}" aria-label="{}" title="{}">{}<span class="sr-only">{}</span></button></form>"#,
        html_escape::encode_double_quoted_attribute(action),
        html_escape::encode_double_quoted_attribute(csrf),
        if active { " active" } else { "" },
        if active { "true" } else { "false" },
        html_escape::encode_double_quoted_attribute(label),
        html_escape::encode_double_quoted_attribute(label),
        icon_svg("pin"),
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

fn admin_nsfw_form(post: &PostView, csrf: &str) -> String {
    let flagged = post.media.iter().any(|media| media.is_nsfw);
    let (value, label) = if flagged {
        ("false", "Unmark NSFW")
    } else {
        ("true", "Mark NSFW")
    };
    format!(
        r#"<form method="post" action="/admin/posts/{}/nsfw" class="admin-nsfw-form"><input type="hidden" name="csrf" value="{}"><input type="hidden" name="nsfw" value="{value}"><button class="admin-nsfw-button" type="submit">{}</button></form>"#,
        post.id,
        html_escape::encode_double_quoted_attribute(csrf),
        html_escape::encode_text(label),
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

pub(crate) fn icon_svg(icon: &str) -> &'static str {
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
        "edit" => {
            r#"<svg aria-hidden="true" viewBox="0 0 24 24"><path d="M4 17.8V21h3.2L18.7 9.5l-3.2-3.2L4 17.8zM17.1 4.7l3.2 3.2 1.4-1.4c.4-.4.4-1 0-1.4l-1.8-1.8c-.4-.4-1-.4-1.4 0l-1.4 1.4z"/></svg>"#
        }
        "pin" => {
            r#"<svg aria-hidden="true" viewBox="0 0 24 24"><path d="M14 2l8 8-2.1 2.1-1.4-1.4-4.2 4.2.7 4.9-1.6 1.6-4.2-4.2L4.4 22 2 19.6l4.8-4.8-4.2-4.2L4.2 9l4.9.7 4.2-4.2L11.9 4 14 2z"/></svg>"#
        }
        "paperclip" => {
            r#"<svg aria-hidden="true" viewBox="0 0 24 24"><path d="M8.2 20.5a5.2 5.2 0 0 1-3.7-8.9l7.8-7.8a3.7 3.7 0 0 1 5.2 5.2l-7.8 7.8a2.2 2.2 0 0 1-3.1-3.1l7.3-7.3L16 8.5l-7.3 7.3.2.2 7.8-7.8a.7.7 0 0 0-1-1l-7.8 7.8a2.2 2.2 0 0 0 3.1 3.1l8.2-8.2 2.1 2.1-8.2 8.2a5.2 5.2 0 0 1-3.7 1.5c-.4 0-.8 0-1.2-.1z"/></svg>"#
        }
        _ => r#"<svg aria-hidden="true" viewBox="0 0 24 24"><circle cx="12" cy="12" r="8"/></svg>"#,
    }
}

fn render_media(media: &MediaView, post_id: i64, index: usize, blur_nsfw_media: bool) -> String {
    let path = html_escape::encode_double_quoted_attribute(&media.public_path);
    let alt = html_escape::encode_double_quoted_attribute(&media.alt_text);
    let item = if media.media_kind == "video" {
        format!(r#"<video controls preload="metadata" src="{path}"></video>"#)
    } else {
        format!(r#"<img src="{path}" alt="{alt}" loading="lazy">"#)
    };
    if !media.is_nsfw || !blur_nsfw_media {
        return item;
    }
    let toggle_id = format!("nsfw-media-{post_id}-{index}");
    format!(
        r#"<div class="nsfw-media" data-testid="nsfw-media"><input class="nsfw-toggle sr-only" id="{toggle_id}" type="checkbox" aria-label="Show NSFW media"><div class="nsfw-media-frame">{item}<span class="nsfw-badge">NSFW</span></div><label class="nsfw-show" for="{toggle_id}">Show<span class="sr-only"> NSFW media</span></label></div>"#
    )
}

fn render_youtube_previews(post: &PostView) -> String {
    let fallback_embeds;
    let previews = if post.youtube_embeds.is_empty() {
        fallback_embeds = youtube::embeds_for_text(&post.text);
        fallback_embeds.as_slice()
    } else {
        post.youtube_embeds.as_slice()
    };
    if previews.is_empty() {
        return String::new();
    }
    let cards = previews
        .iter()
        .map(render_youtube_preview_card)
        .collect::<Vec<_>>()
        .join("");
    format!(r#"<aside class="youtube-previews" aria-label="YouTube link previews">{cards}</aside>"#)
}

fn render_youtube_preview_card(preview: &YoutubeEmbed) -> String {
    let href = html_escape::encode_double_quoted_attribute(&preview.canonical_url);
    let display_url = html_escape::encode_text(&preview.source_url);
    let thumbnail = html_escape::encode_double_quoted_attribute(&preview.thumbnail_url);
    let embed_src = html_escape::encode_double_quoted_attribute(&preview.embed_url);
    let title = preview.display_title();
    let title_text = html_escape::encode_text(title);
    let title_attr = html_escape::encode_double_quoted_attribute(title);
    format!(
        r#"<div class="youtube-preview-card" data-testid="youtube-preview-card" data-youtube-preview><a class="youtube-preview-main" data-youtube-play href="{href}" data-youtube-embed-src="{embed_src}" data-youtube-title="{title_attr}" rel="noopener noreferrer" referrerpolicy="no-referrer"><span class="youtube-thumbnail-frame"><img class="youtube-thumbnail" src="{thumbnail}" alt="" loading="lazy" decoding="async" referrerpolicy="no-referrer"><span class="youtube-play" aria-hidden="true"></span><span class="sr-only">Play {title_text}</span></span><span class="youtube-preview-body"><span class="youtube-preview-source">YouTube</span><span class="youtube-preview-title">{title_text}</span><span class="youtube-preview-url">{display_url}</span></span></a><div class="youtube-preview-actions"><a class="youtube-open-link" href="{href}" rel="noopener noreferrer" referrerpolicy="no-referrer">Open on YouTube</a></div><div class="youtube-player-frame" data-youtube-player hidden></div></div>"#,
    )
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
    let message = empty_state_message(message);
    format!(
        r#"<section class="empty-state" data-testid="empty-state"><h2>{}</h2>{message}</section>"#,
        html_escape::encode_text(title),
    )
}

pub fn compact_empty_state(title: &str, message: &str) -> String {
    compact_empty_state_with_class("", title, message)
}

pub fn compact_empty_state_with_class(extra_class: &str, title: &str, message: &str) -> String {
    let class = if extra_class.trim().is_empty() {
        "compact-empty".to_owned()
    } else {
        format!("compact-empty {}", extra_class.trim())
    };
    let message = empty_state_message(message);
    format!(
        r#"<div class="{}"><strong>{}</strong>{message}</div>"#,
        html_escape::encode_double_quoted_attribute(&class),
        html_escape::encode_text(title),
    )
}

fn empty_state_message(message: &str) -> String {
    if message.trim().is_empty() {
        String::new()
    } else {
        format!(r#"<p>{}</p>"#, html_escape::encode_text(message))
    }
}

pub fn page_header(title: &str, subtitle: &str) -> String {
    format!(
        r#"<section class="page-header"><h1>{}</h1><p>{}</p></section>"#,
        html_escape::encode_text(title),
        html_escape::encode_text(subtitle)
    )
}

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

fn grouped_notification_rows(notifications: &[NotificationGroupView], csrf: &str) -> String {
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

fn notification_group(notification: &NotificationGroupView) -> &'static str {
    if notification.unread_count > 0 {
        "New"
    } else if is_today(&notification.created_at) {
        "Today"
    } else {
        "Earlier"
    }
}

fn notification_row(notification: &NotificationGroupView, csrf: &str) -> String {
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

fn notification_line(notification: &NotificationGroupView) -> String {
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

fn notification_actor(
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

fn notification_actor_details(notification: &NotificationGroupView) -> String {
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

fn notification_open_control(
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
    format!(
        r#"<form id="{}" class="notification-open-form" method="post" action="/notifications/open"><input type="hidden" name="csrf" value="{}"><input type="hidden" name="notification_ids" value="{}"><input type="hidden" name="group_kind" value="{}">{group_target}<input type="hidden" name="return_to" value="{}"><button class="button-link notification-open" type="submit">Open</button></form>"#,
        html_escape::encode_double_quoted_attribute(form_id),
        html_escape::encode_double_quoted_attribute(csrf),
        html_escape::encode_double_quoted_attribute(&notification_ids),
        html_escape::encode_double_quoted_attribute(&notification.kind),
        html_escape::encode_double_quoted_attribute(target)
    )
}

fn notification_preview(notification: &NotificationGroupView, target: Option<&str>) -> String {
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

fn notification_target(notification: &NotificationGroupView) -> Option<String> {
    if notification.kind == "follow" {
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

fn notification_count_meta(notification: &NotificationGroupView) -> String {
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

fn notification_people_label(count: usize) -> String {
    if count == 1 {
        "1 person".to_owned()
    } else {
        format!("{count} people")
    }
}

fn notification_items_label(count: usize) -> String {
    if count == 1 {
        "1 notification".to_owned()
    } else {
        format!("{count} notifications")
    }
}

fn notification_unread_label(count: i64) -> String {
    if count == 1 {
        "1 unread".to_owned()
    } else {
        format!("{count} unread")
    }
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

fn notification_group_action_text(kind: &str) -> &'static str {
    match kind {
        "reply" => "replied to your post",
        "like" => "liked your post",
        "repost" => "reposted your post",
        "quote" => "quoted your post",
        "follow" => "followed you",
        "mention" => "mentioned you in a post",
        _ => "sent you notifications",
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
:root{font-family:Inter,ui-sans-serif,system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;color-scheme:light;line-height:1.5;--bg:#f5f6f1;--surface:#fff;--surface-subtle:#fbfcfa;--surface-muted:#f4f5f2;--header-bg:rgba(255,255,255,.96);--text:#202124;--text-strong:#172017;--muted:#667064;--muted-strong:#59625a;--border:#dfe4dc;--border-strong:#b9c2b8;--link:#1f5f8b;--link-strong:#24445f;--brand:#163b2f;--brand-hover:#235544;--brand-text:#fff;--hover:#eef3f0;--focus:#93c5fd;--shadow:rgba(20,35,30,.04);--reply-border:#c8d8d0;--avatar-bg:#eef3f0;--warning:#9a5a00;--danger:#8a3d2d;--danger-strong:#6f2f22;--danger-bg:#fff8f5;--danger-border:#e6b8a8;--success-bg:#f4fbf5;--success-border:#add7b4;--media-bg:#f6f7f4;--card-gap:.5rem;--section-gap:.75rem;--shell-side:240px;--shell-primary:640px;--shell-gap:1.25rem;--shell-max:1160px;--header-padding-y:.8rem;--header-brand-size:2rem;--hairline:1px;--rail-sticky-top:calc(var(--header-brand-size) + var(--header-padding-y) + var(--header-padding-y) + var(--shell-gap) + var(--hairline))}
:root[data-theme="dark"]{color-scheme:dark;--bg:#111827;--surface:#182231;--surface-subtle:#1d2939;--surface-muted:#233044;--header-bg:rgba(17,24,39,.96);--text:#eef4fb;--text-strong:#f8fafc;--muted:#c3cfdd;--muted-strong:#d4deea;--border:#344256;--border-strong:#596b83;--link:#8fc7ff;--link-strong:#badcff;--brand:#4f8fc7;--brand-hover:#6aa8df;--brand-text:#06111f;--hover:#243349;--focus:#fbbf24;--shadow:rgba(0,0,0,.26);--reply-border:#4f6680;--avatar-bg:#243349;--warning:#f6c36b;--danger:#ffb4a2;--danger-strong:#ffd2c7;--danger-bg:#3a2020;--danger-border:#8f4d43;--success-bg:#163321;--success-border:#4c8a61;--media-bg:#0f172a}
*{box-sizing:border-box}body{margin:0;min-width:320px;color:var(--text);background:var(--bg)}a{color:var(--link);text-decoration:none}a:hover{text-decoration:underline}
.site-header{position:sticky;top:0;z-index:10;background:var(--header-bg);border-bottom:1px solid var(--border);backdrop-filter:blur(8px)}
.header-inner{max-width:var(--shell-max);margin:0 auto;padding:var(--header-padding-y) 1rem;display:flex;align-items:center;justify-content:space-between;gap:1rem}
.header-brand-row{display:flex;align-items:center;gap:.75rem;min-width:0;max-width:100%}.brand{display:flex;align-items:center;gap:.55rem;font-weight:800;color:var(--text-strong);min-width:0}.brand span:last-child{overflow-wrap:anywhere}.brand-mark{display:inline-grid;place-items:center;width:var(--header-brand-size);height:var(--header-brand-size);border-radius:7px;background:var(--brand);color:var(--brand-text);flex:0 0 auto}
.tor-indicator{position:relative;display:inline-flex;align-items:center;min-width:0;max-width:min(18rem,100%);flex:0 1 auto;color:var(--muted-strong);font-size:.84rem;line-height:1;white-space:nowrap}.tor-disclosure{position:relative;min-width:0;max-width:100%;flex:0 1 auto}.tor-pill{display:inline-flex;align-items:center;gap:.38rem;max-width:100%;min-height:2.15rem;border:1px solid var(--border);border-radius:999px;padding:.25rem .68rem;background:var(--surface-subtle);color:var(--link-strong);font-weight:800;cursor:pointer;list-style:none;overflow:hidden}.js-enabled .tor-pill{padding-right:3.85rem}.tor-pill::-webkit-details-marker{display:none}.tor-pill::marker{content:""}.tor-pill:hover{background:var(--hover);text-decoration:none}.tor-pill-label{flex:0 0 auto;color:var(--muted);font-weight:900}.tor-summary-text{display:block;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;font-variant-numeric:tabular-nums}.tor-details{position:absolute;z-index:12;left:0;top:calc(100% + .35rem);width:max-content;max-width:min(30rem,calc(100vw - 2rem));padding:.55rem .65rem;border:1px solid var(--border-strong);border-radius:8px;background:var(--surface);box-shadow:0 8px 24px var(--shadow);white-space:normal}.tor-full-link{display:block;max-width:min(28rem,calc(100vw - 3.5rem));overflow-wrap:anywhere;color:var(--text-strong);font-weight:800;line-height:1.3}.tor-full-link:hover{color:var(--link-strong)}.tor-copy-button{display:none;position:absolute;z-index:13;right:.22rem;top:50%;transform:translateY(-50%);align-items:center;justify-content:center;width:3.25rem;min-height:1.65rem;border:1px solid var(--border);border-radius:999px;background:var(--surface);color:var(--link-strong);padding:.12rem .35rem;font-size:.78rem;font-weight:850;line-height:1;box-shadow:none}.js-enabled .tor-copy-button{display:inline-flex}.tor-copy-button:hover{background:var(--hover);color:var(--text-strong)}
nav{display:flex;gap:.35rem;align-items:center;flex-wrap:wrap;justify-content:flex-end}nav a,nav button,.button-link{display:inline-flex;align-items:center;gap:.35rem;min-height:2.15rem;border-radius:7px;padding:.42rem .65rem;color:var(--link-strong);border:1px solid transparent;background:transparent}
nav a:hover,nav button:hover,.button-link:hover{background:var(--hover);text-decoration:none}nav form,.actions form{display:inline}
nav svg{width:1.05rem;height:1.05rem;fill:currentColor;flex:0 0 auto}
.nav-badge{display:inline-grid;place-items:center;min-width:1.25rem;height:1.25rem;border-radius:999px;padding:0 .35rem;background:var(--brand);color:var(--brand-text);font-size:.78rem;font-weight:800;line-height:1}
main{padding:var(--shell-gap)}.app-shell{width:min(100%,var(--shell-max));margin:0 auto;display:grid;grid-template-columns:var(--shell-side) minmax(0,var(--shell-primary)) var(--shell-side);gap:var(--shell-gap);align-items:start;justify-content:center}.primary-column{min-width:0;width:100%}.left-rail,.right-rail{min-width:0;position:sticky;top:var(--rail-sticky-top);display:grid;gap:.75rem;align-items:start}.side-rail-card,.rail-nav{background:var(--surface);border:1px solid var(--border);border-radius:8px;color:var(--muted-strong);box-shadow:0 1px 2px var(--shadow)}.side-rail-card{padding:.85rem}.side-rail-card h2{margin:.1rem 0 .6rem;font-size:1rem;color:var(--text)}.rail-nav{display:grid;grid-template-columns:minmax(0,1fr);gap:.2rem;width:100%;padding:.35rem;justify-content:stretch}.rail-nav a,.rail-nav button{width:100%;min-height:2.35rem;justify-content:flex-start;padding:.5rem .65rem}.rail-nav form{display:block}.mobile-nav{display:none}.dashboard-list{display:grid;grid-template-columns:auto minmax(0,1fr);gap:.45rem .75rem;margin:.25rem 0 .85rem}.dashboard-list dt{font-weight:800;color:var(--text)}.dashboard-list dd{margin:0;overflow-wrap:anywhere}.dashboard-account{color:var(--text)}.dashboard-account:hover{text-decoration:none}.dashboard-actions{display:flex;flex-wrap:wrap;gap:.4rem}.site-footer{max-width:var(--shell-max);margin:0 auto;padding:1rem;color:var(--muted);font-size:.9rem}
.page-header,.post,.composer,.panel,.empty-state,.notice{background:var(--surface);border:1px solid var(--border);border-radius:8px;padding:.85rem;box-shadow:0 1px 2px var(--shadow)}.page-header,.composer,.panel,.empty-state,.notice{margin:0 0 var(--section-gap)}
.page-header h1,.section-heading h1,.panel h1{margin:0;font-size:1.45rem;line-height:1.2}.panel h1+table,.panel h1+form,.panel h1+p,.panel h1+dl{margin-top:.85rem}.page-header p,.muted,.empty-state p{color:var(--muted);margin:.35rem 0 0}.section-heading{display:flex;justify-content:space-between;gap:1rem;align-items:baseline;margin-bottom:.8rem}
.character-counter{display:inline-block;flex:0 0 auto;min-width:8.5rem;text-align:right;white-space:nowrap;font-variant-numeric:tabular-nums}.character-counter-normal{color:var(--muted)}.character-counter-warning{color:var(--warning)}.character-counter-danger{color:var(--danger)}
.notifications-hero{background:var(--surface);border:1px solid var(--border);border-radius:8px;margin:0 0 var(--section-gap);padding:1rem;box-shadow:0 1px 2px var(--shadow);display:flex;align-items:center;justify-content:space-between;gap:1rem}.notifications-hero h1{margin:0;font-size:1.55rem;line-height:1.15}.notifications-hero p:not(.eyebrow){margin:.35rem 0 0;color:var(--muted-strong)}.caught-up-pill{display:inline-flex;align-items:center;min-height:2rem;border:1px solid var(--success-border);border-radius:999px;background:var(--success-bg);color:var(--text-strong);padding:.32rem .75rem;font-weight:800}.caught-up{padding:.75rem .85rem}.caught-up p{margin:0}.notification-group{margin:var(--section-gap) .15rem 0;color:var(--muted);font-size:.82rem;text-transform:uppercase;letter-spacing:.08em}.notification-group:first-child{margin-top:0}.notification-row{position:relative;display:grid;grid-template-columns:auto minmax(0,1fr) auto auto;gap:.75rem;align-items:start;background:var(--surface);border:1px solid var(--border);border-radius:8px;padding:.8rem;box-shadow:0 1px 2px var(--shadow)}.notification-row.unread{border-color:var(--border-strong);background:var(--surface-subtle)}.js-enabled .notification-row[data-card-href],.js-enabled .notification-row[data-card-form]{cursor:pointer}.js-enabled .notification-row[data-card-href]:hover,.js-enabled .notification-row[data-card-form]:hover{border-color:var(--border-strong);background:var(--hover)}.notification-kind{display:grid;place-items:center;width:2rem;height:2rem;border-radius:7px;background:var(--surface-muted);color:var(--link-strong);font-weight:900;font-size:.8rem}.notification-row.unread .notification-kind{background:var(--brand);color:var(--brand-text)}.notification-body{min-width:0}.notification-line{margin:0;overflow-wrap:anywhere}.notification-meta{margin:.35rem 0 0;color:var(--muted);font-size:.88rem}.notification-counts{color:var(--muted-strong);font-weight:800}.notification-preview{display:block;margin:.5rem 0 0;border:1px solid var(--border);border-radius:7px;padding:.55rem .65rem;background:var(--surface-subtle);color:var(--muted-strong);overflow-wrap:anywhere}.notification-preview:hover{background:var(--surface);text-decoration:none}.notification-preview.unavailable{border-style:dashed}.notification-actors{margin:.45rem 0 0}.notification-actors summary{display:inline-flex;align-items:center;min-height:1.7rem;color:var(--link-strong);font-weight:800;cursor:pointer}.notification-actors ul{list-style:none;margin:.25rem 0 0;padding:0;display:flex;flex-wrap:wrap;gap:.35rem}.notification-actors li{border:1px solid var(--border);border-radius:999px;background:var(--surface-subtle);padding:.16rem .5rem;font-size:.88rem}.notification-open-form{display:flex;align-items:flex-start}.notification-open{min-height:2rem;padding:.3rem .55rem;background:var(--surface);border-color:var(--border)}.unread-dot{width:.65rem;height:.65rem;border-radius:999px;background:var(--brand);margin-top:.7rem}
label{display:block;font-weight:700;margin:.85rem 0 .35rem}input,textarea,button,select{font:inherit}input[type=text],input[type=search],input[type=password],input[type=url],input:not([type]),textarea,select{width:100%;padding:.72rem .8rem;border:1px solid var(--border-strong);border-radius:7px;background:var(--surface);color:var(--text)}textarea{resize:vertical;min-height:7rem}::placeholder{color:var(--muted)}
input[type=checkbox]{accent-color:var(--brand)}.check-row,.theme-toggle{display:flex;align-items:center;gap:.55rem;font-weight:700;color:var(--text)}.theme-toggle{padding:.65rem .75rem;border:1px solid var(--border);border-radius:8px;background:var(--surface-subtle)}.theme-toggle input{width:auto}
input[type=text].password-visible{padding-right:.8rem}.password-control{display:grid;grid-template-columns:minmax(0,1fr) auto;gap:.45rem;align-items:center}.password-control input{min-width:0}.password-toggle{display:none;background:var(--surface);color:var(--link-strong);border-color:var(--border);min-width:4.5rem}.js-enabled .password-toggle{display:inline-block}.auth-submit{margin-top:1.15rem}.auth-form{margin-top:.35rem}.auth-form .field-help{margin:.15rem 0 .4rem;color:var(--muted-strong)}
.search-panel h1{margin-bottom:.75rem}.search-form{display:grid;grid-template-columns:minmax(0,1fr) auto;gap:.55rem;align-items:center}.search-form input{min-width:0}.search-results{display:grid;gap:var(--section-gap)}.section-title{margin:.2rem 0 var(--section-gap);font-size:1.05rem;color:var(--text)}.search-results>.section-title{margin:0}.search-users{margin:0}.search-users .section-title{margin-top:0}.search-account{grid-template-columns:auto minmax(0,1fr)}
input:focus,textarea:focus,select:focus,button:focus-visible,a:focus-visible{outline:3px solid var(--focus);outline-offset:2px}button,.primary{border:1px solid var(--brand);background:var(--brand);color:var(--brand-text);border-radius:7px;padding:.5rem .8rem;cursor:pointer;font-weight:700}button:hover,.primary:hover{background:var(--brand-hover);text-decoration:none}button:disabled,.primary:disabled,button:disabled:hover,.primary:disabled:hover{border-color:var(--border);background:var(--surface-muted);color:var(--muted);cursor:not-allowed;opacity:1}
nav button{border-color:transparent;background:transparent;color:var(--link-strong);padding:.42rem .65rem}.rail-nav button{border-color:transparent;background:transparent;color:var(--link-strong);padding:.5rem .65rem}.rail-nav button:hover,.mobile-nav button:hover{background:var(--hover);color:var(--link-strong)}
.composer-surface{position:relative;border:1px solid var(--border-strong);border-radius:7px;background:var(--surface);overflow:visible}.composer-surface textarea{border:0;border-radius:0;background:transparent;min-height:7rem;resize:vertical}.composer-surface textarea:focus{outline:0;box-shadow:inset 0 0 0 3px var(--focus)}.mention-menu[hidden]{display:none}.mention-menu{position:absolute;z-index:9;left:.55rem;right:.55rem;top:3.1rem;max-height:12rem;overflow:auto;border:1px solid var(--border-strong);border-radius:7px;background:var(--surface);box-shadow:0 8px 24px var(--shadow);padding:.25rem}.mention-option{display:grid;width:100%;grid-template-columns:minmax(0,1fr) auto;gap:.65rem;align-items:center;border:0;border-radius:6px;background:transparent;color:var(--text);padding:.42rem .5rem;text-align:left}.mention-option:hover,.mention-option[aria-selected="true"]{background:var(--hover);color:var(--text-strong)}.mention-name{font-weight:800;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}.mention-handle{color:var(--muted);font-size:.9rem}.composer-footer{display:flex;align-items:center;justify-content:space-between;gap:.55rem;border-top:1px solid var(--border);padding:.42rem .5rem;background:var(--surface-subtle)}.composer-file-control{position:relative;display:inline-flex;align-items:center;gap:.45rem;max-width:100%;margin:0;color:var(--link-strong);font-weight:800}.composer-file-input{max-width:100%;color:var(--muted-strong)}.composer-file-input::file-selector-button{border:1px solid var(--border);border-radius:7px;background:var(--surface);color:var(--link-strong);padding:.34rem .58rem;font-weight:800;cursor:pointer}.composer-file-input::file-selector-button:hover{background:var(--hover);color:var(--text-strong)}.composer-file-button{display:none}.js-enabled .composer-file-input{position:absolute;width:1px;height:1px;padding:0;margin:-1px;overflow:hidden;clip:rect(0,0,0,0);white-space:nowrap;border:0}.js-enabled .composer-file-button{display:inline-flex;align-items:center;gap:.38rem;min-height:2rem;border:1px solid var(--border);border-radius:7px;background:var(--surface);color:var(--link-strong);padding:.3rem .55rem;cursor:pointer}.composer-file-button svg{width:1rem;height:1rem;fill:currentColor;flex:0 0 auto}.js-enabled .composer-file-control:hover .composer-file-button{background:var(--hover);color:var(--text-strong)}.js-enabled .composer-file-input:focus-visible+.composer-file-button{outline:3px solid var(--focus);outline-offset:2px}.composer-media-selection[hidden]{display:none}.composer-media-selection{display:flex;align-items:center;gap:.7rem;flex-wrap:wrap;border-top:1px solid var(--border);padding:.5rem;background:var(--surface)}.composer-media-summary{font-weight:800;color:var(--muted-strong);overflow-wrap:anywhere}.composer-nsfw{margin:0}.composer-clear-media{background:var(--surface);color:var(--link-strong);border-color:var(--border);padding:.32rem .55rem}.composer-clear-media:hover{background:var(--hover);color:var(--text-strong)}.composer-tools{display:flex;align-items:center;justify-content:space-between;gap:.75rem;margin-top:.85rem}
.thread-nav{display:flex;margin:0 0 var(--section-gap) .85rem}.thread-back{width:2rem;height:2rem;display:inline-flex;align-items:center;justify-content:center;border-radius:999px;color:var(--link-strong)}.thread-back svg{width:1.2rem;height:1.2rem;fill:currentColor}.thread-back:hover{background:var(--hover);text-decoration:none}
.timeline,.notifications-list,.account-list{display:grid;gap:var(--card-gap)}.timeline+.composer{margin-top:var(--section-gap)}.pinned-timeline{gap:var(--section-gap);margin-bottom:var(--section-gap)}.pinned-timeline>.section-title{margin:0}.post{margin:0;overflow:hidden;position:relative}.js-enabled .post[data-card-href]{cursor:pointer}.js-enabled .post[data-card-href]:hover{border-color:var(--border-strong)}.reply-post{margin-left:1.1rem;border-left:4px solid var(--reply-border);background:var(--surface-subtle)}.reply-post::before{content:"";position:absolute;left:-1.1rem;top:1.25rem;width:1.1rem;border-top:2px solid var(--reply-border)}.anchor-target{position:absolute;top:-5rem}.post-header{display:flex;justify-content:space-between;gap:.65rem;align-items:flex-start}.author-block{display:flex;gap:.55rem;align-items:center;min-width:0}.post-avatar{width:2rem;height:2rem;object-fit:cover;border-radius:999px;border:1px solid var(--border);background:var(--avatar-bg);flex:0 0 auto;margin:0}.post-avatar.placeholder{display:inline-grid;place-items:center;color:var(--muted-strong);font-weight:800}.author-name{font-weight:800;color:var(--text-strong)}.username,.post-time,.counts{color:var(--muted);font-size:.92rem}.text{white-space:pre-wrap;margin:.55rem 0;line-height:1.5;overflow-wrap:anywhere}.post img,.post video{display:block;max-width:100%;border-radius:8px;border:1px solid var(--border);margin-top:.5rem;background:var(--media-bg)}.post img.post-avatar{display:block;margin:0;border-radius:999px}.youtube-previews{display:grid;gap:.5rem;margin:.35rem 0 .55rem}.youtube-preview-card{display:grid;border:1px solid var(--border);border-radius:7px;background:var(--surface-subtle);color:var(--text);overflow:hidden}.youtube-preview-card:hover{border-color:var(--border-strong);background:var(--hover)}.youtube-preview-main{display:grid;grid-template-columns:minmax(5.5rem,7.5rem) minmax(0,1fr);gap:.65rem;align-items:center;min-height:4.5rem;color:inherit}.youtube-preview-main:hover{text-decoration:none}.youtube-thumbnail-frame{position:relative;display:block;width:100%;aspect-ratio:16/9;overflow:hidden;background:var(--media-bg)}.post img.youtube-thumbnail{width:100%;height:100%;object-fit:cover;margin:0;border:0;border-radius:0}.youtube-play{position:absolute;left:50%;top:50%;width:2rem;height:2rem;border-radius:999px;background:rgba(0,0,0,.68);box-shadow:0 1px 4px rgba(0,0,0,.35);transform:translate(-50%,-50%)}.youtube-play::before{content:"";position:absolute;left:.78rem;top:.55rem;border-top:.45rem solid transparent;border-bottom:.45rem solid transparent;border-left:.65rem solid #fff}.youtube-preview-body{display:grid;gap:.08rem;min-width:0;padding:.45rem .55rem .45rem 0}.youtube-preview-source{color:var(--muted);font-size:.78rem;font-weight:900;text-transform:uppercase}.youtube-preview-title{color:var(--text-strong);font-weight:850;overflow-wrap:anywhere}.youtube-preview-url{color:var(--muted-strong);font-size:.86rem;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}.youtube-preview-actions{display:flex;justify-content:flex-end;border-top:1px solid var(--border);padding:.32rem .55rem}.youtube-open-link{color:var(--muted-strong);font-size:.84rem;font-weight:800}.youtube-player-frame{width:100%;aspect-ratio:16/9;background:#000}.youtube-preview-playing .youtube-preview-main{display:none}.youtube-iframe{display:block;width:100%;height:100%;border:0;background:#000}.nsfw-media{position:relative;margin-top:.5rem}.nsfw-media .post img,.nsfw-media .post video{margin-top:0}.nsfw-media-frame{position:relative;display:block;overflow:hidden;border-radius:8px}.nsfw-media-frame img,.nsfw-media-frame video{margin-top:0;filter:blur(24px);transform:scale(1.02)}.nsfw-toggle:checked+.nsfw-media-frame img,.nsfw-toggle:checked+.nsfw-media-frame video{filter:none;transform:none}.nsfw-badge{position:absolute;left:.55rem;bottom:.55rem;border-radius:999px;padding:.18rem .5rem;background:rgba(0,0,0,.72);color:#fff;font-size:.78rem;font-weight:900;letter-spacing:.03em}.nsfw-show{position:absolute;right:.55rem;bottom:.55rem;margin:0;border:1px solid var(--border-strong);border-radius:7px;background:var(--surface);color:var(--text-strong);padding:.32rem .65rem;font-weight:900;box-shadow:0 1px 2px var(--shadow);cursor:pointer}.nsfw-show:hover{background:var(--hover)}.nsfw-toggle:focus-visible~.nsfw-show{outline:3px solid var(--focus);outline-offset:2px}.nsfw-toggle:checked~.nsfw-show,.nsfw-toggle:checked+.nsfw-media-frame .nsfw-badge{display:none}
.counts{display:flex;gap:.5rem;flex-wrap:wrap;margin-top:.3rem;min-height:1.4rem}.edited-marker{font-weight:800;color:var(--muted-strong)}.post-permalink{font-weight:700;color:var(--link-strong)}.js-enabled .post-permalink{display:none}.actions{display:inline-flex;gap:.25rem;flex-wrap:wrap;align-items:center;margin-top:.5rem;max-width:100%}.icon-button{width:2.2rem;height:2.2rem;display:inline-flex;align-items:center;justify-content:center;border:1px solid var(--border);border-radius:7px;background:var(--surface);color:var(--link-strong);padding:0}.icon-button svg{width:1.05rem;height:1.05rem;fill:currentColor}.icon-button:hover,.icon-button.active{background:var(--hover);color:var(--text-strong);text-decoration:none}.icon-button.disabled,.icon-button:disabled{color:var(--muted);background:var(--surface-muted);border-color:var(--border);cursor:not-allowed;opacity:.75}.icon-button.disabled:hover,.icon-button:disabled:hover{background:var(--surface-muted);color:var(--muted)}.admin-nsfw-button{min-height:2.2rem;padding:.3rem .55rem;background:var(--surface);color:var(--link-strong);border-color:var(--border);font-size:.86rem}.admin-nsfw-button:hover{background:var(--hover);color:var(--text-strong)}.repost-control{position:relative;display:inline-flex;align-items:center;gap:.25rem}.repost-menu{position:absolute;z-index:8;left:0;top:calc(100% + .25rem);min-width:8.5rem;padding:.3rem;border:1px solid var(--border-strong);border-radius:7px;background:var(--surface);box-shadow:0 6px 18px var(--shadow)}.repost-menu a{display:inline-flex;align-items:center;gap:.35rem;width:100%;min-height:2rem;border-radius:6px;padding:.32rem .55rem;color:var(--link-strong);font-weight:700}.repost-menu a svg{width:1rem;height:1rem;fill:currentColor;flex:0 0 auto}.repost-menu a:hover,.quote-fallback:hover{background:var(--hover);text-decoration:none}.quote-preview{display:block;margin:.6rem 0 .25rem;border:1px solid var(--border);border-radius:7px;background:var(--surface-subtle);overflow:hidden}.quote-preview p{margin:.65rem;color:var(--muted-strong)}.quote-link{display:grid;gap:.2rem;padding:.6rem;color:var(--text)}.quote-link:hover{background:var(--hover);text-decoration:none}.quote-author{font-weight:800}.quote-text{white-space:pre-wrap;overflow-wrap:anywhere}.quote-time{color:var(--muted);font-size:.86rem}.follow-button{min-width:6.6rem}.follow-button.active{background:var(--hover);color:var(--text-strong);border-color:var(--border-strong)}.profile-actions{margin-top:0}.profile-secondary button{background:var(--surface);color:var(--danger);border-color:var(--danger-border);padding:.32rem .5rem;min-height:1.85rem;font-size:.86rem}.profile-secondary button:hover{background:var(--danger-bg);color:var(--danger-strong)}.profile-title-row{display:flex;align-items:flex-start;justify-content:space-between;gap:.75rem}.profile-tabs{display:grid;grid-template-columns:repeat(4,minmax(0,1fr));gap:.35rem;margin:0 0 var(--section-gap);border-bottom:1px solid var(--border)}.profile-tabs a{display:flex;align-items:center;justify-content:center;min-height:2.6rem;border-radius:7px 7px 0 0;color:var(--muted-strong);font-weight:850}.profile-tabs a:hover{background:var(--hover);color:var(--text-strong);text-decoration:none}.profile-tabs a.active{color:var(--text-strong);background:var(--surface);box-shadow:inset 0 -3px 0 var(--brand)}.sr-only{position:absolute;width:1px;height:1px;padding:0;margin:-1px;overflow:hidden;clip:rect(0,0,0,0);white-space:nowrap;border:0}.repost-banner{color:var(--muted-strong);font-size:.9rem;font-weight:800;margin-bottom:.35rem}.unavailable{color:var(--muted)}.empty-state{text-align:center;padding:2rem 1rem}.empty-state h2{margin:0;font-size:1.2rem}.notice.error,.error-panel{border-color:var(--danger-border);background:var(--danger-bg)}.notice.success{border-color:var(--success-border);background:var(--success-bg)}.eyebrow{text-transform:uppercase;letter-spacing:.08em;font-weight:800;color:var(--muted);font-size:.78rem}.noscript-banner{max-width:1100px;margin:.7rem auto 0;padding:.65rem .85rem;border:1px solid var(--border);border-radius:8px;background:var(--surface-subtle);color:var(--muted-strong)}
.profile-banner{width:100%;max-height:220px;object-fit:cover;border-radius:8px;border:1px solid var(--border);background:var(--surface-muted)}.profile-heading{display:flex;gap:1rem;align-items:flex-start;margin-top:.85rem}.profile-main{min-width:0;flex:1}.profile-picture{width:88px;height:88px;object-fit:cover;border-radius:999px;border:3px solid var(--surface);background:var(--avatar-bg);flex:0 0 auto}.profile-meta{color:var(--muted-strong);margin:.45rem 0 0}.settings-profile-editor{padding:0;overflow:hidden}.settings-editor-bar{display:flex;justify-content:space-between;align-items:center;gap:1rem;padding:.85rem;border-bottom:1px solid var(--border)}.settings-editor-bar h1{margin:0}.settings-editor-bar .primary{flex:0 0 auto}.settings-profile-form{padding:.85rem;display:grid;gap:1rem}.settings-section{display:grid;gap:.75rem;min-width:0}.settings-section+.settings-section{border-top:1px solid var(--border);padding-top:1rem}.settings-section-heading{display:grid;gap:.18rem}.settings-section-heading h2,.settings-list-panel h2,.settings-security-panel h2,.danger-panel h2{margin:0;font-size:1.05rem;line-height:1.25;color:var(--text-strong)}.settings-section-help,.settings-switch-help{margin:0;color:var(--muted-strong);font-size:.9rem;line-height:1.35;overflow-wrap:anywhere}.settings-profile-fields{gap:.28rem}.settings-profile-fields label,.settings-password-form label{margin:.55rem 0 .28rem}.settings-profile-media{display:grid;gap:.65rem;min-width:0}.settings-banner-wrap{background:var(--media-bg);border-radius:8px;overflow:hidden}.settings-banner-preview{display:block;width:100%;height:170px;object-fit:cover;object-position:left center;background:linear-gradient(135deg,var(--surface-muted),var(--hover));border:1px solid var(--border);border-radius:8px}.settings-banner-preview.placeholder::before{content:"";display:block;width:100%;height:100%}.settings-picture-row{display:grid;grid-template-columns:auto minmax(0,1fr);gap:1rem;align-items:end;margin-top:-42px;padding:0 .75rem .2rem}.settings-picture-preview{width:104px;height:104px;object-fit:cover;border-radius:999px;border:5px solid var(--surface);background:var(--avatar-bg);box-shadow:0 1px 4px var(--shadow)}.settings-picture-preview.placeholder{display:block}.settings-media-controls{display:grid;gap:.45rem;align-content:end;padding-top:2.85rem;min-width:0}.media-control-row{display:flex;align-items:center;gap:.65rem;flex-wrap:wrap;min-width:0}.media-control-row .file-control{display:flex;align-items:center;gap:.35rem;flex-wrap:wrap;margin:0;max-width:100%;overflow-wrap:anywhere}.media-control-row .file-control input[type=file]{max-width:100%}.media-control-row .check-row{margin:0}.settings-switch-list{display:grid;gap:0;border:1px solid var(--border);border-radius:8px;background:var(--surface-subtle);overflow:hidden}.settings-switch-row{display:grid;grid-template-columns:minmax(0,1fr) auto;gap:.85rem;align-items:center;margin:0;padding:.75rem .8rem;color:var(--text);cursor:pointer}.settings-switch-row+.settings-switch-row{border-top:1px solid var(--border)}.settings-switch-copy{display:grid;gap:.12rem;min-width:0}.settings-switch-label{font-weight:800;color:var(--text-strong);overflow-wrap:anywhere}.settings-switch-toggle{position:relative;display:inline-grid;width:2.75rem;height:1.55rem;justify-self:end;flex:0 0 auto}.settings-switch-input{position:absolute;inset:0;z-index:1;width:100%;height:100%;padding:0;margin:0;opacity:0;cursor:pointer}.settings-switch-control{position:relative;display:block;width:2.75rem;height:1.55rem;border:1px solid var(--border-strong);border-radius:999px;background:var(--surface-muted);box-shadow:inset 0 0 0 1px var(--shadow);transition:background-color .15s ease,border-color .15s ease}.settings-switch-control::before{content:"";position:absolute;left:.16rem;top:50%;width:1.15rem;height:1.15rem;border-radius:999px;background:var(--surface);box-shadow:0 1px 3px var(--shadow);transform:translateY(-50%);transition:transform .15s ease}.settings-switch-input:checked+.settings-switch-control{border-color:var(--brand);background:var(--brand)}.settings-switch-input:checked+.settings-switch-control::before{transform:translate(1.2rem,-50%)}.settings-switch-input:focus-visible+.settings-switch-control{outline:3px solid var(--focus);outline-offset:2px}.settings-form-actions{display:flex;justify-content:flex-end;gap:.5rem;border-top:1px solid var(--border);padding-top:.85rem}.onboarding-panel{overflow:hidden}.onboarding-form{display:grid;gap:.9rem}.onboarding-media-row{display:grid;grid-template-columns:auto minmax(0,1fr);gap:1rem;align-items:center}.onboarding-suggestions{border:1px solid var(--border);border-radius:8px;padding:.75rem;display:grid;gap:.55rem}.onboarding-suggestions legend{font-weight:800;padding:0 .35rem}.onboarding-suggestion{display:grid;grid-template-columns:auto auto minmax(0,1fr);gap:.6rem;align-items:center;margin:0;border:1px solid var(--border);border-radius:7px;padding:.55rem;background:var(--surface-subtle);cursor:pointer}.onboarding-suggestion input{margin:0}.settings-grid{display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:.7rem}.deep-settings-panel{padding:0;overflow:hidden}.deep-settings-form{padding:.85rem;display:grid;gap:.85rem}.deep-settings-group{border:1px solid var(--border);border-radius:8px;padding:.8rem;display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:.7rem}.deep-settings-group legend{font-weight:800;padding:0 .35rem}.deep-settings-field{display:grid;gap:.25rem;align-content:start}.deep-settings-field label{font-weight:800}.deep-settings-field input,.deep-settings-field select{min-width:0}.field-help{font-size:.88rem;margin:.05rem 0 .35rem;color:var(--muted-strong)}.deep-settings-confirm .settings-item-list li{display:block}.compact-panel h2,.danger-panel h2{margin:0;font-size:1.05rem}.inline-settings-form{display:grid;grid-template-columns:minmax(0,1fr) auto;gap:.55rem;align-items:center;margin:.65rem 0 .75rem}.inline-settings-form input{min-width:0}.settings-password-form{display:grid;gap:0;margin-top:.65rem}.settings-password-form button[type=submit]{margin-top:0}.settings-list-panel,.settings-security-panel{display:grid;gap:.55rem}.settings-item-list{list-style:none;margin:.25rem 0 0;padding:0;display:grid;gap:.45rem}.settings-item-list li{display:flex;justify-content:space-between;align-items:center;gap:.75rem;border:1px solid var(--border);border-radius:7px;padding:.55rem .65rem;background:var(--surface-subtle)}.settings-item-list li>span{min-width:0;overflow-wrap:anywhere}.settings-item-list form{flex:0 0 auto}.settings-item-list button{padding:.32rem .55rem;background:var(--surface);color:var(--link-strong);border-color:var(--border)}.compact-empty{border:1px dashed var(--border);border-radius:7px;padding:.75rem;background:var(--surface-subtle);color:var(--muted-strong)}.compact-empty p{margin:.25rem 0 0}.danger-panel{border-color:var(--danger-border);background:var(--danger-bg)}.danger,.danger-link{border-color:var(--danger-border);background:var(--danger);color:var(--brand-text)}.danger:hover,.danger-link:hover{background:var(--danger-strong);color:var(--brand-text);text-decoration:none}.delete-account-panel p,.danger-panel p{max-width:62ch}.settings-danger-action{margin:.7rem 0 0}.favicon-preview{width:32px;height:32px;object-fit:contain;border:1px solid var(--border);border-radius:6px;background:var(--surface)}.admin-user-search{display:grid;grid-template-columns:minmax(0,1fr) minmax(0,1fr) auto;gap:.65rem;align-items:end;margin-top:.75rem}.admin-user-search label{margin-top:0}.admin-user-search-actions{display:flex;gap:.4rem;align-items:center;margin-bottom:.05rem}.admin-user-row{display:grid;grid-template-columns:minmax(0,1fr) auto;gap:.75rem;border:1px solid var(--border);border-radius:8px;padding:.75rem;margin-top:.65rem;background:var(--surface-subtle)}.admin-user-heading{overflow-wrap:anywhere}.admin-user-statuses,.admin-user-matches{display:flex;flex-wrap:wrap;gap:.35rem;margin-top:.45rem}.admin-user-pill,.admin-user-match{display:inline-flex;align-items:center;min-height:1.55rem;border:1px solid var(--border);border-radius:999px;padding:.15rem .5rem;background:var(--surface);font-size:.82rem;font-weight:800;color:var(--muted-strong)}.admin-user-match{border-color:var(--success-border);background:var(--success-bg);color:var(--text)}.admin-user-meta{margin:.6rem 0 0}.admin-post-preview{margin:.55rem 0 0;color:var(--muted-strong);overflow-wrap:anywhere}.admin-user-actions{display:flex;align-items:flex-start}.admin-users-empty{margin-top:.75rem}.account-row{display:grid;grid-template-columns:auto minmax(0,1fr) auto;gap:.75rem;align-items:center;background:var(--surface);border:1px solid var(--border);border-radius:8px;padding:.85rem}.account-row p{margin:.3rem 0 0;color:var(--muted-strong);overflow-wrap:anywhere}.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(180px,1fr));gap:.85rem}.item-list{margin:.75rem 0 0;padding-left:1.2rem}.item-list li{margin:.45rem 0}.panel dl:not(.dashboard-list){display:grid;grid-template-columns:max-content minmax(0,1fr);gap:.45rem .85rem}.panel dl:not(.dashboard-list) dt{font-weight:800}.panel dl:not(.dashboard-list) dd{margin:0;overflow-wrap:anywhere}table{width:100%;border-collapse:collapse}td,th{border-bottom:1px solid var(--border);text-align:left;padding:.55rem;vertical-align:top}pre{white-space:pre-wrap;overflow:auto;max-width:100%}
.settings-media-frame{position:relative;min-width:0}.settings-picture-row{display:flex;align-items:flex-end;gap:0}.settings-picture-wrap{display:inline-block;max-width:100%;line-height:0}.settings-picture-preview{display:block}.settings-media-actions{position:absolute;z-index:2;display:flex;gap:.35rem;align-items:center}.settings-banner-actions{top:.55rem;right:.55rem}.settings-picture-actions{left:50%;bottom:.45rem;transform:translateX(-50%)}.settings-media-control{position:relative;display:inline-flex}.settings-media-input,.settings-media-delete-input{position:absolute;width:1px;height:1px;padding:0;margin:-1px;overflow:hidden;clip:rect(0,0,0,0);white-space:nowrap;border:0}.settings-media-icon-button{display:inline-flex;align-items:center;justify-content:center;width:2rem;height:2rem;margin:0;border:1px solid rgba(255,255,255,.62);border-radius:999px;background:rgba(23,32,23,.56);color:#fff;padding:0;box-shadow:0 1px 4px rgba(0,0,0,.22);cursor:pointer;opacity:.72;transition:opacity .15s ease,background-color .15s ease,border-color .15s ease,transform .15s ease}.settings-media-icon-button svg{width:1rem;height:1rem;fill:currentColor}.settings-media-frame:hover .settings-media-icon-button,.settings-media-frame:focus-within .settings-media-icon-button,.settings-media-icon-button:hover{opacity:1}.settings-media-icon-button:hover{background:rgba(23,32,23,.82);text-decoration:none}.settings-media-input:focus-visible+.settings-media-icon-button,.settings-media-delete-input:focus-visible+.settings-media-icon-button{outline:3px solid var(--focus);outline-offset:2px;opacity:1}.settings-media-delete-input:checked+.settings-media-icon-button,.settings-media-removing .settings-media-remove{background:var(--danger);border-color:var(--danger-border);color:var(--brand-text);opacity:1}.settings-media-has-file .settings-media-change{background:var(--brand);border-color:rgba(255,255,255,.72);color:var(--brand-text);opacity:1}.settings-media-disabled{position:absolute;right:.55rem;bottom:.55rem;max-width:calc(100% - 1.1rem);margin:0;border:1px solid rgba(255,255,255,.5);border-radius:999px;background:rgba(23,32,23,.62);color:#fff;padding:.22rem .55rem;font-size:.82rem;font-weight:800;line-height:1.2;overflow-wrap:anywhere}
@media (max-width:1100px){.app-shell{--shell-side:220px;--shell-max:880px;grid-template-columns:var(--shell-side) minmax(0,var(--shell-primary))}.right-rail{display:none}}
@media (max-width:820px){.app-shell{grid-template-columns:minmax(0,680px)}.left-rail,.right-rail{display:none}.mobile-nav{display:flex}}
@media (max-width:600px){main{padding:.75rem}.header-inner{align-items:flex-start;flex-direction:column}.header-brand-row{align-items:center;width:100%;gap:.55rem}.tor-indicator{max-width:calc(100% - 7rem);margin-left:auto}.tor-details{left:auto;right:0;max-width:calc(100vw - 1.5rem)}.site-header{position:static}nav{justify-content:flex-start}.mobile-nav{width:100%}.search-form,.inline-settings-form,.settings-grid,.deep-settings-group,.admin-user-search,.admin-user-row,.onboarding-media-row{grid-template-columns:1fr}.search-form button,.inline-settings-form button{width:100%}.composer-tools,.post-header,.profile-heading,.profile-title-row,.account-row,.settings-editor-bar,.notifications-hero{align-items:stretch;grid-template-columns:1fr;flex-direction:column}.composer-footer,.composer-media-selection{align-items:flex-start;flex-direction:column}.composer-file-input{max-width:100%}.settings-banner-preview{height:150px}.settings-picture-row{grid-template-columns:1fr;margin-top:-38px;gap:.5rem}.settings-picture-preview{width:92px;height:92px}.settings-media-controls{padding-top:0}.media-control-row{align-items:flex-start}.settings-switch-row{grid-template-columns:1fr;gap:.55rem}.settings-switch-toggle,.settings-switch-control{justify-self:start}.settings-form-actions{justify-content:stretch}.settings-form-actions button,.settings-danger-action .button-link{width:100%;justify-content:center}.settings-item-list li{align-items:stretch;flex-direction:column}.admin-user-search-actions,.admin-user-actions{align-items:stretch;flex-direction:column}.admin-user-search-actions button,.admin-user-search-actions .button-link,.admin-user-actions button{width:100%;justify-content:center}.panel dl:not(.dashboard-list){grid-template-columns:1fr}table{display:block;max-width:100%;overflow-x:auto}.author-block{align-items:flex-start}.reply-post{margin-left:.65rem;padding-left:.8rem}.reply-post::before{left:-.65rem;width:.65rem}.button-link{padding:.42rem .55rem}.counts{gap:.45rem}.page-header h1,.section-heading h1,.panel h1,.notifications-hero h1{font-size:1.25rem}.notification-row{grid-template-columns:auto minmax(0,1fr);gap:.6rem}.unread-dot{position:absolute;right:.75rem;top:.75rem;margin:0}.notification-preview{padding:.5rem}}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_uses_configured_site_name() {
        let body = layout(None, "Home Feed", "<p>body</p>", "My Microblog");
        assert!(body.contains("<title>Home Feed - My Microblog</title>"));
        assert!(body.contains("<span>My Microblog</span>"));
        assert!(body.contains(r#"<footer class="site-footer">My Microblog</footer>"#));
        assert!(!body.contains("<span>RustPost</span>"));
    }

    #[test]
    fn layout_uses_dashboard_side_panel() {
        let body = layout(None, "Home Feed", "<p>body</p>", "My Microblog");
        assert!(body.contains("<h2>Dashboard</h2>"));
        assert!(body.contains("Login required"));
        assert!(!body.contains("Release status"));
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
            nsfw_blur_enabled: true,
        };
        let body = layout(Some(&user), "Home Feed", "<p>body</p>", "My Microblog");

        assert!(body.contains(r#"<html lang="en" data-theme="dark">"#));
    }

    #[test]
    fn stacked_cards_use_shared_spacing_without_individual_post_margins() {
        assert!(CSS.contains("--card-gap:.5rem;--section-gap:.75rem"));
        assert!(CSS.contains(
            ".timeline,.notifications-list,.account-list{display:grid;gap:var(--card-gap)}"
        ));
        assert!(CSS.contains(".post{margin:0;overflow:hidden;position:relative}"));
        assert!(CSS.contains(
            ".page-header,.composer,.panel,.empty-state,.notice{margin:0 0 var(--section-gap)}"
        ));
    }

    #[test]
    fn layout_includes_no_javascript_status_as_noscript_only() {
        let body = layout(None, "Home Feed", "<p>body</p>", "My Microblog");

        assert!(body.contains(r#"<noscript><section class="noscript-banner" role="status">"#));
        assert!(!body.contains(r#"<body><section class="noscript-banner""#));
        assert!(!body.contains(".js-enabled .noscript-banner"));
        assert!(body.contains(".js-enabled .post-permalink"));
        assert!(body.contains("display:none"));
        assert!(
            client_script().contains(r#"document.documentElement.classList.add("js-enabled")"#)
        );
        assert!(client_script().contains("data-mention-autocomplete"));
        assert!(client_script().contains("textContent = suggestion.display_name"));
        assert!(!client_script().contains("innerHTML = suggestion"));
        assert!(
            client_boot_script()
                .contains(r#"document.documentElement.classList.add("js-enabled")"#)
        );
        assert!(
            body.find(r#"<script src="/assets/rustpost-boot.js"></script>"#) < body.find("<style>")
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
            nsfw_blur_enabled: true,
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
            nsfw_blur_enabled: true,
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
        assert!(!without_tor.contains("tor-header-indicator"));

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
        assert!(with_tor.contains("tor-header-indicator"));
        assert!(with_tor.contains(r#"<details class="tor-disclosure">"#));
        assert!(with_tor.contains(r#"<summary class="tor-pill""#));
        assert!(with_tor.contains(r#"data-testid="tor-pill""#));
        assert!(with_tor.contains(r#"data-testid="tor-popover""#));
        assert!(with_tor.contains(r#"data-testid="tor-full-address""#));
        assert!(with_tor.contains("tor-summary-text"));
        assert!(with_tor.contains(r#"<span class="tor-pill-label">Tor</span>"#));
        assert!(with_tor.contains(r#"href="http://examplehiddenservice.onion""#));
        assert!(with_tor.contains(r#"title="Open Tor mirror: examplehiddenservice.onion""#));
        assert!(with_tor.contains(r#"aria-label="Open Tor mirror at examplehiddenservice.onion""#));
        assert!(with_tor.contains("exampl...ice.onion"));
        assert!(with_tor.contains(r#"data-copy-text="examplehiddenservice.onion""#));
        assert!(with_tor.contains(r#"aria-label="Copy Tor onion address""#));
        assert!(with_tor.contains(r#"</details><button class="tor-copy-button""#));
        assert!(with_tor.contains(r#"<span data-copy-feedback aria-live="polite">Copy</span>"#));
        assert!(!with_tor.contains("footer-onion"));
        assert!(!with_tor.contains("tor-address-link"));
        assert!(!with_tor.contains("Tor mirror: <code>"));
        assert!(!with_tor.contains("Onion: <code>"));
    }

    #[test]
    fn short_onion_address_preserves_short_values() {
        assert_eq!(
            short_onion_address("abc.onion"),
            "abc.onion",
            "short or unusual values should remain readable"
        );
        assert_eq!(
            short_onion_address("abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx.onion"),
            "abcdef...vwx.onion"
        );
    }

    #[test]
    fn composer_has_live_remaining_counter_and_contextual_placeholder() {
        let post = composer(Some("csrf"), None, 512);
        assert!(post.contains("512 remaining"));
        assert!(post.contains(r#"class="muted character-counter character-counter-normal""#));
        assert!(post.contains(r#"aria-live="polite""#));
        assert!(post.contains("data-character-limit=\"512\""));
        assert!(post.contains("data-mention-autocomplete"));
        assert!(post.contains(r#"aria-autocomplete="list""#));
        assert!(post.contains(r#"role="listbox" data-mention-menu hidden"#));
        assert!(post.contains(r#"aria-controls="post-text-mention-menu""#));
        assert!(!post.contains("maxlength="));
        assert!(post.contains("What is happening?"));
        assert!(post.contains(r#"placeholder="What's happening?""#));
        assert!(post.contains(r#"<div class="composer-surface">"#));
        assert!(post.contains(r#"id="post-media" name="media" type="file""#));
        assert!(post.contains(r#"aria-label="Attach media" data-composer-media"#));
        assert!(
            post.contains(
                r#"class="composer-media-selection" data-composer-media-selection hidden"#
            )
        );
        assert!(post.contains(r#"id="post-nsfw" name="nsfw" type="checkbox""#));
        assert!(post.contains(r#"data-composer-clear-media"#));
        assert!(!post.contains(r#"class="file-control""#));

        let reply = composer(Some("csrf"), Some(10), 512);
        assert!(reply.contains(r#"id="reply-text-10""#));
        assert!(reply.contains(r#"data-character-counter="reply-text-10""#));
        assert!(reply.contains(r#"aria-controls="reply-text-10-mention-menu""#));
        assert!(reply.contains(r#"id="reply-media-10" name="media" type="file""#));
        assert!(reply.contains(r#"id="reply-nsfw-10" name="nsfw" type="checkbox""#));
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
        assert!(body.contains(r#"class="muted character-counter character-counter-normal""#));
        assert!(body.contains(r#"aria-live="polite""#));
        assert!(body.contains("data-character-limit=\"512\""));
        assert!(!body.contains("maxlength="));
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
    fn empty_state_escapes_text_and_omits_empty_description() {
        let body = empty_state(r#"<missing>"#, "");

        assert!(body.contains("&lt;missing&gt;"));
        assert!(!body.contains("<missing>"));
        assert!(!body.contains("<p></p>"));
    }

    #[test]
    fn search_page_preserves_and_escapes_query() {
        let body = search_page(
            "RustPost",
            r#"<rust> "query""#,
            &[],
            &[],
            None,
            None,
            SearchRenderOptions::default(),
        );

        assert!(body.contains(r#"value="&lt;rust&gt; &quot;query&quot;""#));
        assert!(body.contains("No matching posts or users found."));
        assert!(body.contains("Try another search."));
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

        let body = search_page(
            "RustPost",
            "ada",
            &[account],
            &[post],
            None,
            None,
            SearchRenderOptions::default(),
        );

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
            nsfw_blur_enabled: true,
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
            nsfw_blur_enabled: true,
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

    #[test]
    fn nsfw_media_renders_blurred_with_accessible_reveal_control() {
        let mut post = test_post();
        post.media = vec![MediaView {
            public_path: "/uploads/images/flagged.webp".to_owned(),
            mime_type: "image/webp".to_owned(),
            media_kind: "image".to_owned(),
            alt_text: "Flagged image".to_owned(),
            is_nsfw: true,
        }];

        let body = post_card(&post, None, None);

        assert!(body.contains(r#"data-testid="nsfw-media""#));
        assert!(body.contains(r#"aria-label="Show NSFW media""#));
        assert!(body.contains(">Show<span"));
        assert!(!body.contains(r#"class="nsfw-open""#));
    }

    #[test]
    fn nsfw_media_blur_can_be_disabled_for_rendering() {
        let mut post = test_post();
        post.media = vec![MediaView {
            public_path: "/uploads/images/flagged.webp".to_owned(),
            mime_type: "image/webp".to_owned(),
            media_kind: "image".to_owned(),
            alt_text: "Flagged image".to_owned(),
            is_nsfw: true,
        }];

        let body = post_card_with_nsfw_blur(&post, None, None, false);

        assert!(!body.contains(r#"data-testid="nsfw-media""#));
        assert!(body.contains(r#"<img src="/uploads/images/flagged.webp""#));
    }

    #[test]
    fn youtube_preview_rendering_escapes_user_controlled_text_and_urls() {
        let mut post = test_post();
        post.text =
            "unsafe <script>alert(1)</script> https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=1"
                .to_owned();

        let body = post_card(&post, None, None);

        assert!(body.contains("unsafe &lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(!body.contains("<script>alert(1)</script>"));
        assert!(body.contains(r#"href="https://www.youtube.com/watch?v=dQw4w9WgXcQ""#));
        assert!(body.contains(
            r#"data-youtube-embed-src="https://www.youtube-nocookie.com/embed/dQw4w9WgXcQ""#
        ));
        assert!(body.contains(
            r#"<span class="youtube-preview-url">https://www.youtube.com/watch?v=dQw4w9WgXcQ&amp;t=1</span>"#
        ));
    }

    #[test]
    fn youtube_preview_uses_stored_title_metadata_when_available() {
        let mut post = test_post();
        post.text = "stored metadata wins https://youtu.be/aaaaaaaaaaa".to_owned();
        post.youtube_embeds = vec![
            youtube::embed_from_stored("dQw4w9WgXcQ", Some("Fetched <Title>".to_owned()))
                .expect("stored embed"),
        ];

        let body = post_card(&post, None, None);

        assert!(
            body.contains(r#"<span class="youtube-preview-title">Fetched &lt;Title&gt;</span>"#)
        );
        assert!(body.contains(r#"data-youtube-title="Fetched &lt;Title&gt;""#));
        assert!(body.contains("https://i.ytimg.com/vi/dQw4w9WgXcQ/hqdefault.jpg"));
        assert!(!body.contains("https://i.ytimg.com/vi/aaaaaaaaaaa/hqdefault.jpg"));
        assert!(!body.contains("<iframe"));
    }

    #[test]
    fn posts_without_youtube_links_do_not_render_preview_cards() {
        let mut post = test_post();
        post.text = "hello https://example.com/watch?v=dQw4w9WgXcQ #rust".to_owned();

        let body = post_card(&post, None, None);

        assert!(!body.contains("youtube-preview-card"));
        assert!(body.contains(
            r#"<div class="text">hello https://example.com/watch?v=dQw4w9WgXcQ <a href="/tags/rust">#rust</a></div>"#
        ));
    }

    #[test]
    fn multiple_youtube_links_render_up_to_documented_cap() {
        let mut post = test_post();
        post.text = "https://youtu.be/dQw4w9WgXcQ https://youtube.com/shorts/aaaaaaaaaaa https://youtube.com/embed/bbbbbbbbbbb https://youtube.com/watch?v=ccccccccccc".to_owned();

        let body = post_card(&post, None, None);

        assert_eq!(
            body.matches(r#"data-testid="youtube-preview-card""#)
                .count(),
            3
        );
        assert!(body.contains("https://i.ytimg.com/vi/dQw4w9WgXcQ/hqdefault.jpg"));
        assert!(body.contains("https://i.ytimg.com/vi/aaaaaaaaaaa/hqdefault.jpg"));
        assert!(body.contains("https://i.ytimg.com/vi/bbbbbbbbbbb/hqdefault.jpg"));
        assert!(!body.contains("https://i.ytimg.com/vi/ccccccccccc/hqdefault.jpg"));
    }

    #[test]
    fn no_javascript_youtube_preview_shows_thumbnail_and_link_card() {
        let mut post = test_post();
        post.text = "watch https://youtu.be/dQw4w9WgXcQ".to_owned();

        let body = post_card(&post, None, None);

        assert!(body.contains(
            r#"<div class="youtube-preview-card" data-testid="youtube-preview-card" data-youtube-preview><a class="youtube-preview-main" data-youtube-play href="https://www.youtube.com/watch?v=dQw4w9WgXcQ""#
        ));
        assert!(body.contains(
            r#"<img class="youtube-thumbnail" src="https://i.ytimg.com/vi/dQw4w9WgXcQ/hqdefault.jpg" alt="" loading="lazy" decoding="async" referrerpolicy="no-referrer">"#
        ));
        assert!(body.contains(r#"<span class="youtube-preview-title">YouTube video</span>"#));
        assert!(body.contains(
            r#"<a class="youtube-open-link" href="https://www.youtube.com/watch?v=dQw4w9WgXcQ""#
        ));
        assert!(!body.contains("<iframe"));
        assert!(!body.contains("<script"));
    }

    #[test]
    fn layout_allows_youtube_thumbnail_and_nocookie_frame_origins() {
        let body = layout(None, "Home Feed", "<p>body</p>", "RustPost");

        assert!(body.contains("img-src 'self' data: https://i.ytimg.com"));
        assert!(body.contains("frame-src https://www.youtube-nocookie.com"));
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
            edited_at: None,
            event_created_at: "2026-05-18 10:30".to_owned(),
            like_count: 1,
            repost_count: 2,
            reply_count: 3,
            viewer_liked: false,
            viewer_bookmarked: false,
            viewer_reposted: false,
            viewer_can_repost: false,
            pinned_by_author: false,
            original_unavailable: false,
            reposted_by_user_id: None,
            reposted_by_username: None,
            reposted_by_display_name: None,
            reposted_at: None,
            quote: None,
            media: Vec::new(),
            youtube_embeds: Vec::new(),
        }
    }
}
