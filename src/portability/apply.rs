//! Apply for the portability subsystem.

use super::*;

/// Applies the import in one transaction, retaining file ownership through
/// commit even if the HTTP client disconnects while the job is queued.
#[expect(
    clippy::too_many_lines,
    reason = "the import transaction inserts every record family in one auditable block"
)]
pub(super) async fn apply_import(
    pool: &SqlitePool,
    user_id: i64,
    max_username_len: usize,
    mut plan: ImportPlan,
) -> anyhow::Result<ImportReport> {
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

        tx.execute("DELETE FROM storage_reservations WHERE id=?",[&plan.reservation_id])?;
        tx.commit()?;
        for guard in &mut plan.cleanup { guard.disarm(); }
        remove_files_best_effort(&unused_copies);
        Ok(report)
    })
    .await
}

pub(super) fn fill_empty_field(
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

pub(super) fn replace_preference<T: PartialEq>(
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

pub(super) fn find_canonical_media_tx(
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

pub(super) fn is_blocked_tx(
    tx: &rusqlite::Transaction<'_>,
    left: i64,
    right: i64,
) -> anyhow::Result<bool> {
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
pub(super) fn create_notification_tx(
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

pub(super) fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}
