use std::fmt::Write as _;

use crate::auth::{CurrentUser, Theme};
use crate::social::{
    AccountView, FollowRequestView, MediaView, NotificationGroupView, PostView, ProfileTimelineTab,
    QuotePreview, TimelineEventKind,
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
    /// Enabled instance announcement, rendered beside the site name.
    pub announcement: Option<String>,
    /// Maintenance notice, rendered under the header while maintenance is on.
    pub maintenance_notice: Option<String>,
    /// Notice for accounts with a pending deletion deadline.
    pub account_notice: Option<String>,
    /// Pending incoming follow requests, shown as a nav badge.
    pub pending_follow_requests: Option<i64>,
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
            announcement: None,
            maintenance_notice: None,
            account_notice: None,
            pending_follow_requests: None,
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

const CLIENT_BOOT_SCRIPT: &str = include_str!("../assets/client-boot.js");

const CLIENT_SCRIPT: &str = include_str!("../assets/client.js");

/// Follow button state on profiles, account lists, and search results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowButtonState {
    /// No relationship; submitting sends a follow or a follow request.
    Follow,
    /// An accepted follow exists; submitting unfollows.
    Following,
    /// A pending request exists; submitting cancels the request.
    Requested,
}

impl FollowButtonState {
    #[must_use]
    pub const fn from_flags(following: bool, requested: bool) -> Self {
        if following {
            Self::Following
        } else if requested {
            Self::Requested
        } else {
            Self::Follow
        }
    }
}

// Rendering a post card stays centralized because the markup, counts, media,
// and action controls must remain consistent between timelines and threads.

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FollowRequestDirection {
    Incoming,
    Outgoing,
}

pub fn notice(kind: &str, message: &str) -> String {
    let role = if kind == "error" { "alert" } else { "status" };
    format!(
        r#"<section class="notice {}" role="{role}"><p>{}</p></section>"#,
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

const CSS: &str = include_str!("../assets/site.css");

mod layout;
use layout::plural;
pub use layout::{layout, layout_with_context, layout_with_csrf};
mod forms;
pub use forms::{
    account_links, account_links_with_empty_state, accounts, client_boot_script, client_script,
    composer, edit_post_form, follow_form, login_form, onboarding_page, password_length_attrs,
    quote_composer, register_form, search_page,
};
mod posts;
pub(crate) use posts::icon_svg;
pub use posts::{
    linkify, pinned_post_with_controls, post_card, post_card_with_controls,
    post_card_with_nsfw_blur, posts, posts_with_controls, posts_with_controls_empty_state,
    posts_with_nsfw_blur, profile_tabs, thread_post_card, thread_post_card_with_controls,
    thread_post_card_with_nsfw_blur, thread_posts, thread_posts_with_controls,
    thread_posts_with_nsfw_blur,
};
use posts::{posts_with_options, quote_preview_card};
mod notifications;
pub use notifications::notifications_page;
use notifications::parse_timestamp;
mod accounts;
pub use accounts::{
    follow_requests_page, historical_username_page, maintenance_page, released_username_page,
    released_username_profile_note, thread_back_control, username_history_note,
};
