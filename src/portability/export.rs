//! Export for the portability subsystem.

use super::*;

/// Writes a complete archive for `user_id` into `destination`.
///
/// `destination` must name a file inside an existing directory; the archive is
/// written to a temporary sibling and renamed into place, so a failed export
/// never leaves a partial file. A `..` component, a symlink, a non-file
/// destination, or a missing parent directory is rejected. The caller usually
/// stages under `RuntimePaths::tmp_dir` using [`EXPORT_TMP_PREFIX`].
pub async fn export_account(
    pool: &SqlitePool,
    paths: &RuntimePaths,
    user_id: i64,
    destination: &Path,
    limits: ArchiveLimits,
) -> anyhow::Result<ExportReport> {
    let snapshot = load_export_snapshot(pool, user_id).await?;
    let paths = paths.clone();
    let destination = destination.to_owned();
    tokio::task::spawn_blocking(move || {
        let _operation = crate::runtime::begin_temp_operation();
        write_export_snapshot(&paths, &snapshot, &destination, limits, user_id)
    })
    .await
    .context("account export worker failed")?
}

fn write_export_snapshot(
    paths: &RuntimePaths,
    snapshot: &ExportSnapshot,
    destination: &Path,
    limits: ArchiveLimits,
    user_id: i64,
) -> anyhow::Result<ExportReport> {
    let destination_dir = validate_export_destination(destination)?;
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
pub(super) fn validate_export_destination(destination: &Path) -> anyhow::Result<PathBuf> {
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
pub(super) fn keep_exported_reference(
    reference: Option<i64>,
    exported: &BTreeSet<i64>,
) -> Option<i64> {
    reference.filter(|id| exported.contains(id))
}

/// Reads every exportable record in one consistent snapshot. Only non-deleted
/// posts authored by the account are considered, and media is limited to rows
/// owned by the account that are attached to one of those posts.
#[expect(
    clippy::too_many_lines,
    reason = "one consistent read of every exportable record family keeps the snapshot coherent"
)]
pub(super) async fn load_export_snapshot(
    pool: &SqlitePool,
    user_id: i64,
) -> anyhow::Result<ExportSnapshot> {
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
pub(super) fn validate_export_media_source(
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

/// Writes the gzip tar archive to a temporary sibling and renames it into
/// place. The temporary file is removed on every failure path.
pub(super) fn write_export_archive(
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

pub(super) fn append_json_entry<T: Serialize + ?Sized>(
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

pub(super) fn append_media_entry(
    builder: &mut ExportBuilder,
    plan: &ExportMediaPlan,
) -> anyhow::Result<()> {
    let mut source = File::open(&plan.source)
        .with_context(|| format!("failed to open media file {}", plan.source.display()))?;
    let header = archive_entry_header(&plan.archive_path, plan.byte_len)?;
    builder
        .append(&header, &mut source)
        .with_context(|| format!("failed to write archive entry {}", plan.archive_path))?;
    Ok(())
}

pub(super) fn archive_entry_header(archive_path: &str, size: u64) -> anyhow::Result<Header> {
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

pub(super) fn hash_file_streaming(path: &Path) -> anyhow::Result<(u64, String)> {
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
pub(super) fn export_media_extension(mime_type: &str, media_kind: &str) -> &'static str {
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
