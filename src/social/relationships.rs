//! Relationships for the social subsystem.

use super::*;

/// Follows an account, or records a request when the target requires approval.
///
/// Targets with `users.follow_approval_required = 1` receive a
/// `follow_request` notification and must approve the request before the
/// follow counts or grants follower privileges. Existing followers are never
/// removed when an account enables protection. Repeated and concurrent
/// requests are idempotent, and blocks in either direction reject the action.
/// Deleted or suspended accounts can neither follow nor be followed.
pub async fn follow(
    pool: &SqlitePool,
    follower_id: i64,
    followed_id: i64,
) -> anyhow::Result<FollowOutcome> {
    pool.call(move |conn| {
        let tx = conn.transaction()?;
        let outcome = follow_in_transaction(&tx, follower_id, followed_id)?;
        tx.commit()?;
        Ok(outcome)
    })
    .await
}

pub fn follow_in_transaction(
    tx: &rusqlite::Transaction<'_>,
    follower_id: i64,
    followed_id: i64,
) -> anyhow::Result<FollowOutcome> {
    anyhow::ensure!(follower_id != followed_id, "cannot follow yourself");
    let actor_available = tx
        .query_row(
            "SELECT is_deleted = 0 AND is_suspended = 0 FROM users WHERE id = ?",
            [follower_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(0)
        != 0;
    if !actor_available {
        anyhow::bail!("account cannot follow");
    }
    let target = tx
            .query_row(
                "SELECT is_deleted = 0 AND is_suspended = 0, follow_approval_required FROM users WHERE id = ?",
                [followed_id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?;
    let Some((available, approval_required)) = target else {
        anyhow::bail!("account cannot be followed");
    };
    if available == 0 || blocks_exist_tx(tx, follower_id, followed_id)? {
        anyhow::bail!("account cannot be followed");
    }
    if follow_exists_tx(tx, follower_id, followed_id)? {
        return Ok(FollowOutcome::AlreadyFollowing);
    }
    // Pending requests stay pending even after the target disables
    // protection: the owner can still approve or reject them, and asking
    // again does not create a second request.
    if follow_request_exists_tx(tx, follower_id, followed_id)? {
        return Ok(FollowOutcome::AlreadyRequested);
    }
    if approval_required != 0 {
        let changed = tx.execute(
            "INSERT OR IGNORE INTO follow_requests (requester_id, target_id) VALUES (?, ?)",
            params![follower_id, followed_id],
        )?;
        if changed > 0 {
            create_notification_tx(
                tx,
                followed_id,
                Some(follower_id),
                None,
                "follow_request",
                "requested to follow you",
            )?;
        }
        return Ok(if changed > 0 {
            FollowOutcome::Requested
        } else {
            FollowOutcome::AlreadyRequested
        });
    }
    let changed = tx.execute(
        "INSERT OR IGNORE INTO follows (follower_id, followed_id) VALUES (?, ?)",
        params![follower_id, followed_id],
    )?;
    if changed > 0 {
        create_notification_tx(
            tx,
            followed_id,
            Some(follower_id),
            None,
            "follow",
            "followed you",
        )?;
    }
    Ok(if changed > 0 {
        FollowOutcome::Followed
    } else {
        FollowOutcome::AlreadyFollowing
    })
}

pub(super) fn follow_exists_tx(
    tx: &rusqlite::Transaction<'_>,
    follower_id: i64,
    followed_id: i64,
) -> anyhow::Result<bool> {
    Ok(tx
        .query_row(
            "SELECT 1 FROM follows WHERE follower_id = ? AND followed_id = ?",
            params![follower_id, followed_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

pub(super) fn follow_request_exists_tx(
    tx: &rusqlite::Transaction<'_>,
    requester_id: i64,
    target_id: i64,
) -> anyhow::Result<bool> {
    Ok(tx
        .query_row(
            "SELECT 1 FROM follow_requests WHERE requester_id = ? AND target_id = ?",
            params![requester_id, target_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Approves a pending request, creating the follow and notifying the requester.
///
/// The request row is deleted first and only a successful delete allows the
/// follow to be created. Requests from deleted, suspended, or blocked
/// accounts are discarded without creating a follow or notification.
/// Returns `true` when a pending request was consumed; when the follow already
/// exists the request is still consumed, but the follow row and the approval
/// notification are never duplicated.
pub async fn approve_follow_request(
    pool: &SqlitePool,
    target_id: i64,
    requester_id: i64,
) -> anyhow::Result<bool> {
    pool.call(move |conn| {
        let tx = conn.transaction()?;
        let changed = tx.execute(
            "DELETE FROM follow_requests WHERE requester_id = ? AND target_id = ?",
            params![requester_id, target_id],
        )?;
        if changed == 0 {
            tx.commit()?;
            return Ok(false);
        }
        let requester_available = tx
            .query_row(
                "SELECT is_deleted = 0 AND is_suspended = 0 FROM users WHERE id = ?",
                [requester_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0)
            != 0;
        if !requester_available || blocks_exist_tx(&tx, requester_id, target_id)? {
            tx.commit()?;
            return Ok(false);
        }
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO follows (follower_id, followed_id) VALUES (?, ?)",
            params![requester_id, target_id],
        )?;
        if inserted > 0 {
            create_notification_tx(
                &tx,
                requester_id,
                Some(target_id),
                None,
                "follow_request_approved",
                "approved your follow request",
            )?;
        }
        tx.commit()?;
        Ok(true)
    })
    .await
}

/// Rejects a pending request. Returns `true` when a request existed.
pub async fn reject_follow_request(
    pool: &SqlitePool,
    target_id: i64,
    requester_id: i64,
) -> anyhow::Result<bool> {
    pool.call(move |conn| {
        let changed = conn.execute(
            "DELETE FROM follow_requests WHERE requester_id = ? AND target_id = ?",
            params![requester_id, target_id],
        )?;
        Ok(changed > 0)
    })
    .await
}

/// Cancels the viewer's own pending request to another account.
pub async fn cancel_follow_request(
    pool: &SqlitePool,
    requester_id: i64,
    target_id: i64,
) -> anyhow::Result<bool> {
    pool.call(move |conn| {
        let changed = conn.execute(
            "DELETE FROM follow_requests WHERE requester_id = ? AND target_id = ?",
            params![requester_id, target_id],
        )?;
        Ok(changed > 0)
    })
    .await
}

/// Pending incoming requests awaiting `user_id`'s approval, oldest first.
///
/// Blocked pairs are hidden in both directions. Suspended requesters are not
/// filtered here because approval re-validates the requester and discards
/// requests that can no longer be approved.
pub async fn incoming_follow_requests(
    pool: &SqlitePool,
    user_id: i64,
) -> anyhow::Result<Vec<FollowRequestView>> {
    pool.call(move |conn| {
        let mut stmt = conn.prepare(
            r#"
            SELECT u.id, u.username, u.display_name, u.bio,
              COALESCE(pic.thumbnail_public_path, pic.public_path),
              r.created_at
            FROM follow_requests r
            JOIN users u ON u.id = r.requester_id
            LEFT JOIN media pic ON pic.id = u.profile_picture_media_id
            WHERE r.target_id = ? AND u.is_deleted = 0
              AND NOT EXISTS (SELECT 1 FROM blocks WHERE blocker_id = ? AND blocked_id = u.id)
              AND NOT EXISTS (SELECT 1 FROM blocks WHERE blocker_id = u.id AND blocked_id = ?)
            ORDER BY r.created_at ASC, r.requester_id ASC
            "#,
        )?;
        let rows = stmt
            .query_map(params![user_id, user_id, user_id], map_follow_request_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}

/// Pending outgoing requests created by `user_id`, oldest first.
///
/// The returned accounts are the request targets, so the UI can link to them.
pub async fn outgoing_follow_requests(
    pool: &SqlitePool,
    user_id: i64,
) -> anyhow::Result<Vec<FollowRequestView>> {
    pool.call(move |conn| {
        let mut stmt = conn.prepare(
            r#"
            SELECT u.id, u.username, u.display_name, u.bio,
              COALESCE(pic.thumbnail_public_path, pic.public_path),
              r.created_at
            FROM follow_requests r
            JOIN users u ON u.id = r.target_id
            LEFT JOIN media pic ON pic.id = u.profile_picture_media_id
            WHERE r.requester_id = ? AND u.is_deleted = 0 AND u.is_suspended = 0
              AND NOT EXISTS (SELECT 1 FROM blocks WHERE blocker_id = ? AND blocked_id = u.id)
              AND NOT EXISTS (SELECT 1 FROM blocks WHERE blocker_id = u.id AND blocked_id = ?)
            ORDER BY r.created_at ASC, r.target_id ASC
            "#,
        )?;
        let rows = stmt
            .query_map(params![user_id, user_id, user_id], map_follow_request_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}

pub(super) fn map_follow_request_row(row: &Row<'_>) -> rusqlite::Result<FollowRequestView> {
    Ok(FollowRequestView {
        user_id: row.get(0)?,
        username: row.get(1)?,
        display_name: row.get(2)?,
        bio: row.get(3)?,
        profile_picture_path: row.get(4)?,
        created_at: row.get(5)?,
    })
}

/// Number of pending incoming requests for `user_id`.
pub async fn pending_follow_request_count(pool: &SqlitePool, user_id: i64) -> anyhow::Result<i64> {
    pool.call(move |conn| {
        Ok(conn.query_row(
            r#"
            SELECT COUNT(*)
            FROM follow_requests r
            JOIN users u ON u.id = r.requester_id
            WHERE r.target_id = ? AND u.is_deleted = 0
              AND NOT EXISTS (SELECT 1 FROM blocks WHERE blocker_id = ? AND blocked_id = u.id)
              AND NOT EXISTS (SELECT 1 FROM blocks WHERE blocker_id = u.id AND blocked_id = ?)
            "#,
            params![user_id, user_id, user_id],
            |row| row.get(0),
        )?)
    })
    .await
}

/// Enables or disables follow approval for an account.
///
/// Enabling protection never removes existing follows. Disabling protection
/// leaves existing pending requests pending: the owner can still approve or
/// reject them, and new follows are accepted directly.
pub async fn set_follow_approval_required(
    pool: &SqlitePool,
    user_id: i64,
    required: bool,
) -> anyhow::Result<()> {
    pool.call(move |conn| {
        let changed = conn.execute(
            "UPDATE users SET follow_approval_required = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ? AND is_deleted = 0",
            params![i64::from(required), user_id],
        )?;
        if changed == 0 {
            anyhow::bail!("account not found");
        }
        Ok(())
    })
    .await
}

pub async fn active_follow_targets(pool: &SqlitePool, ids: &[i64]) -> anyhow::Result<Vec<i64>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = vec!["?"; ids.len()].join(", ");
    let sql = format!(
        "SELECT id FROM users WHERE id IN ({placeholders}) AND is_deleted = 0 AND is_suspended = 0"
    );
    let candidate_ids = ids.to_vec();
    let query_ids = candidate_ids.clone();
    let available = pool
        .call(move |conn| {
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map(params_from_iter(query_ids.iter()), |row| {
                    row.get::<_, i64>(0)
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?
        .into_iter()
        .collect::<BTreeSet<_>>();
    Ok(candidate_ids
        .iter()
        .copied()
        .filter(|id| available.contains(id))
        .collect())
}

/// Removes a follow, or cancels the viewer's pending request in that direction.
///
/// Returns `true` only when a follow row was deleted; cancelling a pending
/// request is a side effect so the button works in the requested state too.
pub async fn unfollow(
    pool: &SqlitePool,
    follower_id: i64,
    followed_id: i64,
) -> anyhow::Result<bool> {
    pool.call(move |conn| {
        let tx = conn.transaction()?;
        let changed = tx.execute(
            "DELETE FROM follows WHERE follower_id = ? AND followed_id = ?",
            params![follower_id, followed_id],
        )?;
        tx.execute(
            "DELETE FROM follow_requests WHERE requester_id = ? AND target_id = ?",
            params![follower_id, followed_id],
        )?;
        tx.commit()?;
        Ok(changed > 0)
    })
    .await
}

pub async fn profile_relationship(
    pool: &SqlitePool,
    viewer_id: Option<i64>,
    profile_id: i64,
) -> anyhow::Result<ProfileRelationship> {
    let Some(viewer_id) = viewer_id else {
        return Ok(ProfileRelationship::default());
    };
    if viewer_id == profile_id {
        return Ok(ProfileRelationship::default());
    }
    pool.call(move |conn| {
        Ok(conn.query_row(
            r#"
            SELECT
              EXISTS(SELECT 1 FROM follows WHERE follower_id = ? AND followed_id = ?),
              EXISTS(SELECT 1 FROM blocks WHERE blocker_id = ? AND blocked_id = ?),
              EXISTS(SELECT 1 FROM mutes WHERE muter_id = ? AND muted_id = ?),
              EXISTS(SELECT 1 FROM blocks WHERE blocker_id = ? AND blocked_id = ?),
              EXISTS(SELECT 1 FROM follow_requests WHERE requester_id = ? AND target_id = ?)
            "#,
            params![
                viewer_id, profile_id, viewer_id, profile_id, viewer_id, profile_id, profile_id,
                viewer_id, viewer_id, profile_id
            ],
            |row| {
                Ok(ProfileRelationship {
                    following: row.get::<_, i64>(0)? != 0,
                    blocked: row.get::<_, i64>(1)? != 0,
                    muted: row.get::<_, i64>(2)? != 0,
                    blocks_viewer: row.get::<_, i64>(3)? != 0,
                    requested: row.get::<_, i64>(4)? != 0,
                })
            },
        )?)
    })
    .await
}

pub async fn is_following(
    pool: &SqlitePool,
    follower_id: i64,
    followed_id: i64,
) -> anyhow::Result<bool> {
    pool.call(move |conn| {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM follows WHERE follower_id = ? AND followed_id = ?",
            params![follower_id, followed_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    })
    .await
}

pub async fn follow_counts(pool: &SqlitePool, user_id: i64) -> anyhow::Result<(i64, i64)> {
    pool.call(move |conn| {
        let followers = conn.query_row(
            "SELECT COUNT(*) FROM follows f JOIN users u ON u.id = f.follower_id WHERE f.followed_id = ? AND u.is_deleted = 0",
            [user_id],
            |row| row.get(0),
        )?;
        let following = conn.query_row(
            "SELECT COUNT(*) FROM follows f JOIN users u ON u.id = f.followed_id WHERE f.follower_id = ? AND u.is_deleted = 0",
            [user_id],
            |row| row.get(0),
        )?;
        Ok((followers, following))
    })
    .await
}

pub async fn instance_counts(pool: &SqlitePool) -> anyhow::Result<(i64, i64)> {
    pool.call(|conn| {
        let users = conn.query_row(
            "SELECT COUNT(*) FROM users WHERE is_deleted = 0",
            [],
            |row| row.get(0),
        )?;
        let posts = conn.query_row(
            "SELECT COUNT(*) FROM posts WHERE is_deleted = 0",
            [],
            |row| row.get(0),
        )?;
        Ok((users, posts))
    })
    .await
}

/// Shared follower/following list query with keyset pagination and viewer
/// visibility filtering. Returns the page and whether more accounts follow.
pub(super) async fn follow_account_list(
    pool: &SqlitePool,
    account_id: i64,
    viewer_id: Option<i64>,
    edge: FollowEdge,
    after: Option<String>,
) -> anyhow::Result<(Vec<AccountView>, bool)> {
    let (edge_column, anchor_column) = match edge {
        FollowEdge::Followers => ("follower_id", "followed_id"),
        FollowEdge::Following => ("followed_id", "follower_id"),
    };
    let mut sql = format!(
        r#"
        SELECT u.id, u.username, u.display_name, u.bio,
          COALESCE(pic.thumbnail_public_path, pic.public_path),
          EXISTS(
            SELECT 1 FROM follows vf
            WHERE vf.follower_id = ? AND vf.followed_id = u.id
          )
        FROM follows f
        JOIN users u ON u.id = f.{edge_column}
        LEFT JOIN media pic ON pic.id = u.profile_picture_media_id
        WHERE f.{anchor_column} = ? AND u.is_deleted = 0 AND u.is_suspended = 0
        "#
    );
    let mut bindings = vec![
        rusqlite::types::Value::Integer(viewer_id.unwrap_or(0)),
        rusqlite::types::Value::Integer(account_id),
    ];
    if let Some(cursor) = &after {
        sql.push_str(" AND u.normalized_username > ?");
        bindings.push(rusqlite::types::Value::Text(cursor.to_ascii_lowercase()));
    }
    if viewer_id.is_some() {
        sql.push_str(
            r#"
            AND u.id NOT IN (SELECT blocked_id FROM blocks WHERE blocker_id = ?)
            AND u.id NOT IN (SELECT muted_id FROM mutes WHERE muter_id = ?)
            AND NOT EXISTS (SELECT 1 FROM blocks WHERE blocker_id = u.id AND blocked_id = ?)
            "#,
        );
        if let Some(viewer_id) = viewer_id {
            bindings.extend(std::iter::repeat_n(
                rusqlite::types::Value::Integer(viewer_id),
                3,
            ));
        }
    }
    sql.push_str(" ORDER BY u.normalized_username ASC LIMIT ?");
    let limit = i64::try_from(ACCOUNT_LIST_PAGE_SIZE + 1)?;
    bindings.push(rusqlite::types::Value::Integer(limit));
    pool.call(move |conn| {
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params_from_iter(bindings), |row| {
                Ok(AccountView {
                    id: row.get(0)?,
                    username: row.get(1)?,
                    display_name: row.get(2)?,
                    bio: row.get(3)?,
                    profile_picture_path: row.get(4)?,
                    viewer_following: row.get::<_, i64>(5)? != 0,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
    .map(|mut accounts| {
        let has_more = accounts.len() > ACCOUNT_LIST_PAGE_SIZE;
        accounts.truncate(ACCOUNT_LIST_PAGE_SIZE);
        (accounts, has_more)
    })
}

pub async fn following_accounts(
    pool: &SqlitePool,
    viewer_id: i64,
    after: Option<String>,
) -> anyhow::Result<(Vec<AccountView>, bool)> {
    follow_account_list(
        pool,
        viewer_id,
        Some(viewer_id),
        FollowEdge::Following,
        after,
    )
    .await
}

pub async fn onboarding_suggestions(
    pool: &SqlitePool,
    viewer_id: i64,
    limit: usize,
) -> anyhow::Result<Vec<AccountView>> {
    let limit = i64::try_from(limit)?;
    pool.call(move |conn| {
        let mut stmt = conn.prepare(
            r#"
            SELECT u.id, u.username, u.display_name, u.bio,
              COALESCE(pic.thumbnail_public_path, pic.public_path),
              EXISTS(
                SELECT 1 FROM follows vf
                WHERE vf.follower_id = ? AND vf.followed_id = u.id
              )
            FROM users u
            LEFT JOIN media pic ON pic.id = u.profile_picture_media_id
            WHERE u.id != ?
              AND u.is_deleted = 0
              AND u.is_suspended = 0
              AND u.id NOT IN (SELECT blocked_id FROM blocks WHERE blocker_id = ?)
              AND u.id NOT IN (SELECT muted_id FROM mutes WHERE muter_id = ?)
              AND NOT EXISTS (SELECT 1 FROM blocks WHERE blocker_id = u.id AND blocked_id = ?)
            ORDER BY lower(u.username), u.id
            LIMIT ?
            "#,
        )?;
        let rows = stmt
            .query_map(
                params![viewer_id, viewer_id, viewer_id, viewer_id, viewer_id, limit],
                |row| {
                    Ok(AccountView {
                        id: row.get(0)?,
                        username: row.get(1)?,
                        display_name: row.get(2)?,
                        bio: row.get(3)?,
                        profile_picture_path: row.get(4)?,
                        viewer_following: row.get::<_, i64>(5)? != 0,
                    })
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}

pub async fn followers_accounts(
    pool: &SqlitePool,
    account_id: i64,
    viewer_id: Option<i64>,
    after: Option<String>,
) -> anyhow::Result<(Vec<AccountView>, bool)> {
    follow_account_list(pool, account_id, viewer_id, FollowEdge::Followers, after).await
}

pub async fn following_accounts_for_profile(
    pool: &SqlitePool,
    account_id: i64,
    viewer_id: Option<i64>,
    after: Option<String>,
) -> anyhow::Result<(Vec<AccountView>, bool)> {
    follow_account_list(pool, account_id, viewer_id, FollowEdge::Following, after).await
}

pub async fn block(pool: &SqlitePool, blocker_id: i64, blocked_id: i64) -> anyhow::Result<()> {
    if blocker_id == blocked_id {
        anyhow::bail!("cannot block yourself");
    }
    pool.call(move |conn| {
        let tx = conn.transaction()?;
        ensure_account_actionable_tx(&tx, blocked_id)?;
        tx.execute(
            "INSERT OR IGNORE INTO blocks (blocker_id, blocked_id) VALUES (?, ?)",
            params![blocker_id, blocked_id],
        )?;
        tx.execute(
            "DELETE FROM follows WHERE (follower_id = ? AND followed_id = ?) OR (follower_id = ? AND followed_id = ?)",
            params![blocker_id, blocked_id, blocked_id, blocker_id],
        )?;
        tx.execute(
            "DELETE FROM follow_requests WHERE (requester_id = ? AND target_id = ?) OR (requester_id = ? AND target_id = ?)",
            params![blocker_id, blocked_id, blocked_id, blocker_id],
        )?;
        tx.commit()?;
        Ok(())
    })
    .await
}

pub async fn unblock(pool: &SqlitePool, blocker_id: i64, blocked_id: i64) -> anyhow::Result<()> {
    pool.call(move |conn| {
        conn.execute(
            "DELETE FROM blocks WHERE blocker_id = ? AND blocked_id = ?",
            params![blocker_id, blocked_id],
        )?;
        Ok(())
    })
    .await
}

pub async fn blocked_users(
    pool: &SqlitePool,
    blocker_id: i64,
) -> anyhow::Result<Vec<(i64, String, String)>> {
    pool.call(move |conn| {
        let mut stmt = conn.prepare(
            r#"
            SELECT u.id, u.username, u.display_name
            FROM blocks b
            JOIN users u ON u.id = b.blocked_id
            WHERE b.blocker_id = ? AND u.is_deleted = 0
            ORDER BY lower(u.username)
            "#,
        )?;
        let rows = stmt
            .query_map([blocker_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}

pub async fn mute(pool: &SqlitePool, muter_id: i64, muted_id: i64) -> anyhow::Result<()> {
    if muter_id == muted_id {
        anyhow::bail!("cannot mute yourself");
    }
    pool.call(move |conn| {
        let tx = conn.transaction()?;
        ensure_account_actionable_tx(&tx, muted_id)?;
        tx.execute(
            "INSERT OR IGNORE INTO mutes (muter_id, muted_id) VALUES (?, ?)",
            params![muter_id, muted_id],
        )?;
        tx.commit()?;
        Ok(())
    })
    .await
}

/// Rejects block/mute targets that do not exist or were deleted, mirroring the
/// follow target check so deleted accounts cannot accumulate invisible rows.
pub(super) fn ensure_account_actionable_tx(
    tx: &rusqlite::Transaction<'_>,
    account_id: i64,
) -> anyhow::Result<()> {
    let available = tx
        .query_row(
            "SELECT is_deleted = 0 FROM users WHERE id = ?",
            [account_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(0)
        != 0;
    if !available {
        anyhow::bail!("account not found");
    }
    Ok(())
}

pub async fn unmute(pool: &SqlitePool, muter_id: i64, muted_id: i64) -> anyhow::Result<()> {
    pool.call(move |conn| {
        conn.execute(
            "DELETE FROM mutes WHERE muter_id = ? AND muted_id = ?",
            params![muter_id, muted_id],
        )?;
        Ok(())
    })
    .await
}

pub async fn muted_users(
    pool: &SqlitePool,
    muter_id: i64,
) -> anyhow::Result<Vec<(i64, String, String)>> {
    pool.call(move |conn| {
        let mut stmt = conn.prepare(
            r#"
            SELECT u.id, u.username, u.display_name
            FROM mutes m
            JOIN users u ON u.id = m.muted_id
            WHERE m.muter_id = ? AND u.is_deleted = 0
            ORDER BY lower(u.username)
            "#,
        )?;
        let rows = stmt
            .query_map([muter_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}

pub async fn add_muted_word(pool: &SqlitePool, user_id: i64, term: &str) -> anyhow::Result<()> {
    let term = clean_muted_word(term)?;
    let normalized = term.to_lowercase();
    pool.call(move |conn| {
        conn.execute(
            "INSERT OR IGNORE INTO muted_words (user_id, term, normalized_term) VALUES (?, ?, ?)",
            params![user_id, term, normalized],
        )?;
        Ok(())
    })
    .await
}

pub async fn remove_muted_word(
    pool: &SqlitePool,
    user_id: i64,
    muted_word_id: i64,
) -> anyhow::Result<()> {
    pool.call(move |conn| {
        conn.execute(
            "DELETE FROM muted_words WHERE user_id = ? AND id = ?",
            params![user_id, muted_word_id],
        )?;
        Ok(())
    })
    .await
}

pub async fn muted_words(pool: &SqlitePool, user_id: i64) -> anyhow::Result<Vec<MutedWord>> {
    pool.call(move |conn| {
        let mut stmt = conn
            .prepare("SELECT id, term FROM muted_words WHERE user_id = ? ORDER BY lower(term)")?;
        let rows = stmt
            .query_map([user_id], |row| {
                Ok(MutedWord {
                    id: row.get(0)?,
                    term: row.get(1)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}
