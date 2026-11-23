//! Posts for the render subsystem.

use super::*;

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

pub(super) fn profile_tab_href(username: &str, tab: ProfileTimelineTab) -> String {
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

pub(super) fn thread_posts_with_options(
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

pub(super) fn posts_with_options(
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

#[expect(
    clippy::too_many_lines,
    reason = "post card markup is centralized to keep timeline and thread rendering identical"
)]
pub(super) fn post_card_with_options(
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
    let media = format!("{}{}", media, render_poll(post, user, csrf));
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
        r#"<article class="{}" data-testid="post-card" id="post-{}" data-post-id="{}" data-event-id="{}"{}>{}{}<header class="post-header"><div class="author-block">{}<div>{}</div></div>{}</header><div class="text">{}</div>{}{}{}<div class="counts"><span data-count="likes">{} {}</span><span data-count="reposts">{} {}</span><span data-count="replies">{} {}</span>{}{}</div>{}</article>"#,
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
        plural(post.like_count, "like", "likes"),
        post.repost_count,
        plural(post.repost_count, "repost", "reposts"),
        post.reply_count,
        plural(post.reply_count, "reply", "replies"),
        edited,
        permalink,
        controls
    )
}

pub(super) fn post_edit_available(created_at: &str, window_seconds: u64) -> bool {
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

pub(super) fn repost_action_with_quote(
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

pub(super) fn icon_action_form(
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

pub(super) fn pin_action_form(action: &str, csrf: &str, label: &str, active: bool) -> String {
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

pub(super) fn icon_link(href: &str, label: &str, icon: &str) -> String {
    format!(
        r#"<a class="icon-button" href="{}" aria-label="{}" title="{}">{}<span class="sr-only">{}</span></a>"#,
        html_escape::encode_double_quoted_attribute(href),
        html_escape::encode_double_quoted_attribute(label),
        html_escape::encode_double_quoted_attribute(label),
        icon_svg(icon),
        html_escape::encode_text(label)
    )
}

pub(super) fn disabled_icon_button(label: &str, icon: &str) -> String {
    format!(
        r#"<button class="icon-button disabled" type="button" aria-label="{}" title="{}" disabled>{}<span class="sr-only">{}</span></button>"#,
        html_escape::encode_double_quoted_attribute(label),
        html_escape::encode_double_quoted_attribute(label),
        icon_svg(icon),
        html_escape::encode_text(label)
    )
}

pub(super) fn admin_nsfw_form(post: &PostView, csrf: &str) -> String {
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

pub(super) fn quote_preview_card(quote: &QuotePreview) -> String {
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

pub fn icon_svg(icon: &str) -> &'static str {
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

pub(super) fn render_media(
    media: &MediaView,
    post_id: i64,
    index: usize,
    blur_nsfw_media: bool,
) -> String {
    let path = html_escape::encode_double_quoted_attribute(&media.public_path);
    let alt = html_escape::encode_double_quoted_attribute(&media.alt_text);
    let item = if media.media_kind == "video" {
        format!(
            r#"<div class="post-video"><video controls playsinline preload="metadata" src="{path}">Your browser cannot play this video.</video><p><a href="{path}" download>Download video</a></p></div>"#
        )
    } else {
        format!(r#"<img src="{path}" alt="{alt}" loading="lazy">"#)
    };
    let item = if matches!(
        media.conversion_state.as_str(),
        "queued" | "processing" | "failed"
    ) {
        let label = if media.conversion_state == "failed" {
            "Conversion failed; original retained"
        } else {
            "Media processing; original available"
        };
        format!("{item}<p class=\"field-help\">{label}</p>")
    } else {
        item
    };
    if !media.is_nsfw || !blur_nsfw_media {
        return item;
    }
    let toggle_id = format!("nsfw-media-{post_id}-{index}");
    format!(
        r#"<div class="nsfw-media" data-testid="nsfw-media"><input class="nsfw-toggle sr-only" id="{toggle_id}" type="checkbox" aria-label="Show NSFW media"><div class="nsfw-media-frame">{item}<span class="nsfw-badge">NSFW</span></div><label class="nsfw-show" for="{toggle_id}">Show<span class="sr-only"> NSFW media</span></label></div>"#
    )
}

pub(super) fn render_youtube_previews(post: &PostView) -> String {
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

pub(super) fn render_youtube_preview_card(preview: &YoutubeEmbed) -> String {
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

/// Escapes post text and turns `#tags`, `@mentions`, and plain URLs into links.
///
/// Whitespace, including newlines, is preserved so multi-line posts render the
/// way they were composed.
pub fn linkify(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        let whitespace_run = rest.chars().next().is_some_and(char::is_whitespace);
        let end = rest
            .find(|ch: char| ch.is_whitespace() != whitespace_run)
            .unwrap_or(rest.len());
        let (chunk, tail) = rest.split_at(end);
        if whitespace_run {
            out.push_str(&html_escape::encode_text(chunk));
        } else {
            out.push_str(&linkify_token(chunk));
        }
        rest = tail;
    }
    out
}

pub(super) fn linkify_token(token: &str) -> String {
    let leading_len = token
        .find(|ch: char| !matches!(ch, '(' | '[' | '{' | '"' | '\'' | '<' | '“' | '‘'))
        .unwrap_or(token.len());
    let (leading, remainder) = token.split_at(leading_len);
    let core_len = remainder
        .trim_end_matches(|ch: char| {
            matches!(
                ch,
                '.' | ',' | '!' | '?' | ';' | ':' | ')' | ']' | '}' | '"' | '\'' | '”' | '’'
            )
        })
        .len();
    let (core, trailing) = remainder.split_at(core_len);
    let mut out = html_escape::encode_text(leading).into_owned();
    if let Some(tag) = core
        .strip_prefix('#')
        .filter(|value| !value.is_empty() && value.chars().any(char::is_alphanumeric))
    {
        let _ = write!(
            out,
            r##"<a href="/tags/{}">#{}</a>"##,
            html_escape::encode_double_quoted_attribute(tag),
            html_escape::encode_text(tag)
        );
    } else if let Some(name) = core
        .strip_prefix('@')
        .filter(|value| !value.is_empty() && value.chars().any(char::is_alphanumeric))
    {
        let _ = write!(
            out,
            r#"<a href="/users/{}">@{}</a>"#,
            html_escape::encode_double_quoted_attribute(name),
            html_escape::encode_text(name)
        );
    } else if is_linkable_url(core) {
        let _ = write!(
            out,
            r#"<a href="{}" rel="noopener noreferrer nofollow">{}</a>"#,
            html_escape::encode_double_quoted_attribute(core),
            html_escape::encode_text(core)
        );
    } else {
        out.push_str(&html_escape::encode_text(core));
    }
    out.push_str(&html_escape::encode_text(trailing));
    out
}

pub(super) fn is_linkable_url(value: &str) -> bool {
    let Some(rest) = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
    else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    host.contains('.') && !host.starts_with('.') && !host.ends_with('.')
}

fn render_poll(post: &PostView, user: Option<&CurrentUser>, csrf: Option<&str>) -> String {
    let Some(poll) = &post.poll else {
        return String::new();
    };
    let voting = user.is_some() && csrf.is_some() && !poll.closed;
    let mut choices = String::new();
    for choice in &poll.choices {
        let label = html_escape::encode_text(&choice.label);
        let selected = if choice.selected { " checked" } else { "" };
        let input = if voting {
            format!(
                r#"<input type="{}" name="choice" value="{}"{selected}> "#,
                if poll.multiple { "checkbox" } else { "radio" },
                choice.id
            )
        } else {
            String::new()
        };
        let own = if choice.selected {
            " · your choice"
        } else {
            ""
        };
        let _ = write!(
            choices,
            r#"<p><label>{input}{label} — {} votes{own}</label></p>"#,
            choice.votes
        );
    }
    let privacy = "Only totals and your own choices are shown. Votes are linked to accounts; the operator can access vote records.";
    let status = if poll.closed {
        "Poll closed"
    } else if poll.multiple {
        "Choose any options"
    } else {
        "Choose one option"
    };
    let close = poll
        .closes_at
        .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
        .map_or_else(String::new, |t| {
            format!(" · closes {} UTC", t.format("%Y-%m-%d %H:%M"))
        });
    let content = if voting {
        format!(
            r#"<form method="post" action="/polls/{}/vote"><input type="hidden" name="csrf" value="{}"><fieldset><legend>{status}</legend>{choices}</fieldset><button type="submit">Save vote</button></form>"#,
            post.id,
            html_escape::encode_double_quoted_attribute(csrf.unwrap_or_default())
        )
    } else {
        choices
    };
    let close_button = if !poll.closed
        && user.is_some_and(|u| post.user_id == Some(u.id))
        && csrf.is_some()
    {
        format!(
            r#"<form method="post" action="/polls/{}/close"><input type="hidden" name="csrf" value="{}"><button>Close poll</button></form>"#,
            post.id,
            html_escape::encode_double_quoted_attribute(csrf.unwrap_or_default())
        )
    } else {
        String::new()
    };
    format!(
        r#"<section class="poll" aria-label="Community poll"><p>{status}{close} · {} voters</p>{content}<p class="field-help">{privacy}</p>{close_button}</section>"#,
        poll.voters
    )
}
