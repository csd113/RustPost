use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context as _;
use chrono::{Duration, Utc};
use rusqlite::params;

use crate::auth;
use crate::config::Settings;
use crate::db::SqlitePool;
use crate::runtime::RuntimePaths;
use crate::social;

const DEMO_PASSWORD: &str = "rustpost demo password";

struct DemoAccount {
    username: &'static str,
    display_name: &'static str,
    bio: &'static str,
    website: &'static str,
    avatar_color: &'static str,
    accent_color: &'static str,
    is_admin: bool,
}

struct DemoIds {
    ada: i64,
    nova: i64,
    milo: i64,
    jun: i64,
    tess: i64,
    omar: i64,
}

struct PostRef {
    id: i64,
    user_id: i64,
}

struct MediaInsert {
    owner_user_id: i64,
    original_filename: String,
    stored_path: PathBuf,
    public_path: String,
    mime_type: &'static str,
    media_kind: &'static str,
    alt_text: String,
    conversion_state: String,
}

pub async fn seed(
    pool: &SqlitePool,
    paths: &RuntimePaths,
    settings: &Settings,
    settings_path: &Path,
) -> anyhow::Result<String> {
    ensure_demo_path(&paths.data_dir)?;
    ensure_empty_users(pool).await?;
    write_demo_settings(settings, settings_path)?;

    let accounts = demo_accounts();
    let ids = create_accounts(pool, settings, paths, &accounts).await?;
    create_social_graph(pool, &ids).await?;
    let posts = create_posts(pool, settings, paths, &ids).await?;
    create_interactions(pool, &posts).await?;

    Ok(format!(
        "seeded RustPost demo at {}\naccounts: ada, nova, milo, jun, tess, omar\npassword: {DEMO_PASSWORD}",
        paths.data_dir.display()
    ))
}

fn ensure_demo_path(data_dir: &Path) -> anyhow::Result<()> {
    let path = data_dir.to_string_lossy();
    if !path.contains("target/debug/") || !path.contains("rustpost-demo") {
        anyhow::bail!("seed-demo only writes to a target/debug/rustpost-demo data directory");
    }
    Ok(())
}

async fn ensure_empty_users(pool: &SqlitePool) -> anyhow::Result<()> {
    let count = pool
        .call(|conn| {
            conn.query_row("SELECT COUNT(*) FROM users", [], |row| row.get::<_, i64>(0))
                .map_err(Into::into)
        })
        .await?;
    if count != 0 {
        anyhow::bail!("demo database already has users; remove the demo data directory first");
    }
    Ok(())
}

fn write_demo_settings(settings: &Settings, settings_path: &Path) -> anyhow::Result<()> {
    let mut demo = settings.clone();
    "RustPost Demo".clone_into(&mut demo.site.name);
    demo.server.port = 8098;
    demo.media.keep_original_uploads = true;
    fs::write(settings_path, toml::to_string_pretty(&demo)?)?;
    Ok(())
}

fn demo_accounts() -> Vec<DemoAccount> {
    vec![
        DemoAccount {
            username: "ada",
            display_name: "Ada Byte",
            bio: "Systems programmer sharing small, sharp notes on Rust services and observability.",
            website: "https://example.local/ada",
            avatar_color: "#163b2f",
            accent_color: "#b8d8c0",
            is_admin: false,
        },
        DemoAccount {
            username: "nova",
            display_name: "Nova Fields",
            bio: "Photographer chasing honest light, quiet streets, and useful metadata.",
            website: "https://example.local/nova",
            avatar_color: "#6b4f9f",
            accent_color: "#d8c7ff",
            is_admin: false,
        },
        DemoAccount {
            username: "milo",
            display_name: "Milo Reed",
            bio: "Indie maker building small tools in public, one careful release at a time.",
            website: "https://example.local/milo",
            avatar_color: "#ad5c35",
            accent_color: "#ffd0a8",
            is_admin: false,
        },
        DemoAccount {
            username: "jun",
            display_name: "Jun Park",
            bio: "UI designer focused on calm workflows, readable states, and boringly good forms.",
            website: "https://example.local/jun",
            avatar_color: "#245f8b",
            accent_color: "#b9dcf4",
            is_admin: false,
        },
        DemoAccount {
            username: "tess",
            display_name: "Tess Vale",
            bio: "Video creator testing short-form publishing, captions, and media previews.",
            website: "https://example.local/tess",
            avatar_color: "#9b3f5f",
            accent_color: "#ffc1d5",
            is_admin: false,
        },
        DemoAccount {
            username: "omar",
            display_name: "Omar Stone",
            bio: "Infrastructure operator watching backups, restore drills, and quiet admin surfaces.",
            website: "https://example.local/omar",
            avatar_color: "#4f5f36",
            accent_color: "#d6e0a6",
            is_admin: true,
        },
    ]
}

async fn create_accounts(
    pool: &SqlitePool,
    settings: &Settings,
    paths: &RuntimePaths,
    accounts: &[DemoAccount],
) -> anyhow::Result<DemoIds> {
    let mut ids = Vec::with_capacity(accounts.len());
    for account in accounts {
        let user_id = auth::register_user(
            pool,
            settings,
            account.username,
            DEMO_PASSWORD,
            account.is_admin,
        )
        .await?;
        let avatar = create_svg_media(
            pool,
            paths,
            user_id,
            &format!("avatar-{}.svg", account.username),
            &avatar_svg(
                account.display_name,
                account.avatar_color,
                account.accent_color,
            ),
            "avatar",
            &format!("{} profile picture", account.display_name),
        )
        .await?;
        let banner = create_svg_media(
            pool,
            paths,
            user_id,
            &format!("banner-{}.svg", account.username),
            &banner_svg(
                account.display_name,
                account.avatar_color,
                account.accent_color,
            ),
            "banner",
            &format!("{} profile banner", account.display_name),
        )
        .await?;
        update_profile(pool, user_id, account, avatar, banner).await?;
        ids.push(user_id);
    }
    Ok(DemoIds {
        ada: ids[0],
        nova: ids[1],
        milo: ids[2],
        jun: ids[3],
        tess: ids[4],
        omar: ids[5],
    })
}

async fn update_profile(
    pool: &SqlitePool,
    user_id: i64,
    account: &DemoAccount,
    avatar: i64,
    banner: i64,
) -> anyhow::Result<()> {
    let display_name = account.display_name.to_owned();
    let bio = account.bio.to_owned();
    let website = account.website.to_owned();
    pool.call(move |conn| {
        conn.execute(
            "UPDATE users SET display_name = ?, bio = ?, website = ?, profile_picture_media_id = ?, banner_media_id = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
            params![display_name, bio, website, avatar, banner, user_id],
        )?;
        Ok(())
    })
    .await
}

async fn create_social_graph(pool: &SqlitePool, ids: &DemoIds) -> anyhow::Result<()> {
    for (follower, followed) in [
        (ids.ada, ids.nova),
        (ids.ada, ids.jun),
        (ids.nova, ids.tess),
        (ids.nova, ids.ada),
        (ids.milo, ids.ada),
        (ids.milo, ids.jun),
        (ids.jun, ids.nova),
        (ids.jun, ids.milo),
        (ids.tess, ids.nova),
        (ids.tess, ids.omar),
        (ids.omar, ids.ada),
        (ids.omar, ids.tess),
    ] {
        social::follow(pool, follower, followed).await?;
    }
    Ok(())
}

// The seed content is intentionally kept in one ordered block so the demo feed
// reads naturally and post IDs stay easy to map to screenshots.
#[expect(
    clippy::too_many_lines,
    reason = "demo post fixtures stay ordered so IDs and screenshot content remain stable"
)]
async fn create_posts(
    pool: &SqlitePool,
    settings: &Settings,
    paths: &RuntimePaths,
    ids: &DemoIds,
) -> anyhow::Result<Vec<PostRef>> {
    let image_morning = create_svg_media(
        pool,
        paths,
        ids.nova,
        "post-morning-light.svg",
        &post_image_svg("Morning light study", "#6b4f9f", "#f5d76e"),
        "images",
        "abstract morning light photo placeholder",
    )
    .await?;
    let image_board = create_svg_media(
        pool,
        paths,
        ids.jun,
        "post-interface-board.svg",
        &post_image_svg("Interface board", "#245f8b", "#9fd0ff"),
        "images",
        "interface board placeholder",
    )
    .await?;
    let image_launch = create_svg_media(
        pool,
        paths,
        ids.milo,
        "post-launch-notes.svg",
        &post_image_svg("Launch notes", "#ad5c35", "#ffd0a8"),
        "images",
        "launch notes placeholder",
    )
    .await?;
    let video_clip =
        create_video_media(pool, paths, ids.tess, "demo-video-cut.webm", "Demo cut").await?;
    let video_health = create_video_media(
        pool,
        paths,
        ids.omar,
        "demo-health-loop.webm",
        "Health loop",
    )
    .await?;

    let mut posts = Vec::new();
    let top_level = [
        (
            ids.ada,
            "Spent the morning shaving allocations from the timeline query. The nice part: the code got easier to read after the temporary Vec went away. #rust",
            vec![],
        ),
        (
            ids.nova,
            "Three frames from a dawn walk. I like when a demo feed can show media without pretending every photo is perfect.",
            vec![image_morning],
        ),
        (
            ids.milo,
            "Shipping a tiny changelog panel today. The trick is keeping the empty state useful before the first real update lands.",
            vec![],
        ),
        (
            ids.jun,
            "Buttons should reveal intent before color does. Icon, label, disabled state, then color. In that order.",
            vec![image_board],
        ),
        (
            ids.tess,
            "Testing a short clip upload with captions planned next. A good media card should be quiet until someone presses play.",
            vec![video_clip],
        ),
        (
            ids.omar,
            "Demo ops note: backups are only useful if restore has been rehearsed. The admin page should make that boring.",
            vec![video_health],
        ),
        (
            ids.ada,
            "SQLite WAL plus small transactions keeps this kind of single-instance app surprisingly comfortable.",
            vec![],
        ),
        (
            ids.nova,
            "Metadata habit: write the alt text while the image is still fresh in your head.",
            vec![],
        ),
        (
            ids.milo,
            "Launch checklist: create account, post, reply, bookmark, restore backup, then walk away for coffee.",
            vec![image_launch],
        ),
        (
            ids.jun,
            "I keep coming back to server-rendered HTML for tools where the content is already the product.",
            vec![],
        ),
        (
            ids.tess,
            "Short videos need good surrounding text. Otherwise the feed becomes a wall of mystery rectangles.",
            vec![],
        ),
        (
            ids.omar,
            "Rate limits should fail clearly. A confusing rejection is an incident waiting to happen.",
            vec![],
        ),
        (
            ids.ada,
            "New thread: what should a tiny self-hosted social app optimize for first?",
            vec![],
        ),
        (
            ids.nova,
            "Answer from the photo corner: media that loads predictably on hotel Wi-Fi.",
            vec![],
        ),
        (
            ids.milo,
            "Answer from maker land: a setup path you can explain in one terminal pane.",
            vec![],
        ),
        (
            ids.jun,
            "Answer from design: forms that keep their promises, especially after validation fails.",
            vec![],
        ),
        (
            ids.tess,
            "Answer from video: uploads that make progress obvious and playback unremarkable.",
            vec![],
        ),
        (
            ids.omar,
            "Answer from ops: a database file you understand and a backup you have actually opened.",
            vec![],
        ),
    ];
    for (index, (user_id, text, media_ids)) in top_level.into_iter().enumerate() {
        let id = social::create_post(pool, settings, Some(user_id), text, None, &media_ids).await?;
        set_post_created_at(pool, id, i64::try_from(90 - index * 3)?).await?;
        posts.push(PostRef { id, user_id });
    }

    let root = posts[12].id;
    for (index, (user_id, text)) in [
        (ids.jun, "For me it is trust in the interface. If the controls feel consistent, people explore more."),
        (ids.ada, "Agreed. Consistency also makes the server handlers easier to reason about."),
        (ids.omar, "And easier to audit. Predictable flows produce cleaner logs."),
        (ids.nova, "Plus screenshots tell the story faster when the same patterns repeat."),
        (ids.tess, "The video path benefits too. One attachment model, many media types."),
    ]
    .into_iter()
    .enumerate()
    {
        let reply_id = social::create_post(pool, settings, Some(user_id), text, Some(root), &[]).await?;
        set_post_created_at(pool, reply_id, i64::try_from(30 - index * 2)?).await?;
        posts.push(PostRef { id: reply_id, user_id });
    }

    for (index, (parent, user_id, text)) in [
        (
            posts[1].id,
            ids.jun,
            "The first image reads well in the feed. Nice contrast without shouting.",
        ),
        (
            posts[2].id,
            ids.ada,
            "That changelog idea would pair well with a backup reminder.",
        ),
        (
            posts[3].id,
            ids.milo,
            "Stealing this order for my settings page checklist.",
        ),
        (
            posts[4].id,
            ids.nova,
            "The clip thumbnail feels clear even before playback.",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let reply_id =
            social::create_post(pool, settings, Some(user_id), text, Some(parent), &[]).await?;
        set_post_created_at(pool, reply_id, i64::try_from(20 - index * 2)?).await?;
        posts.push(PostRef {
            id: reply_id,
            user_id,
        });
    }
    Ok(posts)
}

async fn create_interactions(pool: &SqlitePool, posts: &[PostRef]) -> anyhow::Result<()> {
    for (user_id, post_index) in [
        (posts[1].user_id, 0),
        (posts[2].user_id, 0),
        (posts[3].user_id, 1),
        (posts[4].user_id, 1),
        (posts[5].user_id, 2),
        (posts[0].user_id, 3),
        (posts[1].user_id, 4),
        (posts[2].user_id, 5),
        (posts[3].user_id, 6),
        (posts[4].user_id, 8),
        (posts[5].user_id, 12),
        (posts[0].user_id, 13),
    ] {
        social::like(pool, user_id, posts[post_index].id).await?;
    }
    for (user_id, post_index) in [
        (posts[2].user_id, 1),
        (posts[3].user_id, 0),
        (posts[4].user_id, 3),
        (posts[5].user_id, 4),
        (posts[0].user_id, 5),
        (posts[1].user_id, 8),
    ] {
        social::repost(pool, user_id, posts[post_index].id).await?;
    }
    for (user_id, post_index) in [
        (posts[0].user_id, 1),
        (posts[1].user_id, 4),
        (posts[2].user_id, 12),
        (posts[3].user_id, 8),
        (posts[5].user_id, 0),
    ] {
        social::bookmark(pool, user_id, posts[post_index].id).await?;
    }
    Ok(())
}

async fn set_post_created_at(
    pool: &SqlitePool,
    post_id: i64,
    minutes_ago: i64,
) -> anyhow::Result<()> {
    let created_at = (Utc::now() - Duration::minutes(minutes_ago))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    pool.call(move |conn| {
        conn.execute(
            "UPDATE posts SET created_at = ? WHERE id = ?",
            params![created_at, post_id],
        )?;
        Ok(())
    })
    .await
}

async fn create_svg_media(
    pool: &SqlitePool,
    paths: &RuntimePaths,
    owner_user_id: i64,
    filename: &str,
    svg: &str,
    folder: &str,
    alt_text: &str,
) -> anyhow::Result<i64> {
    let stored_path = paths.uploads_images.join(filename);
    fs::write(&stored_path, svg)?;
    insert_media(
        pool,
        MediaInsert {
            owner_user_id,
            original_filename: filename.to_owned(),
            stored_path,
            public_path: format!("/uploads/images/{filename}"),
            mime_type: "image/svg+xml",
            media_kind: "image",
            alt_text: alt_text.to_owned(),
            conversion_state: folder.to_owned(),
        },
    )
    .await
}

async fn create_video_media(
    pool: &SqlitePool,
    paths: &RuntimePaths,
    owner_user_id: i64,
    filename: &str,
    label: &str,
) -> anyhow::Result<i64> {
    let stored_path = paths.uploads_videos.join(filename);
    let filter = format!(
        "color=c=#172017:s=960x540:d=2,drawbox=x=60:y=60:w=840:h=420:color=#dfe4dc@0.28:t=fill,drawtext=text='{}':fontcolor=white:fontsize=56:x=(w-text_w)/2:y=(h-text_h)/2",
        label.replace(':', " ")
    );
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            &filter,
            "-an",
            "-c:v",
            "libvpx-vp9",
            "-pix_fmt",
            "yuv420p",
            stored_path
                .to_str()
                .context("demo video path is not valid UTF-8")?,
        ])
        .status()
        .context("failed to run ffmpeg for demo video")?;
    if !status.success() {
        anyhow::bail!("ffmpeg failed while creating demo video");
    }
    insert_media(
        pool,
        MediaInsert {
            owner_user_id,
            original_filename: filename.to_owned(),
            stored_path,
            public_path: format!("/uploads/videos/{filename}"),
            mime_type: "video/webm",
            media_kind: "video",
            alt_text: label.to_owned(),
            conversion_state: "videos".to_owned(),
        },
    )
    .await
}

async fn insert_media(pool: &SqlitePool, media: MediaInsert) -> anyhow::Result<i64> {
    let byte_len = i64::try_from(fs::metadata(&media.stored_path)?.len())?;
    let stored_path = media.stored_path.to_string_lossy().to_string();
    pool.call(move |conn| {
        conn.execute(
            "INSERT INTO media (owner_user_id, original_filename, stored_path, public_path, mime_type, media_kind, byte_len, alt_text, conversion_state) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                media.owner_user_id,
                media.original_filename,
                stored_path,
                media.public_path,
                media.mime_type,
                media.media_kind,
                byte_len,
                media.alt_text,
                media.conversion_state
            ],
        )?;
        Ok(conn.last_insert_rowid())
    })
    .await
}

fn avatar_svg(name: &str, background: &str, accent: &str) -> String {
    let initials = initials(name);
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 256 256"><rect width="256" height="256" rx="40" fill="{background}"/><circle cx="196" cy="58" r="42" fill="{accent}" opacity=".85"/><path d="M32 202c48-58 109-68 192-37v59H32z" fill="{accent}" opacity=".35"/><text x="128" y="148" text-anchor="middle" font-family="Inter,Arial,sans-serif" font-size="72" font-weight="800" fill="#fff">{initials}</text></svg>"##
    )
}

fn banner_svg(name: &str, background: &str, accent: &str) -> String {
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1200 360"><rect width="1200" height="360" fill="{background}"/><path d="M0 260 C210 150 360 360 600 240 S1010 120 1200 210v150H0z" fill="{accent}" opacity=".55"/><circle cx="960" cy="90" r="86" fill="{accent}" opacity=".35"/><text x="80" y="116" font-family="Inter,Arial,sans-serif" font-size="52" font-weight="800" fill="#fff">{name}</text></svg>"##
    )
}

fn post_image_svg(title: &str, background: &str, accent: &str) -> String {
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1200 760"><rect width="1200" height="760" fill="{background}"/><rect x="72" y="72" width="1056" height="616" rx="28" fill="#fff" opacity=".12"/><circle cx="920" cy="210" r="130" fill="{accent}" opacity=".8"/><path d="M72 560 L300 350 L465 480 L620 280 L1128 630 V688 H72z" fill="{accent}" opacity=".52"/><text x="96" y="150" font-family="Inter,Arial,sans-serif" font-size="58" font-weight="800" fill="#fff">{title}</text></svg>"##
    )
}

fn initials(name: &str) -> String {
    let mut output = String::new();
    for part in name.split_whitespace().take(2) {
        if let Some(character) = part.chars().next() {
            let _ = write!(output, "{character}");
        }
    }
    output
}
