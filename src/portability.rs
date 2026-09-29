//! Account export and import archives.
//!
//! `RustPost` user archives are distinct from full-instance backups: they carry
//! one account's posts, profile data, media, outgoing follows, muted words and
//! media bytes, and are imported into an authenticated destination account.
//! Password hashes, sessions, tokens, administrative flags, instance
//! configuration, notifications, interactions (likes/bookmarks/reposts), and
//! other accounts' data are never included.
//!
//! The on-disk format is a gzip-compressed tar archive:
//!
//! ```text
//! manifest.json   { format_version, app, created_at, archive_id, username, entry_count }
//! profile.json    { display_name, bio, location, website, theme, nsfw_blur_enabled,
//!                   liked_posts_public, follow_approval_required, created_at }
//! posts.json      [ { id, text, created_at, edited_at?, parent_id?, root_id?,
//!                     quote_id?, media_ids, embeds } ]
//! media.json      [ { id, original_filename, mime_type, media_kind, byte_len,
//!                     alt_text, is_nsfw, conversion_state, sha256, archive_path } ]
//! follows.json    [ { username, created_at } ]
//! settings.json   { muted_words: [ { term, created_at } ] }
//! media/<n>.<ext> one entry per listed media document, bytes included
//! ```
//!
//! Every entry is validated before any database or filesystem mutation. Media
//! bytes are streamed and hashed, never buffered whole; a bounded prefix is
//! sniffed with `infer` and must match the declared mime type. The compressed
//! size, entry count, decompressed media total, and cumulative document bytes
//! are all enforced incrementally while reading, so a compressed bomb cannot
//! consume unbounded disk or memory. Imports apply in one database transaction
//! plus a staged-file install: media files are copied to the destination
//! upload directories first, the transaction commits the database rows, and
//! copied files are removed again if anything fails.
//!
//! Behaviour decisions worth knowing:
//!
//! * Posts are exported only when authored by the account and not deleted.
//!   Reply/root/quote references are kept only when the referenced post is
//!   also part of the export, otherwise the reference is dropped (export) or
//!   set to `NULL` (import).
//! * Media is exported for media rows owned by the account that are attached
//!   to an exported post. `sha256` and `byte_len` are computed from the bytes
//!   actually written to the archive, not from the source row.
//! * Import fills empty destination profile text fields (`display_name`,
//!   `bio`, `location`, `website`) from the archive and always applies the
//!   archive's preference fields (theme, NSFW blur, public likes, follow
//!   approval). Conflicts are counted in [`ImportReport`].
//! * Imported follows resolve against the destination instance by normalized
//!   handle. Missing, deleted, suspended, self, and blocked (either direction)
//!   targets are skipped; protected targets always produce a pending
//!   `follow_requests` row and can never be followed directly.
//! * Imported media keeps the existing content-dedupe invariants: a matching
//!   canonical row is reused (no second copy) via `canonical_media_id`.
//! * Usernames, password hashes, administrative flags, suspension, and
//!   deletion state are never written from an archive.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::Context as _;
use chrono::{SecondsFormat, Utc};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use rusqlite::{OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tar::{Archive, Builder, EntryType, Header};
use uuid::Uuid;

use crate::config::Settings;
use crate::db::SqlitePool;
use crate::runtime::RuntimePaths;
use crate::validation::{clean_post_text, normalize_username, validate_profile_text};

/// Current archive format version. Importers reject other versions.
pub const FORMAT_VERSION: u32 = 1;
/// Manifest path inside the archive.
pub const MANIFEST_PATH: &str = "manifest.json";
/// Hard ceiling for one archive, matching the instance upload body limit.
pub const MAX_ARCHIVE_BYTES: u64 = 300 * 1024 * 1024;
/// Hard ceiling for the number of archive entries.
pub const MAX_ENTRIES: usize = 10_000;
/// Prefix used for staged export files under `RuntimePaths::tmp_dir`.
pub const EXPORT_TMP_PREFIX: &str = "account-export-";
/// Prefix used for staged import uploads under `RuntimePaths::tmp_dir`.
pub const IMPORT_TMP_PREFIX: &str = "account-import-";

const APP_NAME: &str = "rustpost";
const MEDIA_PREFIX: &str = "media/";
const MEDIA_DIR: &str = "media";
const PROFILE_PATH: &str = "profile.json";
const POSTS_PATH: &str = "posts.json";
const MEDIA_PATH: &str = "media.json";
const FOLLOWS_PATH: &str = "follows.json";
const SETTINGS_PATH: &str = "settings.json";
const DOCUMENT_PATHS: [&str; 6] = [
    MANIFEST_PATH,
    PROFILE_PATH,
    POSTS_PATH,
    MEDIA_PATH,
    FOLLOWS_PATH,
    SETTINGS_PATH,
];

/// Independent limits applied to account archive export and import.
///
/// `compressed_bytes` bounds the archive file itself (upload and export),
/// while `expanded_bytes` bounds the total decompressed media content. The two
/// limits are deliberately independent: raising the upload ceiling never
/// raises the decompressed ceiling, and a tiny compressed archive can never
/// expand past `expanded_bytes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveLimits {
    pub compressed_bytes: u64,
    pub expanded_bytes: u64,
    pub entries: usize,
}

impl ArchiveLimits {
    #[must_use]
    pub fn from_settings(settings: &Settings) -> Self {
        Self {
            compressed_bytes: settings.accounts.max_archive_upload_bytes,
            expanded_bytes: settings.accounts.max_archive_expanded_bytes,
            entries: settings.accounts.max_archive_entries,
        }
    }
}

/// Records that must be duplicated rather than silently truncated.
const MAX_POSTS: usize = 5_000;
const MAX_MEDIA: usize = 1_000;
const MAX_FOLLOWS: usize = 5_000;
const MAX_MUTED_WORDS: usize = 500;
const MAX_EMBEDS_PER_POST: usize = 32;
const MAX_ALT_TEXT_CHARS: usize = 4_096;
const MAX_ORIGINAL_FILENAME_CHARS: usize = 255;
const MAX_CONVERSION_STATE_CHARS: usize = 64;
const MAX_EMBED_TITLE_CHARS: usize = 512;
const MAX_EMBED_URL_CHARS: usize = 2_048;
const MAX_EMBED_VIDEO_ID_CHARS: usize = 128;
const MAX_LOCATION_CHARS: usize = 100;
const MAX_WEBSITE_CHARS: usize = 2_048;
const MAX_TIMESTAMP_CHARS: usize = 64;
const MANIFEST_MAX_BYTES: u64 = 256 * 1024;
const DOCUMENT_MAX_BYTES: u64 = 16 * 1024 * 1024;
/// Cumulative ceiling for every JSON document in one archive, in addition to
/// the per-document caps. Six maximum-size documents would otherwise hold up
/// to 96 MiB in memory at once.
const MAX_TOTAL_DOCUMENT_BYTES: u64 = 64 * 1024 * 1024;
/// Number of leading media bytes collected while streaming extraction so the
/// `infer` crate can check the content type without buffering whole entries.
const MEDIA_SNIFF_PREFIX_BYTES: usize = 512;
/// Longest accepted media entry file name, in bytes. Longer names are rejected
/// because they cannot be represented safely on common filesystems.
const MAX_MEDIA_FILE_NAME_BYTES: usize = 255;
const HASH_PREFIX_LEN: usize = 16;
const COPY_BUFFER_BYTES: usize = 64 * 1024;
const ALREADY_IMPORTED_MESSAGE: &str = "this archive has already been imported into this account";

/// `SQLite` extended result codes for the `UNIQUE(user_id, archive_id)`
/// constraint on `account_imports`.
const SQLITE_CONSTRAINT_UNIQUE: i32 = 2_067;
const SQLITE_CONSTRAINT_PRIMARYKEY: i32 = 1_555;

const FOLLOW_NOTIFICATION_MESSAGE: &str = "followed you";
const FOLLOW_REQUEST_NOTIFICATION_MESSAGE: &str = "requested to follow you";

/// Result of writing one account archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportReport {
    pub archive_path: PathBuf,
    pub archive_id: String,
    pub bytes: u64,
    pub posts: usize,
    pub media: usize,
    pub follows: usize,
    pub muted_words: usize,
}

/// Result of importing one account archive into a destination account.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportReport {
    pub archive_id: String,
    pub archive_username: String,
    pub posts_imported: usize,
    pub media_imported: usize,
    pub follows_imported: usize,
    pub follows_pending: usize,
    pub follows_skipped: usize,
    /// Reply/root/quote references in the archive that pointed at posts which
    /// are not part of the archive and were therefore dropped.
    pub post_references_dropped: usize,
    pub muted_words_imported: usize,
    pub profile_fields_applied: usize,
    pub profile_fields_skipped: usize,
}

// ---------------------------------------------------------------------------
// Archive documents
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManifestDocument {
    format_version: u32,
    app: String,
    created_at: String,
    archive_id: String,
    username: String,
    entry_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProfileDocument {
    display_name: String,
    bio: String,
    location: String,
    website: String,
    theme: String,
    nsfw_blur_enabled: bool,
    liked_posts_public: bool,
    follow_approval_required: bool,
    created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PostDocument {
    id: i64,
    text: String,
    created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    edited_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    root_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    quote_id: Option<i64>,
    #[serde(default)]
    media_ids: Vec<i64>,
    #[serde(default)]
    embeds: Vec<EmbedDocument>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EmbedDocument {
    provider: String,
    video_id: String,
    original_url: String,
    canonical_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    thumbnail_url: String,
    embed_url: String,
    position: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MediaDocument {
    id: i64,
    original_filename: String,
    mime_type: String,
    media_kind: String,
    byte_len: u64,
    alt_text: String,
    is_nsfw: bool,
    conversion_state: String,
    sha256: String,
    archive_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FollowDocument {
    username: String,
    created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SettingsDocument {
    muted_words: Vec<MutedWordDocument>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MutedWordDocument {
    term: String,
    created_at: String,
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

/// Writes a complete archive for `user_id` into `destination`.
///
/// `destination` must name a file inside an existing directory; the archive is
/// written to a temporary sibling and renamed into place, so a failed export
/// never leaves a partial file. A `..` component, a symlink, a non-file
/// destination, or a missing parent directory is rejected. The caller usually
/// stages under `RuntimePaths::tmp_dir` using [`EXPORT_TMP_PREFIX`].
#[expect(
    clippy::too_many_lines,
    reason = "export builds the manifest, documents, and media entries in one ordered pass"
)]
pub async fn export_account(
    pool: &SqlitePool,
    paths: &RuntimePaths,
    user_id: i64,
    destination: &Path,
    limits: ArchiveLimits,
) -> anyhow::Result<ExportReport> {
    let destination_dir = validate_export_destination(destination)?;
    let snapshot = load_export_snapshot(pool, user_id).await?;

    let mut media_plans = Vec::with_capacity(snapshot.media.len());
    let mut media_archive_ids: HashMap<i64, i64> = HashMap::with_capacity(snapshot.media.len());
    for (index, media) in snapshot.media.iter().enumerate() {
        let archive_id = i64::try_from(index)?;
        let source = validate_export_media_source(
            paths,
            &media.media_kind,
            &media.mime_type,
            &media.stored_path,
        )?;
        let (byte_len, sha256) = hash_file_streaming(&source)?;
        let extension = export_media_extension(&media.mime_type, &media.media_kind);
        media_archive_ids.insert(media.id, archive_id);
        media_plans.push(ExportMediaPlan {
            archive_id,
            archive_path: format!("{MEDIA_PREFIX}{index}.{extension}"),
            source,
            byte_len,
            sha256,
            original_filename: media.original_filename.clone(),
            mime_type: media.mime_type.clone(),
            media_kind: media.media_kind.clone(),
            alt_text: media.alt_text.clone(),
            is_nsfw: media.is_nsfw,
            conversion_state: media.conversion_state.clone(),
        });
    }

    let post_ids = snapshot
        .posts
        .iter()
        .map(|post| post.id)
        .collect::<BTreeSet<_>>();
    let mut posts = Vec::with_capacity(snapshot.posts.len());
    let mut dropped_references = 0usize;
    for post in &snapshot.posts {
        let parent_id = keep_exported_reference(post.parent_post_id, &post_ids);
        let root_id = keep_exported_reference(post.root_post_id, &post_ids);
        let quote_id = keep_exported_reference(post.quote_post_id, &post_ids);
        dropped_references += usize::from(post.parent_post_id.is_some() && parent_id.is_none())
            + usize::from(post.root_post_id.is_some() && root_id.is_none())
            + usize::from(post.quote_post_id.is_some() && quote_id.is_none());
        posts.push(PostDocument {
            id: post.id,
            text: post.text.clone(),
            created_at: post.created_at.clone(),
            edited_at: post.edited_at.clone(),
            parent_id,
            root_id,
            quote_id,
            media_ids: post
                .media_ids
                .iter()
                .filter_map(|id| media_archive_ids.get(id).copied())
                .collect(),
            embeds: post.embeds.clone(),
        });
    }
    if dropped_references > 0 {
        // `ExportReport` has no counter for this, so record it in the log.
        tracing::info!(
            user_id,
            dropped_references,
            "account export dropped post references whose target is not part of the archive"
        );
    }

    let entry_count = DOCUMENT_PATHS.len() + media_plans.len();
    if entry_count > limits.entries {
        anyhow::bail!(
            "account archive would contain {entry_count} entries; the limit is {}",
            limits.entries
        );
    }
    let manifest = ManifestDocument {
        format_version: FORMAT_VERSION,
        app: APP_NAME.to_owned(),
        created_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        archive_id: Uuid::new_v4().to_string(),
        username: snapshot.username.clone(),
        entry_count,
    };
    let settings_document = snapshot.muted_words_document();
    let documents = ExportDocuments {
        manifest: &manifest,
        profile: &snapshot.profile,
        posts: &posts,
        follows: &snapshot.follows,
        settings: &settings_document,
        media_plans: &media_plans,
    };
    let bytes = write_export_archive(destination, &destination_dir, &documents, limits)?;

    Ok(ExportReport {
        archive_path: destination.to_path_buf(),
        archive_id: manifest.archive_id,
        bytes,
        posts: posts.len(),
        media: media_plans.len(),
        follows: snapshot.follows.len(),
        muted_words: snapshot.muted_words.len(),
    })
}

/// Rejects destinations that are not a plain file name inside an existing
/// directory, and returns that parent directory.
fn validate_export_destination(destination: &Path) -> anyhow::Result<PathBuf> {
    if destination
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        anyhow::bail!("export destination must not contain '..' components");
    }
    let Some(file_name) = destination.file_name().and_then(|name| name.to_str()) else {
        anyhow::bail!("export destination must be a plain file name");
    };
    if file_name.is_empty() || file_name == "." || file_name == ".." {
        anyhow::bail!("export destination must be a plain file name");
    }
    if file_name.contains(['/', '\\', ':']) || file_name.chars().any(char::is_control) {
        anyhow::bail!("export destination file name contains unsupported characters");
    }
    match fs::symlink_metadata(destination) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                anyhow::bail!("export destination must not be a symbolic link");
            }
            if !metadata.is_file() {
                anyhow::bail!("export destination already exists and is not a regular file");
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to inspect export destination {}",
                    destination.display()
                )
            });
        }
    }
    let parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let metadata = fs::metadata(&parent).with_context(|| {
        format!(
            "export destination directory {} does not exist",
            parent.display()
        )
    })?;
    if !metadata.is_dir() {
        anyhow::bail!("export destination parent is not a directory");
    }
    Ok(parent)
}

/// Keeps a post reference only when the referenced post is also exported.
fn keep_exported_reference(reference: Option<i64>, exported: &BTreeSet<i64>) -> Option<i64> {
    reference.filter(|id| exported.contains(id))
}

struct ExportSnapshot {
    username: String,
    profile: ProfileDocument,
    posts: Vec<ExportPostRow>,
    media: Vec<ExportMediaRow>,
    follows: Vec<FollowDocument>,
    muted_words: Vec<MutedWordDocument>,
}

impl ExportSnapshot {
    fn muted_words_document(&self) -> SettingsDocument {
        SettingsDocument {
            muted_words: self.muted_words.clone(),
        }
    }
}

struct ExportPostRow {
    id: i64,
    text: String,
    created_at: String,
    edited_at: Option<String>,
    parent_post_id: Option<i64>,
    root_post_id: Option<i64>,
    quote_post_id: Option<i64>,
    media_ids: Vec<i64>,
    embeds: Vec<EmbedDocument>,
}

struct ExportMediaRow {
    id: i64,
    original_filename: String,
    mime_type: String,
    media_kind: String,
    alt_text: String,
    is_nsfw: bool,
    conversion_state: String,
    stored_path: String,
}

/// Reads every exportable record in one consistent snapshot. Only non-deleted
/// posts authored by the account are considered, and media is limited to rows
/// owned by the account that are attached to one of those posts.
#[expect(
    clippy::too_many_lines,
    reason = "one consistent read of every exportable record family keeps the snapshot coherent"
)]
async fn load_export_snapshot(pool: &SqlitePool, user_id: i64) -> anyhow::Result<ExportSnapshot> {
    pool.call(move |conn| {
        let tx = conn.transaction()?;
        let account = tx
            .query_row(
                r#"
                SELECT username, display_name, bio, location, website, theme,
                  nsfw_blur_enabled, liked_posts_public, follow_approval_required, created_at
                FROM users WHERE id = ? AND is_deleted = 0
                "#,
                [user_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, i64>(6)? != 0,
                        row.get::<_, i64>(7)? != 0,
                        row.get::<_, i64>(8)? != 0,
                        row.get::<_, String>(9)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            username,
            display_name,
            bio,
            location,
            website,
            theme,
            nsfw_blur_enabled,
            liked_posts_public,
            follow_approval_required,
            created_at,
        )) = account
        else {
            anyhow::bail!("account does not exist or has been deleted");
        };
        let profile = ProfileDocument {
            display_name,
            bio,
            location,
            website,
            theme,
            nsfw_blur_enabled,
            liked_posts_public,
            follow_approval_required,
            created_at,
        };

        let mut posts = {
            let mut statement = tx.prepare(
                r#"
                SELECT id, text, created_at, edited_at, parent_post_id, root_post_id, quote_post_id
                FROM posts WHERE user_id = ? AND is_deleted = 0 ORDER BY id ASC
                "#,
            )?;
            statement
                .query_map([user_id], |row| {
                    Ok(ExportPostRow {
                        id: row.get(0)?,
                        text: row.get(1)?,
                        created_at: row.get(2)?,
                        edited_at: row.get(3)?,
                        parent_post_id: row.get(4)?,
                        root_post_id: row.get(5)?,
                        quote_post_id: row.get(6)?,
                        media_ids: Vec::new(),
                        embeds: Vec::new(),
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        let post_index: HashMap<i64, usize> = posts
            .iter()
            .enumerate()
            .map(|(index, post)| (post.id, index))
            .collect();
        {
            let mut statement = tx.prepare(
                r#"
                SELECT pm.post_id, pm.media_id
                FROM post_media pm
                JOIN posts p ON p.id = pm.post_id
                WHERE p.user_id = ? AND p.is_deleted = 0
                ORDER BY pm.post_id ASC, pm.position ASC, pm.media_id ASC
                "#,
            )?;
            for row in statement.query_map([user_id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
            })? {
                let (post_id, media_id) = row?;
                if let Some(index) = post_index.get(&post_id) {
                    posts[*index].media_ids.push(media_id);
                }
            }
        }
        {
            let mut statement = tx.prepare(
                r#"
                SELECT post_id, provider, video_id, original_url, canonical_url, title,
                  thumbnail_url, embed_url, position
                FROM post_embeds
                WHERE post_id IN (SELECT id FROM posts WHERE user_id = ? AND is_deleted = 0)
                ORDER BY post_id ASC, position ASC
                "#,
            )?;
            for row in statement.query_map([user_id], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    EmbedDocument {
                        provider: row.get(1)?,
                        video_id: row.get(2)?,
                        original_url: row.get(3)?,
                        canonical_url: row.get(4)?,
                        title: row.get(5)?,
                        thumbnail_url: row.get(6)?,
                        embed_url: row.get(7)?,
                        position: row.get(8)?,
                    },
                ))
            })? {
                let (post_id, embed) = row?;
                if let Some(index) = post_index.get(&post_id) {
                    posts[*index].embeds.push(embed);
                }
            }
        }

        let media = {
            let mut statement = tx.prepare(
                r#"
                SELECT DISTINCT m.id, m.original_filename, m.mime_type, m.media_kind,
                  m.alt_text, m.is_nsfw, m.conversion_state, m.stored_path
                FROM media m
                JOIN post_media pm ON pm.media_id = m.id
                JOIN posts p ON p.id = pm.post_id
                WHERE p.user_id = ? AND p.is_deleted = 0 AND m.owner_user_id = ?
                ORDER BY m.id ASC
                "#,
            )?;
            statement
                .query_map(params![user_id, user_id], |row| {
                    Ok(ExportMediaRow {
                        id: row.get(0)?,
                        original_filename: row.get(1)?,
                        mime_type: row.get(2)?,
                        media_kind: row.get(3)?,
                        alt_text: row.get(4)?,
                        is_nsfw: row.get::<_, i64>(5)? != 0,
                        conversion_state: row.get(6)?,
                        stored_path: row.get(7)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?
        };

        let follows = {
            let mut statement = tx.prepare(
                r#"
                SELECT u.username, f.created_at
                FROM follows f JOIN users u ON u.id = f.followed_id
                WHERE f.follower_id = ?
                ORDER BY u.normalized_username ASC, u.id ASC
                "#,
            )?;
            statement
                .query_map([user_id], |row| {
                    Ok(FollowDocument {
                        username: row.get(0)?,
                        created_at: row.get(1)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?
        };

        let muted_words = {
            let mut statement = tx.prepare(
                "SELECT term, created_at FROM muted_words WHERE user_id = ? ORDER BY normalized_term ASC, id ASC",
            )?;
            statement
                .query_map([user_id], |row| {
                    Ok(MutedWordDocument {
                        term: row.get(0)?,
                        created_at: row.get(1)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?
        };

        tx.commit()?;
        Ok(ExportSnapshot {
            username,
            profile,
            posts,
            media,
            follows,
            muted_words,
        })
    })
    .await
}

/// Confirms a stored media path is a regular file inside the upload roots the
/// media kind may live in. Stored paths come from the database, which must be
/// treated as untrusted after a restore.
fn validate_export_media_source(
    paths: &RuntimePaths,
    media_kind: &str,
    mime_type: &str,
    stored_path: &str,
) -> anyhow::Result<PathBuf> {
    let allowed_roots: [&Path; 2] = match media_kind {
        "image" => [&paths.uploads_images, &paths.uploads_originals],
        "video" => [&paths.uploads_videos, &paths.uploads_originals],
        _ => anyhow::bail!("media row has an unsupported media kind"),
    };
    if mime_type.is_empty() || !mime_type.starts_with(&format!("{media_kind}/")) {
        anyhow::bail!("media row mime type does not match its media kind");
    }
    let path = PathBuf::from(stored_path);
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        anyhow::bail!("stored media path is not a safe absolute path");
    }
    if !allowed_roots.iter().any(|root| path.starts_with(root)) {
        anyhow::bail!("stored media path is outside the upload directories");
    }
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("media file {} is missing", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!("stored media path is not a regular file");
    }
    Ok(path)
}

struct ExportMediaPlan {
    archive_id: i64,
    archive_path: String,
    source: PathBuf,
    byte_len: u64,
    sha256: String,
    original_filename: String,
    mime_type: String,
    media_kind: String,
    alt_text: String,
    is_nsfw: bool,
    conversion_state: String,
}

impl ExportMediaPlan {
    fn document(&self) -> MediaDocument {
        MediaDocument {
            id: self.archive_id,
            original_filename: self.original_filename.clone(),
            mime_type: self.mime_type.clone(),
            media_kind: self.media_kind.clone(),
            byte_len: self.byte_len,
            alt_text: self.alt_text.clone(),
            is_nsfw: self.is_nsfw,
            conversion_state: self.conversion_state.clone(),
            sha256: self.sha256.clone(),
            archive_path: self.archive_path.clone(),
        }
    }
}

struct ExportDocuments<'a> {
    manifest: &'a ManifestDocument,
    profile: &'a ProfileDocument,
    posts: &'a [PostDocument],
    follows: &'a [FollowDocument],
    settings: &'a SettingsDocument,
    media_plans: &'a [ExportMediaPlan],
}

type ExportBuilder = Builder<GzEncoder<CappedWriter<File>>>;

/// Writes the gzip tar archive to a temporary sibling and renames it into
/// place. The temporary file is removed on every failure path.
fn write_export_archive(
    destination: &Path,
    destination_dir: &Path,
    documents: &ExportDocuments<'_>,
    limits: ArchiveLimits,
) -> anyhow::Result<u64> {
    let Some(file_name) = destination.file_name().and_then(|name| name.to_str()) else {
        anyhow::bail!("export destination must be a plain file name");
    };
    let temp_path = destination_dir.join(format!(".{file_name}.{}.tmp", Uuid::new_v4().simple()));
    let mut guard = TempOutputGuard::new(temp_path.clone());
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .with_context(|| format!("failed to create account archive {}", temp_path.display()))?;
    let writer = CappedWriter::new(file, limits.compressed_bytes);
    let encoder = GzEncoder::new(writer, Compression::default());
    let mut builder = Builder::new(encoder);

    append_json_entry(&mut builder, MANIFEST_PATH, documents.manifest)?;
    append_json_entry(&mut builder, PROFILE_PATH, documents.profile)?;
    append_json_entry(&mut builder, POSTS_PATH, documents.posts)?;
    let media_documents = documents
        .media_plans
        .iter()
        .map(ExportMediaPlan::document)
        .collect::<Vec<_>>();
    append_json_entry(&mut builder, MEDIA_PATH, &media_documents)?;
    append_json_entry(&mut builder, FOLLOWS_PATH, documents.follows)?;
    append_json_entry(&mut builder, SETTINGS_PATH, documents.settings)?;
    for plan in documents.media_plans {
        append_media_entry(&mut builder, plan)?;
    }

    builder
        .finish()
        .with_context(|| "failed to finish account archive")?;
    let encoder = builder
        .into_inner()
        .context("failed to finish account archive")?;
    let writer = encoder
        .finish()
        .context("failed to finish account archive")?;
    let bytes = writer.written;
    let file = writer.into_inner();
    file.sync_all()
        .with_context(|| "failed to flush account archive")?;
    drop(file);
    fs::rename(&temp_path, destination).with_context(|| {
        format!(
            "failed to install account archive {}",
            destination.display()
        )
    })?;
    guard.disarm();
    Ok(bytes)
}

fn append_json_entry<T: Serialize + ?Sized>(
    builder: &mut ExportBuilder,
    archive_path: &str,
    value: &T,
) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(value)?;
    let size = u64::try_from(bytes.len())?;
    let header = archive_entry_header(archive_path, size)?;
    builder
        .append(&header, bytes.as_slice())
        .with_context(|| format!("failed to write archive entry {archive_path}"))?;
    Ok(())
}

fn append_media_entry(builder: &mut ExportBuilder, plan: &ExportMediaPlan) -> anyhow::Result<()> {
    let mut source = File::open(&plan.source)
        .with_context(|| format!("failed to open media file {}", plan.source.display()))?;
    let header = archive_entry_header(&plan.archive_path, plan.byte_len)?;
    builder
        .append(&header, &mut source)
        .with_context(|| format!("failed to write archive entry {}", plan.archive_path))?;
    Ok(())
}

fn archive_entry_header(archive_path: &str, size: u64) -> anyhow::Result<Header> {
    let mut header = Header::new_ustar();
    header.set_path(archive_path)?;
    header.set_entry_type(EntryType::Regular);
    header.set_size(size);
    header.set_mode(0o600);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_cksum();
    Ok(header)
}

/// Removes the staged archive if it was not renamed into place.
struct TempOutputGuard {
    path: PathBuf,
    armed: bool,
}

impl TempOutputGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempOutputGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Err(error) = fs::remove_file(&self.path)
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::debug!(
                path = %self.path.display(),
                error = %error,
                "failed to remove staged account archive"
            );
        }
    }
}

/// Counts written bytes and fails once the archive would exceed `limit`.
/// Wrapping the gzip writer means the cap applies to the compressed archive
/// size, which is what the upload limit constrains.
struct CappedWriter<W> {
    inner: W,
    written: u64,
    limit: u64,
}

impl<W> CappedWriter<W> {
    fn new(inner: W, limit: u64) -> Self {
        Self {
            inner,
            written: 0,
            limit,
        }
    }

    fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: Write> Write for CappedWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let next = self
            .written
            .saturating_add(u64::try_from(buffer.len()).unwrap_or(u64::MAX));
        if next > self.limit {
            return Err(io::Error::other(format!(
                "account archive exceeds the {}-byte limit",
                self.limit
            )));
        }
        let written = self.inner.write(buffer)?;
        self.written = self
            .written
            .saturating_add(u64::try_from(written).unwrap_or(u64::MAX));
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn hash_file_streaming(path: &Path) -> anyhow::Result<(u64, String)> {
    let mut file = File::open(path)
        .with_context(|| format!("failed to open media file {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    let mut size = 0_u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        size = size.saturating_add(u64::try_from(read)?);
        hasher.update(&buffer[..read]);
    }
    Ok((size, hex_lower(hasher.finalize().as_ref())))
}

/// Maps an archive mime type to a sanitized extension. The stored host path
/// and original file name are never used for archive entry names.
fn export_media_extension(mime_type: &str, media_kind: &str) -> &'static str {
    match (media_kind, mime_type) {
        ("image", "image/jpeg") => "jpg",
        ("image", "image/png") => "png",
        ("image", "image/gif") => "gif",
        ("image", "image/webp") => "webp",
        ("image", _) => "img",
        ("video", "video/webm") => "webm",
        ("video", "video/quicktime") => "mov",
        ("video", _) => "mp4",
        _ => "bin",
    }
}

// ---------------------------------------------------------------------------
// Import: archive validation and extraction
// ---------------------------------------------------------------------------

/// Validates and imports `archive_path` into the authenticated destination
/// account `user_id`.
///
/// Nothing is written until the whole archive validates. Media files are
/// copied from the staging directory into the destination upload directories
/// first, then one database transaction inserts every row; if the transaction
/// fails the copied files are removed. Imported follows respect protected
/// accounts by creating pending requests, and archives cannot change the
/// destination username, password, or administrator status.
pub async fn import_account(
    pool: &SqlitePool,
    paths: &RuntimePaths,
    settings: &Settings,
    user_id: i64,
    archive_path: &Path,
) -> anyhow::Result<ImportReport> {
    // 1. Extract and validate the entire archive before any mutation.
    fs::create_dir_all(&paths.tmp_dir).with_context(|| {
        format!(
            "failed to create account import staging directory {}",
            paths.tmp_dir.display()
        )
    })?;
    let staging = tempfile::Builder::new()
        .prefix(IMPORT_TMP_PREFIX)
        .tempdir_in(&paths.tmp_dir)
        .with_context(|| "failed to create account import staging directory")?;
    let limits = ArchiveLimits::from_settings(settings);
    let extracted = extract_archive(archive_path, staging.path(), limits)?;
    let staged = parse_archive(settings, &extracted, limits)?;
    drop(extracted);

    // 2. Destination and repeat-import checks, still before any mutation.
    ensure_import_destination(pool, user_id).await?;
    if archive_already_imported(pool, user_id, &staged.manifest.archive_id).await? {
        anyhow::bail!(ALREADY_IMPORTED_MESSAGE);
    }

    // 3. Copy media bytes into the upload directories, claiming unique names.
    let installed = install_media_files(pool, paths, settings, &staged.media).await?;

    // 4. Apply every database row in one transaction. The transaction reads
    //    the installed file paths, so the files must exist before it starts.
    let plan = ImportPlan {
        archive_id: staged.manifest.archive_id,
        archive_username: staged.manifest.username,
        profile: staged.profile,
        posts: staged.posts,
        media: staged.media,
        media_install: installed.decisions,
        follows: staged.follows,
        muted_words: staged.muted_words,
    };
    let max_username_len = settings.accounts.max_username_len;
    let result = apply_import(pool, user_id, max_username_len, plan).await;
    match result {
        Ok((report, unused_files)) => {
            // Cleanup order: files that turned out to duplicate an existing
            // canonical row are only removed after the transaction commits.
            remove_files_best_effort(&unused_files);
            Ok(report)
        }
        Err(error) => {
            // Cleanup order: the transaction rolled back, so remove every file
            // copied into the upload directories. The staging directory removes
            // itself when `staging` drops at the end of this scope.
            remove_files_best_effort(&installed.installed_paths);
            Err(error)
        }
    }
}

/// Everything read from a validated archive, plus the staging location of the
/// media bytes. Documents are small JSON buffers; media stays on disk.
struct ExtractedArchive {
    documents: BTreeMap<String, Vec<u8>>,
    media_files: BTreeMap<String, ExtractedMediaFile>,
    entry_count: usize,
}

struct ExtractedMediaFile {
    staged_path: PathBuf,
    size: u64,
    sha256: String,
    /// Bounded copy of the entry's first bytes, used for content sniffing.
    prefix: Vec<u8>,
}

/// Extracts the archive into `staging`, enforcing structural limits. Every
/// path, entry type, duplicate name, and size is checked before a single byte
/// is written outside the staging directory, and byte limits are enforced
/// incrementally while entries stream so a compressed bomb never expands past
/// `limits.expanded_bytes` on disk or in memory.
#[expect(
    clippy::too_many_lines,
    reason = "one ordered pass validates paths, documents, and streamed media together"
)]
fn extract_archive(
    archive_path: &Path,
    staging: &Path,
    limits: ArchiveLimits,
) -> anyhow::Result<ExtractedArchive> {
    let metadata = fs::symlink_metadata(archive_path)
        .with_context(|| format!("account archive {} does not exist", archive_path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!("account archive is not a regular file");
    }
    if metadata.len() > limits.compressed_bytes {
        anyhow::bail!(
            "account archive is too large ({} bytes); the limit is {} bytes",
            metadata.len(),
            limits.compressed_bytes
        );
    }

    let file = File::open(archive_path)
        .with_context(|| format!("failed to open account archive {}", archive_path.display()))?;
    let decoder = GzDecoder::new(file);
    let mut archive = Archive::new(decoder);
    let entries = archive
        .entries()
        .with_context(|| "account archive is not a valid gzip tar archive")?;

    let mut documents = BTreeMap::new();
    let mut media_files = BTreeMap::new();
    let mut seen = BTreeSet::new();
    let mut entry_count = 0usize;
    let mut total_document_bytes = 0u64;
    let mut extracted_media_bytes = 0u64;

    for entry in entries {
        let mut entry = entry.with_context(|| "account archive is not a valid gzip tar archive")?;
        entry_count += 1;
        if entry_count > limits.entries {
            anyhow::bail!(
                "account archive contains more than {} entries",
                limits.entries
            );
        }
        let (name, entry_type) = validate_entry_path(&entry)?;
        // Extraction targets can live on case-insensitive filesystems, so
        // `media/0.png` and `media/0.PNG` must collide before either is written.
        if !seen.insert(name.to_ascii_lowercase()) {
            anyhow::bail!("account archive contains duplicate entry {name}");
        }
        if entry_type.is_dir() {
            if name != MEDIA_DIR {
                anyhow::bail!("account archive contains unexpected directory {name}");
            }
            continue;
        }
        if !entry_type.is_file() {
            anyhow::bail!("account archive contains an unsupported entry type for {name}");
        }
        if DOCUMENT_PATHS.contains(&name.as_str()) {
            let limit = document_limit(&name);
            let bytes = read_limited_entry(&mut entry, limit)
                .with_context(|| format!("{name} is too large or unreadable"))?;
            total_document_bytes = total_document_bytes
                .checked_add(u64::try_from(bytes.len())?)
                .ok_or_else(|| anyhow::anyhow!("account archive document size overflow"))?;
            if total_document_bytes > MAX_TOTAL_DOCUMENT_BYTES {
                anyhow::bail!(
                    "account archive JSON documents exceed the {MAX_TOTAL_DOCUMENT_BYTES}-byte total limit"
                );
            }
            documents.insert(name, bytes);
        } else if let Some(relative) = name.strip_prefix(MEDIA_PREFIX) {
            let relative_path = Path::new(relative);
            let valid_relative = !relative.is_empty()
                && relative_path
                    .components()
                    .all(|component| matches!(component, Component::Normal(_)));
            if !valid_relative || relative_path.components().count() != 1 {
                anyhow::bail!("account archive contains an invalid media path {name}");
            }
            // Reject an oversized entry from its header before reading any of
            // its bytes so a declared giant size cannot consume disk.
            let declared_size = entry
                .header()
                .size()
                .with_context(|| format!("account archive entry {name} has an invalid size"))?;
            if declared_size > limits.expanded_bytes {
                anyhow::bail!(
                    "account archive media entry {name} declares {declared_size} bytes; the limit is {} bytes",
                    limits.expanded_bytes
                );
            }
            // A valid archive can never list more media documents than
            // `media.json` accepts, so stop extracting sooner than the general
            // entry cap allows.
            if media_files.len() >= MAX_MEDIA {
                anyhow::bail!("account archive contains more than {MAX_MEDIA} media files");
            }
            let media_dir = staging.join(MEDIA_DIR);
            fs::create_dir_all(&media_dir)?;
            let target = media_dir.join(relative);
            let (size, sha256, prefix, total) = extract_media_entry(
                &mut entry,
                &target,
                extracted_media_bytes,
                limits.expanded_bytes,
            )?;
            extracted_media_bytes = total;
            media_files.insert(
                name,
                ExtractedMediaFile {
                    staged_path: target,
                    size,
                    sha256,
                    prefix,
                },
            );
        } else {
            anyhow::bail!("account archive contains unexpected entry {name}");
        }
    }

    for document in DOCUMENT_PATHS {
        if !documents.contains_key(document) {
            anyhow::bail!("account archive is missing required document {document}");
        }
    }
    Ok(ExtractedArchive {
        documents,
        media_files,
        entry_count,
    })
}

/// Validates one tar path and returns its normalized name and type.
fn validate_entry_path(entry: &tar::Entry<'_, impl Read>) -> anyhow::Result<(String, EntryType)> {
    let entry_type = entry.header().entry_type();
    if !entry_type.is_file() && !entry_type.is_dir() {
        anyhow::bail!("account archive contains an unsupported entry type");
    }
    let raw = entry.path_bytes();
    let raw = std::str::from_utf8(raw.as_ref())
        .map_err(|_utf8| anyhow::anyhow!("account archive contains a non-UTF-8 path"))?;
    if raw.is_empty() || raw.starts_with('/') {
        anyhow::bail!("account archive contains an absolute or empty path");
    }
    // Archives written by RustPost only use ASCII names; anything else is
    // almost certainly crafted to confuse normalization or filesystems.
    if !raw.is_ascii() {
        anyhow::bail!("account archive contains a non-ASCII path");
    }
    if raw.contains('\\')
        || raw.contains(':')
        || raw.contains("//")
        || raw.contains('\u{2215}')
        || raw.contains('\u{2044}')
        || raw.contains('\u{29f8}')
        || raw.contains('\u{ff0f}')
    {
        anyhow::bail!("account archive path contains unsafe characters");
    }
    let lower = raw.to_ascii_lowercase();
    if lower.contains("%2e") || lower.contains("%2f") || lower.contains("%5c") {
        anyhow::bail!("account archive path contains unsafe characters");
    }
    let normalized = if entry_type.is_dir() {
        raw.strip_suffix('/').unwrap_or(raw)
    } else {
        raw
    };
    if normalized.is_empty() || normalized.contains("//") || normalized.ends_with('/') {
        anyhow::bail!("account archive path contains unsafe separators");
    }
    let path = Path::new(normalized);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        anyhow::bail!("account archive entry path contains traversal");
    }
    if let Some(relative) = normalized.strip_prefix(MEDIA_PREFIX)
        && !relative.is_empty()
    {
        let file_name = Path::new(relative)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(relative);
        if file_name.len() > MAX_MEDIA_FILE_NAME_BYTES {
            anyhow::bail!("account archive media entry name is too long");
        }
        if file_name.ends_with('.') || file_name.ends_with(' ') {
            anyhow::bail!("account archive media entry name ends with '.' or a space");
        }
    }
    Ok((normalized.to_owned(), entry_type))
}

fn document_limit(name: &str) -> u64 {
    if name == MANIFEST_PATH {
        MANIFEST_MAX_BYTES
    } else {
        DOCUMENT_MAX_BYTES
    }
}

fn read_limited_entry(
    entry: &mut tar::Entry<'_, impl Read>,
    limit: u64,
) -> anyhow::Result<Vec<u8>> {
    let mut buffer = Vec::new();
    let mut limited = entry.take(limit.saturating_add(1));
    limited.read_to_end(&mut buffer)?;
    if u64::try_from(buffer.len()).unwrap_or(u64::MAX) > limit {
        anyhow::bail!("document exceeds the {limit}-byte limit");
    }
    Ok(buffer)
}

/// Streams one media entry into staging while hashing it, collecting a small
/// content-sniffing prefix, and enforcing the per-entry and running-total byte
/// caps. Memory use is one fixed-size buffer plus the bounded prefix.
fn extract_media_entry(
    entry: &mut tar::Entry<'_, impl Read>,
    target: &Path,
    already_extracted: u64,
    expanded_limit: u64,
) -> anyhow::Result<(u64, String, Vec<u8>, u64)> {
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)
        .with_context(|| format!("failed to extract media file {}", target.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    let mut prefix = Vec::with_capacity(MEDIA_SNIFF_PREFIX_BYTES);
    let mut size = 0_u64;
    let mut total = already_extracted;
    loop {
        let read = entry.read(&mut buffer).with_context(|| {
            format!(
                "failed to read account archive media entry for {}",
                target.display()
            )
        })?;
        if read == 0 {
            break;
        }
        let read = u64::try_from(read)?;
        size = size
            .checked_add(read)
            .ok_or_else(|| anyhow::anyhow!("account archive media size overflow"))?;
        total = total
            .checked_add(read)
            .ok_or_else(|| anyhow::anyhow!("account archive media size overflow"))?;
        if size > expanded_limit || total > expanded_limit {
            anyhow::bail!("account archive media exceeds the {expanded_limit}-byte limit");
        }
        let chunk = &buffer[..usize::try_from(read)?];
        if prefix.len() < MEDIA_SNIFF_PREFIX_BYTES {
            let take = (MEDIA_SNIFF_PREFIX_BYTES - prefix.len()).min(chunk.len());
            prefix.extend_from_slice(&chunk[..take]);
        }
        hasher.update(chunk);
        output.write_all(chunk)?;
    }
    output
        .sync_all()
        .with_context(|| format!("failed to flush extracted media file {}", target.display()))?;
    Ok((size, hex_lower(hasher.finalize().as_ref()), prefix, total))
}

// ---------------------------------------------------------------------------
// Import: document validation
// ---------------------------------------------------------------------------

struct StagedArchive {
    manifest: ManifestDocument,
    profile: ProfileDocument,
    posts: Vec<ValidatedPost>,
    media: Vec<ValidatedMedia>,
    follows: Vec<FollowDocument>,
    muted_words: Vec<MutedWordDocument>,
}

struct ValidatedPost {
    archive_id: i64,
    text: String,
    created_at: String,
    edited_at: Option<String>,
    parent_id: Option<i64>,
    root_id: Option<i64>,
    quote_id: Option<i64>,
    media_ids: Vec<i64>,
    embeds: Vec<EmbedDocument>,
}

struct ValidatedMedia {
    id: i64,
    original_filename: String,
    mime_type: String,
    media_kind: String,
    byte_len: u64,
    alt_text: String,
    is_nsfw: bool,
    sha256: String,
    archive_path: String,
    staged_path: PathBuf,
}

/// Parses and validates every document, applying destination instance limits.
fn parse_archive(
    settings: &Settings,
    extracted: &ExtractedArchive,
    limits: ArchiveLimits,
) -> anyhow::Result<StagedArchive> {
    let document = |name: &str| -> anyhow::Result<&[u8]> {
        extracted
            .documents
            .get(name)
            .map(Vec::as_slice)
            .ok_or_else(|| anyhow::anyhow!("account archive is missing required document {name}"))
    };

    let manifest: ManifestDocument = parse_document(MANIFEST_PATH, document(MANIFEST_PATH)?)?;
    if manifest.format_version != FORMAT_VERSION {
        anyhow::bail!(
            "unsupported account archive format version {} (this instance supports {FORMAT_VERSION})",
            manifest.format_version
        );
    }
    if manifest.app != APP_NAME {
        anyhow::bail!("account archive is not a RustPost account archive");
    }
    validate_timestamp_text(&manifest.created_at, "manifest created_at")?;
    if manifest.archive_id.is_empty()
        || manifest.archive_id.chars().count() > 128
        || manifest.archive_id.chars().any(char::is_control)
    {
        anyhow::bail!("manifest archive_id is invalid");
    }
    if manifest.username.is_empty()
        || manifest.username.chars().count() > 128
        || manifest.username.chars().any(char::is_control)
    {
        anyhow::bail!("manifest username is invalid");
    }
    if manifest.entry_count != extracted.entry_count {
        anyhow::bail!("manifest entry_count does not match the number of archive entries");
    }

    let mut profile: ProfileDocument = parse_document(PROFILE_PATH, document(PROFILE_PATH)?)?;
    if profile.theme != "light" && profile.theme != "dark" {
        anyhow::bail!("profile theme must be light or dark");
    }
    validate_profile_text(&profile.display_name, &profile.bio, settings)
        .with_context(|| "profile contains invalid text")?;
    if profile.location.chars().count() > MAX_LOCATION_CHARS
        || profile.location.chars().any(char::is_control)
    {
        anyhow::bail!("profile location is invalid");
    }
    let website = profile.website.trim().to_owned();
    if website.chars().count() > MAX_WEBSITE_CHARS || website.chars().any(char::is_control) {
        anyhow::bail!("profile website is invalid");
    }
    if !(website.is_empty() || website.starts_with("http://") || website.starts_with("https://")) {
        anyhow::bail!("profile website must start with http:// or https://");
    }
    profile.website = website;
    validate_timestamp_text(&profile.created_at, "profile created_at")?;

    let media_documents: Vec<MediaDocument> = parse_document(MEDIA_PATH, document(MEDIA_PATH)?)?;
    let media = validate_media_documents(&media_documents, extracted, limits)?;
    let media_ids = media.iter().map(|entry| entry.id).collect::<BTreeSet<_>>();

    let post_documents: Vec<PostDocument> = parse_document(POSTS_PATH, document(POSTS_PATH)?)?;
    let posts = validate_post_documents(&post_documents, settings, &media_ids)?;

    let follows: Vec<FollowDocument> = parse_document(FOLLOWS_PATH, document(FOLLOWS_PATH)?)?;
    if follows.len() > MAX_FOLLOWS {
        anyhow::bail!("follows.json contains more than {MAX_FOLLOWS} entries");
    }
    for follow in &follows {
        if follow.username.is_empty()
            || follow.username.chars().count() > 128
            || follow.username.chars().any(char::is_control)
        {
            anyhow::bail!("follows.json contains an invalid username");
        }
        validate_timestamp_text(&follow.created_at, "follows.json created_at")?;
    }

    let settings_document: SettingsDocument =
        parse_document(SETTINGS_PATH, document(SETTINGS_PATH)?)?;
    if settings_document.muted_words.len() > MAX_MUTED_WORDS {
        anyhow::bail!("settings.json contains more than {MAX_MUTED_WORDS} muted words");
    }
    let mut muted_words = Vec::with_capacity(settings_document.muted_words.len());
    for word in settings_document.muted_words {
        let term = clean_muted_word(&word.term)?;
        validate_timestamp_text(&word.created_at, "settings.json muted word created_at")?;
        muted_words.push(MutedWordDocument {
            term,
            created_at: word.created_at,
        });
    }

    Ok(StagedArchive {
        manifest,
        profile,
        posts,
        media,
        follows,
        muted_words,
    })
}

/// `serde_json` applies its default recursion limit (128), so deeply nested
/// documents are rejected instead of risking stack exhaustion.
fn parse_document<T: serde::de::DeserializeOwned>(name: &str, bytes: &[u8]) -> anyhow::Result<T> {
    serde_json::from_slice(bytes).with_context(|| format!("{name} is not a valid JSON document"))
}

#[expect(
    clippy::too_many_lines,
    reason = "media documents are validated in two passes over ids, paths, bytes, and sniffed content"
)]
fn validate_media_documents(
    documents: &[MediaDocument],
    extracted: &ExtractedArchive,
    limits: ArchiveLimits,
) -> anyhow::Result<Vec<ValidatedMedia>> {
    if documents.len() > MAX_MEDIA {
        anyhow::bail!("media.json contains more than {MAX_MEDIA} media entries");
    }

    let mut ids = BTreeSet::new();
    let mut paths = BTreeSet::new();
    let mut declared_total = 0_u64;
    for document in documents {
        if document.id < 0 {
            anyhow::bail!("media.json contains a negative media id");
        }
        if !ids.insert(document.id) {
            anyhow::bail!("media.json contains duplicate media id {}", document.id);
        }
        if document.media_kind != "image" && document.media_kind != "video" {
            anyhow::bail!("media {} has an unsupported media kind", document.id);
        }
        if document.mime_type.is_empty()
            || document.mime_type.chars().count() > 255
            || document.mime_type.chars().any(char::is_control)
            || !document
                .mime_type
                .starts_with(&format!("{}/", document.media_kind))
        {
            anyhow::bail!(
                "media {} mime type does not match its media kind",
                document.id
            );
        }
        if document.sha256.len() != 64
            || !document.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            anyhow::bail!("media {} sha256 is not 64 hex characters", document.id);
        }
        if document.original_filename.chars().count() > MAX_ORIGINAL_FILENAME_CHARS {
            anyhow::bail!("media {} original filename is too long", document.id);
        }
        if document.alt_text.chars().count() > MAX_ALT_TEXT_CHARS {
            anyhow::bail!("media {} alt text is too long", document.id);
        }
        if document.conversion_state.chars().count() > MAX_CONVERSION_STATE_CHARS {
            anyhow::bail!("media {} conversion state is too long", document.id);
        }
        if !paths.insert(document.archive_path.clone()) {
            anyhow::bail!(
                "media.json lists archive path {} more than once",
                document.archive_path
            );
        }
        declared_total = declared_total
            .checked_add(document.byte_len)
            .ok_or_else(|| anyhow::anyhow!("media.json byte length total overflow"))?;
        if declared_total > limits.expanded_bytes {
            anyhow::bail!(
                "media.json declares more than the {}-byte account archive media limit",
                limits.expanded_bytes
            );
        }
    }

    let mut validated = Vec::with_capacity(documents.len());
    for document in documents {
        let Some(extracted_file) = extracted.media_files.get(&document.archive_path) else {
            anyhow::bail!(
                "media {} archive path {} is missing from the archive",
                document.id,
                document.archive_path
            );
        };
        if extracted_file.size != document.byte_len {
            anyhow::bail!(
                "media {} byte length does not match the extracted file",
                document.id
            );
        }
        if !extracted_file.sha256.eq_ignore_ascii_case(&document.sha256) {
            anyhow::bail!(
                "media {} sha256 does not match the extracted file",
                document.id
            );
        }
        // Interactive uploads derive the stored mime type from `infer`; an
        // imported file must agree with its declared type so a mislabeled
        // payload is never installed as media.
        let Some(sniffed) = infer::get(&extracted_file.prefix) else {
            anyhow::bail!(
                "media {} content is not a recognized media type",
                document.id
            );
        };
        if !sniffed
            .mime_type()
            .eq_ignore_ascii_case(&document.mime_type)
        {
            anyhow::bail!(
                "media {} content type {} does not match the declared {}",
                document.id,
                sniffed.mime_type(),
                document.mime_type
            );
        }
        validated.push(ValidatedMedia {
            id: document.id,
            original_filename: document.original_filename.clone(),
            mime_type: document.mime_type.clone(),
            media_kind: document.media_kind.clone(),
            byte_len: document.byte_len,
            alt_text: document.alt_text.clone(),
            is_nsfw: document.is_nsfw,
            sha256: document.sha256.to_ascii_lowercase(),
            archive_path: document.archive_path.clone(),
            staged_path: extracted_file.staged_path.clone(),
        });
    }

    if extracted.media_files.len() != documents.len() {
        for name in extracted.media_files.keys() {
            if !paths.contains(name) {
                anyhow::bail!("account archive contains unlisted media file {name}");
            }
        }
        anyhow::bail!("media.json does not list every media file in the archive");
    }
    Ok(validated)
}

fn validate_post_documents(
    documents: &[PostDocument],
    settings: &Settings,
    media_ids: &BTreeSet<i64>,
) -> anyhow::Result<Vec<ValidatedPost>> {
    if documents.len() > MAX_POSTS {
        anyhow::bail!("posts.json contains more than {MAX_POSTS} posts");
    }
    let mut ids = BTreeSet::new();
    for document in documents {
        if document.id < 0 {
            anyhow::bail!("posts.json contains a negative post id");
        }
        if !ids.insert(document.id) {
            anyhow::bail!("posts.json contains duplicate post id {}", document.id);
        }
    }

    let mut validated = Vec::with_capacity(documents.len());
    for document in documents {
        let text = clean_post_text(
            &document.text,
            settings.posts.max_text_chars,
            document.media_ids.len(),
        )
        .with_context(|| format!("post {} text is invalid", document.id))?;
        validate_timestamp_text(&document.created_at, "post created_at")?;
        if let Some(edited_at) = &document.edited_at {
            validate_timestamp_text(edited_at, "post edited_at")?;
        }
        for media_id in &document.media_ids {
            if !media_ids.contains(media_id) {
                anyhow::bail!(
                    "post {} references unknown media id {media_id}",
                    document.id
                );
            }
        }
        if document.embeds.len() > MAX_EMBEDS_PER_POST {
            anyhow::bail!(
                "post {} contains more than {MAX_EMBEDS_PER_POST} embeds",
                document.id
            );
        }
        let mut embed_positions = BTreeSet::new();
        let mut embed_videos = BTreeSet::new();
        for embed in &document.embeds {
            if embed.provider != "youtube" {
                anyhow::bail!(
                    "post {} contains an unsupported embed provider",
                    document.id
                );
            }
            if embed.video_id.is_empty()
                || embed.video_id.chars().count() > MAX_EMBED_VIDEO_ID_CHARS
                || embed.video_id.chars().any(char::is_control)
            {
                anyhow::bail!("post {} contains an invalid embed video id", document.id);
            }
            if embed.position < 0 || !embed_positions.insert(embed.position) {
                anyhow::bail!("post {} contains duplicate embed positions", document.id);
            }
            if !embed_videos.insert(embed.video_id.clone()) {
                anyhow::bail!("post {} lists the same embed video twice", document.id);
            }
            for url in [
                &embed.original_url,
                &embed.canonical_url,
                &embed.thumbnail_url,
                &embed.embed_url,
            ] {
                if url.is_empty()
                    || url.chars().count() > MAX_EMBED_URL_CHARS
                    || url.chars().any(char::is_control)
                {
                    anyhow::bail!("post {} contains an invalid embed URL", document.id);
                }
            }
            if let Some(title) = &embed.title
                && (title.chars().count() > MAX_EMBED_TITLE_CHARS
                    || title.chars().any(char::is_control))
            {
                anyhow::bail!("post {} contains an invalid embed title", document.id);
            }
        }
        validated.push(ValidatedPost {
            archive_id: document.id,
            text,
            created_at: document.created_at.clone(),
            edited_at: document.edited_at.clone(),
            parent_id: document.parent_id,
            root_id: document.root_id,
            quote_id: document.quote_id,
            media_ids: document.media_ids.clone(),
            embeds: document.embeds.clone(),
        });
    }
    Ok(validated)
}

fn validate_timestamp_text(value: &str, field: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.chars().count() > MAX_TIMESTAMP_CHARS
        || value.chars().any(char::is_control)
    {
        anyhow::bail!("{field} is not a valid timestamp");
    }
    Ok(())
}

/// Mirrors `social::clean_muted_word` (private) so imports normalize muted
/// words exactly like the interactive path.
fn clean_muted_word(term: &str) -> anyhow::Result<String> {
    let trimmed = term.trim();
    if trimmed.is_empty() {
        anyhow::bail!("muted word cannot be empty");
    }
    if trimmed.chars().count() > 100 {
        anyhow::bail!("muted word is too long");
    }
    if trimmed
        .chars()
        .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
    {
        anyhow::bail!("muted word contains unsupported control characters");
    }
    Ok(trimmed.to_owned())
}

// ---------------------------------------------------------------------------
// Import: destination checks and media installation
// ---------------------------------------------------------------------------

/// Rejects deleted and deletion-pending destination accounts.
async fn ensure_import_destination(pool: &SqlitePool, user_id: i64) -> anyhow::Result<()> {
    let state = pool
        .call(move |conn| {
            conn.query_row(
                "SELECT is_deleted, deletion_requested_at, deletion_scheduled_at FROM users WHERE id = ?",
                [user_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(Into::into)
        })
        .await?;
    match state {
        None => anyhow::bail!("import destination account does not exist"),
        Some((is_deleted, _, _)) if is_deleted != 0 => {
            anyhow::bail!("import destination account is deleted")
        }
        Some((_, requested_at, scheduled_at))
            if requested_at.is_some() || scheduled_at.is_some() =>
        {
            anyhow::bail!("import destination account is pending deletion")
        }
        Some(_) => Ok(()),
    }
}

async fn archive_already_imported(
    pool: &SqlitePool,
    user_id: i64,
    archive_id: &str,
) -> anyhow::Result<bool> {
    let archive_id = archive_id.to_owned();
    let existing = pool
        .call(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT 1 FROM account_imports WHERE user_id = ? AND archive_id = ?",
                    params![user_id, archive_id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some())
        })
        .await?;
    Ok(existing)
}

/// One archived media entry's installation decision.
enum MediaInstallDecision {
    /// The bytes were copied by us to these destination paths.
    Copied {
        stored_path: PathBuf,
        public_path: String,
    },
    /// An existing canonical row already stores these bytes.
    Canonical {
        canonical_id: i64,
        stored_path: String,
        public_path: String,
    },
}

struct InstalledMedia {
    decisions: Vec<MediaInstallDecision>,
    installed_paths: Vec<PathBuf>,
}

struct CanonicalMedia {
    id: i64,
    stored_path: String,
    public_path: String,
}

/// Copies every non-duplicate media entry into the destination upload
/// directories before the database transaction runs. On failure all files
/// copied so far are removed.
async fn install_media_files(
    pool: &SqlitePool,
    paths: &RuntimePaths,
    settings: &Settings,
    media: &[ValidatedMedia],
) -> anyhow::Result<InstalledMedia> {
    let mut decisions = Vec::with_capacity(media.len());
    let mut installed_paths = Vec::new();
    for entry in media {
        match install_one_media(pool, paths, settings, entry).await {
            Ok((decision, copied_path)) => {
                if let Some(copied_path) = copied_path {
                    installed_paths.push(copied_path);
                }
                decisions.push(decision);
            }
            Err(error) => {
                remove_files_best_effort(&installed_paths);
                return Err(error);
            }
        }
    }
    Ok(InstalledMedia {
        decisions,
        installed_paths,
    })
}

async fn install_one_media(
    pool: &SqlitePool,
    paths: &RuntimePaths,
    settings: &Settings,
    entry: &ValidatedMedia,
) -> anyhow::Result<(MediaInstallDecision, Option<PathBuf>)> {
    if let Some(canonical) = find_canonical_media(pool, &entry.media_kind, &entry.sha256).await? {
        return Ok((
            MediaInstallDecision::Canonical {
                canonical_id: canonical.id,
                stored_path: canonical.stored_path,
                public_path: canonical.public_path,
            },
            None,
        ));
    }
    let directory = media_install_dir(paths, settings, &entry.media_kind, &entry.mime_type)?;
    fs::create_dir_all(directory).with_context(|| {
        format!(
            "failed to create media upload directory {}",
            directory.display()
        )
    })?;
    let extension = import_media_extension(&entry.mime_type, &entry.media_kind)?;
    let basename = import_media_basename(&entry.original_filename, &entry.sha256);
    let target = claim_media_path(directory, &basename, extension)?;
    if let Err(error) = copy_media_bytes(&entry.staged_path, &target, entry.byte_len) {
        let _ = fs::remove_file(&target);
        return Err(error)
            .with_context(|| format!("failed to install imported media {}", entry.archive_path));
    }
    let public_path = public_media_path(paths, &target)?;
    Ok((
        MediaInstallDecision::Copied {
            stored_path: target.clone(),
            public_path,
        },
        Some(target),
    ))
}

async fn find_canonical_media(
    pool: &SqlitePool,
    media_kind: &str,
    sha256: &str,
) -> anyhow::Result<Option<CanonicalMedia>> {
    let media_kind = media_kind.to_owned();
    let sha256 = sha256.to_owned();
    pool.call(move |conn| {
        conn.query_row(
            r#"
            SELECT id, stored_path, public_path FROM media
            WHERE canonical_media_id IS NULL AND media_kind = ? AND original_sha256 = ?
            ORDER BY id ASC LIMIT 1
            "#,
            params![media_kind, sha256],
            |row| {
                Ok(CanonicalMedia {
                    id: row.get(0)?,
                    stored_path: row.get(1)?,
                    public_path: row.get(2)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    })
    .await
}

/// Chooses the upload directory for imported bytes. Types outside the
/// instance's convertible allow-list are stored in `originals` because the
/// server layer would never convert or serve them from images/videos.
fn media_install_dir<'a>(
    paths: &'a RuntimePaths,
    settings: &Settings,
    media_kind: &str,
    mime_type: &str,
) -> anyhow::Result<&'a Path> {
    match media_kind {
        "image" => {
            if settings
                .media
                .allowed_image_mime_types
                .iter()
                .any(|allowed| allowed == mime_type)
            {
                Ok(&paths.uploads_images)
            } else {
                Ok(&paths.uploads_originals)
            }
        }
        "video" => {
            if settings
                .media
                .allowed_video_mime_types
                .iter()
                .any(|allowed| allowed == mime_type)
            {
                Ok(&paths.uploads_videos)
            } else {
                Ok(&paths.uploads_originals)
            }
        }
        _ => anyhow::bail!("media entry has an unsupported media kind"),
    }
}

fn public_media_path(paths: &RuntimePaths, path: &Path) -> anyhow::Result<String> {
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("installed media path has no file name"))?;
    if path.starts_with(&paths.uploads_images) {
        return Ok(format!("/uploads/images/{filename}"));
    }
    if path.starts_with(&paths.uploads_videos) {
        return Ok(format!("/uploads/videos/{filename}"));
    }
    if path.starts_with(&paths.uploads_originals) {
        return Ok(format!("/uploads/originals/{filename}"));
    }
    anyhow::bail!("installed media path is outside the upload directories")
}

/// Claims an unused destination file name with `create_new` so concurrent
/// imports can never overwrite each other's files.
fn claim_media_path(directory: &Path, basename: &str, extension: &str) -> anyhow::Result<PathBuf> {
    let fallback = directory.join(format!(
        "{basename}-{}.{extension}",
        Uuid::new_v4().simple()
    ));
    let candidates = [
        directory.join(format!("{basename}.{extension}")),
        fallback,
        directory.join(format!(
            "{basename}-{}.{extension}",
            Uuid::new_v4().simple()
        )),
    ];
    for candidate in candidates {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(_claimed) => return Ok(candidate),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to create media file {}", candidate.display())
                });
            }
        }
    }
    anyhow::bail!("failed to claim a unique file name for imported media")
}

fn copy_media_bytes(source: &Path, target: &Path, expected: u64) -> anyhow::Result<()> {
    let mut source_file = File::open(source)
        .with_context(|| format!("failed to open staged media file {}", source.display()))?;
    let mut target_file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(target)
        .with_context(|| format!("failed to open media destination {}", target.display()))?;
    let copied = io::copy(&mut source_file, &mut target_file)?;
    target_file
        .sync_all()
        .with_context(|| format!("failed to flush media destination {}", target.display()))?;
    if copied != expected {
        anyhow::bail!("copied media size does not match the archive entry");
    }
    Ok(())
}

/// Mirrors `media::stable_media_basename` (private): sanitized original stem
/// plus a short content-hash prefix.
fn import_media_basename(original_filename: &str, sha256: &str) -> String {
    let stem = Path::new(original_filename)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("upload");
    let mut sanitized = String::with_capacity(stem.len().min(64) + 1 + HASH_PREFIX_LEN);
    let mut last_was_separator = false;
    for character in stem.chars().flat_map(char::to_lowercase) {
        let next = if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
            Some(character)
        } else if character.is_ascii_whitespace() {
            Some('-')
        } else {
            None
        };
        let Some(next) = next else {
            continue;
        };
        if matches!(next, '-' | '_' | '.') {
            if sanitized.is_empty() || last_was_separator {
                continue;
            }
            last_was_separator = true;
        } else {
            last_was_separator = false;
        }
        sanitized.push(next);
        if sanitized.len() >= 64 {
            break;
        }
    }
    while sanitized.ends_with(['-', '_', '.']) {
        sanitized.pop();
    }
    if sanitized.is_empty() {
        sanitized.push_str("upload");
    }
    let prefix_len = sha256.len().min(HASH_PREFIX_LEN);
    format!("{}-{}", sanitized, &sha256[..prefix_len])
}

/// Mirrors `media::safe_extension` (private).
fn import_media_extension(mime_type: &str, media_kind: &str) -> anyhow::Result<&'static str> {
    match (media_kind, mime_type) {
        ("image", "image/jpeg") => Ok("jpg"),
        ("image", "image/png") => Ok("png"),
        ("image", "image/gif") => Ok("gif"),
        ("image", "image/webp") => Ok("webp"),
        ("image", _) => Ok("img"),
        ("video", "video/webm") => Ok("webm"),
        ("video", "video/quicktime") => Ok("mov"),
        ("video", _) => Ok("mp4"),
        _ => anyhow::bail!("media entry has an unsupported media kind"),
    }
}

fn remove_files_best_effort(paths: &[PathBuf]) {
    for path in paths {
        if let Err(error) = fs::remove_file(path)
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "failed to remove imported media file during cleanup"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Import: database transaction
// ---------------------------------------------------------------------------

struct ImportPlan {
    archive_id: String,
    archive_username: String,
    profile: ProfileDocument,
    posts: Vec<ValidatedPost>,
    media: Vec<ValidatedMedia>,
    media_install: Vec<MediaInstallDecision>,
    follows: Vec<FollowDocument>,
    muted_words: Vec<MutedWordDocument>,
}

struct DestinationRow {
    display_name: String,
    bio: String,
    location: String,
    website: String,
    theme: String,
    nsfw_blur_enabled: bool,
    liked_posts_public: bool,
    follow_approval_required: bool,
}

/// Applies the import in one transaction. Returns the report plus files that
/// were copied but turned out to be unnecessary duplicates; the caller removes
/// those only after the commit succeeds.
#[expect(
    clippy::too_many_lines,
    reason = "the import transaction inserts every record family in one auditable block"
)]
async fn apply_import(
    pool: &SqlitePool,
    user_id: i64,
    max_username_len: usize,
    plan: ImportPlan,
) -> anyhow::Result<(ImportReport, Vec<PathBuf>)> {
    pool.call(move |conn| {
        let tx = conn.transaction()?;

        // Re-check the destination inside the transaction; concurrent account
        // deletion must not race with the import.
        let destination = tx
            .query_row(
                r#"
                SELECT is_deleted, deletion_requested_at, deletion_scheduled_at,
                  display_name, bio, location, website, theme, nsfw_blur_enabled,
                  liked_posts_public, follow_approval_required
                FROM users WHERE id = ?
                "#,
                [user_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        DestinationRow {
                            display_name: row.get(3)?,
                            bio: row.get(4)?,
                            location: row.get(5)?,
                            website: row.get(6)?,
                            theme: row.get(7)?,
                            nsfw_blur_enabled: row.get::<_, i64>(8)? != 0,
                            liked_posts_public: row.get::<_, i64>(9)? != 0,
                            follow_approval_required: row.get::<_, i64>(10)? != 0,
                        },
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| anyhow::anyhow!("import destination account does not exist"))?;
        if destination.0 != 0 {
            anyhow::bail!("import destination account is deleted");
        }
        if destination.1.is_some() || destination.2.is_some() {
            anyhow::bail!("import destination account is pending deletion");
        }
        let current = destination.3;

        // Profile: fill only empty text fields, apply preference fields as-is.
        let mut profile_fields_applied = 0usize;
        let mut profile_fields_skipped = 0usize;
        let display_name = fill_empty_field(
            &current.display_name,
            &plan.profile.display_name,
            &mut profile_fields_applied,
            &mut profile_fields_skipped,
        );
        let bio = fill_empty_field(
            &current.bio,
            &plan.profile.bio,
            &mut profile_fields_applied,
            &mut profile_fields_skipped,
        );
        let location = fill_empty_field(
            &current.location,
            &plan.profile.location,
            &mut profile_fields_applied,
            &mut profile_fields_skipped,
        );
        let website = fill_empty_field(
            &current.website,
            &plan.profile.website,
            &mut profile_fields_applied,
            &mut profile_fields_skipped,
        );
        let theme = replace_preference(
            &current.theme,
            &plan.profile.theme,
            &mut profile_fields_applied,
            &mut profile_fields_skipped,
        );
        let nsfw_blur_enabled = replace_preference(
            current.nsfw_blur_enabled,
            plan.profile.nsfw_blur_enabled,
            &mut profile_fields_applied,
            &mut profile_fields_skipped,
        );
        let liked_posts_public = replace_preference(
            current.liked_posts_public,
            plan.profile.liked_posts_public,
            &mut profile_fields_applied,
            &mut profile_fields_skipped,
        );
        let follow_approval_required = replace_preference(
            current.follow_approval_required,
            plan.profile.follow_approval_required,
            &mut profile_fields_applied,
            &mut profile_fields_skipped,
        );
        tx.execute(
            r#"
            UPDATE users SET display_name = ?, bio = ?, location = ?, website = ?,
              theme = ?, nsfw_blur_enabled = ?, liked_posts_public = ?,
              follow_approval_required = ?, updated_at = CURRENT_TIMESTAMP
            WHERE id = ?
            "#,
            params![
                display_name,
                bio,
                location,
                website,
                theme,
                i64::from(nsfw_blur_enabled),
                i64::from(liked_posts_public),
                i64::from(follow_approval_required),
                user_id,
            ],
        )?;

        // Media rows first so posts can attach remapped media ids.
        if plan.media.len() != plan.media_install.len() {
            anyhow::bail!("internal import error: media install plan is incomplete");
        }
        let mut media_id_map: HashMap<i64, i64> = HashMap::with_capacity(plan.media.len());
        let mut unused_copies: Vec<PathBuf> = Vec::new();
        for (media, decision) in plan.media.iter().zip(plan.media_install.iter()) {
            let (stored_path, public_path, canonical_id) = match decision {
                MediaInstallDecision::Copied {
                    stored_path,
                    public_path,
                } => {
                    // A canonical row may have appeared since planning. Prefer
                    // it and leave our copy for post-commit cleanup.
                    match find_canonical_media_tx(&tx, &media.media_kind, &media.sha256)? {
                        Some((canonical_id, canonical_stored, canonical_public)) => {
                            unused_copies.push(stored_path.clone());
                            (canonical_stored, canonical_public, Some(canonical_id))
                        }
                        None => (
                            stored_path.to_string_lossy().to_string(),
                            public_path.clone(),
                            None,
                        ),
                    }
                }
                MediaInstallDecision::Canonical {
                    canonical_id,
                    stored_path,
                    public_path,
                } => {
                    let still_valid = tx
                        .query_row(
                            r#"
                            SELECT 1 FROM media
                            WHERE id = ? AND canonical_media_id IS NULL
                              AND media_kind = ? AND original_sha256 = ?
                            "#,
                            params![canonical_id, media.media_kind, media.sha256],
                            |_| Ok(()),
                        )
                        .optional()?
                        .is_some();
                    if !still_valid {
                        anyhow::bail!(
                            "media deduplication target disappeared during import"
                        );
                    }
                    (stored_path.clone(), public_path.clone(), Some(*canonical_id))
                }
            };
            tx.execute(
                r#"
                INSERT INTO media (
                  owner_user_id, original_filename, stored_path, public_path,
                  mime_type, media_kind, byte_len, alt_text, conversion_state,
                  original_sha256, canonical_media_id, is_nsfw
                )
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'imported', ?, ?, ?)
                "#,
                params![
                    user_id,
                    media.original_filename,
                    stored_path,
                    public_path,
                    media.mime_type,
                    media.media_kind,
                    i64::try_from(media.byte_len)?,
                    media.alt_text,
                    media.sha256,
                    canonical_id,
                    i64::from(media.is_nsfw),
                ],
            )?;
            media_id_map.insert(media.id, tx.last_insert_rowid());
        }

        // Posts are inserted in two phases: first without references, then an
        // UPDATE per row. This avoids ordering problems when a quote points at
        // a post with a higher row id while keeping foreign keys valid.
        let mut post_id_map: HashMap<i64, i64> = HashMap::with_capacity(plan.posts.len());
        for post in &plan.posts {
            tx.execute(
                "INSERT INTO posts (user_id, text, created_at, edited_at) VALUES (?, ?, ?, ?)",
                params![user_id, post.text, post.created_at, post.edited_at],
            )?;
            post_id_map.insert(post.archive_id, tx.last_insert_rowid());
        }
        let mut post_references_dropped = 0usize;
        for post in &plan.posts {
            let new_id = *post_id_map
                .get(&post.archive_id)
                .ok_or_else(|| anyhow::anyhow!("internal import error: post id missing"))?;
            let parent_id = post.parent_id.and_then(|id| post_id_map.get(&id).copied());
            let root_id = post.root_id.and_then(|id| post_id_map.get(&id).copied());
            let quote_id = post.quote_id.and_then(|id| post_id_map.get(&id).copied());
            post_references_dropped += usize::from(post.parent_id.is_some() && parent_id.is_none())
                + usize::from(post.root_id.is_some() && root_id.is_none())
                + usize::from(post.quote_id.is_some() && quote_id.is_none());
            if parent_id.is_some() || root_id.is_some() || quote_id.is_some() {
                tx.execute(
                    "UPDATE posts SET parent_post_id = ?, root_post_id = ?, quote_post_id = ? WHERE id = ?",
                    params![parent_id, root_id, quote_id, new_id],
                )?;
            }
            for (position, media_id) in post.media_ids.iter().enumerate() {
                let new_media_id = media_id_map.get(media_id).copied().ok_or_else(|| {
                    anyhow::anyhow!("internal import error: media id missing")
                })?;
                tx.execute(
                    "INSERT INTO post_media (post_id, media_id, position) VALUES (?, ?, ?)",
                    params![new_id, new_media_id, i64::try_from(position)?],
                )?;
            }
            for embed in &post.embeds {
                tx.execute(
                    r#"
                    INSERT INTO post_embeds (
                      post_id, provider, video_id, original_url, canonical_url, title,
                      thumbnail_url, embed_url, position, fetched_at
                    )
                    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, CASE WHEN ? IS NULL THEN NULL ELSE CURRENT_TIMESTAMP END)
                    "#,
                    params![
                        new_id,
                        embed.provider,
                        embed.video_id,
                        embed.original_url,
                        embed.canonical_url,
                        embed.title,
                        embed.thumbnail_url,
                        embed.embed_url,
                        embed.position,
                        embed.title,
                    ],
                )?;
            }
        }

        // Follows resolve against the destination instance; protected targets
        // always use follow_requests and never bypass approval.
        let mut follows_imported = 0usize;
        let mut follows_pending = 0usize;
        let mut follows_skipped = 0usize;
        for follow in &plan.follows {
            let Ok(normalized) = normalize_username(&follow.username, max_username_len) else {
                follows_skipped += 1;
                continue;
            };
            let target = tx
                .query_row(
                    "SELECT id, is_deleted, is_suspended, follow_approval_required FROM users WHERE normalized_username = ?",
                    [normalized],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, i64>(3)?,
                        ))
                    },
                )
                .optional()?;
            let Some((target_id, is_deleted, is_suspended, approval_required)) = target else {
                follows_skipped += 1;
                continue;
            };
            if target_id == user_id || is_deleted != 0 || is_suspended != 0 {
                follows_skipped += 1;
                continue;
            }
            if is_blocked_tx(&tx, user_id, target_id)? {
                follows_skipped += 1;
                continue;
            }
            if approval_required != 0 {
                let already_following = tx
                    .query_row(
                        "SELECT 1 FROM follows WHERE follower_id = ? AND followed_id = ?",
                        params![user_id, target_id],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
                if already_following {
                    follows_skipped += 1;
                    continue;
                }
                let inserted = tx.execute(
                    "INSERT OR IGNORE INTO follow_requests (requester_id, target_id, created_at) VALUES (?, ?, ?)",
                    params![user_id, target_id, follow.created_at],
                )?;
                if inserted > 0 {
                    create_notification_tx(
                        &tx,
                        target_id,
                        user_id,
                        "follow_request",
                        FOLLOW_REQUEST_NOTIFICATION_MESSAGE,
                    )?;
                    follows_pending += 1;
                } else {
                    follows_skipped += 1;
                }
            } else {
                let inserted = tx.execute(
                    "INSERT OR IGNORE INTO follows (follower_id, followed_id, created_at) VALUES (?, ?, ?)",
                    params![user_id, target_id, follow.created_at],
                )?;
                if inserted > 0 {
                    create_notification_tx(
                        &tx,
                        target_id,
                        user_id,
                        "follow",
                        FOLLOW_NOTIFICATION_MESSAGE,
                    )?;
                    follows_imported += 1;
                } else {
                    follows_skipped += 1;
                }
            }
        }

        // Muted words: duplicates are silently skipped by the unique index.
        let mut muted_words_imported = 0usize;
        for word in &plan.muted_words {
            let normalized = word.term.to_lowercase();
            let inserted = tx.execute(
                "INSERT OR IGNORE INTO muted_words (user_id, term, normalized_term, created_at) VALUES (?, ?, ?, ?)",
                params![user_id, word.term, normalized, word.created_at],
            )?;
            if inserted > 0 {
                muted_words_imported += 1;
            }
        }

        let report = ImportReport {
            archive_id: plan.archive_id.clone(),
            archive_username: plan.archive_username.clone(),
            posts_imported: plan.posts.len(),
            media_imported: plan.media.len(),
            follows_imported,
            follows_pending,
            follows_skipped,
            post_references_dropped,
            muted_words_imported,
            profile_fields_applied,
            profile_fields_skipped,
        };
        // The UNIQUE(user_id, archive_id) constraint makes concurrent double
        // imports fail here, inside the transaction, and roll everything back.
        let insert_result = tx.execute(
            r#"
            INSERT INTO account_imports (
              user_id, archive_id, format_version, posts_imported, media_imported,
              follows_imported, follows_pending, follows_skipped
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            "#,
            params![
                user_id,
                plan.archive_id,
                i64::from(FORMAT_VERSION),
                i64::try_from(report.posts_imported)?,
                i64::try_from(report.media_imported)?,
                i64::try_from(report.follows_imported)?,
                i64::try_from(report.follows_pending)?,
                i64::try_from(report.follows_skipped)?,
            ],
        );
        match insert_result {
            Ok(_) => {}
            Err(rusqlite::Error::SqliteFailure(error, _message))
                if error.code == rusqlite::ErrorCode::ConstraintViolation
                    && matches!(error.extended_code, SQLITE_CONSTRAINT_UNIQUE | SQLITE_CONSTRAINT_PRIMARYKEY) =>
            {
                anyhow::bail!(ALREADY_IMPORTED_MESSAGE);
            }
            Err(error) => return Err(error.into()),
        }

        tx.commit()?;
        Ok((report, unused_copies))
    })
    .await
}

fn fill_empty_field(
    current: &str,
    archived: &str,
    applied: &mut usize,
    skipped: &mut usize,
) -> String {
    if current.is_empty() && !archived.is_empty() {
        *applied += 1;
        archived.to_owned()
    } else {
        *skipped += 1;
        current.to_owned()
    }
}

fn replace_preference<T: PartialEq>(
    current: T,
    archived: T,
    applied: &mut usize,
    skipped: &mut usize,
) -> T {
    if current == archived {
        *skipped += 1;
        current
    } else {
        *applied += 1;
        archived
    }
}

fn find_canonical_media_tx(
    tx: &rusqlite::Transaction<'_>,
    media_kind: &str,
    sha256: &str,
) -> anyhow::Result<Option<(i64, String, String)>> {
    let canonical = tx
        .query_row(
            r#"
            SELECT id, stored_path, public_path FROM media
            WHERE canonical_media_id IS NULL AND media_kind = ? AND original_sha256 = ?
            ORDER BY id ASC LIMIT 1
            "#,
            params![media_kind, sha256],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;
    Ok(canonical)
}

fn is_blocked_tx(tx: &rusqlite::Transaction<'_>, left: i64, right: i64) -> anyhow::Result<bool> {
    let blocked = tx
        .query_row(
            r#"
            SELECT 1 FROM blocks
            WHERE (blocker_id = ? AND blocked_id = ?) OR (blocker_id = ? AND blocked_id = ?)
            LIMIT 1
            "#,
            params![left, right, right, left],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    Ok(blocked)
}

/// Mirrors `social::create_notification_tx` (private): one notification per
/// (actor, kind) pair, and never a self-notification.
fn create_notification_tx(
    tx: &rusqlite::Transaction<'_>,
    user_id: i64,
    actor_id: i64,
    kind: &str,
    message: &str,
) -> anyhow::Result<()> {
    if user_id == actor_id {
        return Ok(());
    }
    let exists = tx
        .query_row(
            r#"
            SELECT 1 FROM notifications
            WHERE user_id = ? AND kind = ? AND actor_user_id = ? AND post_id IS NULL
            LIMIT 1
            "#,
            params![user_id, kind, actor_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if exists {
        return Ok(());
    }
    tx.execute(
        "INSERT INTO notifications (user_id, actor_user_id, post_id, kind, message) VALUES (?, ?, NULL, ?, ?)",
        params![user_id, actor_id, kind, message],
    )?;
    Ok(())
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{auth, config::Settings, db};

    struct TestInstance {
        temp: tempfile::TempDir,
        paths: RuntimePaths,
        pool: SqlitePool,
        settings: Settings,
    }

    async fn test_instance() -> TestInstance {
        let temp = tempfile::tempdir().expect("temp dir");
        let paths = RuntimePaths::from_data_dir(temp.path().join("data"));
        paths.ensure().expect("runtime paths");
        let pool = db::connect(&paths.database_path).await.expect("connect");
        db::migrate(&pool).await.expect("migrate");
        TestInstance {
            temp,
            paths,
            pool,
            settings: Settings::default(),
        }
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let digest = hasher.finalize();
        let mut encoded = String::with_capacity(digest.len() * 2);
        for byte in digest {
            encoded.push_str(&format!("{byte:02x}"));
        }
        encoded
    }

    fn document_bytes<T: Serialize>(value: &T) -> Vec<u8> {
        serde_json::to_vec(value).expect("document json")
    }

    fn build_archive(path: &Path, entries: &[(String, Vec<u8>)]) {
        let typed = entries
            .iter()
            .map(|(name, bytes)| (name.as_str(), EntryType::Regular, bytes.as_slice()))
            .collect::<Vec<_>>();
        write_archive_entries(path, &typed);
    }

    /// Builds a gzip tar archive from explicit entry types. Names up to 100
    /// bytes are written verbatim so adversarial names survive unmodified;
    /// longer names go through tar-rs's GNU long-name extension.
    fn build_typed_archive(path: &Path, entries: &[(String, EntryType, Vec<u8>)]) {
        let typed = entries
            .iter()
            .map(|(name, entry_type, bytes)| (name.as_str(), *entry_type, bytes.as_slice()))
            .collect::<Vec<_>>();
        write_archive_entries(path, &typed);
    }

    fn write_archive_entries(path: &Path, entries: &[(&str, EntryType, &[u8])]) {
        let file = File::create(path).expect("archive file");
        let encoder = GzEncoder::new(file, Compression::default());
        let mut builder = Builder::new(encoder);
        for (name, entry_type, bytes) in entries {
            let mut header = Header::new_gnu();
            header.set_entry_type(*entry_type);
            header.set_size(u64::try_from(bytes.len()).expect("entry size"));
            header.set_mode(0o600);
            if name.len() <= 100 {
                let name_bytes = name.as_bytes();
                header.as_gnu_mut().expect("gnu header").name[..name_bytes.len()]
                    .copy_from_slice(name_bytes);
                header.set_cksum();
                builder.append(&header, *bytes).expect("append entry");
            } else {
                builder
                    .append_data(&mut header, name, *bytes)
                    .expect("append long entry");
            }
        }
        let encoder = builder.into_inner().expect("tar finish");
        encoder.finish().expect("gzip finish");
    }

    /// Minimal PNG-magic bytes; enough for `infer` to accept the content.
    fn png_bytes(payload: &[u8]) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(payload);
        bytes
    }

    /// Minimal JPEG-magic bytes; enough for `infer` to accept the content.
    fn jpeg_bytes(payload: &[u8]) -> Vec<u8> {
        let mut bytes = b"\xff\xd8\xff\xe0".to_vec();
        bytes.extend_from_slice(payload);
        bytes
    }

    /// A raw tar header with the name written verbatim into the GNU name
    /// field, bypassing tar-rs path normalization that would strip `.`
    /// components or reject unsafe names.
    fn raw_header(name: &str, entry_type: EntryType, size: u64) -> Header {
        let mut header = Header::new_gnu();
        let name_bytes = name.as_bytes();
        assert!(name_bytes.len() <= 100, "raw tar names must fit one field");
        header.as_gnu_mut().expect("gnu header").name[..name_bytes.len()]
            .copy_from_slice(name_bytes);
        header.set_entry_type(entry_type);
        header.set_size(size);
        header.set_mode(0o600);
        header.set_cksum();
        header
    }

    fn push_raw_entry(tar_bytes: &mut Vec<u8>, name: &str, entry_type: EntryType, data: &[u8]) {
        push_raw_header(
            tar_bytes,
            name,
            entry_type,
            u64::try_from(data.len()).expect("entry size"),
        );
        tar_bytes.extend_from_slice(data);
        let padding = (512 - data.len() % 512) % 512;
        tar_bytes.extend(std::iter::repeat_n(0_u8, padding));
    }

    fn push_raw_header(tar_bytes: &mut Vec<u8>, name: &str, entry_type: EntryType, size: u64) {
        tar_bytes.extend_from_slice(raw_header(name, entry_type, size).as_bytes());
    }

    fn write_gzip(path: &Path, bytes: &[u8]) {
        let file = File::create(path).expect("archive file");
        let mut encoder = GzEncoder::new(file, Compression::default());
        encoder.write_all(bytes).expect("gzip write");
        encoder.finish().expect("gzip finish");
    }

    fn manifest_document(entry_count: usize) -> ManifestDocument {
        ManifestDocument {
            format_version: FORMAT_VERSION,
            app: APP_NAME.to_owned(),
            created_at: "2026-01-01T00:00:00Z".to_owned(),
            archive_id: Uuid::new_v4().to_string(),
            username: "alice".to_owned(),
            entry_count,
        }
    }

    fn valid_profile_document() -> ProfileDocument {
        ProfileDocument {
            display_name: "Archived".to_owned(),
            bio: "archived bio".to_owned(),
            location: "Somewhere".to_owned(),
            website: "https://example.com".to_owned(),
            theme: "dark".to_owned(),
            nsfw_blur_enabled: false,
            liked_posts_public: false,
            follow_approval_required: false,
            created_at: "2026-01-01 00:00:00".to_owned(),
        }
    }

    fn standard_documents() -> Vec<(String, Vec<u8>)> {
        vec![
            (
                PROFILE_PATH.to_owned(),
                document_bytes(&valid_profile_document()),
            ),
            (
                POSTS_PATH.to_owned(),
                document_bytes(&Vec::<PostDocument>::new()),
            ),
            (
                MEDIA_PATH.to_owned(),
                document_bytes(&Vec::<MediaDocument>::new()),
            ),
            (
                FOLLOWS_PATH.to_owned(),
                document_bytes(&Vec::<FollowDocument>::new()),
            ),
            (
                SETTINGS_PATH.to_owned(),
                document_bytes(&SettingsDocument {
                    muted_words: Vec::new(),
                }),
            ),
        ]
    }

    fn replace_document(documents: &mut [(String, Vec<u8>)], name: &str, bytes: Vec<u8>) {
        let entry = documents
            .iter_mut()
            .find(|(entry_name, _)| entry_name == name)
            .expect("document exists");
        entry.1 = bytes;
    }

    fn write_test_archive(path: &Path, documents: Vec<(String, Vec<u8>)>, format_version: u32) {
        let mut manifest = manifest_document(documents.len() + 1);
        manifest.format_version = format_version;
        let mut entries = vec![(MANIFEST_PATH.to_owned(), document_bytes(&manifest))];
        entries.extend(documents);
        build_archive(path, &entries);
    }

    /// Writes an archive whose documents may use non-regular entry types. The
    /// manifest's `entry_count` always covers every entry written.
    fn write_typed_test_archive(
        path: &Path,
        entries: Vec<(String, EntryType, Vec<u8>)>,
        format_version: u32,
    ) {
        let mut manifest = manifest_document(entries.len() + 1);
        manifest.format_version = format_version;
        let mut all = vec![(
            MANIFEST_PATH.to_owned(),
            EntryType::Regular,
            document_bytes(&manifest),
        )];
        all.extend(entries);
        build_typed_archive(path, &all);
    }

    fn test_media_document(byte_len: u64, sha256: String) -> MediaDocument {
        media_document(0, "media/0.png", byte_len, sha256)
    }

    fn media_document(id: i64, archive_path: &str, byte_len: u64, sha256: String) -> MediaDocument {
        MediaDocument {
            id,
            original_filename: "photo.png".to_owned(),
            mime_type: "image/png".to_owned(),
            media_kind: "image".to_owned(),
            byte_len,
            alt_text: String::new(),
            is_nsfw: false,
            conversion_state: "original".to_owned(),
            sha256,
            archive_path: archive_path.to_owned(),
        }
    }

    async fn register_user(instance: &TestInstance, username: &str) -> i64 {
        auth::register_user(
            &instance.pool,
            &instance.settings,
            username,
            "very secure password",
            false,
        )
        .await
        .expect("registered user")
    }

    /// Limits an operator could not configure through validation, used to make
    /// overflow and boundary cases reachable without huge fixtures.
    fn adversarial_settings(expanded_bytes: u64, entries: usize) -> Settings {
        let mut settings = Settings::default();
        settings.accounts.max_archive_expanded_bytes = expanded_bytes;
        settings.accounts.max_archive_entries = entries;
        settings
    }

    #[derive(Debug, PartialEq, Eq)]
    struct DestinationSnapshot {
        posts: i64,
        media: i64,
        post_media: i64,
        follows: i64,
        follow_requests: i64,
        muted_words: i64,
        notifications: i64,
        account_imports: i64,
        upload_files: Vec<String>,
        tmp_entries: Vec<String>,
    }

    async fn snapshot_instance(instance: &TestInstance, user_id: i64) -> DestinationSnapshot {
        let counts = instance
            .pool
            .call(move |conn| {
                let count = |sql: &str| -> rusqlite::Result<i64> {
                    conn.query_row(sql, [user_id], |row| row.get(0))
                };
                Ok((
                    count("SELECT COUNT(*) FROM posts WHERE user_id = ?")?,
                    count("SELECT COUNT(*) FROM media WHERE owner_user_id = ?")?,
                    count(
                        "SELECT COUNT(*) FROM post_media pm JOIN posts p ON p.id = pm.post_id WHERE p.user_id = ?",
                    )?,
                    count("SELECT COUNT(*) FROM follows WHERE follower_id = ?")?,
                    count("SELECT COUNT(*) FROM follow_requests WHERE requester_id = ?")?,
                    count("SELECT COUNT(*) FROM muted_words WHERE user_id = ?")?,
                    count("SELECT COUNT(*) FROM notifications WHERE user_id = ?")?,
                    count("SELECT COUNT(*) FROM account_imports WHERE user_id = ?")?,
                ))
            })
            .await
            .expect("snapshot counts");
        DestinationSnapshot {
            posts: counts.0,
            media: counts.1,
            post_media: counts.2,
            follows: counts.3,
            follow_requests: counts.4,
            muted_words: counts.5,
            notifications: counts.6,
            account_imports: counts.7,
            upload_files: upload_listing(&instance.paths),
            tmp_entries: directory_listing(&instance.paths.tmp_dir),
        }
    }

    fn upload_listing(paths: &RuntimePaths) -> Vec<String> {
        let mut files = Vec::new();
        for (label, root) in [
            ("originals", &paths.uploads_originals),
            ("images", &paths.uploads_images),
            ("videos", &paths.uploads_videos),
            ("thumbs", &paths.uploads_thumbs),
        ] {
            collect_files(root, label, &mut files);
        }
        files.sort();
        files
    }

    fn collect_files(root: &Path, prefix: &str, out: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(root) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            if path.is_dir() {
                collect_files(&path, &format!("{prefix}/{name}"), out);
            } else {
                let size = fs::symlink_metadata(&path).map_or(0, |metadata| metadata.len());
                out.push(format!("{prefix}/{name}:{size}"));
            }
        }
    }

    fn directory_listing(dir: &Path) -> Vec<String> {
        let mut names = fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    /// Runs one import that must fail, asserting the user-facing message and
    /// that the destination database, upload directories, and staging
    /// directory are byte-for-byte unchanged.
    async fn import_rejected(
        instance: &TestInstance,
        settings: &Settings,
        user_id: i64,
        archive_path: &Path,
        expected: &str,
    ) -> String {
        let before = snapshot_instance(instance, user_id).await;
        let error = import_account(
            &instance.pool,
            &instance.paths,
            settings,
            user_id,
            archive_path,
        )
        .await
        .expect_err("archive must be rejected");
        let message = format!("{error:#}");
        assert!(
            message.contains(expected),
            "expected {expected:?} in rejection message {message:?}"
        );
        let after = snapshot_instance(instance, user_id).await;
        assert_eq!(
            before, after,
            "a rejected import must not mutate the destination"
        );
        assert!(
            after
                .tmp_entries
                .iter()
                .all(|name| !name.starts_with(IMPORT_TMP_PREFIX)),
            "import staging entries left behind: {:?}",
            after.tmp_entries
        );
        message
    }

    #[tokio::test]
    #[expect(
        clippy::too_many_lines,
        reason = "one end-to-end scenario covering export, import, conflicts, and repeat import"
    )]
    async fn export_import_roundtrip_between_instances() {
        let source = test_instance().await;
        let destination = test_instance().await;
        let source_settings = source.settings.clone();
        let destination_settings = destination.settings.clone();

        let alice = auth::register_user(
            &source.pool,
            &source_settings,
            "alice",
            "very secure password",
            false,
        )
        .await
        .expect("alice");
        let carol = auth::register_user(
            &source.pool,
            &source_settings,
            "carol",
            "very secure password",
            false,
        )
        .await
        .expect("carol");
        let dave = auth::register_user(
            &source.pool,
            &source_settings,
            "dave",
            "very secure password",
            false,
        )
        .await
        .expect("dave");
        let ghost = auth::register_user(
            &source.pool,
            &source_settings,
            "ghost",
            "very secure password",
            false,
        )
        .await
        .expect("ghost");
        let blocked_user = auth::register_user(
            &source.pool,
            &source_settings,
            "blocked_user",
            "very secure password",
            false,
        )
        .await
        .expect("blocked_user");
        let suspended_user = auth::register_user(
            &source.pool,
            &source_settings,
            "suspended_user",
            "very secure password",
            false,
        )
        .await
        .expect("suspended_user");
        let source_bob = auth::register_user(
            &source.pool,
            &source_settings,
            "bob",
            "very secure password",
            false,
        )
        .await
        .expect("bob");

        source
            .pool
            .call(move |conn| {
                conn.execute(
                    r#"
                    UPDATE users SET display_name = 'Alice', bio = 'alice bio',
                      location = 'Berlin', website = 'https://example.com',
                      theme = 'dark', nsfw_blur_enabled = 0,
                      liked_posts_public = 0, follow_approval_required = 1
                    WHERE id = ?
                    "#,
                    [alice],
                )?;
                Ok(())
            })
            .await
            .expect("profile");
        source
            .pool
            .call(move |conn| {
                for target in [
                    carol,
                    dave,
                    ghost,
                    blocked_user,
                    suspended_user,
                    source_bob,
                ] {
                    conn.execute(
                        "INSERT INTO follows (follower_id, followed_id, created_at) VALUES (?, ?, '2026-01-01 00:00:00')",
                        params![alice, target],
                    )?;
                }
                Ok(())
            })
            .await
            .expect("follows");
        source
            .pool
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO muted_words (user_id, term, normalized_term, created_at) VALUES (?, 'Spoiler', 'spoiler', '2026-01-01 00:00:00')",
                    [alice],
                )?;
                conn.execute(
                    "INSERT INTO muted_words (user_id, term, normalized_term, created_at) VALUES (?, 'politics', 'politics', '2026-01-02 00:00:00')",
                    [alice],
                )?;
                Ok(())
            })
            .await
            .expect("muted words");

        let media_bytes = png_bytes(b"alice media bytes");
        let media_path = source.paths.uploads_images.join("alice-photo.png");
        fs::write(&media_path, &media_bytes).expect("media file");
        let media_path_string = media_path.to_string_lossy().to_string();
        let media_sha = sha256_hex(&media_bytes);
        let media_len = i64::try_from(media_bytes.len()).expect("media len");
        let (_source_post1, _source_post2) = source
            .pool
            .call(move |conn| {
                conn.execute(
                    r#"
                    INSERT INTO media (
                      owner_user_id, original_filename, stored_path, public_path,
                      mime_type, media_kind, byte_len, alt_text, conversion_state,
                      original_sha256, is_nsfw
                    )
                    VALUES (?, 'alice-photo.png', ?, '/uploads/images/alice-photo.png', 'image/png', 'image', ?, 'alt text', 'original', ?, 1)
                    "#,
                    params![alice, media_path_string, media_len, media_sha],
                )?;
                let media_id = conn.last_insert_rowid();
                conn.execute(
                    "INSERT INTO posts (user_id, text, created_at) VALUES (?, 'alice first post', '2026-01-02 03:04:05')",
                    [alice],
                )?;
                let post1 = conn.last_insert_rowid();
                conn.execute(
                    "INSERT INTO post_media (post_id, media_id, position) VALUES (?, ?, 0)",
                    params![post1, media_id],
                )?;
                conn.execute(
                    r#"
                    INSERT INTO post_embeds (
                      post_id, provider, video_id, original_url, canonical_url, title,
                      thumbnail_url, embed_url, position, fetched_at
                    )
                    VALUES (?, 'youtube', 'dQw4w9WgXcQ', 'https://www.youtube.com/watch?v=dQw4w9WgXcQ', 'https://www.youtube.com/watch?v=dQw4w9WgXcQ', 'A video', 'https://i.ytimg.com/vi/dQw4w9WgXcQ/hqdefault.jpg', 'https://www.youtube.com/embed/dQw4w9WgXcQ', 0, CURRENT_TIMESTAMP)
                    "#,
                    [post1],
                )?;
                conn.execute(
                    "INSERT INTO posts (user_id, text, parent_post_id, root_post_id, created_at) VALUES (?, 'alice reply', ?, ?, '2026-01-02 03:05:06')",
                    params![alice, post1, post1],
                )?;
                let post2 = conn.last_insert_rowid();
                Ok((post1, post2))
            })
            .await
            .expect("posts");

        let export_dir = source.temp.path().join("exports");
        fs::create_dir_all(&export_dir).expect("export dir");
        let archive_path = export_dir.join("alice-account.tar.gz");
        let export = export_account(
            &source.pool,
            &source.paths,
            alice,
            &archive_path,
            ArchiveLimits::from_settings(&source_settings),
        )
        .await
        .expect("export");
        assert_eq!(export.posts, 2);
        assert_eq!(export.media, 1);
        assert_eq!(export.follows, 6);
        assert_eq!(export.muted_words, 2);
        assert!(export.bytes > 0);
        assert!(archive_path.is_file());

        let traversal_destination = export_dir.join("..").join("escape.tar.gz");
        let error = export_account(
            &source.pool,
            &source.paths,
            alice,
            &traversal_destination,
            ArchiveLimits::from_settings(&source_settings),
        )
        .await
        .expect_err("traversal destination");
        assert!(error.to_string().contains(".."), "{error}");
        let missing_parent = export_dir.join("missing").join("out.tar.gz");
        let error = export_account(
            &source.pool,
            &source.paths,
            alice,
            &missing_parent,
            ArchiveLimits::from_settings(&source_settings),
        )
        .await
        .expect_err("missing parent");
        assert!(error.to_string().contains("does not exist"), "{error}");

        let bob = auth::register_user(
            &destination.pool,
            &destination_settings,
            "bob",
            "very secure password",
            false,
        )
        .await
        .expect("bob");
        let carol_dest = auth::register_user(
            &destination.pool,
            &destination_settings,
            "carol",
            "very secure password",
            false,
        )
        .await
        .expect("carol destination");
        let dave_dest = auth::register_user(
            &destination.pool,
            &destination_settings,
            "dave",
            "very secure password",
            false,
        )
        .await
        .expect("dave destination");
        let blocked_dest = auth::register_user(
            &destination.pool,
            &destination_settings,
            "blocked_user",
            "very secure password",
            false,
        )
        .await
        .expect("blocked destination");
        let suspended_dest = auth::register_user(
            &destination.pool,
            &destination_settings,
            "suspended_user",
            "very secure password",
            false,
        )
        .await
        .expect("suspended destination");
        destination
            .pool
            .call(move |conn| {
                conn.execute(
                    "UPDATE users SET follow_approval_required = 1 WHERE id = ?",
                    [dave_dest],
                )?;
                conn.execute(
                    "UPDATE users SET is_suspended = 1 WHERE id = ?",
                    [suspended_dest],
                )?;
                conn.execute(
                    "INSERT INTO blocks (blocker_id, blocked_id) VALUES (?, ?)",
                    params![blocked_dest, bob],
                )?;
                conn.execute(
                    "INSERT INTO muted_words (user_id, term, normalized_term) VALUES (?, 'Spoiler', 'spoiler')",
                    [bob],
                )?;
                Ok(())
            })
            .await
            .expect("destination setup");

        let report = import_account(
            &destination.pool,
            &destination.paths,
            &destination_settings,
            bob,
            &archive_path,
        )
        .await
        .expect("import");
        assert_eq!(report.archive_username, "alice");
        assert_eq!(report.posts_imported, 2);
        assert_eq!(report.media_imported, 1);
        assert_eq!(report.follows_imported, 1);
        assert_eq!(report.follows_pending, 1);
        assert_eq!(report.follows_skipped, 4);
        assert_eq!(report.muted_words_imported, 1);
        assert_eq!(report.profile_fields_applied, 7);
        assert_eq!(report.profile_fields_skipped, 1);

        let account = destination
            .pool
            .call(move |conn| {
                Ok(conn.query_row(
                    r#"
                    SELECT username, is_admin, theme, nsfw_blur_enabled, liked_posts_public,
                      follow_approval_required, display_name, bio, location, website
                    FROM users WHERE id = ?
                    "#,
                    [bob],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, i64>(4)?,
                            row.get::<_, i64>(5)?,
                            row.get::<_, String>(6)?,
                            row.get::<_, String>(7)?,
                            row.get::<_, String>(8)?,
                            row.get::<_, String>(9)?,
                        ))
                    },
                )?)
            })
            .await
            .expect("account");
        assert_eq!(account.0, "bob");
        assert_eq!(account.1, 0);
        assert_eq!(account.2, "dark");
        assert_eq!(account.3, 0);
        assert_eq!(account.4, 0);
        assert_eq!(account.5, 1);
        assert_eq!(account.6, "bob");
        assert_eq!(account.7, "alice bio");
        assert_eq!(account.8, "Berlin");
        assert_eq!(account.9, "https://example.com");
        assert!(
            auth::verify_user_password(&destination.pool, bob, "very secure password")
                .await
                .expect("password check")
        );
        let sessions: i64 = destination
            .pool
            .call(|conn| Ok(conn.query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))?))
            .await
            .expect("sessions");
        assert_eq!(sessions, 0);

        let posts = destination
            .pool
            .call(move |conn| {
                let mut statement = conn.prepare(
                    "SELECT id, text, parent_post_id, root_post_id, created_at FROM posts WHERE user_id = ? ORDER BY id ASC",
                )?;
                let rows = statement
                    .query_map([bob], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, Option<i64>>(2)?,
                            row.get::<_, Option<i64>>(3)?,
                            row.get::<_, String>(4)?,
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
            .expect("posts");
        assert_eq!(posts.len(), 2);
        assert_eq!(posts[0].1, "alice first post");
        assert_eq!(posts[1].1, "alice reply");
        assert_eq!(posts[0].4, "2026-01-02 03:04:05");
        assert_eq!(posts[1].2, Some(posts[0].0));
        assert_eq!(posts[1].3, Some(posts[0].0));

        let media = destination
            .pool
            .call(move |conn| {
                Ok(conn.query_row(
                    r#"
                    SELECT owner_user_id, stored_path, public_path, byte_len, conversion_state,
                      original_sha256, mime_type, media_kind, alt_text, is_nsfw, original_filename
                    FROM media WHERE owner_user_id = ?
                    "#,
                    [bob],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, String>(5)?,
                            row.get::<_, String>(6)?,
                            row.get::<_, String>(7)?,
                            row.get::<_, String>(8)?,
                            row.get::<_, i64>(9)?,
                            row.get::<_, String>(10)?,
                        ))
                    },
                )?)
            })
            .await
            .expect("media");
        assert_eq!(media.0, bob);
        assert_eq!(media.3, media_len);
        assert_eq!(media.4, "imported");
        assert_eq!(media.5, sha256_hex(&media_bytes));
        assert_eq!(media.6, "image/png");
        assert_eq!(media.7, "image");
        assert_eq!(media.8, "alt text");
        assert_eq!(media.9, 1);
        assert_eq!(media.10, "alice-photo.png");
        assert!(Path::new(&media.1).starts_with(&destination.paths.uploads_images));
        assert!(media.2.starts_with("/uploads/images/"));
        assert_eq!(
            fs::read(&media.1).expect("imported media bytes"),
            media_bytes
        );

        let media_links: i64 = destination
            .pool
            .call(move |conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM post_media pm JOIN posts p ON p.id = pm.post_id WHERE p.user_id = ?",
                    [bob],
                    |row| row.get(0),
                )?)
            })
            .await
            .expect("post media");
        assert_eq!(media_links, 1);
        let embed_video: String = destination
            .pool
            .call(move |conn| {
                Ok(conn.query_row(
                    "SELECT pe.video_id FROM post_embeds pe JOIN posts p ON p.id = pe.post_id WHERE p.user_id = ?",
                    [bob],
                    |row| row.get(0),
                )?)
            })
            .await
            .expect("embed");
        assert_eq!(embed_video, "dQw4w9WgXcQ");

        let follows = destination
            .pool
            .call(move |conn| {
                let mut statement = conn.prepare(
                    "SELECT followed_id FROM follows WHERE follower_id = ? ORDER BY followed_id",
                )?;
                let rows = statement
                    .query_map([bob], |row| row.get::<_, i64>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
            .expect("follows");
        assert_eq!(follows, vec![carol_dest]);
        let pending = destination
            .pool
            .call(move |conn| {
                let mut statement = conn.prepare(
                    "SELECT target_id FROM follow_requests WHERE requester_id = ? ORDER BY target_id",
                )?;
                let rows = statement
                    .query_map([bob], |row| row.get::<_, i64>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
            .expect("pending requests");
        assert_eq!(pending, vec![dave_dest]);
        let notifications = destination
            .pool
            .call(move |conn| {
                let mut statement = conn.prepare(
                    "SELECT user_id, kind FROM notifications WHERE actor_user_id = ? ORDER BY user_id",
                )?;
                let rows = statement
                    .query_map([bob], |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
            .expect("notifications");
        assert!(notifications.contains(&(carol_dest, "follow".to_owned())));
        assert!(notifications.contains(&(dave_dest, "follow_request".to_owned())));
        assert_eq!(notifications.len(), 2);
        let muted_terms = destination
            .pool
            .call(move |conn| {
                let mut statement =
                    conn.prepare("SELECT term FROM muted_words WHERE user_id = ? ORDER BY term")?;
                let rows = statement
                    .query_map([bob], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
            .expect("muted words");
        assert_eq!(
            muted_terms,
            vec!["Spoiler".to_owned(), "politics".to_owned()]
        );

        // A second export has a new archive id, so importing it into another
        // destination account is allowed and must reuse the canonical media
        // row instead of copying the bytes again.
        let dedupe_archive = export_dir.join("alice-account-dedupe.tar.gz");
        export_account(
            &source.pool,
            &source.paths,
            alice,
            &dedupe_archive,
            ArchiveLimits::from_settings(&source_settings),
        )
        .await
        .expect("dedupe export");
        let dedupe_report = import_account(
            &destination.pool,
            &destination.paths,
            &destination_settings,
            carol_dest,
            &dedupe_archive,
        )
        .await
        .expect("dedupe import");
        assert_eq!(dedupe_report.media_imported, 1);
        let duplicate_media = destination
            .pool
            .call(move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT canonical_media_id, stored_path FROM media WHERE owner_user_id = ?",
                        [carol_dest],
                        |row| Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, String>(1)?)),
                    )
                    .optional()?)
            })
            .await
            .expect("duplicate media");
        let (canonical_media_id, duplicate_stored_path) =
            duplicate_media.expect("duplicate media row");
        assert!(canonical_media_id.is_some());
        assert_eq!(duplicate_stored_path, media.1);
        let image_files = fs::read_dir(&destination.paths.uploads_images)
            .expect("uploads dir")
            .count();
        assert_eq!(image_files, 1);

        let error = import_account(
            &destination.pool,
            &destination.paths,
            &destination_settings,
            bob,
            &archive_path,
        )
        .await
        .expect_err("double import");
        assert!(
            error.to_string().contains("already been imported"),
            "{error}"
        );
        let posts_after: i64 = destination
            .pool
            .call(move |conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM posts WHERE user_id = ?",
                    [bob],
                    |row| row.get(0),
                )?)
            })
            .await
            .expect("posts after");
        assert_eq!(posts_after, 2);
    }

    #[tokio::test]
    async fn import_rejects_invalid_archives() {
        let instance = test_instance().await;
        let user = register_user(&instance, "importer").await;
        let settings = instance.settings.clone();
        let archive_dir = instance.temp.path().join("archives");
        fs::create_dir_all(&archive_dir).expect("archive dir");

        let path = archive_dir.join("wrong-version.tar.gz");
        write_test_archive(&path, standard_documents(), 2);
        import_rejected(&instance, &settings, user, &path, "format version").await;

        let path = archive_dir.join("traversal.tar.gz");
        let mut documents = standard_documents();
        documents.push(("../evil".to_owned(), b"x".to_vec()));
        write_test_archive(&path, documents, FORMAT_VERSION);
        import_rejected(&instance, &settings, user, &path, "traversal").await;

        let path = archive_dir.join("absolute.tar.gz");
        let mut documents = standard_documents();
        documents.push(("/evil".to_owned(), b"x".to_vec()));
        write_test_archive(&path, documents, FORMAT_VERSION);
        import_rejected(&instance, &settings, user, &path, "absolute").await;

        let path = archive_dir.join("duplicate.tar.gz");
        let mut documents = standard_documents();
        documents.push((
            POSTS_PATH.to_owned(),
            document_bytes(&Vec::<PostDocument>::new()),
        ));
        write_test_archive(&path, documents, FORMAT_VERSION);
        import_rejected(&instance, &settings, user, &path, "duplicate").await;

        let path = archive_dir.join("duplicate-manifest.tar.gz");
        let mut documents = standard_documents();
        documents.push((
            MANIFEST_PATH.to_owned(),
            document_bytes(&manifest_document(7)),
        ));
        write_test_archive(&path, documents, FORMAT_VERSION);
        import_rejected(&instance, &settings, user, &path, "duplicate").await;

        let path = archive_dir.join("malformed.tar.gz");
        let mut documents = standard_documents();
        replace_document(&mut documents, POSTS_PATH, b"{".to_vec());
        write_test_archive(&path, documents, FORMAT_VERSION);
        import_rejected(&instance, &settings, user, &path, "JSON").await;

        let path = archive_dir.join("sha-mismatch.tar.gz");
        let media_bytes = b"tiny archive media".to_vec();
        let mut documents = standard_documents();
        replace_document(
            &mut documents,
            MEDIA_PATH,
            document_bytes(&vec![test_media_document(
                u64::try_from(media_bytes.len()).expect("media len"),
                "0".repeat(64),
            )]),
        );
        documents.push(("media/0.png".to_owned(), media_bytes));
        write_test_archive(&path, documents, FORMAT_VERSION);
        import_rejected(&instance, &settings, user, &path, "sha256").await;

        // The declared byte total must respect the settings-derived expanded
        // limit, not the legacy compile-time constant.
        let path = archive_dir.join("too-many-bytes.tar.gz");
        let media_bytes = b"tiny archive media".to_vec();
        let mut documents = standard_documents();
        replace_document(
            &mut documents,
            MEDIA_PATH,
            document_bytes(&vec![test_media_document(
                MAX_ARCHIVE_BYTES + 1,
                sha256_hex(&media_bytes),
            )]),
        );
        documents.push(("media/0.png".to_owned(), media_bytes));
        write_test_archive(&path, documents, FORMAT_VERSION);
        let bounded = adversarial_settings(64 * 1024, MAX_ENTRIES);
        import_rejected(&instance, &bounded, user, &path, "declares more than").await;

        let path = archive_dir.join("not-gzip.tar.gz");
        fs::write(&path, b"not a gzip archive").expect("write");
        import_rejected(&instance, &settings, user, &path, "gzip tar").await;

        let path = archive_dir.join("corrupt-gzip-header.tar.gz");
        let mut bytes = fs::read(archive_dir.join("wrong-version.tar.gz")).expect("read archive");
        bytes[3] = 0xff; // invalid gzip compression method
        fs::write(&path, bytes).expect("write");
        import_rejected(&instance, &settings, user, &path, "gzip tar").await;
    }

    #[test]
    fn capped_writer_rejects_output_past_the_limit() {
        let mut writer = CappedWriter::new(Vec::new(), 4);
        writer.write_all(b"abc").expect("within limit");
        let error = writer.write_all(b"de").expect_err("over limit");
        assert!(error.to_string().contains("limit"), "{error}");
    }

    #[tokio::test]
    async fn export_allows_account_with_no_posts_or_media() {
        let instance = test_instance().await;
        let user = auth::register_user(
            &instance.pool,
            &instance.settings,
            "empty_account",
            "very secure password",
            false,
        )
        .await
        .expect("user");
        let export_dir = instance.temp.path().join("exports");
        fs::create_dir_all(&export_dir).expect("export dir");
        let archive_path = export_dir.join("empty-account.tar.gz");

        let report = export_account(
            &instance.pool,
            &instance.paths,
            user,
            &archive_path,
            ArchiveLimits::from_settings(&instance.settings),
        )
        .await
        .expect("export");

        assert_eq!(report.posts, 0);
        assert_eq!(report.media, 0);
        assert_eq!(report.follows, 0);
        assert_eq!(report.muted_words, 0);
        assert!(archive_path.is_file());
    }

    #[tokio::test]
    async fn import_rejects_gzip_bomb_without_mutations() {
        let instance = test_instance().await;
        let user = register_user(&instance, "bomb_target").await;
        let archive_dir = instance.temp.path().join("archives");
        fs::create_dir_all(&archive_dir).expect("archive dir");

        // 4 MiB of zeros compresses to a few KiB, so only the decompressed
        // limit can stop this archive.
        let media_bytes = vec![0_u8; 4 * 1024 * 1024];
        let mut documents = standard_documents();
        replace_document(
            &mut documents,
            MEDIA_PATH,
            document_bytes(&vec![test_media_document(
                u64::try_from(media_bytes.len()).expect("media len"),
                sha256_hex(&media_bytes),
            )]),
        );
        documents.push(("media/0.png".to_owned(), media_bytes));
        let path = archive_dir.join("bomb.tar.gz");
        write_test_archive(&path, documents, FORMAT_VERSION);
        let compressed_size = fs::metadata(&path).expect("archive metadata").len();
        assert!(
            compressed_size < 64 * 1024,
            "fixture must remain a tiny compressed bomb, got {compressed_size} bytes"
        );

        let settings = adversarial_settings(64 * 1024, MAX_ENTRIES);
        import_rejected(&instance, &settings, user, &path, "declares").await;
    }

    #[tokio::test]
    async fn import_rejects_entry_count_over_limit() {
        let instance = test_instance().await;
        let user = register_user(&instance, "entry_limit").await;
        let archive_dir = instance.temp.path().join("archives");
        fs::create_dir_all(&archive_dir).expect("archive dir");

        let path = archive_dir.join("many-entries.tar.gz");
        write_test_archive(&path, standard_documents(), FORMAT_VERSION);

        let mut settings = instance.settings.clone();
        settings.accounts.max_archive_entries = 3;
        import_rejected(&instance, &settings, user, &path, "more than 3 entries").await;
    }

    #[tokio::test]
    async fn import_rejects_media_entry_count_over_limit() {
        let instance = test_instance().await;
        let user = register_user(&instance, "media_count").await;
        let archive_dir = instance.temp.path().join("archives");
        fs::create_dir_all(&archive_dir).expect("archive dir");

        // One more media entry than media.json would ever accept must be
        // rejected while extracting, not after staging a thousand files.
        let mut documents = standard_documents();
        for index in 0..=MAX_MEDIA {
            documents.push((
                format!("{MEDIA_PREFIX}{index}.png"),
                png_bytes(b"media count"),
            ));
        }
        let path = archive_dir.join("too-many-media-files.tar.gz");
        write_test_archive(&path, documents, FORMAT_VERSION);
        import_rejected(
            &instance,
            &instance.settings,
            user,
            &path,
            "more than 1000 media files",
        )
        .await;
    }

    #[tokio::test]
    async fn import_rejects_oversized_media_headers() {
        let instance = test_instance().await;
        let user = register_user(&instance, "oversized_header").await;
        let archive_dir = instance.temp.path().join("archives");
        fs::create_dir_all(&archive_dir).expect("archive dir");

        // The tar header itself declares more bytes than the expanded limit,
        // so the entry must be rejected before any of it is written.
        let media_bytes = vec![0x42_u8; 2 * 1024 * 1024];
        let mut documents = standard_documents();
        replace_document(
            &mut documents,
            MEDIA_PATH,
            document_bytes(&vec![test_media_document(
                u64::try_from(media_bytes.len()).expect("media len"),
                sha256_hex(&media_bytes),
            )]),
        );
        documents.push(("media/0.png".to_owned(), media_bytes));
        let path = archive_dir.join("oversized-entry.tar.gz");
        write_test_archive(&path, documents, FORMAT_VERSION);
        let settings = adversarial_settings(1024 * 1024, MAX_ENTRIES);
        import_rejected(&instance, &settings, user, &path, "declares").await;

        // A hostile header may declare u64::MAX bytes with no data at all.
        let mut entries = vec![(
            MANIFEST_PATH.to_owned(),
            EntryType::Regular,
            document_bytes(&manifest_document(7)),
        )];
        entries.extend(
            standard_documents()
                .into_iter()
                .map(|(name, bytes)| (name, EntryType::Regular, bytes)),
        );
        let mut tar_bytes = Vec::new();
        for (name, entry_type, bytes) in &entries {
            push_raw_entry(&mut tar_bytes, name, *entry_type, bytes);
        }
        // tar-rs itself rejects sizes that overflow its position counter, so
        // declare the largest size it parses and let the entry header check
        // stop it before any bytes are written.
        push_raw_header(
            &mut tar_bytes,
            "media/0.png",
            EntryType::Regular,
            u64::MAX / 2,
        );
        let path = archive_dir.join("huge-declared-header.tar.gz");
        write_gzip(&path, &tar_bytes);
        import_rejected(&instance, &instance.settings, user, &path, "declares").await;
    }

    #[tokio::test]
    async fn import_rejects_declared_byte_len_overflow() {
        let instance = test_instance().await;
        let user = register_user(&instance, "overflow_target").await;
        let archive_dir = instance.temp.path().join("archives");
        fs::create_dir_all(&archive_dir).expect("archive dir");

        // One document alone declares more than the expanded limit.
        let media_bytes = png_bytes(b"overflow-check");
        let mut documents = standard_documents();
        replace_document(
            &mut documents,
            MEDIA_PATH,
            document_bytes(&vec![test_media_document(
                u64::MAX,
                sha256_hex(&media_bytes),
            )]),
        );
        documents.push(("media/0.png".to_owned(), media_bytes.clone()));
        let path = archive_dir.join("u64-max-declared.tar.gz");
        write_test_archive(&path, documents, FORMAT_VERSION);
        import_rejected(
            &instance,
            &instance.settings,
            user,
            &path,
            "declares more than",
        )
        .await;

        // Two documents whose declared total overflows a u64 addition.
        let half = u64::MAX / 2 + 1;
        let mut documents = standard_documents();
        replace_document(
            &mut documents,
            MEDIA_PATH,
            document_bytes(&vec![
                media_document(0, "media/0.png", half, sha256_hex(&media_bytes)),
                media_document(1, "media/1.png", half, sha256_hex(&media_bytes)),
            ]),
        );
        documents.push(("media/0.png".to_owned(), media_bytes.clone()));
        documents.push(("media/1.png".to_owned(), media_bytes));
        let path = archive_dir.join("declared-total-overflow.tar.gz");
        write_test_archive(&path, documents, FORMAT_VERSION);
        // The largest representable limit still must not wrap on addition.
        let settings = adversarial_settings(u64::MAX, MAX_ENTRIES);
        import_rejected(&instance, &settings, user, &path, "overflow").await;
    }

    #[tokio::test]
    async fn import_rejects_total_document_bytes_over_limit() {
        let instance = test_instance().await;
        let user = register_user(&instance, "document_limit").await;
        let archive_dir = instance.temp.path().join("archives");
        fs::create_dir_all(&archive_dir).expect("archive dir");

        // Four maximum-size documents plus the manifest cross the cumulative
        // 64 MiB document cap even though each one is individually allowed.
        let big = vec![b'x'; usize::try_from(DOCUMENT_MAX_BYTES).expect("document size")];
        let mut documents = standard_documents();
        for name in [PROFILE_PATH, POSTS_PATH, MEDIA_PATH, FOLLOWS_PATH] {
            replace_document(&mut documents, name, big.clone());
        }
        let path = archive_dir.join("total-documents.tar.gz");
        write_test_archive(&path, documents, FORMAT_VERSION);
        import_rejected(&instance, &instance.settings, user, &path, "total limit").await;
    }

    #[tokio::test]
    async fn import_rejects_media_metadata_disagreement() {
        let instance = test_instance().await;
        let user = register_user(&instance, "metadata_target").await;
        let archive_dir = instance.temp.path().join("archives");
        fs::create_dir_all(&archive_dir).expect("archive dir");

        // Declared PNG but the bytes are a JPEG.
        let jpeg = jpeg_bytes(b"jpeg payload");
        let mut documents = standard_documents();
        replace_document(
            &mut documents,
            MEDIA_PATH,
            document_bytes(&vec![test_media_document(
                u64::try_from(jpeg.len()).expect("media len"),
                sha256_hex(&jpeg),
            )]),
        );
        documents.push(("media/0.png".to_owned(), jpeg));
        let path = archive_dir.join("sniff-mismatch.tar.gz");
        write_test_archive(&path, documents, FORMAT_VERSION);
        import_rejected(
            &instance,
            &instance.settings,
            user,
            &path,
            "does not match the declared",
        )
        .await;

        // Bytes `infer` cannot classify must not be installed as media.
        let junk = b"not a media file".to_vec();
        let mut documents = standard_documents();
        replace_document(
            &mut documents,
            MEDIA_PATH,
            document_bytes(&vec![test_media_document(
                u64::try_from(junk.len()).expect("media len"),
                sha256_hex(&junk),
            )]),
        );
        documents.push(("media/0.png".to_owned(), junk));
        let path = archive_dir.join("sniff-unknown.tar.gz");
        write_test_archive(&path, documents, FORMAT_VERSION);
        import_rejected(
            &instance,
            &instance.settings,
            user,
            &path,
            "recognized media type",
        )
        .await;

        // Declared byte_len disagreeing with the extracted size.
        let png = png_bytes(b"byte len mismatch");
        let mut documents = standard_documents();
        replace_document(
            &mut documents,
            MEDIA_PATH,
            document_bytes(&vec![test_media_document(
                u64::try_from(png.len()).expect("media len") + 1,
                sha256_hex(&png),
            )]),
        );
        documents.push(("media/0.png".to_owned(), png));
        let path = archive_dir.join("byte-len-mismatch.tar.gz");
        write_test_archive(&path, documents, FORMAT_VERSION);
        import_rejected(&instance, &instance.settings, user, &path, "does not match").await;
    }

    #[tokio::test]
    async fn import_rejects_media_document_mismatches() {
        let instance = test_instance().await;
        let user = register_user(&instance, "media_docs").await;
        let archive_dir = instance.temp.path().join("archives");
        fs::create_dir_all(&archive_dir).expect("archive dir");

        let media_bytes = png_bytes(b"media document");
        let media_len = u64::try_from(media_bytes.len()).expect("media len");
        let media_sha = sha256_hex(&media_bytes);

        // Duplicate media ids.
        let mut documents = standard_documents();
        replace_document(
            &mut documents,
            MEDIA_PATH,
            document_bytes(&vec![
                media_document(0, "media/0.png", media_len, media_sha.clone()),
                media_document(0, "media/0.png", media_len, media_sha.clone()),
            ]),
        );
        documents.push(("media/0.png".to_owned(), media_bytes.clone()));
        let path = archive_dir.join("duplicate-media-ids.tar.gz");
        write_test_archive(&path, documents, FORMAT_VERSION);
        import_rejected(
            &instance,
            &instance.settings,
            user,
            &path,
            "duplicate media id",
        )
        .await;

        // The same archive path listed twice.
        let mut documents = standard_documents();
        replace_document(
            &mut documents,
            MEDIA_PATH,
            document_bytes(&vec![
                media_document(0, "media/0.png", media_len, media_sha.clone()),
                media_document(1, "media/0.png", media_len, media_sha.clone()),
            ]),
        );
        documents.push(("media/0.png".to_owned(), media_bytes.clone()));
        let path = archive_dir.join("duplicate-media-paths.tar.gz");
        write_test_archive(&path, documents, FORMAT_VERSION);
        import_rejected(&instance, &instance.settings, user, &path, "more than once").await;

        // A listed file that is not in the archive.
        let mut documents = standard_documents();
        replace_document(
            &mut documents,
            MEDIA_PATH,
            document_bytes(&vec![media_document(
                0,
                "media/0.png",
                media_len,
                media_sha.clone(),
            )]),
        );
        let path = archive_dir.join("listed-file-missing.tar.gz");
        write_test_archive(&path, documents, FORMAT_VERSION);
        import_rejected(
            &instance,
            &instance.settings,
            user,
            &path,
            "missing from the archive",
        )
        .await;

        // An archive media file that media.json never lists.
        let mut documents = standard_documents();
        documents.push(("media/0.png".to_owned(), media_bytes.clone()));
        let path = archive_dir.join("unlisted-media-file.tar.gz");
        write_test_archive(&path, documents, FORMAT_VERSION);
        import_rejected(
            &instance,
            &instance.settings,
            user,
            &path,
            "unlisted media file",
        )
        .await;

        // Case-variant media entry names collide on case-insensitive targets.
        let mut documents = standard_documents();
        documents.push(("media/0.png".to_owned(), media_bytes.clone()));
        documents.push(("media/0.PNG".to_owned(), media_bytes));
        let path = archive_dir.join("case-variant-media.tar.gz");
        write_test_archive(&path, documents, FORMAT_VERSION);
        import_rejected(
            &instance,
            &instance.settings,
            user,
            &path,
            "duplicate entry",
        )
        .await;
    }

    #[tokio::test]
    async fn import_rejects_unsafe_entry_paths() {
        let instance = test_instance().await;
        let user = register_user(&instance, "path_target").await;
        let archive_dir = instance.temp.path().join("archives");
        fs::create_dir_all(&archive_dir).expect("archive dir");

        let cases = [
            ("média.png", "non-ASCII"),
            ("media/../evil.png", "traversal"),
            ("/evil.png", "absolute"),
            ("media\\evil.png", "unsafe characters"),
            ("C:evil.png", "unsafe characters"),
            ("media/./evil.png", "invalid media path"),
            ("media//evil.png", "unsafe characters"),
            ("%2e%2e/evil.png", "unsafe characters"),
            ("media/a/b.png", "invalid media path"),
        ];
        for (index, (name, expected)) in cases.iter().enumerate() {
            let mut documents = standard_documents();
            documents.push(((*name).to_owned(), png_bytes(b"path payload")));
            let path = archive_dir.join(format!("unsafe-{index}.tar.gz"));
            write_test_archive(&path, documents, FORMAT_VERSION);
            import_rejected(&instance, &instance.settings, user, &path, expected).await;
        }

        // Media file names may not end in '.' or a space, and may not exceed
        // the per-component length every common filesystem accepts.
        let long_name = format!("{MEDIA_PREFIX}{}", "a".repeat(300));
        let tail_cases = [
            ("media/evil.png.", "ends with"),
            ("media/evil.png ", "ends with"),
            (long_name.as_str(), "too long"),
        ];
        for (index, (name, expected)) in tail_cases.iter().enumerate() {
            let mut documents = standard_documents();
            documents.push(((*name).to_owned(), png_bytes(b"path payload")));
            let path = archive_dir.join(format!("media-name-{index}.tar.gz"));
            write_test_archive(&path, documents, FORMAT_VERSION);
            import_rejected(&instance, &instance.settings, user, &path, expected).await;
        }
    }

    #[tokio::test]
    async fn import_rejects_unsupported_entry_types() {
        let instance = test_instance().await;
        let user = register_user(&instance, "type_target").await;
        let archive_dir = instance.temp.path().join("archives");
        fs::create_dir_all(&archive_dir).expect("archive dir");

        let cases = [
            (EntryType::Symlink, "unsupported entry type"),
            (EntryType::Link, "unsupported entry type"),
            (EntryType::Char, "unsupported entry type"),
            (EntryType::Fifo, "unsupported entry type"),
        ];
        for (index, (entry_type, expected)) in cases.into_iter().enumerate() {
            let mut entries = standard_documents()
                .into_iter()
                .map(|(name, bytes)| (name, EntryType::Regular, bytes))
                .collect::<Vec<_>>();
            entries.push(("media/link".to_owned(), entry_type, Vec::new()));
            let path = archive_dir.join(format!("entry-type-{index}.tar.gz"));
            write_typed_test_archive(&path, entries, FORMAT_VERSION);
            import_rejected(&instance, &instance.settings, user, &path, expected).await;
        }

        // A directory entry may only be the flat `media/` directory itself.
        let mut entries = standard_documents()
            .into_iter()
            .map(|(name, bytes)| (name, EntryType::Regular, bytes))
            .collect::<Vec<_>>();
        entries.push(("media/foo".to_owned(), EntryType::Directory, Vec::new()));
        let path = archive_dir.join("directory-media-foo.tar.gz");
        write_typed_test_archive(&path, entries, FORMAT_VERSION);
        import_rejected(
            &instance,
            &instance.settings,
            user,
            &path,
            "unexpected directory",
        )
        .await;
    }

    #[tokio::test]
    async fn import_rejects_truncated_media_entry() {
        let instance = test_instance().await;
        let user = register_user(&instance, "truncated_target").await;
        let archive_dir = instance.temp.path().join("archives");
        fs::create_dir_all(&archive_dir).expect("archive dir");

        let media_bytes = png_bytes(&vec![0xAB_u8; 4096]);
        let media_len = u64::try_from(media_bytes.len()).expect("media len");
        let mut documents = standard_documents();
        replace_document(
            &mut documents,
            MEDIA_PATH,
            document_bytes(&vec![media_document(
                0,
                "media/0.png",
                media_len,
                sha256_hex(&media_bytes),
            )]),
        );

        let mut entries = vec![(
            MANIFEST_PATH.to_owned(),
            EntryType::Regular,
            document_bytes(&manifest_document(documents.len() + 2)),
        )];
        entries.extend(
            documents
                .into_iter()
                .map(|(name, bytes)| (name, EntryType::Regular, bytes)),
        );
        let mut tar_bytes = Vec::new();
        for (name, entry_type, bytes) in &entries {
            push_raw_entry(&mut tar_bytes, name, *entry_type, bytes);
        }
        push_raw_header(&mut tar_bytes, "media/0.png", EntryType::Regular, media_len);
        tar_bytes.extend_from_slice(&media_bytes[..media_bytes.len() / 2]);
        let path = archive_dir.join("truncated-media.tar.gz");
        write_gzip(&path, &tar_bytes);

        // tar-rs notices the entry's remaining declared bytes are missing when
        // it advances to the next header, and reports the truncated stream.
        import_rejected(&instance, &instance.settings, user, &path, "unexpected EOF").await;
    }

    #[tokio::test]
    async fn import_enforces_compressed_size_boundary() {
        let instance = test_instance().await;
        let archive_dir = instance.temp.path().join("archives");
        fs::create_dir_all(&archive_dir).expect("archive dir");

        let path = archive_dir.join("boundary.tar.gz");
        write_test_archive(&path, standard_documents(), FORMAT_VERSION);
        let size = fs::metadata(&path).expect("archive metadata").len();
        assert!(size > 0);

        let mut at_limit = instance.settings.clone();
        at_limit.accounts.max_archive_upload_bytes = size;
        let at_limit_user = register_user(&instance, "at_limit").await;
        let report = import_account(
            &instance.pool,
            &instance.paths,
            &at_limit,
            at_limit_user,
            &path,
        )
        .await
        .expect("an archive exactly at the compressed limit must import");
        assert_eq!(report.posts_imported, 0);

        let mut over_limit = instance.settings.clone();
        over_limit.accounts.max_archive_upload_bytes = size - 1;
        let over_limit_user = register_user(&instance, "over_limit").await;
        import_rejected(&instance, &over_limit, over_limit_user, &path, "too large").await;
    }
}
