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

/// `SQLite` extended result code for a `UNIQUE` constraint failure.
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
