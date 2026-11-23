//! Exercise the production router and multipart ownership boundaries.

use super::*;
use base64::Engine as _;

fn png() -> anyhow::Result<Vec<u8>> {
    Ok(base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO/aY9sAAAAASUVORK5CYII=")?)
}

fn multipart(fields: &[(&str, String)], image: Option<&[u8]>) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, value) in fields {
        body.extend_from_slice(format!("--feature-boundary\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes());
    }
    if let Some(bytes) = image {
        body.extend_from_slice(b"--feature-boundary\r\nContent-Disposition: form-data; name=\"media\"; filename=\"feature.png\"\r\nContent-Type: image/png\r\n\r\n");
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(b"--feature-boundary--\r\n");
    body
}

const FORM: &str = "Content-Type: application/x-www-form-urlencoded\r\n";

impl Fixture {
    async fn count(&self, table: &'static str) -> anyhow::Result<i64> {
        self.state
            .pool
            .call(move |conn| {
                Ok(
                    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                        row.get(0)
                    })?,
                )
            })
            .await
    }
}

const MULTIPART: &str = "Content-Type: multipart/form-data; boundary=feature-boundary\r\n";

async fn raw(
    f: &Fixture,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    body: &[u8],
    headers: &str,
) -> anyhow::Result<(u16, Vec<u8>)> {
    let mut socket = tokio::net::TcpStream::connect(f.address).await?;
    let cookie = cookie.map_or_else(String::new, |c| format!("Cookie: rustpost_session={c}\r\n"));
    let request = format!(
        "{method} {path} HTTP/1.0\r\nHost: {}\r\n{cookie}Connection: close\r\nContent-Length: {}\r\n{headers}\r\n",
        f.address,
        body.len()
    );
    socket.write_all(request.as_bytes()).await?;
    socket.write_all(body).await?;
    let mut response = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        socket.read_to_end(&mut response),
    )
    .await??;
    let text = String::from_utf8_lossy(&response);
    let status = text
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("missing status"))?
        .parse()?;
    Ok((status, response))
}

#[tokio::test]
async fn multipart_retries_preserve_committed_media_and_do_not_charge_again() -> anyhow::Result<()>
{
    let f = Fixture::new(30, 1).await?;
    let bytes = png()?;
    let body = multipart(
        &[
            ("csrf", f.session.csrf_token.clone()),
            ("text", "retry-safe content".into()),
            ("post_key", "same-network-request".into()),
            ("alt_0", "Accessible caption".into()),
        ],
        Some(&bytes),
    );
    let headers = format!("{MULTIPART}Accept: application/json\r\nX-RustPost-Enhance: 1\r\n");
    let first = f.request("POST", "/posts", &body, &headers).await?;
    assert_eq!(first.0, 200, "{}", first.1);
    let second = f.request("POST", "/posts", &body, &headers).await?;
    assert_eq!(second.0, 200, "{}", second.1);
    assert_eq!(f.quota().await?, 1);
    f.state
        .pool
        .call(|conn| {
            assert_eq!(
                conn.query_row("SELECT COUNT(*) FROM posts", [], |r| r.get::<_, i64>(0))?,
                1
            );
            assert_eq!(
                conn.query_row("SELECT COUNT(*) FROM media", [], |r| r.get::<_, i64>(0))?,
                1
            );
            assert_eq!(
                conn.query_row("SELECT alt_text FROM media", [], |r| r.get::<_, String>(0))?,
                "Accessible caption"
            );
            Ok(())
        })
        .await?;
    let (_, page) = f.request("GET", "/posts/1", &[], "").await?;
    assert!(page.contains("Accessible caption"));
    assert_eq!(
        std::fs::read_dir(&f.state.paths.uploads_originals)?.count(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn drafts_are_private_even_through_encoded_media_urls_and_publish_normally()
-> anyhow::Result<()> {
    let f = Fixture::new(30, 10).await?;
    let bytes = png()?;
    let body = multipart(
        &[
            ("csrf", f.session.csrf_token.clone()),
            ("text", "private saved draft".into()),
            ("post_key", "draft-request".into()),
            ("intent", "draft".into()),
            ("alt_0", "Private caption".into()),
        ],
        Some(&bytes),
    );
    let result = f.request("POST", "/posts", &body, MULTIPART).await?;
    assert_eq!(result.0, 303, "{}", result.1);
    let draft = social::drafts::list(&f.state.pool, f.user_id)
        .await?
        .pop()
        .ok_or_else(|| anyhow::anyhow!("missing draft"))?;
    let file = &draft.media[0];
    assert_eq!(
        raw(&f, "GET", &file.public_path, None, &[], "").await?.0,
        404
    );
    let encoded = file.public_path.replace("/originals/", "/%6friginals/");
    assert_eq!(raw(&f, "GET", &encoded, None, &[], "").await?.0, 404);
    assert_eq!(
        raw(
            &f,
            "GET",
            &file.public_path,
            Some(&f.session.token),
            &[],
            ""
        )
        .await?
        .0,
        200
    );
    assert_eq!(raw(&f, "GET", "/drafts", None, &[], "").await?.0, 401);
    let attacker_body = multipart(
        &[
            ("csrf", "forged".into()),
            ("text", "forged changes".into()),
            ("revision", draft.revision.to_string()),
            ("intent", "publish".into()),
        ],
        Some(&bytes),
    );
    assert_eq!(
        f.request(
            "POST",
            &format!("/drafts/{}", draft.id),
            &attacker_body,
            MULTIPART
        )
        .await?
        .0,
        403
    );
    let alt_name = format!("existing_alt_{}", file.id);
    let publish = multipart(
        &[
            ("csrf", f.session.csrf_token.clone()),
            ("text", draft.text.clone()),
            ("revision", draft.revision.to_string()),
            ("intent", "publish".into()),
            ("retain_media", file.id.to_string()),
            (&alt_name, "Published caption".into()),
        ],
        None,
    );
    let result = f
        .request(
            "POST",
            &format!("/drafts/{}", draft.id),
            &publish,
            MULTIPART,
        )
        .await?;
    assert_eq!(result.0, 303, "{}", result.1);
    assert_eq!(
        raw(&f, "GET", &file.public_path, None, &[], "").await?.0,
        200
    );
    assert!(
        social::drafts::list(&f.state.pool, f.user_id)
            .await?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn poll_forms_accept_multiple_choices_and_enforce_csrf_and_close_time() -> anyhow::Result<()>
{
    let f = Fixture::new(30, 10).await?;
    let post = social::create_post_with_options(
        &f.state.pool,
        &f.state.settings,
        Some(f.user_id),
        "HTTP poll",
        None,
        &[],
        social::PostOptions {
            poll: social::polls::PollSpec::parse("A\nB\nC", true, None)?,
            ..Default::default()
        },
    )
    .await?;
    let view = social::history::thread_page(&f.state.pool, Some(f.user_id), post, None, None)
        .await?
        .ok_or_else(|| anyhow::anyhow!("missing post"))?
        .requested;
    let choices = view
        .poll
        .ok_or_else(|| anyhow::anyhow!("missing poll"))?
        .choices;
    let vote = format!(
        "csrf={}&choice={}&choice={}",
        f.session.csrf_token, choices[0].id, choices[1].id
    );
    let path = format!("/polls/{post}/vote");
    let first = f.form(&path, &vote, "").await?;
    assert_eq!(first.0, 303, "{}", first.1);
    assert_eq!(f.form(&path, &vote, "").await?.0, 303);
    assert_eq!(
        f.form(&path, &format!("csrf=forged&choice={}", choices[2].id), "")
            .await?
            .0,
        403
    );
    f.state
        .pool
        .call(|conn| {
            assert_eq!(
                conn.query_row("SELECT COUNT(*) FROM poll_votes", [], |r| r
                    .get::<_, i64>(0))?,
                2
            );
            Ok(())
        })
        .await?;
    let (_, page) = f.request("GET", &format!("/posts/{post}"), &[], "").await?;
    assert!(page.contains("operator can access vote records"));
    assert_eq!(
        f.form(
            &format!("/polls/{post}/close"),
            &format!("csrf={}", f.session.csrf_token),
            ""
        )
        .await?
        .0,
        303
    );
    assert_eq!(f.form(&path, &vote, "").await?.0, 400);
    Ok(())
}

#[tokio::test]
async fn bounded_media_queue_rejects_cleanly_and_releases_upload_reservation() -> anyhow::Result<()>
{
    let mut settings = Settings::default();
    settings.storage.min_free_bytes = 0;
    let f = Fixture::configured(
        settings,
        FfmpegStatus {
            available: true,
            version: "fixture".into(),
            supports_webp: true,
            supports_vp9: true,
            error: None,
        },
    )
    .await?;
    f.state.pool.call(|conn| {
        for _ in 0..media::jobs::QUEUE_LIMIT {
            conn.execute("INSERT INTO media(original_filename,stored_path,public_path,mime_type,media_kind,byte_len,conversion_state) VALUES('queued','/unused','/uploads/originals/unused','image/png','image',0,'queued')",[])?;
            conn.execute("INSERT INTO media_work(media_id,status) VALUES(?,'queued')",[conn.last_insert_rowid()])?;
        }Ok(())
    }).await?;
    let body = multipart(
        &[
            ("csrf", f.session.csrf_token.clone()),
            ("text", "queue full".into()),
            ("post_key", "queue-full".into()),
        ],
        Some(&png()?),
    );
    let result = f.request("POST", "/posts", &body, MULTIPART).await?;
    assert_eq!(result.0, 400, "{}", result.1);
    for _ in 0..100 {
        let reserved = f
            .state
            .pool
            .call(|conn| {
                Ok(
                    conn.query_row("SELECT COUNT(*) FROM storage_reservations", [], |r| {
                        r.get::<_, i64>(0)
                    })?,
                )
            })
            .await?;
        if reserved == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    f.state
        .pool
        .call(|conn| {
            assert_eq!(
                conn.query_row("SELECT COUNT(*) FROM media", [], |r| r.get::<_, i64>(0))?,
                media::jobs::QUEUE_LIMIT
            );
            assert_eq!(
                conn.query_row("SELECT COUNT(*) FROM storage_reservations", [], |r| r
                    .get::<_, i64>(0))?,
                0
            );
            assert_eq!(
                conn.query_row("SELECT COUNT(*) FROM posts", [], |r| r.get::<_, i64>(0))?,
                0
            );
            Ok(())
        })
        .await?;
    assert_eq!(
        std::fs::read_dir(&f.state.paths.uploads_originals)?.count(),
        0
    );
    Ok(())
}

#[tokio::test]
async fn health_liveness_survives_snapshot_pause_and_readiness_reports_busy() -> anyhow::Result<()>
{
    let f = Fixture::new(30, 10).await?;
    assert_eq!(raw(&f, "GET", "/health/live", None, &[], "").await?.0, 200);
    assert_eq!(raw(&f, "GET", "/health/ready", None, &[], "").await?.0, 200);
    let snapshot = crate::runtime::snapshot_lease(&f.state.paths, true)?;
    let live = raw(&f, "GET", "/health/live", None, &[], "").await?;
    assert_eq!(live.0, 200);
    assert_eq!(raw(&f, "GET", "/health/ready", None, &[], "").await?.0, 503);
    drop(snapshot);
    assert_eq!(raw(&f, "GET", "/health/ready", None, &[], "").await?.0, 200);
    assert!(
        !String::from_utf8_lossy(&live.1)
            .contains(&f.state.paths.data_dir.to_string_lossy().to_string())
    );
    Ok(())
}

#[tokio::test]
async fn following_switch_and_deep_thread_pages_work_through_router() -> anyhow::Result<()> {
    let f = Fixture::new(30, 10).await?;
    let owner = f.user_id;
    let deep=f.state.pool.call(move |conn| {
        conn.execute("INSERT INTO users(username,normalized_username,password_hash) VALUES('friend','friend','unused')",[])?;let friend=conn.last_insert_rowid();
        conn.execute("INSERT INTO users(username,normalized_username,password_hash) VALUES('stranger','stranger','unused')",[])?;let stranger=conn.last_insert_rowid();
        conn.execute("INSERT INTO follows(follower_id,followed_id) VALUES(?,?)",params![owner,friend])?;
        for (user,text) in [(owner,"own feed item"),(friend,"friend feed item"),(stranger,"stranger feed item")] {conn.execute("INSERT INTO posts(user_id,text) VALUES(?,?)",params![user,text])?;}
        for _ in 0..225 {conn.execute("INSERT INTO posts(user_id,text,parent_post_id,root_post_id) VALUES(?,'deep reply',1,1)",[owner])?;}
        Ok(conn.last_insert_rowid())
    }).await?;
    let (status, all) = f.request("GET", "/home?feed=everyone", &[], "").await?;
    assert_eq!(status, 200);
    assert!(all.contains("stranger feed item"));
    let (status, following) = f.request("GET", "/home?feed=following", &[], "").await?;
    assert_eq!(status, 200);
    assert!(following.contains("own feed item"));
    assert!(following.contains("friend feed item"));
    assert!(!following.contains("stranger feed item"));
    let (status, page) = f.request("GET", &format!("/posts/{deep}"), &[], "").await?;
    assert_eq!(status, 200);
    assert!(page.contains("Earlier replies"));
    assert!(page.contains("deep reply"));
    Ok(())
}

#[tokio::test]
async fn undo_is_owner_bound_and_retained_media_is_not_public_while_deleted() -> anyhow::Result<()>
{
    let f = Fixture::new(30, 10).await?;
    let body = multipart(
        &[
            ("csrf", f.session.csrf_token.clone()),
            ("text", "undo through HTTP".into()),
            ("post_key", "undo-http".into()),
        ],
        Some(&png()?),
    );
    assert_eq!(f.request("POST", "/posts", &body, MULTIPART).await?.0, 303);
    let file = f
        .state
        .pool
        .call(|conn| {
            Ok(conn.query_row("SELECT public_path FROM media", [], |r| {
                r.get::<_, String>(0)
            })?)
        })
        .await?;
    let deleted = f
        .form(
            "/posts/1/delete",
            &format!("csrf={}&return_to=%2Fhome", f.session.csrf_token),
            "",
        )
        .await?;
    assert_eq!(deleted.0, 303);
    assert!(deleted.1.contains("/settings/deleted-posts"));
    assert_eq!(raw(&f, "GET", &file, None, &[], "").await?.0, 404);
    assert_eq!(f.form("/posts/1/restore", "csrf=forged", "").await?.0, 403);
    assert_eq!(
        f.form(
            "/posts/1/restore",
            &format!("csrf={}", f.session.csrf_token),
            ""
        )
        .await?
        .0,
        303
    );
    assert_eq!(raw(&f, "GET", &file, None, &[], "").await?.0, 200);
    Ok(())
}

async fn attachment_draft(f: &Fixture, key: &str) -> anyhow::Result<social::drafts::Draft> {
    let body = multipart(
        &[
            ("csrf", f.session.csrf_token.clone()),
            ("post_key", key.to_owned()),
            ("text", key.to_owned()),
            ("intent", "draft".into()),
            ("alt_0", "Original caption".into()),
        ],
        Some(&png()?),
    );
    let response = f.request("POST", "/posts", &body, MULTIPART).await?;
    assert_eq!(response.0, 303, "{}", response.1);
    social::drafts::list(&f.state.pool, f.user_id)
        .await?
        .into_iter()
        .find(|draft| draft.text == key)
        .ok_or_else(|| anyhow::anyhow!("draft not found"))
}

#[tokio::test]
async fn unchecked_draft_attachments_can_be_saved_scheduled_or_published() -> anyhow::Result<()> {
    let f = Fixture::new(30, 10).await?;
    for intent in ["draft", "schedule", "publish"] {
        let draft = attachment_draft(&f, intent).await?;
        let removed = &draft.media[0];
        let field = format!("existing_alt_{}", removed.id);
        let body = multipart(
            &[
                ("csrf", f.session.csrf_token.clone()),
                ("text", format!("Removed attachment: {intent}")),
                ("revision", draft.revision.to_string()),
                ("intent", intent.into()),
                (
                    "scheduled_at",
                    (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339(),
                ),
                (
                    &field,
                    "The native form still submits this unchecked attachment's description".into(),
                ),
            ],
            None,
        );
        let result = f
            .request("POST", &format!("/drafts/{}", draft.id), &body, MULTIPART)
            .await?;
        assert_eq!(result.0, 303, "{intent}: {}", result.1);
        assert_eq!(f.count("media").await?, 0);
        assert_eq!(
            std::fs::read_dir(&f.state.paths.uploads_originals)?.count(),
            0
        );
        assert_eq!(
            raw(
                &f,
                "GET",
                &removed.public_path,
                Some(&f.session.token),
                &[],
                ""
            )
            .await?
            .0,
            404
        );
        if intent != "publish" {
            let current = social::drafts::list(&f.state.pool, f.user_id)
                .await?
                .into_iter()
                .find(|item| item.id == draft.id)
                .ok_or_else(|| anyhow::anyhow!("missing edited draft"))?;
            assert!(current.media.is_empty());
            assert_eq!(
                current.status,
                if intent == "schedule" {
                    "scheduled"
                } else {
                    "draft"
                }
            );
        }
    }
    assert_eq!(f.count("posts").await?, 1);
    Ok(())
}

#[tokio::test]
async fn replacing_draft_media_preserves_shared_bytes_and_rejects_stale_or_foreign_fields()
-> anyhow::Result<()> {
    let f = Fixture::new(30, 10).await?;
    let draft = attachment_draft(&f, "replacement").await?;
    let original = &draft.media[0];
    let description = format!("existing_alt_{}", original.id);
    let fields = [
        ("csrf", f.session.csrf_token.clone()),
        ("text", draft.text.clone()),
        ("revision", draft.revision.to_string()),
        ("intent", "draft".into()),
        (&description, "Removed caption".into()),
        ("alt_0", "Replacement caption".into()),
    ];
    let response = f
        .request(
            "POST",
            &format!("/drafts/{}", draft.id),
            &multipart(&fields, Some(&png()?)),
            MULTIPART,
        )
        .await?;
    assert_eq!(response.0, 303, "{}", response.1);
    let current = social::drafts::list(&f.state.pool, f.user_id)
        .await?
        .pop()
        .ok_or_else(|| anyhow::anyhow!("missing draft"))?;
    assert_eq!(current.media.len(), 1);
    assert_ne!(current.media[0].id, original.id);
    assert_eq!(current.media[0].alt_text, "Replacement caption");
    assert_eq!(current.media[0].public_path, original.public_path);
    assert_eq!(
        raw(
            &f,
            "GET",
            &current.media[0].public_path,
            Some(&f.session.token),
            &[],
            ""
        )
        .await?
        .0,
        200
    );
    // A stale editor must not detach the replacement or publish its newly uploaded bytes.
    let response = f
        .request(
            "POST",
            &format!("/drafts/{}", draft.id),
            &multipart(&fields, Some(&png()?)),
            MULTIPART,
        )
        .await?;
    assert_eq!(response.0, 400);
    let replacement = current.media[0].id;
    let retained_description = format!("existing_alt_{replacement}");
    let forged = multipart(
        &[
            ("csrf", f.session.csrf_token.clone()),
            ("text", draft.text),
            ("revision", current.revision.to_string()),
            ("intent", "draft".into()),
            ("retain_media", replacement.to_string()),
            (&retained_description, "Keep caption".into()),
            (
                "existing_alt_9223372036854775807",
                "Foreign description".into(),
            ),
        ],
        Some(&png()?),
    );
    assert_eq!(
        f.request("POST", &format!("/drafts/{}", draft.id), &forged, MULTIPART)
            .await?
            .0,
        400
    );
    // Both rejected edits leave the committed draft, description and shared file intact.
    let remaining = social::drafts::list(&f.state.pool, f.user_id)
        .await?
        .pop()
        .ok_or_else(|| anyhow::anyhow!("missing draft"))?;
    assert_eq!(remaining.revision, current.revision);
    assert_eq!(remaining.media[0].alt_text, "Replacement caption");
    for _ in 0..50 {
        if f.count("media").await? == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(f.count("media").await?, 1);
    assert_eq!(
        std::fs::read_dir(&f.state.paths.uploads_originals)?.count(),
        1
    );
    delete_attachment_draft(&f, &remaining).await
}

#[tokio::test]
async fn concurrent_multipart_uploads_share_logical_quota_and_clean_the_rejected_file()
-> anyhow::Result<()> {
    let mut settings = Settings::default();
    settings.storage.max_upload_bytes = 100;
    settings.storage.max_user_upload_bytes = 100;
    let f = Fixture::configured(
        settings,
        FfmpegStatus {
            available: false,
            version: String::new(),
            supports_webp: false,
            supports_vp9: false,
            error: None,
        },
    )
    .await?;
    let image = png()?;
    let first = multipart(
        &[
            ("csrf", f.session.csrf_token.clone()),
            ("post_key", "quota-first".into()),
            ("text", "first".into()),
        ],
        Some(&image),
    );
    let second = multipart(
        &[
            ("csrf", f.session.csrf_token.clone()),
            ("post_key", "quota-second".into()),
            ("text", "second".into()),
        ],
        Some(&image),
    );
    let (one, two) = tokio::join!(
        f.request("POST", "/posts", &first, MULTIPART),
        f.request("POST", "/posts", &second, MULTIPART)
    );
    let mut statuses = [one?.0, two?.0];
    statuses.sort_unstable();
    assert_eq!(statuses, [303, 400]);
    assert_eq!(f.count("posts").await?, 1);
    assert_eq!(f.count("media").await?, 1);
    for _ in 0..50 {
        if f.count("storage_reservations").await? == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(f.count("storage_reservations").await?, 0);
    assert_eq!(
        std::fs::read_dir(&f.state.paths.uploads_originals)?.count(),
        1
    );
    assert_eq!(std::fs::read_dir(&f.state.paths.tmp_uploads)?.count(), 0);
    Ok(())
}

async fn delete_attachment_draft(f: &Fixture, draft: &social::drafts::Draft) -> anyhow::Result<()> {
    let body = format!("csrf={}&revision={}", f.session.csrf_token, draft.revision);
    assert_eq!(
        f.request(
            "POST",
            &format!("/drafts/{}/delete", draft.id),
            body.as_bytes(),
            FORM
        )
        .await?
        .0,
        303
    );
    assert_eq!(f.count("media").await?, 0);
    assert_eq!(
        std::fs::read_dir(&f.state.paths.uploads_originals)?.count(),
        0
    );
    Ok(())
}

#[tokio::test]
async fn readiness_and_requests_recover_stale_capture_but_preserve_live_or_unexpected_state()
-> anyhow::Result<()> {
    let f = Fixture::new(30, 10).await?;
    let marker = f.state.paths.tmp_dir.join("backup-capture.lock");
    std::fs::create_dir(&marker)?;
    let owner = crate::backup::backup_coordination_lock(&f.state.paths)?;
    assert_eq!(raw(&f, "GET", "/health/live", None, &[], "").await?.0, 200);
    assert_eq!(raw(&f, "GET", "/health/ready", None, &[], "").await?.0, 503);
    assert_eq!(raw(&f, "GET", "/home", None, &[], "").await?.0, 503);
    assert!(marker.is_dir());
    drop(owner);
    std::fs::write(marker.join("unexpected"), b"preserve evidence")?;
    assert_eq!(raw(&f, "GET", "/health/ready", None, &[], "").await?.0, 503);
    assert_eq!(raw(&f, "GET", "/home", None, &[], "").await?.0, 503);
    std::fs::remove_file(marker.join("unexpected"))?;
    assert_eq!(raw(&f, "GET", "/health/ready", None, &[], "").await?.0, 200);
    assert_eq!(raw(&f, "GET", "/home", None, &[], "").await?.0, 200);
    assert!(!marker.exists());
    Ok(())
}
