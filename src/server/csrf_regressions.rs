//! HTTP regressions use disposable state and the complete production router.

use super::*;

const PASSWORD: &str = "local-csrf-regression-password";

struct Fixture {
    state: Arc<AppState>,
    user_id: i64,
    session: auth::Session,
    address: SocketAddr,
    task: tokio::task::JoinHandle<()>,
    directory: tempfile::TempDir,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Fixture {
    async fn new(grace_days: u64, quota: i64) -> anyhow::Result<Self> {
        let mut settings = Settings::default();
        settings.accounts.deletion_grace_period_days = grace_days;
        settings.accounts.max_archive_upload_bytes = 64 * 1024;
        settings.moderation.posts_per_minute = quota;
        Self::configured(
            settings,
            FfmpegStatus {
                available: false,
                version: String::new(),
                supports_webp: false,
                supports_vp9: false,
                error: None,
            },
        )
        .await
    }

    async fn configured(settings: Settings, ffmpeg: FfmpegStatus) -> anyhow::Result<Self> {
        let directory = tempfile::tempdir()?;
        let paths = RuntimePaths::from_data_dir(directory.path().join("instance"));
        paths.ensure()?;
        std::fs::write(&paths.settings_path, toml::to_string(&settings)?)?;
        let pool = crate::db::connect(&paths.database_path).await?;
        crate::db::migrate(&pool).await?;
        let user_id = auth::register_user(&pool, &settings, "csrfuser", PASSWORD, false).await?;
        let session = auth::create_session(&pool, user_id).await?;
        let tor = crate::tor::validate_startup(&settings.tor);
        let state = AppState::new(pool, settings, paths, ffmpeg, tor);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let app = router(state.clone()).into_make_service_with_connect_info::<SocketAddr>();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(Self {
            state,
            user_id,
            session,
            address,
            task,
            directory,
        })
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        headers: &str,
    ) -> anyhow::Result<(u16, String)> {
        let mut socket = tokio::net::TcpStream::connect(self.address).await?;
        // HTTP/1.0 and Connection: close avoid chunk framing in this tiny client.
        let head = format!(
            "{method} {path} HTTP/1.0\r\nHost: {}\r\nCookie: rustpost_session={}\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n",
            self.address,
            self.session.token,
            body.len(),
        );
        socket.write_all(head.as_bytes()).await?;
        socket.write_all(body).await?;
        let mut response = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            socket.read_to_end(&mut response),
        )
        .await??;
        let response = String::from_utf8(response)?;
        let status = response
            .split_whitespace()
            .nth(1)
            .ok_or_else(|| anyhow::anyhow!("missing HTTP status"))?
            .parse()?;
        Ok((status, response))
    }

    async fn quota(&self) -> anyhow::Result<i64> {
        let actor = user_actor(self.user_id);
        self.state
            .pool
            .call(move |conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM rate_limit_events WHERE scope = 'post' AND actor = ?",
                    [actor],
                    |row| row.get(0),
                )?)
            })
            .await
    }

    async fn form(&self, path: &str, fields: &str, headers: &str) -> anyhow::Result<(u16, String)> {
        self.request(
            "POST",
            path,
            fields.as_bytes(),
            &format!("Content-Type: application/x-www-form-urlencoded\r\n{headers}"),
        )
        .await
    }

    async fn prepare(&self) -> anyhow::Result<String> {
        let (status, response) = self
            .form(
                "/settings/delete/prepare",
                &format!("csrf={}", self.session.csrf_token),
                "",
            )
            .await?;
        assert_eq!(status, 200);
        hidden_value(&response, "delete_intent")
    }

    async fn finish(&self, intent: &str, password: &str) -> anyhow::Result<(u16, String)> {
        self.form(
            "/settings/delete/confirm",
            &format!(
                "csrf={}&delete_intent={intent}&password={password}",
                self.session.csrf_token,
            ),
            "",
        )
        .await
    }

    async fn archive(&self) -> anyhow::Result<Vec<u8>> {
        let path = self
            .directory
            .path()
            .join(format!("{}.tar.gz", Uuid::new_v4()));
        portability::export_account(
            &self.state.pool,
            &self.state.paths,
            self.user_id,
            &path,
            portability::ArchiveLimits::from_settings(&self.state.settings),
        )
        .await?;
        Ok(std::fs::read(path)?)
    }

    async fn intent_state(&self) -> anyhow::Result<(Option<String>, Option<String>)> {
        let hash = auth::hash_token(&self.session.token);
        self.state.pool.call(move |conn| Ok(conn.query_row(
            "SELECT delete_account_token_hash, delete_account_token_expires_at FROM sessions WHERE token_hash = ?",
            [hash], |row| Ok((row.get(0)?, row.get(1)?)),
        )?)).await
    }

    fn files(&self) -> anyhow::Result<Vec<std::path::PathBuf>> {
        let mut files = walkdir::WalkDir::new(&self.state.paths.tmp_dir)
            .into_iter()
            .chain(walkdir::WalkDir::new(
                self.state.paths.data_dir.join("uploads"),
            ))
            .map(|entry| entry.map_err(anyhow::Error::from))
            .collect::<anyhow::Result<Vec<_>>>()?
            .into_iter()
            .filter(|entry| entry.file_type().is_file())
            .map(walkdir::DirEntry::into_path)
            .collect::<Vec<_>>();
        files.sort();
        Ok(files)
    }
}

fn hidden_value(response: &str, name: &str) -> anyhow::Result<String> {
    response
        .split(&format!("name=\"{name}\" value=\""))
        .nth(1)
        .and_then(|value| value.split('"').next())
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("missing {name} field"))
}

fn multipart(fields: &[(&str, Option<&str>, &[u8])]) -> (Vec<u8>, &'static str) {
    let mut body = Vec::new();
    for (name, filename, value) in fields {
        body.extend_from_slice(
            format!("--csrf-regression\r\nContent-Disposition: form-data; name=\"{name}\"")
                .as_bytes(),
        );
        if let Some(filename) = filename {
            body.extend_from_slice(format!("; filename=\"{filename}\"").as_bytes());
        }
        body.extend_from_slice(b"\r\n\r\n");
        body.extend_from_slice(value);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(b"--csrf-regression--\r\n");
    (
        body,
        "Content-Type: multipart/form-data; boundary=csrf-regression\r\n",
    )
}

#[tokio::test]
async fn rejected_import_does_not_charge_quota_or_leave_files() -> anyhow::Result<()> {
    let fixture = Fixture::new(30, 5).await?;
    let files = fixture.files()?;
    let other_session = auth::create_session(&fixture.state.pool, fixture.user_id).await?;
    let token = fixture.session.csrf_token.as_bytes();
    let cases = [
        vec![
            ("csrf", None, &b"wrong"[..]),
            ("archive", Some("test.tar.gz"), &b"invalid archive"[..]),
        ],
        vec![("archive", Some("test.tar.gz"), &b"invalid archive"[..])],
        vec![("csrf", None, other_session.csrf_token.as_bytes())],
        vec![("csrf", None, token), ("csrf", None, token)],
        vec![
            ("csrf", None, token),
            ("archive", Some("test.tar.gz"), &b"invalid archive"[..]),
            ("csrf", None, token),
        ],
    ];
    for fields in cases {
        let (body, headers) = multipart(&fields);
        // This matches a privacy-stripped same-site browser submission.
        assert_eq!(
            fixture
                .request("POST", "/settings/import", &body, headers)
                .await?
                .0,
            403
        );
        assert_eq!(fixture.quota().await?, 0);
        assert_eq!(fixture.files()?, files);
    }
    let hash = auth::hash_token(&fixture.session.token);
    fixture.state.pool.call(move |conn| {
        conn.execute("UPDATE sessions SET expires_at = datetime('now', '-1 second') WHERE token_hash = ?", [hash])?;
        Ok(())
    }).await?;
    let (body, headers) = multipart(&[("csrf", None, token)]);
    assert_eq!(
        fixture
            .request("POST", "/settings/import", &body, headers)
            .await?
            .0,
        401
    );
    assert_eq!(fixture.quota().await?, 0);
    assert_eq!(fixture.files()?, files);
    Ok(())
}

#[tokio::test]
async fn safe_confirmation_navigation_preserves_existing_intent() -> anyhow::Result<()> {
    let fixture = Fixture::new(30, 5).await?;
    for method in ["GET", "HEAD"] {
        assert_eq!(
            fixture
                .request(method, "/settings/delete/confirm", b"", "")
                .await?
                .0,
            200
        );
        assert_eq!(fixture.intent_state().await?, (None, None));
    }
    let intent = fixture.prepare().await?;
    let before = fixture.intent_state().await?;
    assert!(before.0.is_some());
    for method in ["GET", "HEAD"] {
        let response = fixture
            .request(
                method,
                "/settings/delete/confirm",
                b"",
                "Sec-Fetch-Site: cross-site\r\nReferer: https://attacker.example/\r\n",
            )
            .await?;
        assert!((200..400).contains(&response.0));
        assert_eq!(
            fixture.intent_state().await?,
            before,
            "{method} changed deletion authorization"
        );
    }
    assert_eq!(fixture.finish(&intent, PASSWORD).await?.0, 303);
    Ok(())
}

#[tokio::test]
async fn authorized_imports_share_an_atomic_budget_and_clean_rejected_uploads() -> anyhow::Result<()>
{
    let fixture = Fixture::new(30, 3).await?;
    let archive1 = fixture.archive().await?;
    let archive2 = fixture.archive().await?;
    let token = fixture.session.csrf_token.as_bytes();
    let (body1, headers) = multipart(&[
        ("csrf", None, token),
        ("archive", Some("a.tar.gz"), &archive1),
    ]);
    let (body2, _) = multipart(&[
        ("csrf", None, token),
        ("archive", Some("b.tar.gz"), &archive2),
    ]);
    // Both absent provenance and Origin:null are supported for token forms.
    let null_headers = format!("{headers}Origin: null\r\n");
    assert_eq!(
        fixture
            .request("POST", "/settings/import", &body1, headers)
            .await?
            .0,
        200
    );
    assert_eq!(
        fixture
            .request("POST", "/settings/import", &body2, &null_headers)
            .await?
            .0,
        200
    );
    let archive3 = fixture.archive().await?;
    let archive4 = fixture.archive().await?;
    let (body1, _) = multipart(&[
        ("csrf", None, token),
        ("archive", Some("c.tar.gz"), &archive3),
    ]);
    let (body2, _) = multipart(&[
        ("csrf", None, token),
        ("archive", Some("d.tar.gz"), &archive4),
    ]);
    let (first, second) = tokio::join!(
        fixture.request("POST", "/settings/import", &body1, headers),
        fixture.request("POST", "/settings/import", &body2, &null_headers),
    );
    let mut statuses = [first?.0, second?.0];
    statuses.sort_unstable();
    assert_eq!(statuses, [200, 429]);
    assert_eq!(fixture.quota().await?, 3);
    assert_eq!(fixture.files()?, Vec::<std::path::PathBuf>::new());
    let (post_body, post_headers) =
        multipart(&[("csrf", None, token), ("text", None, b"over budget")]);
    assert_eq!(
        fixture
            .request("POST", "/posts", &post_body, post_headers)
            .await?
            .0,
        429
    );
    assert_eq!(fixture.quota().await?, 3);
    let imported = fixture
        .state
        .pool
        .call(|conn| {
            Ok(
                conn.query_row("SELECT COUNT(*) FROM account_imports", [], |row| {
                    row.get::<_, i64>(0)
                })?,
            )
        })
        .await?;
    assert_eq!(imported, 3);
    Ok(())
}

#[tokio::test]
async fn import_stream_limits_and_failed_archive_cleanup_remain_enforced() -> anyhow::Result<()> {
    let fixture = Fixture::new(30, 5).await?;
    let token = fixture.session.csrf_token.as_bytes();
    let oversized = vec![0u8; 64 * 1024 + 1];
    let (body, headers) = multipart(&[
        ("csrf", None, token),
        ("archive", Some("large.tar.gz"), &oversized),
    ]);
    assert_eq!(
        fixture
            .request("POST", "/settings/import", &body, headers)
            .await?
            .0,
        413
    );
    assert_eq!(fixture.quota().await?, 0);
    assert_eq!(fixture.files()?, Vec::<std::path::PathBuf>::new());
    let (mut body, _) = multipart(&[
        ("csrf", None, token),
        ("archive", Some("broken.tar.gz"), b"broken"),
    ]);
    body.truncate(body.len() - 22);
    assert_eq!(
        fixture
            .request("POST", "/settings/import", &body, headers)
            .await?
            .0,
        400
    );
    assert_eq!(fixture.quota().await?, 0);
    assert_eq!(fixture.files()?, Vec::<std::path::PathBuf>::new());
    let (body, _) = multipart(&[
        ("csrf", None, token),
        ("archive", Some("invalid.tar.gz"), b"invalid archive"),
    ]);
    assert_eq!(
        fixture
            .request("POST", "/settings/import", &body, headers)
            .await?
            .0,
        400
    );
    assert_eq!(fixture.quota().await?, 1);
    assert_eq!(fixture.files()?, Vec::<std::path::PathBuf>::new());
    Ok(())
}

#[tokio::test]
async fn deletion_preparation_rejects_forgery_without_changing_intent() -> anyhow::Result<()> {
    let fixture = Fixture::new(30, 5).await?;
    let intent = fixture.prepare().await?;
    let before = fixture.intent_state().await?;
    let other = auth::create_session(&fixture.state.pool, fixture.user_id).await?;
    for fields in [
        "csrf=wrong".to_owned(),
        format!("csrf={}", other.csrf_token),
        String::new(),
        format!("csrf={0}&csrf={0}", fixture.session.csrf_token),
    ] {
        let status = fixture
            .form("/settings/delete/prepare", &fields, "")
            .await?
            .0;
        assert!([403, 422].contains(&status));
        assert_eq!(fixture.intent_state().await?, before);
    }
    for headers in [
        "Origin: https://attacker.example\r\n",
        "Sec-Fetch-Site: cross-site\r\n",
    ] {
        assert_eq!(
            fixture
                .form(
                    "/settings/delete/prepare",
                    &format!("csrf={}", fixture.session.csrf_token),
                    headers
                )
                .await?
                .0,
            403
        );
        assert_eq!(fixture.intent_state().await?, before);
    }
    assert_eq!(fixture.finish(&intent, PASSWORD).await?.0, 303);
    Ok(())
}

#[tokio::test]
async fn deletion_requires_password_unexpired_intent_and_session_binding() -> anyhow::Result<()> {
    let mut fixture = Fixture::new(30, 5).await?;
    let warning = fixture.request("GET", "/settings/delete", b"", "").await?;
    assert_eq!(warning.0, 200);
    assert!(
        warning
            .1
            .contains("method=\"post\" action=\"/settings/delete/prepare\"")
    );
    let intent = fixture.prepare().await?;
    let invalid_password = fixture.finish(&intent, "incorrect").await?;
    assert_eq!(invalid_password.0, 401);
    assert!(
        account::deletion_status(&fixture.state.pool, fixture.user_id)
            .await?
            .is_none()
    );
    assert_eq!(
        fixture.finish(&intent, PASSWORD).await?.0,
        400,
        "intent was reusable"
    );
    let fresh_intent = fixture.prepare().await?;
    fixture.state.pool.call(|conn| {
        conn.execute("UPDATE sessions SET delete_account_token_expires_at = datetime('now', '-1 second')", [])?;
        Ok(())
    }).await?;
    let expired = fixture.finish(&fresh_intent, PASSWORD).await?;
    assert_eq!(expired.0, 400);
    assert!(
        account::deletion_status(&fixture.state.pool, fixture.user_id)
            .await?
            .is_none()
    );
    let intent = hidden_value(&expired.1, "delete_intent")?;
    let original_session = std::mem::replace(
        &mut fixture.session,
        auth::create_session(&fixture.state.pool, fixture.user_id).await?,
    );
    assert_eq!(
        fixture.finish(&intent, PASSWORD).await?.0,
        400,
        "foreign-session intent accepted"
    );
    fixture.session = original_session;
    // A failed attempt from another session cannot consume the owner's intent.
    assert_eq!(fixture.finish(&intent, PASSWORD).await?.0, 303);
    assert!(
        account::deletion_status(&fixture.state.pool, fixture.user_id)
            .await?
            .is_some()
    );
    assert_eq!(fixture.intent_state().await?, (None, None));
    assert_eq!(fixture.finish(&intent, PASSWORD).await?.0, 400);
    Ok(())
}

#[tokio::test]
async fn restricted_accounts_keep_the_no_javascript_delete_flow() -> anyhow::Result<()> {
    for grace_days in [0, 30] {
        let fixture = Fixture::new(grace_days, 5).await?;
        fixture
            .state
            .pool
            .call(|conn| {
                conn.execute("UPDATE users SET must_change_password = 1", [])?;
                Ok(())
            })
            .await?;
        if grace_days > 0 {
            account::request_deletion(
                &fixture.state.pool,
                &fixture.state.settings,
                fixture.user_id,
            )
            .await?;
        }
        assert_eq!(
            fixture.request("GET", "/settings/delete", b"", "").await?.0,
            200
        );
        let response = fixture
            .form(
                "/settings/delete/prepare",
                &format!("csrf={}", fixture.session.csrf_token),
                "Origin: null\r\n",
            )
            .await?;
        assert_eq!(response.0, 200);
        let intent = hidden_value(&response.1, "delete_intent")?;
        assert_eq!(fixture.finish(&intent, PASSWORD).await?.0, 303);
        if grace_days == 0 {
            let deleted = fixture
                .state
                .pool
                .call(|conn| {
                    Ok(conn
                        .query_row("SELECT COUNT(*) FROM users", [], |row| row.get::<_, i64>(0))?)
                })
                .await?;
            assert_eq!(deleted, 0);
        } else {
            assert!(
                account::deletion_status(&fixture.state.pool, fixture.user_id)
                    .await?
                    .is_some()
            );
        }
    }
    Ok(())
}

mod feature_http_regressions;
