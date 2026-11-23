//! Failure, concurrency and restart regressions for the 1.2 data paths.

use crate::{
    backup, config::Settings, db, ffmpeg::FfmpegStatus, media, runtime::RuntimePaths, social,
    storage_limits,
};
use rusqlite::params;

struct Fixture {
    directory: tempfile::TempDir,
    paths: RuntimePaths,
    settings: Settings,
    pool: db::SqlitePool,
    alice: i64,
    bob: i64,
}

impl Fixture {
    async fn new() -> anyhow::Result<Self> {
        let directory = tempfile::tempdir()?;
        let paths = RuntimePaths::from_data_dir(directory.path().join("data"));
        paths.ensure()?;
        let mut settings = Settings::default();
        settings.storage.min_free_bytes = 0;
        std::fs::write(&paths.settings_path, toml::to_string(&settings)?)?;
        let pool = db::connect(&paths.database_path).await?;
        db::migrate(&pool).await?;
        let (alice,bob)=pool.call(|conn| {
            conn.execute("INSERT INTO users(username,normalized_username,password_hash,display_name,onboarding_completed_at) VALUES('alice','alice','unused','Alice',CURRENT_TIMESTAMP)",[])?;
            let alice=conn.last_insert_rowid();
            conn.execute("INSERT INTO users(username,normalized_username,password_hash,display_name,onboarding_completed_at) VALUES('bob','bob','unused','Bob',CURRENT_TIMESTAMP)",[])?;
            Ok((alice,conn.last_insert_rowid()))
        }).await?;
        Ok(Self {
            directory,
            paths,
            settings,
            pool,
            alice,
            bob,
        })
    }

    async fn attachment(&self, owner: i64, name: &str, bytes: &[u8]) -> anyhow::Result<i64> {
        let path = self.paths.uploads_originals.join(name);
        std::fs::write(&path, bytes)?;
        let path = path.to_string_lossy().into_owned();
        let public = format!("/uploads/originals/{name}");
        let name = name.to_owned();
        let bytes = i64::try_from(bytes.len())?;
        self.pool.call(move |conn| {
            conn.execute("INSERT INTO media(owner_user_id,original_filename,stored_path,public_path,mime_type,media_kind,byte_len,conversion_state) VALUES(?,?,?,?,'image/png','image',?,'original')",params![owner,name,path,public,bytes])?;
            Ok(conn.last_insert_rowid())
        }).await
    }

    async fn post(&self, user: i64, text: &str) -> anyhow::Result<i64> {
        social::create_post(&self.pool, &self.settings, Some(user), text, None, &[]).await
    }

    async fn count(&self, table: &'static str) -> anyhow::Result<i64> {
        self.pool
            .call(move |conn| {
                Ok(conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?)
            })
            .await
    }

    async fn view(&self, id: i64, viewer: Option<i64>) -> anyhow::Result<social::PostView> {
        Ok(
            social::history::thread_page(&self.pool, viewer, id, None, None)
                .await?
                .ok_or_else(|| anyhow::anyhow!("missing post"))?
                .requested,
        )
    }
}

fn unavailable() -> FfmpegStatus {
    FfmpegStatus {
        available: false,
        version: String::new(),
        supports_webp: false,
        supports_vp9: false,
        error: None,
    }
}

#[tokio::test]
async fn upgrades_old_releases_and_alpha_without_losing_content() -> anyhow::Result<()> {
    for version in [1, 2, 3, 4, 13] {
        let directory = tempfile::tempdir()?;
        let paths = RuntimePaths::from_data_dir(directory.path().join("data"));
        paths.ensure()?;
        let conn = rusqlite::Connection::open(&paths.database_path)?;
        conn.execute_batch(include_str!("../tests/fixtures/schema-v4.sql"))?;
        conn.execute("INSERT INTO users(username,normalized_username,password_hash) VALUES('legacy','legacy','unused')",[])?;
        conn.execute(
            "INSERT INTO posts(user_id,text) VALUES(1,'legacy post survives')",
            [],
        )?;
        if version < 4 || version == 13 {
            for table in [
                "follow_requests",
                "username_history",
                "instance_settings",
                "account_imports",
            ] {
                conn.execute_batch(&format!("DROP TABLE {table}"))?;
            }
            for column in [
                "follow_approval_required",
                "must_change_password",
                "deletion_requested_at",
                "deletion_scheduled_at",
            ] {
                conn.execute_batch(&format!("ALTER TABLE users DROP COLUMN {column}"))?;
            }
        }
        if version < 3 || version == 13 {
            for index in [
                "idx_posts_root",
                "idx_media_owner",
                "idx_sessions_user",
                "idx_post_media_media",
                "idx_rate_limit_created",
            ] {
                conn.execute_batch(&format!("DROP INDEX IF EXISTS {index}"))?;
            }
        }
        if version == 1 || version == 13 {
            conn.execute_batch("DROP TABLE post_embeds")?;
        }
        conn.execute("UPDATE schema_migrations SET version=?", [version])?;
        drop(conn);
        let pool = db::connect(&paths.database_path).await?;
        db::migrate(&pool).await?;
        db::migrate(&pool).await?;
        pool.call(|conn| {
            db::validate_schema(conn)?;
            assert_eq!(
                conn.query_row("SELECT text FROM posts WHERE id=1", [], |r| r
                    .get::<_, String>(0))?,
                "legacy post survives"
            );
            assert_eq!(
                conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get::<_, i64>(0))?,
                1
            );
            Ok(())
        })
        .await?;
    }
    Ok(())
}

#[tokio::test]
async fn failed_upgrade_rolls_back_schema_and_data() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let paths = RuntimePaths::from_data_dir(directory.path().join("data"));
    paths.ensure()?;
    let conn = rusqlite::Connection::open(&paths.database_path)?;
    conn.execute_batch(include_str!("../tests/fixtures/schema-v4.sql"))?;
    conn.execute_batch(
        "CREATE TABLE unexpected(value TEXT); INSERT INTO unexpected VALUES('keep me')",
    )?;
    drop(conn);
    let pool = db::connect(&paths.database_path).await?;
    assert!(db::migrate(&pool).await.is_err());
    pool.call(|conn| {
        assert_eq!(
            conn.query_row("SELECT version FROM schema_migrations", [], |r| r
                .get::<_, i64>(0))?,
            4
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name='drafts'",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            0
        );
        assert_eq!(
            conn.query_row("SELECT value FROM unexpected", [], |r| r
                .get::<_, String>(0))?,
            "keep me"
        );
        Ok(())
    })
    .await
}

#[tokio::test]
async fn draft_ownership_revision_and_attachment_retention() -> anyhow::Result<()> {
    let f = Fixture::new().await?;
    let image = f.attachment(f.alice, "draft.png", b"draft image").await?;
    let input = social::drafts::DraftInput {
        text: "unfinished".into(),
        media_ids: vec![image],
        descriptions: vec![(image, "Accessible draft".into())],
        poll_options: "first unfinished choice".into(),
        ..Default::default()
    };
    let id = social::drafts::save(&f.pool, &f.settings, f.alice, input).await?;
    assert!(social::drafts::list(&f.pool, f.bob).await?.is_empty());
    assert!(media::delete_media(&f.pool, &f.paths, image).await.is_err());
    assert!(f.paths.uploads_originals.join("draft.png").is_file());
    assert!(
        social::drafts::save(
            &f.pool,
            &f.settings,
            f.bob,
            social::drafts::DraftInput {
                media_ids: vec![image],
                ..Default::default()
            }
        )
        .await
        .is_err()
    );
    let edit = social::drafts::DraftInput {
        id: Some(id),
        revision: Some(1),
        text: "edited on another device".into(),
        media_ids: vec![image],
        ..Default::default()
    };
    let (a, b) = tokio::join!(
        social::drafts::save(&f.pool, &f.settings, f.alice, edit.clone()),
        social::drafts::save(&f.pool, &f.settings, f.alice, edit)
    );
    assert_ne!(a.is_ok(), b.is_ok());
    let rows = social::drafts::list(&f.pool, f.alice).await?;
    assert_eq!(rows[0].revision, 2);
    assert_eq!(rows[0].media[0].alt_text, "Accessible draft");
    assert!(
        social::drafts::cancel(&f.pool, f.bob, id, 2, true)
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn scheduled_publication_survives_reopen_and_is_exactly_once() -> anyhow::Result<()> {
    let f = Fixture::new().await?;
    let media = f
        .attachment(f.alice, "scheduled.png", b"scheduled image")
        .await?;
    let id = social::drafts::save(
        &f.pool,
        &f.settings,
        f.alice,
        social::drafts::DraftInput {
            text: "scheduled @bob".into(),
            media_ids: vec![media],
            scheduled_at: Some(chrono::Utc::now().timestamp()),
            poll_options: "Yes\nNo".into(),
            client_key: "scheduled-request".into(),
            ..Default::default()
        },
    )
    .await?;
    let reopened = db::connect(&f.paths.database_path).await?;
    let (a, b) = tokio::join!(
        social::drafts::publish(&reopened, &f.settings, id),
        social::drafts::publish(&reopened, &f.settings, id)
    );
    assert_eq!(a?, b?);
    assert_eq!(f.count("posts").await?, 1);
    assert_eq!(f.count("polls").await?, 1);
    assert_eq!(f.count("post_media").await?, 1);
    assert_eq!(f.count("notifications").await?, 1);
    social::drafts::publish_due(&reopened, &f.settings).await?;
    assert_eq!(f.count("posts").await?, 1);
    assert!(f.paths.uploads_originals.join("scheduled.png").is_file());
    assert!(media::delete_media(&f.pool, &f.paths, media).await.is_err());
    assert!(
        social::drafts::cancel(&f.pool, f.alice, id, 1, false)
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn cancelled_and_invalid_reply_schedules_never_become_root_posts() -> anyhow::Result<()> {
    let f = Fixture::new().await?;
    let parent = f.post(f.bob, "parent").await?;
    let draft = social::drafts::save(
        &f.pool,
        &f.settings,
        f.alice,
        social::drafts::DraftInput {
            text: "reply".into(),
            parent_post_id: Some(parent),
            scheduled_at: Some(chrono::Utc::now().timestamp()),
            ..Default::default()
        },
    )
    .await?;
    social::drafts::cancel(&f.pool, f.alice, draft, 1, false).await?;
    assert!(
        social::drafts::publish(&f.pool, &f.settings, draft)
            .await?
            .is_none()
    );
    assert_eq!(f.count("posts").await?, 1);
    let invalid = social::drafts::save(
        &f.pool,
        &f.settings,
        f.alice,
        social::drafts::DraftInput {
            text: "still a reply".into(),
            parent_post_id: Some(parent),
            scheduled_at: Some(chrono::Utc::now().timestamp()),
            ..Default::default()
        },
    )
    .await?;
    f.pool
        .call(move |conn| {
            conn.execute("DELETE FROM posts WHERE id=?", [parent])?;
            Ok(())
        })
        .await?;
    assert!(
        social::drafts::publish(&f.pool, &f.settings, invalid)
            .await
            .is_err()
    );
    assert_eq!(f.count("posts").await?, 0);
    let draft = social::drafts::list(&f.pool, f.alice)
        .await?
        .into_iter()
        .find(|d| d.id == invalid)
        .ok_or_else(|| anyhow::anyhow!("missing failed draft"))?;
    assert_eq!(draft.status, "failed");
    assert_eq!(draft.parent_post_id, Some(parent));
    Ok(())
}

#[tokio::test]
async fn scheduled_posts_pause_for_maintenance_and_defer_rate_limits() -> anyhow::Result<()> {
    let mut f = Fixture::new().await?;
    f.settings.moderation.posts_per_minute = 1;
    let draft = social::drafts::save(
        &f.pool,
        &f.settings,
        f.alice,
        social::drafts::DraftInput {
            text: "later".into(),
            scheduled_at: Some(chrono::Utc::now().timestamp()),
            ..Default::default()
        },
    )
    .await?;
    crate::instance::save_maintenance(&f.pool, true, "").await?;
    assert!(
        social::drafts::publish(&f.pool, &f.settings, draft)
            .await?
            .is_none()
    );
    assert_eq!(f.count("posts").await?, 0);
    crate::instance::save_maintenance(&f.pool, false, "").await?;
    crate::rate_limit::record(
        &f.pool,
        crate::rate_limit::Scope::Post,
        &format!("user:{}", f.alice),
    )
    .await?;
    assert!(
        social::drafts::publish(&f.pool, &f.settings, draft)
            .await
            .is_err()
    );
    assert_eq!(
        social::drafts::list(&f.pool, f.alice).await?[0].status,
        "scheduled"
    );
    assert_eq!(f.count("posts").await?, 0);
    f.pool
        .call(move |conn| {
            conn.execute("DELETE FROM rate_limit_events", [])?;
            conn.execute(
                "UPDATE drafts SET scheduled_at=unixepoch() WHERE id=?",
                [draft],
            )?;
            Ok(())
        })
        .await?;
    assert!(
        social::drafts::publish(&f.pool, &f.settings, draft)
            .await?
            .is_some()
    );
    Ok(())
}

#[tokio::test]
async fn concurrent_post_retries_commit_one_post_notification_and_rate_event() -> anyhow::Result<()>
{
    let f = Fixture::new().await?;
    let options = || social::PostOptions {
        poll: None,
        request: Some((format!("user:{}", f.alice), "one-request".into())),
        rate: Some(crate::rate_limit::Limit {
            scope: crate::rate_limit::Scope::Post,
            actor: format!("user:{}", f.alice),
            max_events: 1,
            window_secs: 60,
        }),
    };
    let (a, b) = tokio::join!(
        social::create_post_with_options(
            &f.pool,
            &f.settings,
            Some(f.alice),
            "hello @bob",
            None,
            &[],
            options()
        ),
        social::create_post_with_options(
            &f.pool,
            &f.settings,
            Some(f.alice),
            "hello @bob",
            None,
            &[],
            options()
        )
    );
    assert_eq!(a?, b?);
    assert_eq!(f.count("posts").await?, 1);
    assert_eq!(f.count("notifications").await?, 1);
    assert_eq!(f.count("rate_limit_events").await?, 1);
    let another = social::PostOptions {
        request: Some((
            format!("user:{}", f.alice),
            "intentional-second-post".into(),
        )),
        ..Default::default()
    };
    social::create_post_with_options(
        &f.pool,
        &f.settings,
        Some(f.alice),
        "hello @bob",
        None,
        &[],
        another,
    )
    .await?;
    assert_eq!(f.count("posts").await?, 2);
    Ok(())
}

#[tokio::test]
async fn polls_reject_invalid_ballots_and_expose_only_own_selection() -> anyhow::Result<()> {
    let f = Fixture::new().await?;
    assert!(social::polls::PollSpec::parse("same\nSAME", false, None).is_err());
    let options = social::PostOptions {
        poll: social::polls::PollSpec::parse("A\nB", false, None)?,
        ..Default::default()
    };
    let post = social::create_post_with_options(
        &f.pool,
        &f.settings,
        Some(f.alice),
        "Question?",
        None,
        &[],
        options,
    )
    .await?;
    let poll = f
        .view(post, Some(f.bob))
        .await?
        .poll
        .ok_or_else(|| anyhow::anyhow!("poll missing"))?;
    let first = poll.choices[0].id;
    let second = poll.choices[1].id;
    assert!(
        social::polls::vote(&f.pool, f.bob, post, vec![first, second])
            .await
            .is_err()
    );
    assert_eq!(f.count("poll_votes").await?, 0);
    social::polls::vote(&f.pool, f.bob, post, vec![first]).await?;
    social::polls::vote(&f.pool, f.bob, post, vec![first]).await?;
    assert_eq!(f.count("poll_votes").await?, 1);
    assert!(
        f.view(post, Some(f.bob))
            .await?
            .poll
            .ok_or_else(|| anyhow::anyhow!("poll missing"))?
            .choices[0]
            .selected
    );
    assert!(
        !f.view(post, Some(f.alice))
            .await?
            .poll
            .ok_or_else(|| anyhow::anyhow!("poll missing"))?
            .choices[0]
            .selected
    );
    social::polls::vote(&f.pool, f.bob, post, vec![second]).await?;
    assert_eq!(f.count("poll_votes").await?, 1);
    assert!(
        social::polls::vote(&f.pool, f.bob, post, vec![i64::MAX])
            .await
            .is_err()
    );
    assert_eq!(f.count("poll_votes").await?, 1);
    social::polls::close(&f.pool, f.alice, post).await?;
    assert!(
        social::polls::vote(&f.pool, f.bob, post, vec![first])
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn multiple_choice_polls_reject_cross_poll_options_and_blocks() -> anyhow::Result<()> {
    let f = Fixture::new().await?;
    let options = || -> anyhow::Result<_> {
        Ok(social::PostOptions {
            poll: social::polls::PollSpec::parse("A\nB\nC", true, None)?,
            ..Default::default()
        })
    };
    let first = social::create_post_with_options(
        &f.pool,
        &f.settings,
        Some(f.alice),
        "First",
        None,
        &[],
        options()?,
    )
    .await?;
    let second = social::create_post_with_options(
        &f.pool,
        &f.settings,
        Some(f.alice),
        "Second",
        None,
        &[],
        options()?,
    )
    .await?;
    let a = f
        .view(first, None)
        .await?
        .poll
        .ok_or_else(|| anyhow::anyhow!("poll missing"))?
        .choices;
    let b = f
        .view(second, None)
        .await?
        .poll
        .ok_or_else(|| anyhow::anyhow!("poll missing"))?
        .choices;
    social::polls::vote(&f.pool, f.bob, first, vec![a[0].id, a[1].id]).await?;
    assert!(
        social::polls::vote(&f.pool, f.bob, first, vec![b[0].id])
            .await
            .is_err()
    );
    assert_eq!(f.count("poll_votes").await?, 2);
    let bob = f.bob;
    let alice = f.alice;
    f.pool
        .call(move |conn| {
            conn.execute(
                "INSERT INTO blocks(blocker_id,blocked_id) VALUES(?,?)",
                params![alice, bob],
            )?;
            Ok(())
        })
        .await?;
    assert!(
        social::polls::vote(&f.pool, f.bob, first, vec![a[0].id])
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn search_filters_and_keysets_reach_beyond_old_limits() -> anyhow::Result<()> {
    let f = Fixture::new().await?;
    let alice = f.alice;
    f.pool.call(move |conn| {for _ in 0..95{conn.execute("INSERT INTO posts(user_id,text,created_at) VALUES(?,'searchable','2026-10-01 12:00:00')",[alice])?;}Ok(())}).await?;
    let first = social::search_page(
        &f.pool,
        Some(f.alice),
        "searchable from:alice after:2026-10-01 before:2026-10-02",
        None,
        None,
    )
    .await?;
    assert_eq!(first.posts.len(), 40);
    assert!(first.has_more_posts);
    let cursor = first
        .posts
        .last()
        .ok_or_else(|| anyhow::anyhow!("no results"))?
        .id;
    let second = social::search_page(
        &f.pool,
        Some(f.alice),
        "searchable from:alice",
        Some(cursor),
        None,
    )
    .await?;
    assert_eq!(second.posts.len(), 40);
    assert!(second.posts.iter().all(|p| p.id < cursor));
    let last = second
        .posts
        .last()
        .ok_or_else(|| anyhow::anyhow!("no results"))?
        .id;
    assert_eq!(
        social::search_page(
            &f.pool,
            Some(f.alice),
            "searchable from:alice",
            Some(last),
            None
        )
        .await?
        .posts
        .len(),
        15
    );
    assert!(
        social::search_page(&f.pool, None, "is:bookmarked", None, None)
            .await
            .is_err()
    );
    assert!(
        social::search_page(&f.pool, None, "after:invalid", None, None)
            .await
            .is_err()
    );
    assert!(
        social::search_page(&f.pool, None, "has:invalid", None, None)
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn media_and_bookmark_search_keeps_viewer_filters() -> anyhow::Result<()> {
    let f = Fixture::new().await?;
    let image = f.attachment(f.alice, "search.png", b"image").await?;
    let post = social::create_post(
        &f.pool,
        &f.settings,
        Some(f.alice),
        "with image",
        None,
        &[image],
    )
    .await?;
    social::bookmark(&f.pool, f.bob, post).await?;
    assert_eq!(
        social::search_page(&f.pool, Some(f.bob), "has:image is:bookmarked", None, None)
            .await?
            .posts
            .len(),
        1
    );
    assert!(
        social::search_page(&f.pool, Some(f.alice), "is:bookmarked", None, None)
            .await?
            .posts
            .is_empty()
    );
    let bob = f.bob;
    let alice = f.alice;
    f.pool
        .call(move |conn| {
            conn.execute(
                "INSERT INTO mutes(muter_id,muted_id) VALUES(?,?)",
                params![bob, alice],
            )?;
            Ok(())
        })
        .await?;
    assert!(
        social::search_page(&f.pool, Some(f.bob), "has:media is:bookmarked", None, None)
            .await?
            .posts
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn merged_feed_history_has_no_gaps_or_duplicates_at_timestamp_ties() -> anyhow::Result<()> {
    let f = Fixture::new().await?;
    let alice = f.alice;
    let bob = f.bob;
    f.pool.call(move |conn| {
        for n in 0..75 {conn.execute("INSERT INTO posts(user_id,text,created_at) VALUES(?,?,'2026-10-01 12:00:00')",params![alice,format!("post {n}")])?;}
        for id in 1..=55 {conn.execute("INSERT INTO reposts(user_id,post_id,created_at) VALUES(?,?,'2026-10-01 12:00:00')",params![bob,id])?;}
        Ok(())
    }).await?;
    let mut seen = std::collections::BTreeSet::new();
    let mut cursor = None;
    let last = loop {
        let page = social::history::page(
            &f.pool,
            Some(f.alice),
            social::history::Surface::Everyone,
            cursor.as_deref(),
            None,
        )
        .await?;
        for post in &page.posts {
            assert!(seen.insert(post.event_id.clone()), "duplicate event");
        }
        cursor = page.older.clone();
        if cursor.is_none() {
            break page;
        }
    };
    assert_eq!(seen.len(), 130);
    let back = social::history::page(
        &f.pool,
        Some(f.alice),
        social::history::Surface::Everyone,
        None,
        last.newer.as_deref(),
    )
    .await?;
    assert_eq!(back.posts.len(), 40);
    assert!(back.older.is_some());
    let following = social::history::page(
        &f.pool,
        Some(f.bob),
        social::history::Surface::Following,
        None,
        None,
    )
    .await?;
    assert!(
        following
            .posts
            .iter()
            .all(|p| p.event_kind == social::TimelineEventKind::Repost)
    );
    Ok(())
}

#[tokio::test]
async fn long_conversations_and_deep_reply_links_are_navigable() -> anyhow::Result<()> {
    let f = Fixture::new().await?;
    let root = f.post(f.alice, "root").await?;
    let alice = f.alice;
    let deepest=f.pool.call(move |conn|{for _ in 0..275 {conn.execute("INSERT INTO posts(user_id,text,parent_post_id,root_post_id) VALUES(?,'reply',?,?)",params![alice,root,root])?;}Ok(conn.last_insert_rowid())}).await?;
    let first = social::history::thread_page(&f.pool, None, root, None, None)
        .await?
        .ok_or_else(|| anyhow::anyhow!("missing thread"))?;
    assert_eq!(first.posts.len(), 100);
    assert!(first.before.is_none());
    assert!(first.after.is_some());
    let second = social::history::thread_page(&f.pool, None, root, None, first.after)
        .await?
        .ok_or_else(|| anyhow::anyhow!("missing page"))?;
    assert_eq!(second.posts.len(), 100);
    assert!(second.before.is_some());
    let third = social::history::thread_page(&f.pool, None, root, None, second.after)
        .await?
        .ok_or_else(|| anyhow::anyhow!("missing page"))?;
    assert_eq!(third.posts.len(), 76);
    assert!(third.after.is_none());
    let deep = social::history::thread_page(&f.pool, None, deepest, None, None)
        .await?
        .ok_or_else(|| anyhow::anyhow!("missing deep reply"))?;
    assert_eq!(deep.requested.id, deepest);
    assert_eq!(deep.posts[0].id, deepest);
    assert!(deep.before.is_some());
    Ok(())
}

#[tokio::test]
async fn concurrent_upload_reservations_enforce_quotas_and_recover_after_restart()
-> anyhow::Result<()> {
    let mut f = Fixture::new().await?;
    f.settings.storage.max_upload_bytes = 100;
    f.settings.storage.max_user_upload_bytes = 80;
    let a = storage_limits::Reservation::new(&f.pool, &f.settings, &f.paths, Some(f.alice)).await?;
    let b = storage_limits::Reservation::new(&f.pool, &f.settings, &f.paths, Some(f.alice)).await?;
    let (one, two) = tokio::join!(a.grow(60, false), b.grow(60, false));
    assert_ne!(one.is_ok(), two.is_ok());
    assert!(a.grow(101, false).await.is_err());
    drop(a);
    drop(b);
    // Startup recovery is only invoked while the server has exclusive runtime ownership.
    media::jobs::recover(&f.pool).await?;
    assert_eq!(f.count("storage_reservations").await?, 0);
    Ok(())
}

#[test]
fn native_disk_headroom_and_upgrade_defaults_are_valid() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    assert!(storage_limits::available_space(directory.path())? > 0);
    assert!(storage_limits::ensure_headroom(directory.path(), u64::MAX, 1).is_err());
    let mut value = toml::Value::try_from(Settings::default())?;
    value
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("missing config table"))?
        .remove("storage");
    value
        .get_mut("posts")
        .and_then(toml::Value::as_table_mut)
        .ok_or_else(|| anyhow::anyhow!("missing posts settings"))?
        .remove("post_delete_recovery_seconds");
    let old: Settings = toml::from_str(&toml::to_string(&value)?)?;
    assert_eq!(old.posts.post_delete_recovery_seconds, 30);
    assert_eq!(old.storage.min_free_bytes, 512 * 1024 * 1024);
    old.validate()?;
    Ok(())
}

#[tokio::test]
async fn original_media_survives_processing_restart_and_failure() -> anyhow::Result<()> {
    let f = Fixture::new().await?;
    let id = f
        .attachment(f.alice, "original.png", b"original bytes")
        .await?;
    f.pool.call(move |conn|{conn.execute("INSERT INTO media_work(media_id,status,attempts,lease_token) VALUES(?,'running',1,'interrupted')",[id])?;conn.execute("INSERT INTO media_jobs(media_id,status) VALUES(?,'processing')",[id])?;Ok(())}).await?;
    media::jobs::recover(&f.pool).await?;
    assert!(media::jobs::process_next(&f.pool, &f.settings, &f.paths, &unavailable()).await?);
    assert_eq!(
        std::fs::read(f.paths.uploads_originals.join("original.png"))?,
        b"original bytes"
    );
    assert!(!media::jobs::process_next(&f.pool, &f.settings, &f.paths, &unavailable()).await?);
    let missing = f.attachment(f.alice, "missing.png", b"missing").await?;
    std::fs::remove_file(f.paths.uploads_originals.join("missing.png"))?;
    f.pool
        .call(move |conn| {
            conn.execute(
                "INSERT INTO media_work(media_id,status) VALUES(?,'queued')",
                [missing],
            )?;
            Ok(())
        })
        .await?;
    for _ in 0..3 {
        media::jobs::process_next(&f.pool, &f.settings, &f.paths, &unavailable()).await?;
        f.pool
            .call(move |conn| {
                conn.execute(
                    "UPDATE media_work SET run_after=0 WHERE media_id=?",
                    [missing],
                )?;
                Ok(())
            })
            .await?;
    }
    assert!(media::jobs::retry(&f.pool, f.bob, missing).await.is_err());
    media::jobs::retry(&f.pool, f.alice, missing).await?;
    assert_eq!(
        std::fs::read(f.paths.uploads_originals.join("original.png"))?,
        b"original bytes"
    );
    Ok(())
}

#[tokio::test]
async fn own_deletion_retains_media_until_undo_or_expiry_and_admin_removal_is_final()
-> anyhow::Result<()> {
    let f = Fixture::new().await?;
    let image = f
        .attachment(f.alice, "undo.png", b"keep until expiry")
        .await?;
    let post = social::create_post(
        &f.pool,
        &f.settings,
        Some(f.alice),
        "undo me",
        None,
        &[image],
    )
    .await?;
    assert!(social::recovery::delete(&f.pool, f.alice, post, false, 30).await?);
    assert!(f.paths.uploads_originals.join("undo.png").is_file());
    assert!(f.view(post, None).await.is_err());
    assert!(
        social::recovery::restore(&f.pool, f.bob, post)
            .await
            .is_err()
    );
    social::recovery::restore(&f.pool, f.alice, post).await?;
    assert_eq!(f.view(post, None).await?.media[0].alt_text, "");
    social::recovery::delete(&f.pool, f.alice, post, false, 30).await?;
    f.pool
        .call(move |conn| {
            conn.execute(
                "UPDATE recoverable_deletions SET expires_at=unixepoch()-1 WHERE post_id=?",
                [post],
            )?;
            Ok(())
        })
        .await?;
    assert!(
        social::recovery::restore(&f.pool, f.alice, post)
            .await
            .is_err()
    );
    social::recovery::purge_due(&f.pool, &f.paths).await?;
    assert!(!f.paths.uploads_originals.join("undo.png").exists());
    assert_eq!(f.count("media").await?, 0);
    let other = f.post(f.bob, "admin removal").await?;
    assert!(!social::recovery::delete(&f.pool, f.alice, other, true, 30).await?);
    assert!(
        social::recovery::restore(&f.pool, f.bob, other)
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn backup_verification_rehearses_restore_without_changing_live_data() -> anyhow::Result<()> {
    let f = Fixture::new().await?;
    let image = f.attachment(f.alice, "backup.png", b"backup bytes").await?;
    social::create_post(
        &f.pool,
        &f.settings,
        Some(f.alice),
        "backup content",
        None,
        &[image],
    )
    .await?;
    let paths = f.paths.clone();
    let archive =
        tokio::task::spawn_blocking(move || backup::create_backup(&paths, false)).await??;
    let settings = std::fs::read(&f.paths.settings_path)?;
    let database = std::fs::read(&f.paths.database_path)?;
    let paths = f.paths.clone();
    let test_archive = archive.clone();
    let report =
        tokio::task::spawn_blocking(move || backup::verify_backup(&paths, &test_archive, false))
            .await??;
    assert_eq!(report.schema_version, db::CURRENT_SCHEMA_VERSION);
    assert!(report.files >= 3);
    assert_eq!(std::fs::read(&f.paths.settings_path)?, settings);
    assert_eq!(std::fs::read(&f.paths.database_path)?, database);
    assert_eq!(
        std::fs::read(f.paths.uploads_originals.join("backup.png"))?,
        b"backup bytes"
    );
    let mut corrupt = std::fs::read(&archive)?;
    let needle = b"backup bytes";
    let offset = corrupt
        .windows(needle.len())
        .position(|bytes| bytes == needle)
        .ok_or_else(|| anyhow::anyhow!("missing archive media"))?;
    corrupt[offset] ^= 1;
    let bad = f.directory.path().join("corrupt.tar");
    std::fs::write(&bad, corrupt)?;
    let paths = f.paths.clone();
    assert!(
        tokio::task::spawn_blocking(move || backup::verify_backup(&paths, &bad, false))
            .await?
            .is_err()
    );
    assert_eq!(f.count("posts").await?, 1);
    Ok(())
}

#[tokio::test]
async fn canonical_deletion_transfers_pending_media_work_without_losing_original()
-> anyhow::Result<()> {
    let f = Fixture::new().await?;
    let original = f.attachment(f.alice, "shared.png", b"shared bytes").await?;
    let bob = f.bob;
    let duplicate=f.pool.call(move |conn| {
        conn.execute("INSERT INTO media(owner_user_id,original_filename,stored_path,public_path,mime_type,media_kind,byte_len,conversion_state,canonical_media_id) SELECT ?,'shared-copy.png',stored_path,public_path,mime_type,media_kind,byte_len,'processing',id FROM media WHERE id=?",params![bob,original])?;
        let duplicate=conn.last_insert_rowid();
        conn.execute("INSERT INTO media_work(media_id,status,attempts,lease_token) VALUES(?,'running',1,'old-claim')",[original])?;
        conn.execute("INSERT INTO media_jobs(media_id,status) VALUES(?,'processing')",[original])?;Ok(duplicate)
    }).await?;
    media::delete_media(&f.pool, &f.paths, original).await?;
    f.pool
        .call(move |conn| {
            assert_eq!(
                conn.query_row("SELECT media_id FROM media_work", [], |r| r
                    .get::<_, i64>(0))?,
                duplicate
            );
            assert_eq!(
                conn.query_row("SELECT status FROM media_work", [], |r| r
                    .get::<_, String>(0))?,
                "queued"
            );
            Ok(())
        })
        .await?;
    media::jobs::process_next(&f.pool, &f.settings, &f.paths, &unavailable()).await?;
    assert_eq!(
        std::fs::read(f.paths.uploads_originals.join("shared.png"))?,
        b"shared bytes"
    );
    Ok(())
}

#[tokio::test]
async fn administrative_removal_revokes_an_existing_undo_marker() -> anyhow::Result<()> {
    let f = Fixture::new().await?;
    let image = f
        .attachment(f.bob, "admin-final.png", b"admin removal")
        .await?;
    let post = social::create_post(
        &f.pool,
        &f.settings,
        Some(f.bob),
        "remove finally",
        None,
        &[image],
    )
    .await?;
    assert!(social::recovery::delete(&f.pool, f.bob, post, false, 30).await?);
    social::delete_post(&f.pool, f.alice, post, true).await?;
    assert!(
        social::recovery::restore(&f.pool, f.bob, post)
            .await
            .is_err()
    );
    media::delete_post_media(&f.pool, &f.paths, post).await?;
    assert_eq!(f.count("recoverable_deletions").await?, 0);
    assert!(!f.paths.uploads_originals.join("admin-final.png").exists());
    Ok(())
}

#[tokio::test]
async fn retries_remain_idempotent_after_validation_constraints_change() -> anyhow::Result<()> {
    let mut f = Fixture::new().await?;
    let request = || Some((format!("user:{}", f.alice), "stable-key".to_owned()));
    let post = social::create_post_with_options(
        &f.pool,
        &f.settings,
        Some(f.alice),
        "longer original",
        None,
        &[],
        social::PostOptions {
            request: request(),
            ..Default::default()
        },
    )
    .await?;
    f.settings.posts.max_text_chars = 1;
    let replay = social::create_post_with_options(
        &f.pool,
        &f.settings,
        Some(f.alice),
        "longer original",
        None,
        &[],
        social::PostOptions {
            request: request(),
            ..Default::default()
        },
    )
    .await?;
    assert_eq!(post, replay);
    assert_eq!(f.count("posts").await?, 1);
    f.settings.posts.max_text_chars = 280;
    let input = social::drafts::DraftInput {
        text: "scheduled request".into(),
        scheduled_at: Some(chrono::Utc::now().timestamp()),
        client_key: "stable-draft-key".into(),
        ..Default::default()
    };
    let draft = social::drafts::save(&f.pool, &f.settings, f.alice, input.clone()).await?;
    let stale = social::drafts::DraftInput {
        scheduled_at: Some(chrono::Utc::now().timestamp() - 100),
        ..input
    };
    assert_eq!(
        social::drafts::save(&f.pool, &f.settings, f.alice, stale).await?,
        draft
    );
    assert_eq!(f.count("drafts").await?, 1);
    Ok(())
}

#[tokio::test]
async fn interrupted_final_media_attempt_stays_bounded() -> anyhow::Result<()> {
    let f = Fixture::new().await?;
    let id = f
        .attachment(f.alice, "final-attempt.png", b"original")
        .await?;
    f.pool.call(move |conn| {
        conn.execute("INSERT INTO media_work(media_id,status,attempts,lease_token) VALUES(?,'running',9,'interrupted')",[id])?;
        Ok(())
    }).await?;
    media::jobs::recover(&f.pool).await?;
    assert!(!media::jobs::process_next(&f.pool, &f.settings, &f.paths, &unavailable()).await?);
    assert!(media::jobs::retry(&f.pool, f.alice, id).await.is_err());
    assert_eq!(
        std::fs::read(f.paths.uploads_originals.join("final-attempt.png"))?,
        b"original"
    );
    Ok(())
}

#[tokio::test]
async fn shared_media_retry_uses_owned_alias_without_exposing_other_uploads() -> anyhow::Result<()>
{
    let f = Fixture::new().await?;
    let original = f
        .attachment(f.alice, "retry-shared.png", b"original")
        .await?;
    let bob = f.bob;
    let alias = f.pool.call(move |conn| {
        conn.execute("INSERT INTO media(owner_user_id,original_filename,stored_path,public_path,mime_type,media_kind,byte_len,conversion_state,canonical_media_id) SELECT ?,'owned-copy.png',stored_path,public_path,mime_type,media_kind,byte_len,'failed',id FROM media WHERE id=?",params![bob,original])?;
        let alias=conn.last_insert_rowid();
        conn.execute("INSERT INTO media_work(media_id,status,attempts) VALUES(?,'failed',3)",[original])?;
        Ok(alias)
    }).await?;
    assert!(media::jobs::retry(&f.pool, f.bob, original).await.is_err());
    media::jobs::retry(&f.pool, f.bob, alias).await?;
    let state = f.pool.call(move |conn| Ok(conn.query_row("SELECT w.status,m.conversion_state FROM media_work w JOIN media m ON m.canonical_media_id=w.media_id WHERE m.id=?",[alias],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?)).await?;
    assert_eq!(state, ("queued".to_owned(), "queued".to_owned()));
    Ok(())
}
