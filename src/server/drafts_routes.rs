//! Owner-only draft editing, poll ballots and media processing controls.

use super::*;

fn bad(error: impl std::fmt::Display) -> AppError {
    AppError::BadRequest(error.to_string())
}
fn esc(value: &str) -> String {
    html_escape::encode_double_quoted_attribute(value).into_owned()
}

pub(super) fn poll_spec(form: &ParsedPostCreate) -> AppResult<Option<social::polls::PollSpec>> {
    let closes = social::drafts::parse_utc(&form.poll_closes_at).map_err(bad)?;
    social::polls::PollSpec::parse(&form.poll_options, form.poll_multiple, closes).map_err(bad)
}

pub(super) async fn set_new_descriptions(
    pool: &SqlitePool,
    owner: Option<i64>,
    descriptions: Vec<(i64, String)>,
) -> AppResult<()> {
    pool.call(move |conn| {
        let tx=conn.transaction()?;
        for (id,text) in descriptions {
            anyhow::ensure!(text.chars().count()<=1000,"image descriptions are limited to 1000 characters");
            let changed=tx.execute("UPDATE media SET alt_text=? WHERE id=? AND owner_user_id IS ? AND NOT EXISTS(SELECT 1 FROM post_media WHERE media_id=?) AND NOT EXISTS(SELECT 1 FROM draft_media WHERE media_id=?)",params![text,id,owner,id,id])?;
            anyhow::ensure!(changed==1,"new attachment not found");
        }
        tx.commit()?;Ok(())
    }).await.map_err(bad)
}

fn input(form: &ParsedPostCreate, id: Option<i64>) -> AppResult<social::drafts::DraftInput> {
    let at = match form.intent.as_str() {
        "draft" => None,
        "publish" => Some(chrono::Utc::now().timestamp()),
        "schedule" => {
            let at = social::drafts::parse_utc(&form.scheduled_at)
                .map_err(bad)?
                .ok_or_else(|| bad("choose a publication time in UTC"))?;
            if at <= chrono::Utc::now().timestamp() {
                return Err(bad("scheduled time must be in the future"));
            }
            Some(at)
        }
        _ => return Err(bad("invalid draft action")),
    };
    let mut media_ids = form.retained_media.clone();
    media_ids.extend_from_slice(&form.media_ids);
    let mut descriptions = form
        .media_ids
        .iter()
        .enumerate()
        .map(|(i, id)| (*id, form.descriptions.get(&i).cloned().unwrap_or_default()))
        .collect::<Vec<_>>();
    descriptions.extend(
        form.existing_descriptions
            .iter()
            .map(|(id, text)| (*id, text.clone())),
    );
    Ok(social::drafts::DraftInput {
        id,
        revision: form.revision,
        client_key: form.post_key.clone(),
        text: form.text.clone(),
        parent_post_id: form.parent_post_id,
        media_ids,
        descriptions,
        scheduled_at: at,
        poll_options: form.poll_options.clone(),
        poll_multiple: form.poll_multiple,
        poll_closes_at: social::drafts::parse_utc(&form.poll_closes_at).map_err(bad)?,
    })
}

pub(super) async fn save_new(
    state: &AppState,
    headers: &HeaderMap,
    user: Option<&CurrentUser>,
    form: &ParsedPostCreate,
) -> AppResult<Response> {
    let user = user.ok_or(AppError::Unauthorized)?;
    validate_csrf(&state.pool, headers, &form.csrf_token).await?;
    if !form.post_key.is_empty()
        && let Some(id) = social::drafts::id_for_key(&state.pool, user.id, &form.post_key)
            .await
            .map_err(bad)?
    {
        cleanup_unattached(state, &form.media_ids).await;
        return Ok(Redirect::to(&format!("/drafts/{id}")).into_response());
    }
    if !form.retained_media.is_empty() {
        return Err(bad("new drafts cannot retain existing attachments"));
    }
    if form.is_nsfw {
        media::set_media_nsfw(&state.pool, &form.media_ids, true).await?;
    }
    let id = social::drafts::save(&state.pool, &state.settings, user.id, input(form, None)?)
        .await
        .map_err(bad)?;
    cleanup_unattached(state, &form.media_ids).await;
    Ok(Redirect::to(&format!("/drafts/{id}")).into_response())
}

pub(super) async fn index(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AppResult<Html<String>> {
    let user = require_user(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers).await?;
    let drafts = social::drafts::list(&state.pool, user.id).await?;
    let mut body = render::page_header(
        "Drafts and scheduled posts",
        "Saved to your account across devices. Times are UTC. Publishing resumes when the server is running; maintenance pauses publication.",
    );
    body.push_str(r#"<p><a class="button-link" href="/home#reply">Write a post</a></p>"#);
    for draft in drafts {
        let due = draft
            .scheduled_at
            .map_or_else(String::new, |t| format!(" · {} UTC", time_value(Some(t))));
        let _ = write!(
            body,
            r#"<article class="panel"><h2><a href="/drafts/{}">Draft {}</a></h2><p>{}{}</p><p>{}</p><p>{}</p></article>"#,
            draft.id,
            draft.id,
            esc(&draft.status),
            esc(&due),
            esc(&draft.text.chars().take(300).collect::<String>()),
            esc(&draft.error)
        );
    }
    Ok(Html(
        page_layout(&state, Some(&user), csrf.as_deref(), "Drafts", &body).await?,
    ))
}

async fn owned(state: &AppState, user_id: i64, id: i64) -> AppResult<social::drafts::Draft> {
    social::drafts::list(&state.pool, user_id)
        .await?
        .into_iter()
        .find(|d| d.id == id)
        .ok_or(AppError::NotFound)
}

fn time_value(time: Option<i64>) -> String {
    time.and_then(|t| chrono::DateTime::from_timestamp(t, 0))
        .map_or_else(String::new, |d| d.format("%Y-%m-%dT%H:%M").to_string())
}

pub(super) async fn edit(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    let user = require_user(&state, &headers).await?;
    let owner = user.id;
    let published = state
        .pool
        .call(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT published_post_id FROM drafts WHERE id=? AND user_id=?",
                    params![id, owner],
                    |r| r.get::<_, Option<i64>>(0),
                )
                .optional()?
                .flatten())
        })
        .await?;
    if let Some(post) = published {
        return Ok(Redirect::to(&format!("/posts/{post}")).into_response());
    }
    let draft = owned(&state, user.id, id).await?;
    let csrf = form_csrf(&state, &headers)
        .await?
        .ok_or(AppError::Unauthorized)?;
    let mut attachments = String::new();
    for item in &draft.media {
        let image_attr = if item.media_kind == "image" {
            " data-retained-image"
        } else {
            ""
        };
        let preview = if item.media_kind == "image" {
            format!(
                r#"<img src="{}" alt="{}" width="200" loading="lazy">"#,
                esc(&item.public_path),
                esc(&item.alt_text)
            )
        } else {
            format!(
                r#"<video src="{}" controls preload="metadata"></video>"#,
                esc(&item.public_path)
            )
        };
        let _ = write!(
            attachments,
            r#"<fieldset{image_attr}><legend>Attachment {}</legend>{preview}<label><input type="checkbox" name="retain_media" value="{}" checked> Keep attachment</label><label>Image description<textarea name="existing_alt_{}" maxlength="1000">{}</textarea></label><p>Processing: {}</p></fieldset>"#,
            item.id,
            item.id,
            item.id,
            esc(&item.alt_text),
            esc(&item.state)
        );
    }
    let parent = draft.parent_post_id.map_or_else(String::new, |id| {
        format!(r#"<input type="hidden" name="parent_post_id" value="{id}">"#)
    });
    let checked = if draft.poll_multiple { " checked" } else { "" };
    let csrf_attr = esc(&csrf);
    let body = format!(
        r#"<section class="panel"><h1>Edit draft</h1><p>Status: {}. {}</p><p><a href="/drafts">All drafts</a></p><form method="post" action="/drafts/{id}" enctype="multipart/form-data"><input type="hidden" name="csrf" value="{csrf_attr}"><input type="hidden" name="revision" value="{}">{parent}<label>Post text<textarea id="draft-text" name="text" rows="5" data-character-limit="{}">{}</textarea></label>{attachments}<label>Attach more media<input name="media" type="file" multiple accept="image/*,video/mp4,video/webm,video/quicktime" data-composer-media></label><div data-image-descriptions></div><label><input type="checkbox" data-alt-reminder> Remind me if an image has no description</label><noscript><label>New attachment descriptions, one per line in selection order<textarea name="alt_lines"></textarea></label></noscript><label><input type="checkbox" name="nsfw" value="1"> Mark new media as NSFW</label><label>Publish at (UTC)<input type="datetime-local" name="scheduled_at" value="{}"></label><label>Poll choices (2–6, one per line)<textarea name="poll_options" rows="3" maxlength="606">{}</textarea></label><label><input type="checkbox" name="poll_multiple" value="1"{checked}> Allow multiple choices</label><label>Poll closes at (UTC)<input type="datetime-local" name="poll_closes_at" value="{}"></label><button type="submit" name="intent" value="draft">Save draft</button> <button type="submit" name="intent" value="schedule">Schedule</button> <button type="submit" name="intent" value="publish">Publish now</button></form><form method="post" action="/drafts/{id}/cancel"><input type="hidden" name="csrf" value="{csrf_attr}"><input type="hidden" name="revision" value="{}"><button>Cancel scheduled publication</button></form><form method="post" action="/drafts/{id}/delete"><input type="hidden" name="csrf" value="{csrf_attr}"><input type="hidden" name="revision" value="{}"><button>Delete draft</button></form></section>"#,
        esc(&draft.status),
        esc(&draft.error),
        draft.revision,
        state.settings.posts.max_text_chars,
        esc(&draft.text),
        esc(&time_value(draft.scheduled_at)),
        esc(&draft.poll_options),
        esc(&time_value(draft.poll_closes_at)),
        draft.revision,
        draft.revision
    );
    Ok(
        Html(page_layout(&state, Some(&user), Some(&csrf), "Edit draft", &body).await?)
            .into_response(),
    )
}

pub(super) async fn update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    multipart: Multipart,
) -> AppResult<Response> {
    let user = require_active_user(&state, &headers).await?;
    let previous = owned(&state, user.id, id).await?;
    let mut form = posting::parse_post_create(&state, &headers, Some(user.id), multipart).await?;
    validate_csrf(&state.pool, &headers, &form.csrf_token).await?;
    if form
        .retained_media
        .iter()
        .any(|id| !previous.media.iter().any(|m| m.id == *id))
    {
        return Err(bad("attachment does not belong to this draft"));
    }
    if form.is_nsfw {
        media::set_media_nsfw(&state.pool, &form.media_ids, true).await?;
    }
    let input = input(&form, Some(id))?;
    social::drafts::save(&state.pool, &state.settings, user.id, input)
        .await
        .map_err(bad)?;
    // The draft now durably owns these uploads, even if rendering or publication later fails.
    form.pending_uploads.commit();
    for item in previous.media {
        if !form.retained_media.contains(&item.id) {
            posting::cleanup_post_uploads(&state, &[item.id]).await;
        }
    }
    if form.intent == "publish" {
        let id = social::drafts::publish(&state.pool, &state.settings, id)
            .await
            .map_err(bad)?
            .ok_or_else(|| bad("draft was changed; reload to see its state"))?;
        return Ok(Redirect::to(&format!("/posts/{id}")).into_response());
    }
    Ok(Redirect::to(&format!("/drafts/{id}")).into_response())
}

#[derive(Deserialize)]
pub(super) struct RevisionForm {
    csrf: String,
    revision: i64,
}

async fn cancel_or_delete(
    state: &AppState,
    headers: &HeaderMap,
    id: i64,
    form: RevisionForm,
    delete: bool,
) -> AppResult<Response> {
    let user = require_user(state, headers).await?;
    validate_csrf(&state.pool, headers, &form.csrf).await?;
    let media = social::drafts::cancel(&state.pool, user.id, id, form.revision, delete)
        .await
        .map_err(bad)?;
    posting::cleanup_post_uploads(state, &media).await;
    Ok(Redirect::to("/drafts").into_response())
}
pub(super) async fn cancel(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<RevisionForm>,
) -> AppResult<Response> {
    cancel_or_delete(&state, &headers, id, form, false).await
}
pub(super) async fn delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<RevisionForm>,
) -> AppResult<Response> {
    cancel_or_delete(&state, &headers, id, form, true).await
}

pub(super) async fn vote(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(fields): Form<Vec<(String, String)>>,
) -> AppResult<Response> {
    let user = require_active_user(&state, &headers).await?;
    let tokens = fields
        .iter()
        .filter(|(key, _)| key == "csrf")
        .map(|(_, value)| value)
        .collect::<Vec<_>>();
    if tokens.len() != 1 {
        return Err(AppError::Csrf);
    }
    validate_csrf(&state.pool, &headers, tokens[0]).await?;
    let choices = fields
        .iter()
        .filter(|(key, _)| key == "choice")
        .map(|(_, value)| value.parse::<i64>().map_err(bad))
        .collect::<AppResult<Vec<_>>>()?;
    social::polls::vote(&state.pool, user.id, id, choices)
        .await
        .map_err(bad)?;
    Ok(Redirect::to(&format!("/posts/{id}")).into_response())
}

pub(super) async fn close_poll(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> AppResult<Response> {
    let user = require_active_user(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    social::polls::close(&state.pool, user.id, id)
        .await
        .map_err(bad)?;
    Ok(Redirect::to(&format!("/posts/{id}")).into_response())
}

pub(super) async fn retry_media(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> AppResult<Response> {
    let user = require_active_user(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    media::jobs::retry(&state.pool, user.id, id)
        .await
        .map_err(bad)?;
    Ok(Redirect::to("/media/processing").into_response())
}

pub(super) async fn media_jobs(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AppResult<Html<String>> {
    let user = require_user(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers)
        .await?
        .ok_or(AppError::Unauthorized)?;
    let owner = user.id;
    let jobs=state.pool.call(move |conn| {
        let mut statement=conn.prepare("SELECT m.id,m.original_filename,w.status,w.attempts,w.error FROM media m JOIN media_work w ON w.media_id=COALESCE(m.canonical_media_id,m.id) WHERE m.owner_user_id=? ORDER BY w.status='done',m.id DESC LIMIT 256")?;
        Ok(statement.query_map([owner],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(3)?,r.get::<_,String>(4)?)))?.collect::<Result<Vec<_>,_>>()?)
    }).await?;
    let mut body = render::page_header(
        "Media processing",
        "Original uploads remain available while conversion runs. One worker, at most 128 pending conversions. Up to three automatic attempts; explicit retries stop after nine total attempts.",
    );
    let owner = user.id;
    let used = state
        .pool
        .call(move |conn| {
            Ok(conn.query_row(
                "SELECT COALESCE(SUM(byte_len),0) FROM media WHERE owner_user_id=?",
                [owner],
                |r| r.get::<_, i64>(0),
            )?)
        })
        .await?;
    let quota = if state.settings.storage.max_user_upload_bytes == 0 {
        "unlimited".to_owned()
    } else {
        backups::format_bytes(state.settings.storage.max_user_upload_bytes)
    };
    let _ = write!(
        body,
        "<p>Your uploaded bytes: {}. Active account quota: {quota}.</p>",
        backups::format_bytes(u64::try_from(used).map_err(bad)?)
    );
    for (id, name, status, attempts, error) in jobs {
        let retry = if status == "failed" && attempts < 9 {
            format!(
                r#"<form method="post" action="/media/{id}/retry"><input type="hidden" name="csrf" value="{}"><button>Retry conversion</button></form>"#,
                esc(&csrf)
            )
        } else {
            String::new()
        };
        let _ = write!(
            body,
            r#"<article class="panel"><h2>{}</h2><p>{} · {} attempts</p><p>{}</p>{retry}</article>"#,
            esc(&name),
            esc(&status),
            attempts,
            esc(&error)
        );
    }
    Ok(Html(
        page_layout(&state, Some(&user), Some(&csrf), "Media processing", &body).await?,
    ))
}

/// Draft-only media URLs require their owner's session. Public deduplicated files retain public access.
pub(super) async fn private_media_guard(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
    next: middleware::Next,
) -> Response {
    if !request.uri().path().starts_with("/uploads/") {
        return next.run(request).await;
    }
    let Ok(path) = normalized_upload_path(request.uri().path()) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let access=state.pool.call(move |conn| {
        let public:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM media m WHERE ? IN (m.public_path,m.original_public_path,m.thumbnail_public_path) AND (EXISTS(SELECT 1 FROM post_media pm JOIN posts p ON p.id=pm.post_id WHERE pm.media_id=m.id AND p.is_deleted=0) OR EXISTS(SELECT 1 FROM users WHERE profile_picture_media_id=m.id OR banner_media_id=m.id)))",[&path],|r|r.get(0))?;
        if public{return Ok(None)}
        let mut statement=conn.prepare("SELECT DISTINCT d.user_id FROM drafts d JOIN draft_media dm ON dm.draft_id=d.id JOIN media m ON m.id=dm.media_id WHERE ? IN (m.public_path,m.original_public_path,m.thumbnail_public_path) UNION SELECT r.user_id FROM recoverable_deletions r JOIN post_media pm ON pm.post_id=r.post_id JOIN media m ON m.id=pm.media_id WHERE ? IN (m.public_path,m.original_public_path,m.thumbnail_public_path) AND r.expires_at>unixepoch() UNION SELECT -1 FROM post_media pm JOIN posts p ON p.id=pm.post_id JOIN media m ON m.id=pm.media_id WHERE p.is_deleted=1 AND ? IN (m.public_path,m.original_public_path,m.thumbnail_public_path) UNION SELECT m.owner_user_id FROM media m WHERE m.owner_user_id IS NOT NULL AND ? IN (m.public_path,m.original_public_path,m.thumbnail_public_path) AND NOT EXISTS(SELECT 1 FROM post_media pm JOIN posts p ON p.id=pm.post_id WHERE pm.media_id=m.id AND p.is_deleted=1)")?;
        Ok(Some(statement.query_map(params![path,path,path,path],|r|r.get::<_,i64>(0))?.collect::<Result<Vec<_>,_>>()?))
    }).await;
    match access {
        Ok(None) => next.run(request).await,
        Ok(Some(owners)) => match current(&state, request.headers()).await {
            Ok(Some(user)) if owners.contains(&user.id) => next.run(request).await,
            _ => StatusCode::NOT_FOUND.into_response(),
        },
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

fn normalized_upload_path(path: &str) -> Result<String, ()> {
    let mut decoded = Vec::new();
    let mut bytes = path.as_bytes().iter().copied();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let a = bytes
                .next()
                .and_then(|b| char::from(b).to_digit(16))
                .ok_or(())?;
            let b = bytes
                .next()
                .and_then(|b| char::from(b).to_digit(16))
                .ok_or(())?;
            decoded.push(u8::try_from(a * 16 + b).map_err(|_| ())?);
        } else {
            decoded.push(byte);
        }
    }
    let path = String::from_utf8(decoded).map_err(|_| ())?;
    if path.contains('\\')
        || path.contains('\0')
        || path.split('/').any(|s| matches!(s, "." | ".."))
    {
        return Err(());
    }
    Ok(format!(
        "/{}",
        path.split('/')
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("/")
    ))
}

pub(super) async fn cleanup_unattached(state: &AppState, ids: &[i64]) {
    for id in ids {
        let media_id = *id;
        let attached=state.pool.call(move |conn|Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM post_media WHERE media_id=? UNION ALL SELECT 1 FROM draft_media WHERE media_id=? UNION ALL SELECT 1 FROM users WHERE profile_picture_media_id=? OR banner_media_id=?)",params![media_id,media_id,media_id,media_id],|r|r.get::<_,bool>(0))?)).await.unwrap_or(true);
        if !attached {
            posting::cleanup_post_uploads(state, &[media_id]).await;
        }
    }
}

pub(super) async fn deleted_posts(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AppResult<Html<String>> {
    let user = require_user(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers)
        .await?
        .ok_or(AppError::Unauthorized)?;
    let owner = user.id;
    let rows=state.pool.call(move |conn| {
        let mut statement=conn.prepare("SELECT p.id,p.text,r.expires_at FROM recoverable_deletions r JOIN posts p ON p.id=r.post_id WHERE r.user_id=? AND p.user_id=? AND p.is_deleted=1 AND r.expires_at>unixepoch() ORDER BY r.expires_at LIMIT 200")?;
        Ok(statement.query_map(params![owner,owner],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?)))?.collect::<Result<Vec<_>,_>>()?)
    }).await?;
    let mut body = render::page_header(
        "Recently deleted posts",
        "Your own deletions can be undone during the configured recovery window. Administrator removals cannot be undone here. Expired media cleanup resumes when the server runs.",
    );
    if rows.is_empty() {
        body.push_str("<p>No posts are currently recoverable.</p>");
    }
    for (id, text, expires) in rows {
        let _ = write!(
            body,
            r#"<article class="panel"><blockquote>{}</blockquote><p>Undo before {} UTC.</p><form method="post" action="/posts/{id}/restore"><input type="hidden" name="csrf" value="{}"><button>Undo deletion</button></form></article>"#,
            esc(&text),
            esc(&chrono::DateTime::from_timestamp(expires, 0)
                .map_or_else(String::new, |t| t.format("%Y-%m-%d %H:%M:%S").to_string())),
            esc(&csrf)
        );
    }
    Ok(Html(
        page_layout(&state, Some(&user), Some(&csrf), "Recently deleted", &body).await?,
    ))
}

pub(super) async fn restore_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> AppResult<Response> {
    let user = require_active_user(&state, &headers).await?;
    validate_csrf(&state.pool, &headers, &form.csrf).await?;
    social::recovery::restore(&state.pool, user.id, id)
        .await
        .map_err(bad)?;
    Ok(Redirect::to(&format!("/posts/{id}")).into_response())
}

pub(super) async fn admin_storage(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AppResult<Html<String>> {
    let user = require_admin(&state, &headers).await?;
    let csrf = form_csrf(&state, &headers).await?;
    let (used,reserved,owners)=state.pool.call(|conn| {
        let used=conn.query_row("SELECT COALESCE(SUM(byte_len),0) FROM media",[],|r|r.get::<_,i64>(0))?;
        let reserved=conn.query_row("SELECT COALESCE(SUM(bytes),0) FROM storage_reservations",[],|r|r.get::<_,i64>(0))?;
        let mut statement=conn.prepare("SELECT COALESCE(u.username,'Anonymous'),SUM(m.byte_len) FROM media m LEFT JOIN users u ON u.id=m.owner_user_id GROUP BY m.owner_user_id ORDER BY SUM(m.byte_len) DESC LIMIT 50")?;
        let owners=statement.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?)))?.collect::<Result<Vec<_>,_>>()?;
        Ok((u64::try_from(used)?,u64::try_from(reserved)?,owners))
    }).await?;
    let path = state.paths.data_dir.clone();
    let available =
        tokio::task::spawn_blocking(move || crate::storage_limits::available_space(&path))
            .await
            .map_err(|e| bad(e.to_string()))?;
    let free = available.map_or_else(
        |_| "Unavailable; review the filesystem and storage settings.".to_owned(),
        backups::format_bytes,
    );
    let quota = |value| {
        if value == 0 {
            "Unlimited".to_owned()
        } else {
            backups::format_bytes(value)
        }
    };
    let mut body = format!(
        r#"<section class="panel"><h1>Storage</h1><p>Quotas count uploaded bytes per media record, including drafts and duplicate uploads. They are separate from free disk space. Active limits are shown below; restart after changing storage settings.</p><dl><dt>Logical uploaded bytes</dt><dd>{}</dd><dt>Uploads in progress</dt><dd>{}</dd><dt>Available filesystem space</dt><dd>{free}</dd><dt>Required free headroom</dt><dd>{}</dd><dt>Instance upload quota</dt><dd>{}</dd><dt>Per-account upload quota</dt><dd>{}</dd></dl><p><a href="/admin/deep-settings">Change storage settings</a></p><h2>Largest accounts</h2>"#,
        backups::format_bytes(used),
        backups::format_bytes(reserved),
        backups::format_bytes(state.settings.storage.min_free_bytes),
        quota(state.settings.storage.max_upload_bytes),
        quota(state.settings.storage.max_user_upload_bytes)
    );
    for (name, bytes) in owners {
        let _ = write!(
            body,
            "<p>{}: {}</p>",
            esc(&name),
            backups::format_bytes(u64::try_from(bytes).map_err(bad)?)
        );
    }
    body.push_str("</section>");
    Ok(Html(
        page_layout(&state, Some(&user), csrf.as_deref(), "Storage", &body).await?,
    ))
}
