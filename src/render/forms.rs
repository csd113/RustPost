//! Forms for the render subsystem.

use super::{
    AccountView, CLIENT_BOOT_SCRIPT, CLIENT_SCRIPT, CurrentUser, EmptyState, FollowButtonState,
    OnboardingPage, PostRenderOptions, PostView, QuotePreview, SearchRenderOptions, empty_state,
    icon_svg, notice, posts_with_options, quote_preview_card,
};

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

pub(super) fn register_captcha_fields(
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

pub(super) fn password_requirement_hint(min_password_length: usize) -> String {
    if min_password_length == 0 {
        "No minimum password length is currently required.".to_owned()
    } else if min_password_length == 1 {
        "Password must be at least 1 character.".to_owned()
    } else {
        format!("Password must be at least {min_password_length} characters.")
    }
}

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
            let action = follow_form(
                account.id,
                csrf,
                if account.viewer_following {
                    FollowButtonState::Following
                } else {
                    FollowButtonState::Follow
                },
            );
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

pub(super) fn search_form(site_name: &str, query: &str) -> String {
    format!(
        r#"<section class="panel search-panel"><h1>Search</h1><form method="get" action="/search" class="search-form"><label class="sr-only" for="q">Search {}</label><input id="q" name="q" type="search" value="{}" placeholder="Search posts, @users, or #tags" autocomplete="off"><button class="primary" type="submit">Search</button></form></section>"#,
        html_escape::encode_text(site_name),
        html_escape::encode_double_quoted_attribute(query)
    )
}

pub(super) fn search_results(
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

pub(super) fn search_user_results(users: &[AccountView]) -> String {
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

pub fn follow_form(user_id: i64, csrf: &str, state: FollowButtonState) -> String {
    let (action, label, aria_label, active) = match state {
        FollowButtonState::Following => (
            format!("/users/{user_id}/unfollow"),
            "Following",
            "Unfollow this account",
            true,
        ),
        FollowButtonState::Requested => (
            format!("/users/{user_id}/follow/cancel"),
            "Requested",
            "Cancel follow request",
            false,
        ),
        FollowButtonState::Follow => (
            format!("/users/{user_id}/follow"),
            "Follow",
            "Follow this account",
            false,
        ),
    };
    format!(
        r#"<form method="post" action="{}" data-enhance="follow" data-follow-user="{}"><input type="hidden" name="csrf" value="{}"><button class="follow-button{}" type="submit" aria-pressed="{}" aria-label="{}" title="{}">{}</button></form>"#,
        html_escape::encode_double_quoted_attribute(&action),
        user_id,
        html_escape::encode_double_quoted_attribute(csrf),
        if active { " active" } else { "" },
        if active { "true" } else { "false" },
        html_escape::encode_double_quoted_attribute(aria_label),
        html_escape::encode_double_quoted_attribute(aria_label),
        html_escape::encode_text(label)
    )
}
