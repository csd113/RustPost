use std::collections::{BTreeSet, HashMap};

use rusqlite::{Connection, OptionalExtension as _, Row, params, params_from_iter};

use crate::config::Settings;
use crate::db::SqlitePool;
use crate::validation::clean_post_text;
use crate::youtube::YoutubeEmbed;

#[derive(Debug, Clone)]
pub struct AccountView {
    pub id: i64,
    pub username: String,
    pub display_name: String,
    pub bio: String,
    pub profile_picture_path: Option<String>,
    pub viewer_following: bool,
}

#[derive(Debug, Clone)]
pub struct PostView {
    pub event_id: String,
    pub event_kind: TimelineEventKind,
    pub id: i64,
    pub user_id: Option<i64>,
    pub username: Option<String>,
    pub display_name: Option<String>,
    pub profile_picture_path: Option<String>,
    pub anonymous_label: Option<String>,
    pub text: String,
    pub parent_post_id: Option<i64>,
    pub created_at: String,
    pub edited_at: Option<String>,
    pub event_created_at: String,
    pub like_count: i64,
    pub repost_count: i64,
    pub reply_count: i64,
    pub viewer_liked: bool,
    pub viewer_bookmarked: bool,
    pub viewer_reposted: bool,
    pub viewer_can_repost: bool,
    pub pinned_by_author: bool,
    pub original_unavailable: bool,
    pub reposted_by_user_id: Option<i64>,
    pub reposted_by_username: Option<String>,
    pub reposted_by_display_name: Option<String>,
    pub reposted_at: Option<String>,
    pub quote: Option<QuotePreview>,
    pub media: Vec<MediaView>,
    pub youtube_embeds: Vec<YoutubeEmbed>,
    pub poll: Option<polls::PollView>,
}

#[derive(Debug, Clone)]
pub struct QuotePreview {
    pub id: i64,
    pub username: Option<String>,
    pub display_name: Option<String>,
    pub anonymous_label: Option<String>,
    pub text: String,
    pub created_at: String,
    pub unavailable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimelineEventKind {
    Post,
    Repost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileTimelineTab {
    Posts,
    Replies,
    Media,
    Likes,
}

impl ProfileTimelineTab {
    const fn repost_mode(self) -> &'static str {
        match self {
            Self::Media => "profile_media",
            Self::Posts | Self::Replies | Self::Likes => "profile",
        }
    }
}

#[derive(Debug, Clone)]
pub struct MediaView {
    pub id: i64,
    pub conversion_state: String,
    pub public_path: String,
    pub mime_type: String,
    pub media_kind: String,
    pub alt_text: String,
    pub is_nsfw: bool,
}

#[derive(Debug, Clone)]
pub struct MutedWord {
    pub id: i64,
    pub term: String,
}

#[derive(Debug, Clone)]
pub struct NotificationView {
    pub id: i64,
    pub kind: String,
    pub actor_user_id: Option<i64>,
    pub actor_username: Option<String>,
    pub actor_display_name: Option<String>,
    pub post_id: Option<i64>,
    pub group_target_post_id: Option<i64>,
    pub post_text: Option<String>,
    pub post_available: bool,
    pub read_at: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionSuggestion {
    pub username: String,
    pub display_name: String,
}

#[derive(Debug, Clone)]
pub struct NotificationActorView {
    pub user_id: Option<i64>,
    pub username: Option<String>,
    pub display_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NotificationGroupView {
    pub id: i64,
    pub kind: String,
    pub group_target_post_id: Option<i64>,
    pub post_id: Option<i64>,
    pub post_text: Option<String>,
    pub post_available: bool,
    pub unread_count: i64,
    pub total_count: usize,
    pub notification_ids: Vec<i64>,
    pub actors: Vec<NotificationActorView>,
    pub created_at: String,
}

#[derive(Debug)]
struct PostRow {
    event_kind: String,
    event_id: String,
    event_created_at: String,
    repost_user_id: Option<i64>,
    repost_username: Option<String>,
    repost_display_name: Option<String>,
    repost_created_at: Option<String>,
    original_unavailable: bool,
    id: i64,
    user_id: Option<i64>,
    username: Option<String>,
    display_name: Option<String>,
    profile_picture_path: Option<String>,
    anonymous_label: Option<String>,
    text: String,
    parent_post_id: Option<i64>,
    created_at: String,
    edited_at: Option<String>,
    repost_count: i64,
    reply_count: i64,
    quote_post_id: Option<i64>,
    pinned_by_author: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotePostOutcome {
    pub post_id: i64,
    pub created: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditPostError {
    NotFound,
    Forbidden,
    WindowExpired,
    Validation(String),
    Database(String),
}

impl std::fmt::Display for EditPostError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => formatter.write_str("post not found"),
            Self::Forbidden => formatter.write_str("cannot edit this post"),
            Self::WindowExpired => formatter.write_str("the edit window for this post has expired"),
            Self::Validation(message) | Self::Database(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for EditPostError {}

impl From<rusqlite::Error> for EditPostError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinPostError {
    NotFound,
    Forbidden,
    Database(String),
}

impl std::fmt::Display for PinPostError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => formatter.write_str("post not found"),
            Self::Forbidden => formatter.write_str("cannot pin this post"),
            Self::Database(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for PinPostError {}

impl From<rusqlite::Error> for PinPostError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error.to_string())
    }
}

/// Result of asking to follow an account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowOutcome {
    /// A follow row was created immediately.
    Followed,
    /// The target requires approval and a pending request was recorded.
    Requested,
    /// The viewer already follows the target.
    AlreadyFollowing,
    /// A pending request already exists for this pair.
    AlreadyRequested,
}

/// An account involved in a pending follow request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FollowRequestView {
    pub user_id: i64,
    pub username: String,
    pub display_name: String,
    pub bio: String,
    pub profile_picture_path: Option<String>,
    pub created_at: String,
}

/// The viewer's relationship to a profile, used to render accurate controls
/// and profile states instead of always showing unused actions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProfileRelationship {
    pub following: bool,
    /// The viewer has a pending follow request to this profile.
    pub requested: bool,
    pub blocked: bool,
    pub muted: bool,
    pub blocks_viewer: bool,
}

impl ProfileRelationship {
    #[must_use]
    pub const fn has_block(self) -> bool {
        self.blocked || self.blocks_viewer
    }
}

/// Maximum accounts rendered per follower/following page.
pub const ACCOUNT_LIST_PAGE_SIZE: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FollowEdge {
    Followers,
    Following,
}

const MENTION_SUGGESTION_LIMIT: usize = 8;
const MENTION_QUERY_MAX_CHARS: usize = 32;

/// SQL fragment that hides notifications whose actor is blocked in either
/// direction, muted, or whose post text matches one of the recipient's muted
/// words. Binds `VIEWER_FILTER_BINDINGS` copies of the recipient's user id.
const NOTIFICATION_VISIBLE_SQL: &str = r#"
  AND (
    n.actor_user_id IS NULL
    OR (
      n.actor_user_id NOT IN (SELECT blocked_id FROM blocks WHERE blocker_id = ?)
      AND NOT EXISTS (SELECT 1 FROM blocks WHERE blocker_id = n.actor_user_id AND blocked_id = ?)
      AND n.actor_user_id NOT IN (SELECT muted_id FROM mutes WHERE muter_id = ?)
    )
  )
  AND (
    p.user_id = ?
    OR NOT EXISTS (
      SELECT 1 FROM muted_words mw
      WHERE mw.user_id = ? AND rustpost_muted_word_match(COALESCE(p.text, ''), mw.normalized_term)
    )
  )
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
struct NotificationGroupKey {
    kind: String,
    target: NotificationGroupTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum NotificationGroupTarget {
    Follow,
    Post(i64),
    Notification(i64),
}

/// Number of bound parameters added by [`append_viewer_filters`].
const VIEWER_FILTER_BINDINGS: usize = 5;

#[derive(Debug, Clone, Copy)]
enum ViewerRelation {
    Likes,
    Bookmarks,
    Reposts,
}

impl ViewerRelation {
    const fn table(self) -> &'static str {
        match self {
            Self::Likes => "likes",
            Self::Bookmarks => "bookmarks",
            Self::Reposts => "reposts",
        }
    }
}

mod posting;
pub use posting::{
    PostOptions, create_post, create_post_with_options, create_quote_post, edit_post,
    post_for_request, post_thread, profile_activity_visible, profile_pinned_post,
    profile_tab_timeline, profile_timeline, quote_target_preview, repost, timeline, unrepost,
};
mod relationships;
pub(crate) use relationships::follow_in_transaction;
pub use relationships::{
    active_follow_targets, add_muted_word, approve_follow_request, block, blocked_users,
    cancel_follow_request, follow, follow_counts, followers_accounts, following_accounts,
    following_accounts_for_profile, incoming_follow_requests, instance_counts, is_following, mute,
    muted_users, muted_words, onboarding_suggestions, outgoing_follow_requests,
    pending_follow_request_count, profile_relationship, reject_follow_request, remove_muted_word,
    set_follow_approval_required, unblock, unfollow, unmute,
};
mod interactions;
use interactions::{blocks_exist_tx, clean_muted_word, ensure_post_interaction_accessible_tx};
pub use interactions::{
    bookmark, delete_post, like, pin_post, pinned_post_id, set_post_media_nsfw, unbookmark, unlike,
    unpin_post,
};
mod search;
pub use search::{mention_suggestions, search, search_history, search_page};
mod notifications;
pub use notifications::{
    mark_notification_group_read, mark_notification_ids_read, mark_notifications_read,
    notification_groups, notifications, unread_notification_count,
};
mod query;
use query::{
    append_viewer_filters, base_post_query, create_notification_tx,
    ensure_quote_target_accessible_tx, liked_events_for_user, media_events_for_user,
    notify_mentioned_users_tx, notify_post_owner_tx, post_events, post_events_for_user,
    push_viewer_filter_bindings, query_post_rows, quote_previews, replace_youtube_embeds_tx,
    reply_events_for_user, repost_events, reuse_stored_youtube_metadata, rows_to_posts,
    youtube_embeds_for_post_conn,
};

pub mod drafts;
pub mod polls;

pub mod history;

pub mod recovery;
