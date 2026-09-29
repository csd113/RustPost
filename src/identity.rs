//! Account lifecycle operations: administrator-forced password resets, forced
//! session revocation, and username changes with history.
//!
//! The functions here intentionally keep callers on the existing
//! authentication, auditing, and validation paths. They are used by HTTP
//! handlers in [`crate::server`] and by the account settings pages.

use rusqlite::{OptionalExtension as _, params};

use crate::config::Settings;
use crate::db::SqlitePool;
use crate::validation;

/// SQLite extended result code for a UNIQUE constraint failure.
const SQLITE_CONSTRAINT_UNIQUE: i32 = 2067;

fn is_unique_violation(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(code, _) if code.extended_code == SQLITE_CONSTRAINT_UNIQUE
    )
}

/// Returned when a username change cannot be applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsernameChangeError {
    /// The handle is already used by another account, now or in the past.
    Taken,
    /// The handle fails the existing username rules.
    Invalid,
    /// The account does not exist or is marked deleted.
    NotFound,
    /// The requested handle is already the account's current handle.
    Unchanged,
}

impl std::fmt::Display for UsernameChangeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Taken => formatter.write_str("username is already taken"),
            Self::Invalid => formatter.write_str("username is invalid"),
            Self::NotFound => formatter.write_str("account not found"),
            Self::Unchanged => formatter.write_str("that is already your username"),
        }
    }
}

impl std::error::Error for UsernameChangeError {}

/// A completed username change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsernameChange {
    pub previous_username: String,
    pub username: String,
}

/// One previously held handle for an account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsernameHistoryEntry {
    pub username: String,
    pub changed_at: String,
}

/// One account that previously held a handle that no current account owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoricalUsernameHolder {
    pub user_id: i64,
    pub username: String,
    pub display_name: String,
    pub changed_at: String,
}

/// Marks an account as requiring a password change before it can use anything
/// other than the password-change and logout flows, and writes an audit row.
pub async fn require_password_reset(
    pool: &SqlitePool,
    admin_user_id: i64,
    user_id: i64,
) -> anyhow::Result<()> {
    let updated = pool
        .call(move |conn| {
            Ok(conn.execute(
                "UPDATE users SET must_change_password = 1, updated_at = CURRENT_TIMESTAMP WHERE id = ? AND is_deleted = 0",
                [user_id],
            )?)
        })
        .await?;
    if updated == 0 {
        anyhow::bail!("account not found");
    }
    crate::admin::audit(
        pool,
        admin_user_id,
        "require_password_reset",
        &format!("user:{user_id}"),
    )
    .await
}

/// Revokes every active session belonging to one account and writes an audit
/// row. Returns the number of sessions revoked.
pub async fn revoke_user_sessions(
    pool: &SqlitePool,
    admin_user_id: i64,
    user_id: i64,
) -> anyhow::Result<usize> {
    // The account check and the revocation share one transaction so a
    // concurrent deletion cannot slip between them and turn the documented
    // "account not found" rejection into a silent no-op success.
    let revoked = pool
        .call(move |conn| {
            let tx = conn.transaction()?;
            let exists = tx
                .query_row(
                    "SELECT 1 FROM users WHERE id = ? AND is_deleted = 0",
                    [user_id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if !exists {
                return Ok(None);
            }
            let revoked = tx.execute(
                "UPDATE sessions SET revoked_at = CURRENT_TIMESTAMP WHERE user_id = ? AND revoked_at IS NULL",
                [user_id],
            )?;
            tx.commit()?;
            Ok(Some(revoked))
        })
        .await?;
    let Some(revoked) = revoked else {
        anyhow::bail!("account not found");
    };
    crate::admin::audit(
        pool,
        admin_user_id,
        "revoke_user_sessions",
        &format!("user:{user_id}"),
    )
    .await?;
    Ok(revoked)
}

/// Changes an account's handle atomically.
///
/// The previous handle is recorded in `username_history` in the same
/// transaction. Reclaiming a handle the same account previously held is
/// allowed and removes the stale history row that matches the new handle.
pub async fn change_username(
    pool: &SqlitePool,
    settings: &Settings,
    user_id: i64,
    new_username: &str,
) -> Result<UsernameChange, UsernameChangeError> {
    let normalized =
        validation::normalize_username(new_username, settings.accounts.max_username_len)
            .map_err(|_invalid| UsernameChangeError::Invalid)?;
    let display_username = new_username.trim().to_owned();
    let result = pool
        .call(move |conn| {
            let tx = conn.transaction()?;
            let current = tx
                .query_row(
                    "SELECT username, normalized_username, is_deleted FROM users WHERE id = ?",
                    [user_id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)?,
                        ))
                    },
                )
                .optional()?;
            let Some((previous_username, current_normalized, is_deleted)) = current else {
                return Ok(Err(UsernameChangeError::NotFound));
            };
            if is_deleted != 0 {
                return Ok(Err(UsernameChangeError::NotFound));
            }
            if current_normalized == normalized {
                return Ok(Err(UsernameChangeError::Unchanged));
            }
            let current_owner = tx
                .query_row(
                    "SELECT 1 FROM users WHERE normalized_username = ? AND id != ?",
                    params![normalized, user_id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if current_owner {
                return Ok(Err(UsernameChangeError::Taken));
            }
            let history_owner = tx
                .query_row(
                    "SELECT user_id FROM username_history WHERE normalized_username = ? AND user_id != ? LIMIT 1",
                    params![normalized, user_id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?;
            if history_owner.is_some() {
                return Ok(Err(UsernameChangeError::Taken));
            }
            tx.execute(
                "DELETE FROM username_history WHERE user_id = ? AND normalized_username = ?",
                params![user_id, normalized],
            )?;
            tx.execute(
                "INSERT INTO username_history (user_id, username, normalized_username, changed_at) VALUES (?, ?, ?, CURRENT_TIMESTAMP)",
                params![user_id, previous_username, current_normalized],
            )?;
            match tx.execute(
                "UPDATE users SET username = ?, normalized_username = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ? AND is_deleted = 0",
                params![display_username, normalized, user_id],
            ) {
                Ok(_) => {}
                Err(error) if is_unique_violation(&error) => {
                    return Ok(Err(UsernameChangeError::Taken));
                }
                Err(error) => return Err(error.into()),
            }
            match tx.commit() {
                Ok(()) => {}
                Err(error) if is_unique_violation(&error) => {
                    return Ok(Err(UsernameChangeError::Taken));
                }
                Err(error) => return Err(error.into()),
            }
            Ok(Ok(UsernameChange {
                previous_username,
                username: display_username,
            }))
        })
        .await;
    match result {
        Ok(inner) => inner,
        Err(error) => {
            tracing::error!(user_id, error = %error, "username change failed");
            Err(UsernameChangeError::Invalid)
        }
    }
}

/// Historically held handles for one account, newest first.
pub async fn username_history(
    pool: &SqlitePool,
    user_id: i64,
) -> anyhow::Result<Vec<UsernameHistoryEntry>> {
    pool.call(move |conn| {
        let mut stmt = conn.prepare(
            "SELECT username, changed_at FROM username_history WHERE user_id = ? ORDER BY changed_at DESC, id DESC",
        )?;
        let rows = stmt
            .query_map([user_id], |row| {
                Ok(UsernameHistoryEntry {
                    username: row.get(0)?,
                    changed_at: row.get(1)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}

/// Resolves a normalized handle that no current account owns to the accounts
/// that previously held it, newest first.
pub async fn historical_username_holders(
    pool: &SqlitePool,
    normalized_username: &str,
) -> anyhow::Result<Vec<HistoricalUsernameHolder>> {
    let normalized_username = normalized_username.to_owned();
    pool.call(move |conn| {
        let mut stmt = conn.prepare(
            r#"
            SELECT u.id, u.username, u.display_name, h.changed_at
            FROM username_history h
            JOIN users u ON u.id = h.user_id
            WHERE h.normalized_username = ? AND u.is_deleted = 0
            ORDER BY h.changed_at DESC
            "#,
        )?;
        let rows = stmt
            .query_map([normalized_username], |row| {
                Ok(HistoricalUsernameHolder {
                    user_id: row.get(0)?,
                    username: row.get(1)?,
                    display_name: row.get(2)?,
                    changed_at: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}

/// Normalizes a candidate handle with the shared username rules.
///
/// Exposed so callers can validate without duplicating the rules.
pub fn normalize_candidate_username(
    settings: &Settings,
    candidate: &str,
) -> Result<String, UsernameChangeError> {
    validation::normalize_username(candidate, settings.accounts.max_username_len)
        .map_err(|_invalid| UsernameChangeError::Invalid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth;

    async fn fixture() -> (tempfile::TempDir, SqlitePool, Settings, i64, i64) {
        let temp = tempfile::tempdir().expect("temp dir");
        let pool = crate::db::connect(&temp.path().join("test.sqlite3"))
            .await
            .expect("connect");
        crate::db::migrate(&pool).await.expect("migrate");
        let settings = Settings::default();
        let admin =
            auth::register_user(&pool, &settings, "admin_user", "very secure password", true)
                .await
                .expect("admin");
        let alice = auth::register_user(&pool, &settings, "alice", "very secure password", false)
            .await
            .expect("alice");
        (temp, pool, settings, admin, alice)
    }

    async fn must_change_password(pool: &SqlitePool, user_id: i64) -> bool {
        pool.call(move |conn| {
            Ok(conn.query_row(
                "SELECT must_change_password FROM users WHERE id = ?",
                [user_id],
                |row| row.get::<_, i64>(0),
            )? != 0)
        })
        .await
        .expect("must_change_password")
    }

    async fn audit_count(pool: &SqlitePool, action: &str, target: &str) -> i64 {
        let action = action.to_owned();
        let target = target.to_owned();
        pool.call(move |conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM admin_audit_log WHERE action = ? AND target = ?",
                params![action, target],
                |row| row.get(0),
            )?)
        })
        .await
        .expect("audit count")
    }

    async fn username(pool: &SqlitePool, user_id: i64) -> Option<(String, String)> {
        pool.call(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT username, normalized_username FROM users WHERE id = ?",
                    [user_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()?)
        })
        .await
        .expect("username lookup")
    }

    fn session_headers(token: &str) -> axum::http::HeaderMap {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::COOKIE,
            format!("rustpost_session={token}")
                .parse()
                .expect("session cookie"),
        );
        headers
    }

    /// Current plus historical rows for one canonical handle.
    async fn handle_rows(pool: &SqlitePool, normalized: &str) -> i64 {
        let normalized = normalized.to_owned();
        pool.call(move |conn| {
            Ok(conn.query_row(
                "SELECT (SELECT COUNT(*) FROM users WHERE normalized_username = ?1) + (SELECT COUNT(*) FROM username_history WHERE normalized_username = ?1)",
                [&normalized],
                |row| row.get(0),
            )?)
        })
        .await
        .expect("handle rows")
    }

    /// Any canonical handle owned by a current account and also reserved in history.
    async fn cross_table_conflicts(pool: &SqlitePool) -> i64 {
        pool.call(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM users u WHERE EXISTS (SELECT 1 FROM username_history h WHERE h.normalized_username = u.normalized_username)",
                (),
                |row| row.get(0),
            )?)
        })
        .await
        .expect("cross table conflicts")
    }

    async fn duplicate_history_rows(pool: &SqlitePool) -> i64 {
        pool.call(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM (SELECT normalized_username FROM username_history GROUP BY normalized_username HAVING COUNT(*) > 1)",
                (),
                |row| row.get(0),
            )?)
        })
        .await
        .expect("duplicate history rows")
    }

    #[tokio::test]
    async fn require_password_reset_sets_flag_and_audits() {
        let (_temp, pool, _settings, admin, alice) = fixture().await;

        require_password_reset(&pool, admin, alice)
            .await
            .expect("require reset");

        assert!(must_change_password(&pool, alice).await);
        assert_eq!(
            audit_count(&pool, "require_password_reset", &format!("user:{alice}")).await,
            1
        );
        assert_eq!(
            audit_count(&pool, "require_password_reset", "user:9999").await,
            0
        );

        // Repeating the admin action is idempotent for the flag but records
        // one audit row per action.
        require_password_reset(&pool, admin, alice)
            .await
            .expect("repeat reset");
        assert!(must_change_password(&pool, alice).await);
        assert_eq!(
            audit_count(&pool, "require_password_reset", &format!("user:{alice}")).await,
            2
        );
    }

    #[tokio::test]
    async fn require_password_reset_restricts_existing_and_new_sessions() {
        let (_temp, pool, settings, admin, alice) = fixture().await;
        let before = auth::create_session(&pool, alice).await.expect("session");

        require_password_reset(&pool, admin, alice)
            .await
            .expect("require reset");

        let before_user = auth::current_user(&pool, &session_headers(&before.token))
            .await
            .expect("current user")
            .expect("session created before the flag stays valid");
        assert!(before_user.must_change_password);

        let after = auth::create_session(&pool, alice).await.expect("session");
        let after_user = auth::current_user(&pool, &session_headers(&after.token))
            .await
            .expect("current user")
            .expect("session created after the flag is restricted too");
        assert!(after_user.must_change_password);

        // The forced-reset flow clears the restriction and revokes the other
        // session in one change.
        auth::change_password(
            &pool,
            &settings,
            alice,
            "very secure password",
            "much better password",
            "much better password",
            Some(&after.token),
        )
        .await
        .expect("change password");

        let after_user = auth::current_user(&pool, &session_headers(&after.token))
            .await
            .expect("current user")
            .expect("session used for the change");
        assert!(!after_user.must_change_password);
        assert!(
            auth::current_user(&pool, &session_headers(&before.token))
                .await
                .expect("current user")
                .is_none(),
            "the earlier session must be revoked by the forced reset"
        );
    }

    #[tokio::test]
    async fn require_password_reset_rejects_missing_and_deleted_accounts() {
        let (_temp, pool, _settings, admin, alice) = fixture().await;

        let missing = require_password_reset(&pool, admin, 9999).await;
        assert_eq!(
            missing.expect_err("missing account").to_string(),
            "account not found"
        );

        pool.call(move |conn| {
            conn.execute("UPDATE users SET is_deleted = 1 WHERE id = ?", [alice])?;
            Ok(())
        })
        .await
        .expect("mark deleted");
        let deleted = require_password_reset(&pool, admin, alice).await;
        assert_eq!(
            deleted.expect_err("deleted account").to_string(),
            "account not found"
        );
        assert!(!must_change_password(&pool, alice).await);
        assert_eq!(
            audit_count(&pool, "require_password_reset", &format!("user:{alice}")).await,
            0
        );
    }

    #[tokio::test]
    async fn revoke_user_sessions_revokes_and_is_idempotent() {
        let (_temp, pool, settings, admin, alice) = fixture().await;
        let bob = auth::register_user(&pool, &settings, "bob", "very secure password", false)
            .await
            .expect("bob");
        let _alice_one = auth::create_session(&pool, alice).await.expect("alice one");
        let _alice_two = auth::create_session(&pool, alice).await.expect("alice two");
        let _bob_one = auth::create_session(&pool, bob).await.expect("bob one");

        assert_eq!(
            revoke_user_sessions(&pool, admin, alice)
                .await
                .expect("revoke"),
            2
        );
        assert_eq!(
            revoke_user_sessions(&pool, admin, alice)
                .await
                .expect("revoke again"),
            0
        );

        let (alice_active, bob_active): (i64, i64) = pool
            .call(move |conn| {
                Ok((
                    conn.query_row(
                        "SELECT COUNT(*) FROM sessions WHERE user_id = ? AND revoked_at IS NULL",
                        [alice],
                        |row| row.get(0),
                    )?,
                    conn.query_row(
                        "SELECT COUNT(*) FROM sessions WHERE user_id = ? AND revoked_at IS NULL",
                        [bob],
                        |row| row.get(0),
                    )?,
                ))
            })
            .await
            .expect("active sessions");
        assert_eq!(alice_active, 0);
        assert_eq!(bob_active, 1);
        assert_eq!(
            audit_count(&pool, "revoke_user_sessions", &format!("user:{alice}")).await,
            2
        );
    }

    #[tokio::test]
    async fn revoke_user_sessions_rejects_missing_and_deleted_accounts() {
        let (_temp, pool, _settings, admin, alice) = fixture().await;

        let error = revoke_user_sessions(&pool, admin, 9999)
            .await
            .expect_err("missing account");

        assert_eq!(error.to_string(), "account not found");

        pool.call(move |conn| {
            conn.execute("UPDATE users SET is_deleted = 1 WHERE id = ?", [alice])?;
            Ok(())
        })
        .await
        .expect("mark deleted");
        let error = revoke_user_sessions(&pool, admin, alice)
            .await
            .expect_err("deleted account");

        assert_eq!(error.to_string(), "account not found");
        assert_eq!(
            audit_count(&pool, "revoke_user_sessions", &format!("user:{alice}")).await,
            0
        );
    }

    #[tokio::test]
    async fn change_username_records_history_and_normalizes() {
        let (_temp, pool, settings, _admin, alice) = fixture().await;

        let change = change_username(&pool, &settings, alice, " NewName ")
            .await
            .expect("change username");

        assert_eq!(
            change,
            UsernameChange {
                previous_username: "alice".to_owned(),
                username: "NewName".to_owned(),
            }
        );
        let (display, normalized) = username(&pool, alice).await.expect("account");
        assert_eq!(display, "NewName");
        assert_eq!(normalized, "newname");
        let history = username_history(&pool, alice).await.expect("history");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].username, "alice");
        assert!(!history[0].changed_at.is_empty());
    }

    #[tokio::test]
    async fn change_username_rejects_currently_taken_handle() {
        let (_temp, pool, settings, _admin, alice) = fixture().await;
        let bob = auth::register_user(&pool, &settings, "bob", "very secure password", false)
            .await
            .expect("bob");

        let error = change_username(&pool, &settings, alice, "Bob")
            .await
            .expect_err("taken");

        assert_eq!(error, UsernameChangeError::Taken);
        assert_eq!(username(&pool, alice).await.expect("alice").0, "alice");
        assert_eq!(username(&pool, bob).await.expect("bob").0, "bob");
    }

    #[tokio::test]
    async fn change_username_rejects_another_accounts_history() {
        let (_temp, pool, settings, _admin, alice) = fixture().await;
        let bob = auth::register_user(&pool, &settings, "bob", "very secure password", false)
            .await
            .expect("bob");
        change_username(&pool, &settings, alice, "alice_new")
            .await
            .expect("rename alice");

        let error = change_username(&pool, &settings, bob, "alice")
            .await
            .expect_err("historical handle");

        assert_eq!(error, UsernameChangeError::Taken);
    }

    #[tokio::test]
    async fn change_username_allows_same_owner_reclamation() {
        let (_temp, pool, settings, _admin, alice) = fixture().await;
        let bob = auth::register_user(&pool, &settings, "bob", "very secure password", false)
            .await
            .expect("bob");
        change_username(&pool, &settings, alice, "alice_new")
            .await
            .expect("rename away");

        let change = change_username(&pool, &settings, alice, "alice")
            .await
            .expect("reclaim");

        assert_eq!(change.previous_username, "alice_new");
        assert_eq!(change.username, "alice");
        let history = username_history(&pool, alice).await.expect("history");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].username, "alice_new");
        assert_eq!(
            historical_username_holders(&pool, "alice")
                .await
                .expect("holders")
                .len(),
            0
        );
        // Reclaiming replaces the stale row for the reclaimed handle, so it
        // stays reserved for the owner but never also appears in history.
        assert_eq!(handle_rows(&pool, "alice").await, 1);
        assert_eq!(handle_rows(&pool, "alice_new").await, 1);
        assert_eq!(
            change_username(&pool, &settings, bob, "alIce")
                .await
                .expect_err("reserved"),
            UsernameChangeError::Taken
        );
    }

    #[tokio::test]
    async fn change_username_rejects_invalid_handles() {
        let (_temp, pool, settings, _admin, alice) = fixture().await;
        let too_long = "a".repeat(settings.accounts.max_username_len + 1);

        for candidate in ["settings", "bad name", "bad!chars", "", "   ", &too_long] {
            let error = change_username(&pool, &settings, alice, candidate)
                .await
                .expect_err("invalid handle");
            assert_eq!(
                error,
                UsernameChangeError::Invalid,
                "candidate {candidate:?} should be invalid"
            );
        }
        assert_eq!(username(&pool, alice).await.expect("account").0, "alice");
    }

    #[tokio::test]
    async fn change_username_accumulates_history_newest_first() {
        let (_temp, pool, settings, _admin, alice) = fixture().await;
        change_username(&pool, &settings, alice, "handle_one")
            .await
            .expect("first change");
        change_username(&pool, &settings, alice, "handle_two")
            .await
            .expect("second change");
        change_username(&pool, &settings, alice, "handle_three")
            .await
            .expect("third change");

        let history = username_history(&pool, alice).await.expect("history");
        let names = history
            .iter()
            .map(|entry| entry.username.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["handle_two", "handle_one", "alice"]);
    }

    #[tokio::test]
    async fn change_username_unchanged_keeps_display_form() {
        let (_temp, pool, settings, _admin, alice) = fixture().await;

        let error = change_username(&pool, &settings, alice, "ALICE")
            .await
            .expect_err("unchanged");

        assert_eq!(error, UsernameChangeError::Unchanged);
        assert_eq!(username(&pool, alice).await.expect("account").0, "alice");
        assert_eq!(
            username_history(&pool, alice).await.expect("history").len(),
            0
        );
    }

    #[tokio::test]
    async fn change_username_rejects_missing_and_deleted_accounts() {
        let (_temp, pool, settings, _admin, alice) = fixture().await;

        let missing = change_username(&pool, &settings, 9999, "alice_new")
            .await
            .expect_err("missing");
        assert_eq!(missing, UsernameChangeError::NotFound);

        pool.call(move |conn| {
            conn.execute("UPDATE users SET is_deleted = 1 WHERE id = ?", [alice])?;
            Ok(())
        })
        .await
        .expect("mark deleted");
        let deleted = change_username(&pool, &settings, alice, "alice_new")
            .await
            .expect_err("deleted");
        assert_eq!(deleted, UsernameChangeError::NotFound);
    }

    #[tokio::test]
    async fn change_username_concurrent_claims_leave_exactly_one_winner() {
        let (_temp, pool, settings, _admin, alice) = fixture().await;
        let bob = auth::register_user(&pool, &settings, "bob", "very secure password", false)
            .await
            .expect("bob");

        let alice_result = change_username(&pool, &settings, alice, "shared_handle");
        let bob_result = change_username(&pool, &settings, bob, "shared_handle");
        let (alice_result, bob_result) = tokio::join!(alice_result, bob_result);

        let winners = usize::from(alice_result.is_ok()) + usize::from(bob_result.is_ok());
        assert_eq!(winners, 1, "exactly one concurrent claim should win");
        let loser = alice_result
            .err()
            .or_else(|| bob_result.err())
            .expect("one concurrent claim should fail");
        assert_eq!(loser, UsernameChangeError::Taken);
    }

    #[tokio::test]
    async fn historical_holders_exclude_deleted_accounts() {
        let (_temp, pool, settings, _admin, alice) = fixture().await;
        change_username(&pool, &settings, alice, "alice_new")
            .await
            .expect("rename");

        let holders = historical_username_holders(&pool, "alice")
            .await
            .expect("holders");
        assert_eq!(holders.len(), 1);
        assert_eq!(holders[0].user_id, alice);
        assert_eq!(holders[0].username, "alice_new");
        assert_eq!(holders[0].display_name, "alice");
        assert!(!holders[0].changed_at.is_empty());
        assert!(
            historical_username_holders(&pool, "never_used")
                .await
                .expect("empty")
                .is_empty()
        );

        pool.call(move |conn| {
            conn.execute("UPDATE users SET is_deleted = 1 WHERE id = ?", [alice])?;
            Ok(())
        })
        .await
        .expect("mark deleted");
        assert!(
            historical_username_holders(&pool, "alice")
                .await
                .expect("deleted")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn username_history_reserves_case_variants_of_old_handle() {
        let (_temp, pool, settings, _admin, alice) = fixture().await;
        let bob = auth::register_user(&pool, &settings, "bob", "very secure password", false)
            .await
            .expect("bob");
        change_username(&pool, &settings, alice, "MixedCase")
            .await
            .expect("rename alice");

        assert_eq!(username(&pool, alice).await.expect("alice").0, "MixedCase");
        for variant in [
            "mixedcase",
            "MIXEDCASE",
            "MixedCase",
            "MiXeDcAsE",
            "alice",
            "ALICE",
            "aLiCe",
        ] {
            assert_eq!(
                change_username(&pool, &settings, bob, variant)
                    .await
                    .expect_err("historical case variant"),
                UsernameChangeError::Taken,
                "variant {variant:?} must stay reserved"
            );
            let registration =
                auth::register_user(&pool, &settings, variant, "very secure password", false)
                    .await
                    .expect_err("historical case variant registration");
            assert_eq!(registration.to_string(), auth::USERNAME_TAKEN_MESSAGE);
        } // Same owner and same canonical handle: Unchanged, display case kept,
        // and no extra history row appears.
        assert_eq!(
            change_username(&pool, &settings, alice, "MIXEDCASE")
                .await
                .expect_err("unchanged"),
            UsernameChangeError::Unchanged
        );
        assert_eq!(username(&pool, alice).await.expect("alice").0, "MixedCase");
        let history = username_history(&pool, alice).await.expect("history");
        let names = history
            .iter()
            .map(|entry| entry.username.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["alice"]);
    }

    #[tokio::test]
    async fn repeated_reclaims_keep_exact_history_and_reserve_old_handles() {
        let (_temp, pool, settings, _admin, alice) = fixture().await;
        let bob = auth::register_user(&pool, &settings, "bob", "very secure password", false)
            .await
            .expect("bob");

        change_username(&pool, &settings, alice, "handle_b")
            .await
            .expect("first rename");
        assert_eq!(handle_rows(&pool, "alice").await, 1);
        assert_eq!(handle_rows(&pool, "handle_b").await, 1);

        change_username(&pool, &settings, alice, "alice")
            .await
            .expect("reclaim alice");
        assert_eq!(handle_rows(&pool, "alice").await, 1);
        assert_eq!(handle_rows(&pool, "handle_b").await, 1);
        assert_eq!(
            username_history(&pool, alice).await.expect("history").len(),
            1,
            "reclaiming a handle must replace its stale history row"
        );

        change_username(&pool, &settings, alice, "handle_c")
            .await
            .expect("second rename");
        assert_eq!(handle_rows(&pool, "alice").await, 1);
        assert_eq!(handle_rows(&pool, "handle_b").await, 1);
        assert_eq!(handle_rows(&pool, "handle_c").await, 1);

        let history = username_history(&pool, alice).await.expect("history");
        let names = history
            .iter()
            .map(|entry| entry.username.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            ["alice", "handle_b"],
            "history must be newest-first with no stale rows"
        );
        assert_eq!(duplicate_history_rows(&pool).await, 0);
        assert_eq!(cross_table_conflicts(&pool).await, 0);

        for reserved in ["alice", "ALICE", "handle_b", "HANDLE_B"] {
            assert_eq!(
                change_username(&pool, &settings, bob, reserved)
                    .await
                    .expect_err("reserved handle"),
                UsernameChangeError::Taken,
                "{reserved} must stay reserved"
            );
        }
        // Both vacated handles still resolve to their former owner.
        for former in ["alice", "handle_b"] {
            let holders = historical_username_holders(&pool, former)
                .await
                .expect("holders");
            assert_eq!(holders.len(), 1);
            assert_eq!(holders[0].user_id, alice);
            assert_eq!(holders[0].username, "handle_c");
        }
    }

    #[tokio::test]
    async fn concurrent_case_variant_claims_leave_exactly_one_winner() {
        let (_temp, pool, settings, _admin, alice) = fixture().await;
        let bob = auth::register_user(&pool, &settings, "bob", "very secure password", false)
            .await
            .expect("bob");

        let alice_result = change_username(&pool, &settings, alice, "Shared");
        let bob_result = change_username(&pool, &settings, bob, "shared");
        let (alice_result, bob_result) = tokio::join!(alice_result, bob_result);

        let winners = usize::from(alice_result.is_ok()) + usize::from(bob_result.is_ok());
        assert_eq!(winners, 1, "exactly one case variant may win");
        let loser = alice_result
            .err()
            .or_else(|| bob_result.err())
            .expect("one claim must fail");
        assert_eq!(loser, UsernameChangeError::Taken);
        assert_eq!(handle_rows(&pool, "shared").await, 1);
        assert_eq!(cross_table_conflicts(&pool).await, 0);
    }

    #[tokio::test]
    async fn registration_racing_rename_claims_handle_exactly_once() {
        let (_temp, pool, settings, _admin, alice) = fixture().await;

        let registration = auth::register_user(
            &pool,
            &settings,
            "FreshHandle",
            "very secure password",
            false,
        );
        let rename = change_username(&pool, &settings, alice, "freshhandle");
        let (registration, rename) = tokio::join!(registration, rename);

        let registration_won = registration.is_ok();
        let rename_won = rename.is_ok();
        assert!(
            registration_won ^ rename_won,
            "exactly one racer must win: registration={registration:?} rename={rename:?}"
        );
        if let Err(error) = &registration {
            assert_eq!(error.to_string(), auth::USERNAME_TAKEN_MESSAGE);
        }
        if let Err(error) = &rename {
            assert_eq!(*error, UsernameChangeError::Taken);
        }
        assert_eq!(handle_rows(&pool, "freshhandle").await, 1);
        assert_eq!(cross_table_conflicts(&pool).await, 0);

        if registration_won {
            let winner = registration.expect("registration winner");
            let (display, normalized) = username(&pool, winner).await.expect("winner");
            assert_eq!(display, "FreshHandle");
            assert_eq!(normalized, "freshhandle");
            assert_eq!(username(&pool, alice).await.expect("alice").1, "alice");
        } else {
            let (display, normalized) = username(&pool, alice).await.expect("alice");
            assert_eq!(display, "freshhandle");
            assert_eq!(normalized, "freshhandle");
            let history = username_history(&pool, alice).await.expect("history");
            assert_eq!(history.len(), 1);
            assert_eq!(history[0].username, "alice");
        }
    }
}
