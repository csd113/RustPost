use crate::auth::CurrentUser;
use crate::social::{MediaView, PostView, TimelineEventKind};
use axum::http::StatusCode;

pub fn layout(user: Option<&CurrentUser>, title: &str, body: &str) -> String {
    layout_with_csrf(user, None, title, body)
}

pub fn layout_with_csrf(
    user: Option<&CurrentUser>,
    csrf: Option<&str>,
    title: &str,
    body: &str,
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
            r#"<a href="/home">Home Feed</a><a href="/local">Local Feed</a><a href="/search">Search</a><a href="/notifications">Notifications</a><a href="/bookmarks">Bookmarks</a><a href="/settings">Settings</a>{admin}{logout}"#
        )
    } else {
        r#"<a href="/local">Local Feed</a><a href="/search">Search</a><a href="/login">Log in</a><a href="/register">Register</a>"#
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
<script src="/assets/rustpost.js" defer></script>
</head>
<body>
<header class="site-header"><div class="header-inner"><a class="brand" href="/local"><span class="brand-mark">R</span><span>RustPost</span></a><nav>{}</nav></div></header>
<main><div class="content-shell"><section class="content-column">{} </section><aside class="side-panel"><h2>Alpha status</h2><p>Local-first microblog. Anonymous posting is off by default.</p><a href="/local">Local Feed</a></aside></div></main>
<footer class="site-footer">RustPost alpha</footer>
</body>
</html>"#,
        html_escape::encode_text(title),
        CSS,
        auth_nav,
        body
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
});"#
}

pub fn composer(csrf: Option<&str>, parent: Option<i64>) -> String {
    let parent_input = parent.map_or_else(String::new, |id| {
        format!(r#"<input type="hidden" name="parent_post_id" value="{id}">"#)
    });
    let csrf = csrf.unwrap_or_default();
    format!(
        r#"<section class="composer" id="reply" aria-labelledby="composer-title"><div class="section-heading"><h1 id="composer-title">{}</h1><span class="muted">280 characters</span></div><form method="post" action="/posts" enctype="multipart/form-data">
<input type="hidden" name="csrf" value="{}">{}
<label for="text">What is happening?</label>
<textarea id="text" name="text" maxlength="280" rows="4" placeholder="Write a short update..."></textarea>
<div class="composer-tools"><label class="file-control" for="media">Attach media<input id="media" name="media" type="file" multiple accept="image/*,video/mp4,video/webm,video/quicktime"></label><button class="primary" type="submit">Post</button></div>
</form></section>"#,
        if parent.is_some() {
            "Reply"
        } else {
            "New post"
        },
        html_escape::encode_double_quoted_attribute(csrf),
        parent_input
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
            r#"<a class="author-name" href="/users/{}">{}</a> <span class="username">@{}</span>"#,
            html_escape::encode_double_quoted_attribute(username),
            html_escape::encode_text(post.display_name.as_deref().unwrap_or(username)),
            html_escape::encode_text(username)
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
        let reply_link = format!(
            r#"<a class="button-link" href="/posts/{}#reply">Reply</a>"#,
            post.id
        );
        format!(
            r#"<div class="actions">{}{}{}{}{}<a class="button-link" href="/posts/{}">Open thread</a></div>"#,
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
            reply_link,
            delete,
            post.id
        )
    } else {
        format!(
            r#"<div class="actions"><a class="button-link" href="/posts/{}">Open thread</a></div>"#,
            post.id
        )
    };
    format!(
        r#"<article class="post" id="event-{}">{}<header class="post-header"><div>{}</div><a class="post-time" href="/posts/{}">#{}</a></header><div class="text">{}</div>{}<div class="counts"><span>{} likes</span><span>{} reposts</span><span>{} replies</span><span>{}</span></div>{}</article>"#,
        html_escape::encode_double_quoted_attribute(&post.event_id),
        repost_banner,
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

fn action_form(action: &str, csrf: &str, label: &str) -> String {
    format!(
        r#"<form method="post" action="{}"><input type="hidden" name="csrf" value="{}"><button type="submit">{}</button></form>"#,
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
            r#"<section class="panel error-panel"><p class="eyebrow">{} error</p><h1>{}</h1><p>{}</p><p><a class="button-link" href="/local">Back to local timeline</a></p></section>"#,
            status.as_u16(),
            html_escape::encode_text(title),
            html_escape::encode_text(message)
        ),
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
main{padding:1.25rem}.content-shell{max-width:1120px;margin:0 auto;display:grid;grid-template-columns:minmax(0,720px) 280px;gap:1.25rem;align-items:start}.content-column{min-width:0}.side-panel{position:sticky;top:5rem;background:#fff;border:1px solid #dfe4dc;border-radius:8px;padding:1rem;color:#59625a}.side-panel h2{margin:.1rem 0 .35rem;font-size:1rem;color:#202124}.site-footer{max-width:1120px;margin:0 auto;padding:1rem;color:#687068;font-size:.9rem}
.page-header,.post,.composer,.panel,.empty-state,.notice{background:#fff;border:1px solid #dfe4dc;border-radius:8px;margin:0 0 .85rem;padding:1rem;box-shadow:0 1px 2px rgba(20,35,30,.04)}
.page-header h1,.section-heading h1,.panel h1{margin:0;font-size:1.45rem;line-height:1.2}.page-header p,.muted,.empty-state p{color:#667064;margin:.35rem 0 0}.section-heading{display:flex;justify-content:space-between;gap:1rem;align-items:baseline;margin-bottom:.8rem}
label{display:block;font-weight:700;margin:.85rem 0 .35rem}input,textarea,button{font:inherit}input[type=text],input[type=password],input[type=url],input:not([type]),textarea{width:100%;padding:.72rem .8rem;border:1px solid #b9c2b8;border-radius:7px;background:#fff}textarea{resize:vertical;min-height:7rem}
input[type=text].password-visible{padding-right:.8rem}.password-control{display:grid;grid-template-columns:minmax(0,1fr) auto;gap:.45rem;align-items:center}.password-control input{min-width:0}.password-toggle{background:#fff;color:#24445f;border-color:#cdd7d0;min-width:4.5rem}.auth-submit{margin-top:1.15rem}.auth-form{margin-top:.35rem}
input:focus,textarea:focus,button:focus-visible,a:focus-visible{outline:3px solid #93c5fd;outline-offset:2px}button,.primary{border:1px solid #163b2f;background:#163b2f;color:#fff;border-radius:7px;padding:.5rem .8rem;cursor:pointer;font-weight:700}button:hover,.primary:hover{background:#235544;text-decoration:none}.actions button{background:#fff;color:#24445f;border-color:#cdd7d0;font-weight:650}.actions button:hover{background:#eef3f0}
.composer-tools{display:flex;align-items:center;justify-content:space-between;gap:.75rem;margin-top:.85rem}.file-control{display:inline-flex;align-items:center;gap:.6rem;margin:0;color:#24445f;font-weight:700}.file-control input{max-width:15rem}
.timeline{display:grid;gap:.85rem}.post{overflow:hidden}.post-header{display:flex;justify-content:space-between;gap:.75rem;align-items:flex-start}.author-name{font-weight:800;color:#202124}.username,.post-time,.counts{color:#687068;font-size:.92rem}.text{white-space:pre-wrap;margin:.75rem 0;line-height:1.55;overflow-wrap:anywhere}.post img,.post video{display:block;max-width:100%;border-radius:8px;border:1px solid #d9ded6;margin-top:.6rem;background:#f6f7f4}
.counts{display:flex;gap:.8rem;flex-wrap:wrap;margin-top:.4rem}.actions{display:flex;gap:.45rem;flex-wrap:wrap;align-items:center;margin-top:.75rem}.repost-banner{color:#4b655d;font-size:.9rem;font-weight:800;margin-bottom:.45rem}.unavailable{color:#667064}.empty-state{text-align:center;padding:2rem 1rem}.empty-state h2{margin:0;font-size:1.2rem}.notice.error,.error-panel{border-color:#e6b8a8;background:#fff8f5}.notice.success{border-color:#add7b4;background:#f4fbf5}.eyebrow{text-transform:uppercase;letter-spacing:.08em;font-weight:800;color:#6d766e;font-size:.78rem}
.profile-banner{width:100%;max-height:220px;object-fit:cover;border-radius:8px;border:1px solid #d9ded6;background:#dfe9e1}.profile-heading{display:flex;gap:1rem;align-items:flex-start;margin-top:.85rem}.profile-picture{width:88px;height:88px;object-fit:cover;border-radius:8px;border:1px solid #d9ded6;background:#eef3f0;flex:0 0 auto}.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(180px,1fr));gap:.85rem}table{width:100%;border-collapse:collapse}td,th{border-bottom:1px solid #e3e7e0;text-align:left;padding:.55rem;vertical-align:top}pre{white-space:pre-wrap;overflow:auto;max-width:100%}
@media (max-width:900px){.content-shell{grid-template-columns:1fr}.side-panel{position:static;display:none}}
@media (max-width:600px){main{padding:.75rem}.header-inner{align-items:flex-start;flex-direction:column}.site-header{position:static}nav{justify-content:flex-start}.composer-tools,.post-header,.profile-heading{align-items:stretch;flex-direction:column}.file-control{display:block}.file-control input{display:block;max-width:100%;margin-top:.35rem}.actions button,.button-link{padding:.42rem .55rem}.counts{gap:.55rem}.page-header h1,.section-heading h1,.panel h1{font-size:1.25rem}}
"#;
