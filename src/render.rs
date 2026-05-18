use crate::auth::CurrentUser;
use crate::social::{MediaView, PostView, TimelineEventKind};

pub fn layout(user: Option<&CurrentUser>, title: &str, body: &str) -> String {
    let auth_nav = if let Some(user) = user {
        let admin = if user.is_admin {
            r#"<a href="/admin">Admin</a>"#
        } else {
            ""
        };
        format!(
            r#"<a href="/home">Home</a><a href="/local">Local</a><a href="/notifications">Notifications</a><a href="/bookmarks">Bookmarks</a><a href="/settings">Settings</a>{admin}<form method="post" action="/logout"><button>Log out</button></form>"#
        )
    } else {
        r#"<a href="/local">Local</a><a href="/login">Log in</a><a href="/register">Register</a>"#
            .to_owned()
    };
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta http-equiv="Content-Security-Policy" content="default-src 'self'; img-src 'self' data:; media-src 'self'; style-src 'self' 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'">
<meta http-equiv="X-Content-Type-Options" content="nosniff">
<meta name="referrer" content="same-origin">
<title>{} - RustPost</title>
<style>{}</style>
</head>
<body>
<header><a class="brand" href="/local">RustPost</a><nav>{}</nav></header>
<main>{}</main>
</body>
</html>"#,
        html_escape::encode_text(title),
        CSS,
        auth_nav,
        body
    )
}

pub fn composer(csrf: Option<&str>, parent: Option<i64>) -> String {
    let parent_input = parent.map_or_else(String::new, |id| {
        format!(r#"<input type="hidden" name="parent_post_id" value="{id}">"#)
    });
    let csrf = csrf.unwrap_or_default();
    format!(
        r#"<section class="composer"><form method="post" action="/posts" enctype="multipart/form-data">
<input type="hidden" name="csrf" value="{}">{}
<label for="text">Post</label>
<textarea id="text" name="text" maxlength="280" rows="4"></textarea>
<label for="media">Media</label>
<input id="media" name="media" type="file" multiple accept="image/*,video/mp4,video/webm,video/quicktime">
<button type="submit">Post</button>
</form></section>"#,
        html_escape::encode_double_quoted_attribute(csrf),
        parent_input
    )
}

pub fn posts(posts: &[PostView], user: Option<&CurrentUser>, csrf: Option<&str>) -> String {
    posts
        .iter()
        .map(|post| post_card(post, user, csrf))
        .collect::<Vec<_>>()
        .join("")
}

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
            r#"<article class="post unavailable" id="event-{}">{}<div class="text">This post is no longer available.</div></article>"#,
            html_escape::encode_double_quoted_attribute(&post.event_id),
            repost_banner
        );
    }
    let author = match (&post.username, &post.anonymous_label) {
        (Some(username), _) => format!(
            r#"<a href="/users/{}">{}</a>"#,
            html_escape::encode_double_quoted_attribute(username),
            html_escape::encode_text(post.display_name.as_deref().unwrap_or(username))
        ),
        (None, Some(label)) => html_escape::encode_text(label).to_string(),
        _ => "Deleted user".to_owned(),
    };
    let text = linkify(&post.text);
    let media = post
        .media
        .iter()
        .map(render_media)
        .collect::<Vec<_>>()
        .join("");
    let controls = if let (Some(user), Some(csrf)) = (user, csrf) {
        let delete = if post.user_id == Some(user.id) || user.is_admin {
            action_form(&format!("/posts/{}/delete", post.id), csrf, "Delete")
        } else {
            String::new()
        };
        format!(
            r#"<div class="actions">{}{}{}{}{}<a href="/posts/{}">Thread</a></div>"#,
            action_form(
                &format!("/posts/{}/like", post.id),
                csrf,
                if post.viewer_liked { "Unlike" } else { "Like" }
            ),
            action_form(
                &format!("/posts/{}/bookmark", post.id),
                csrf,
                if post.viewer_bookmarked {
                    "Unbookmark"
                } else {
                    "Bookmark"
                }
            ),
            action_form(&format!("/posts/{}/repost", post.id), csrf, "Repost"),
            action_form(&format!("/posts/{}/reply", post.id), csrf, "Reply"),
            delete,
            post.id
        )
    } else {
        format!(
            r#"<div class="actions"><a href="/posts/{}">Thread</a></div>"#,
            post.id
        )
    };
    format!(
        r#"<article class="post" id="event-{}">{}<div class="meta">{} <a href="/posts/{}">#{}</a> <span>{}</span></div><div class="text">{}</div>{}<div class="counts">{} likes · {} reposts · {} replies</div>{}</article>"#,
        html_escape::encode_double_quoted_attribute(&post.event_id),
        repost_banner,
        author,
        post.id,
        post.id,
        html_escape::encode_text(&post.created_at),
        text,
        media,
        post.like_count,
        post.repost_count,
        post.reply_count,
        controls
    )
}

fn action_form(action: &str, csrf: &str, label: &str) -> String {
    format!(
        r#"<form method="post" action="{}"><input type="hidden" name="csrf" value="{}"><button>{}</button></form>"#,
        html_escape::encode_double_quoted_attribute(action),
        html_escape::encode_double_quoted_attribute(csrf),
        html_escape::encode_text(label)
    )
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

const CSS: &str = r#"
:root{font-family:system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;color:#1f2328;background:#f7f7f4}
body{margin:0}header{display:flex;align-items:center;justify-content:space-between;gap:1rem;padding:.85rem 1rem;border-bottom:1px solid #d7d7d0;background:#fff;position:sticky;top:0}
.brand{font-weight:700;color:#132;text-decoration:none}nav{display:flex;gap:.75rem;align-items:center;flex-wrap:wrap}nav a,.actions a{color:#274c77;text-decoration:none}
main{max-width:760px;margin:0 auto;padding:1rem}.post,.composer,.panel{background:#fff;border:1px solid #ddd;border-radius:8px;padding:1rem;margin:.75rem 0}
label{display:block;font-weight:600;margin-top:.75rem}input,textarea,button{font:inherit}input[type=text],input[type=password],input[type=url],textarea{box-sizing:border-box;width:100%;padding:.65rem;border:1px solid #bbb;border-radius:6px}
button{border:1px solid #476072;background:#385568;color:#fff;border-radius:6px;padding:.45rem .75rem;cursor:pointer}button:hover{background:#2f4858}
.meta,.counts{color:#666;font-size:.92rem}.text{white-space:pre-wrap;margin:.7rem 0;line-height:1.45}.post img,.post video{max-width:100%;border-radius:6px;border:1px solid #ddd;margin-top:.5rem}
.repost-banner{color:#536471;font-size:.9rem;font-weight:600;margin-bottom:.35rem}.unavailable{color:#667}
.profile-banner{width:100%;max-height:220px;object-fit:cover;border-radius:6px;border:1px solid #ddd}.profile-heading{display:flex;gap:1rem;align-items:flex-start;margin-top:.75rem}.profile-picture{width:88px;height:88px;object-fit:cover;border-radius:8px;border:1px solid #ddd;background:#f6f8fa}
.actions{display:flex;gap:.45rem;flex-wrap:wrap;align-items:center}.actions form,nav form{display:inline}.actions button{background:#fff;color:#274c77;border-color:#ccd}
.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(180px,1fr));gap:.75rem}.warning{border-color:#c87;background:#fff7ed}
@media (max-width:600px){header{align-items:flex-start;flex-direction:column}main{padding:.75rem}.actions button{padding:.4rem .55rem}}
"#;
