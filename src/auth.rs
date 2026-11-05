use anyhow::Context as _;
use argon2::password_hash::{PasswordHasher as _, PasswordVerifier as _, phc::PasswordHash};
use argon2::{Algorithm, Argon2, Params, Version};
use axum::http::HeaderMap;
use chrono::{Duration, Utc};
use rusqlite::{OptionalExtension as _, params};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use crate::config::Settings;
use crate::db::SqlitePool;
use crate::validation::{normalize_username, validate_password};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    Light,
    Dark,
}

impl Theme {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }
}

impl From<&str> for Theme {
    fn from(value: &str) -> Self {
        match value {
            "dark" => Self::Dark,
            _ => Self::Light,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CurrentUser {
    pub id: i64,
    pub username: String,
    pub display_name: String,
    pub is_admin: bool,
    pub is_suspended: bool,
    pub theme: Theme,
    pub nsfw_blur_enabled: bool,
    /// Set by an administrator; the account is limited to the password-change
    /// and logout flows until the password is changed.
    pub must_change_password: bool,
    /// Deadline for permanent account deletion, when a deletion is pending.
    pub deletion_scheduled_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Session {
    pub token: String,
    pub csrf_token: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginFailure {
    NoAccount,
    InvalidPassword,
    UnavailableAccount,
}

pub const USERNAME_TAKEN_MESSAGE: &str = "username is already taken";

/// `SQLite` extended result code for a `UNIQUE` constraint failure.
const SQLITE_CONSTRAINT_UNIQUE: i32 = 2067;

fn is_unique_violation(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(code, _) if code.extended_code == SQLITE_CONSTRAINT_UNIQUE
    )
}

pub async fn register_user(
    pool: &SqlitePool,
    settings: &Settings,
    username: &str,
    password: &str,
    is_admin: bool,
) -> anyhow::Result<i64> {
    let normalized = normalize_username(username, settings.accounts.max_username_len)?;
    validate_password(password, settings)?;
    let hash = hash_password_async(password.to_owned()).await?;
    let username = username.trim().to_owned();
    pool.call(move |conn| {
        let tx = conn.transaction()?;
        let current_taken = tx
            .query_row(
                "SELECT 1 FROM users WHERE normalized_username = ?",
                [&normalized],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        let historical_taken = tx
            .query_row(
                "SELECT 1 FROM username_history WHERE normalized_username = ?",
                [&normalized],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if current_taken || historical_taken {
            anyhow::bail!(USERNAME_TAKEN_MESSAGE);
        }
        let inserted = tx.execute(
            "INSERT INTO users (username, normalized_username, password_hash, display_name, is_admin) VALUES (?, ?, ?, ?, ?)",
            params![username, normalized, hash, username, i64::from(is_admin)],
        );
        let user_id = match inserted {
            Ok(_) => tx.last_insert_rowid(),
            Err(error) if is_unique_violation(&error) => {
                anyhow::bail!(USERNAME_TAKEN_MESSAGE);
            }
            Err(error) => return Err(error.into()),
        };
        tx.commit()?;
        Ok(user_id)
    })
    .await
}

pub async fn login(
    pool: &SqlitePool,
    username: &str,
    password: &str,
) -> anyhow::Result<Result<Session, LoginFailure>> {
    let normalized = username.trim().to_ascii_lowercase();
    let row = pool
        .call(move |conn| {
            conn.query_row(
                "SELECT id, password_hash, is_suspended, is_deleted FROM users WHERE normalized_username = ?",
                [normalized],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(Into::into)
        })
        .await?;
    let Some(row) = row else {
        return Ok(Err(LoginFailure::NoAccount));
    };
    if row.2 != 0 || row.3 != 0 {
        return Ok(Err(LoginFailure::UnavailableAccount));
    }
    if !verify_password_async(password.to_owned(), row.1.clone()).await? {
        return Ok(Err(LoginFailure::InvalidPassword));
    }
    create_session(pool, row.0).await.map(Ok)
}

pub async fn create_session(pool: &SqlitePool, user_id: i64) -> anyhow::Result<Session> {
    let token = secure_token();
    let csrf_token = session_csrf_token(&token);
    let expires_at = Utc::now() + Duration::days(30);
    let token_hash = hash_token(&token);
    let csrf_hash = hash_token(&csrf_token);
    let expires_at = expires_at.to_rfc3339();
    pool.call(move |conn| {
        conn.execute(
            "INSERT INTO sessions (user_id, token_hash, csrf_token_hash, expires_at) VALUES (?, ?, ?, ?)",
            params![user_id, token_hash, csrf_hash, expires_at],
        )?;
        Ok(())
    })
    .await?;
    Ok(Session { token, csrf_token })
}

/// Deletes sessions that expired or were revoked more than `grace_days` ago.
pub async fn prune_stale_sessions(pool: &SqlitePool, grace_days: i64) -> anyhow::Result<usize> {
    let modifier = format!("-{grace_days} days");
    pool.call(move |conn| {
        let changed = conn.execute(
            r#"
            DELETE FROM sessions
            WHERE (revoked_at IS NOT NULL AND datetime(revoked_at) < datetime('now', ?1))
               OR datetime(expires_at) < datetime('now', ?1)
            "#,
            [modifier],
        )?;
        Ok(changed)
    })
    .await
}

pub async fn revoke_session(pool: &SqlitePool, token: &str) -> anyhow::Result<()> {
    let token_hash = hash_token(token);
    pool.call(move |conn| {
        conn.execute(
            "UPDATE sessions SET revoked_at = CURRENT_TIMESTAMP WHERE token_hash = ?",
            [token_hash],
        )?;
        Ok(())
    })
    .await
}

pub async fn verify_user_password(
    pool: &SqlitePool,
    user_id: i64,
    password: &str,
) -> anyhow::Result<bool> {
    let hash: Option<String> = pool
        .call(move |conn| {
            conn.query_row(
                "SELECT password_hash FROM users WHERE id = ? AND is_deleted = 0",
                [user_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
        })
        .await?;
    let Some(hash) = hash else {
        return Ok(false);
    };
    verify_password_async(password.to_owned(), hash).await
}

/// Argon2id hashing is intentionally CPU and memory heavy; run it on the
/// blocking pool so it cannot stall the async runtime under load.
#[derive(Debug, thiserror::Error)]
#[error("password processing is busy; try again shortly")]
pub(crate) struct PasswordWorkBusy;

static PASSWORD_WORK: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

pub async fn hash_password_async(password: String) -> anyhow::Result<String> {
    let permit = tokio::time::timeout(std::time::Duration::from_secs(30), PASSWORD_WORK.acquire())
        .await
        .map_err(|_| PasswordWorkBusy)??;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        hash_password(&password)
    })
    .await
    .context("password hashing task failed")?
}

async fn verify_password_async(password: String, encoded_hash: String) -> anyhow::Result<bool> {
    let permit = tokio::time::timeout(std::time::Duration::from_secs(30), PASSWORD_WORK.acquire())
        .await
        .map_err(|_| PasswordWorkBusy)??;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        verify_password(&password, &encoded_hash)
    })
    .await
    .context("password verification task failed")?
}

pub async fn change_password(
    pool: &SqlitePool,
    settings: &Settings,
    user_id: i64,
    current_password: &str,
    new_password: &str,
    confirm_new_password: &str,
    current_session_token: Option<&str>,
) -> anyhow::Result<()> {
    if new_password != confirm_new_password {
        anyhow::bail!("new passwords do not match");
    }
    validate_password(new_password, settings)?;
    if !verify_user_password(pool, user_id, current_password).await? {
        anyhow::bail!("current password is incorrect");
    }
    let hash = hash_password_async(new_password.to_owned()).await?;
    let current_session_hash = current_session_token.map(hash_token);
    pool.call(move |conn| {
        let tx = conn.transaction()?;
        let updated = tx.execute(
            "UPDATE users SET password_hash = ?, must_change_password = 0, updated_at = CURRENT_TIMESTAMP WHERE id = ? AND is_deleted = 0",
            params![hash, user_id],
        )?;
        if updated == 0 {
            // The account was deleted after the password was verified. Rolling
            // back keeps the flag and every session untouched instead of
            // reporting a successful change that never happened.
            anyhow::bail!("account not found");
        }
        if let Some(current_session_hash) = current_session_hash {
            tx.execute(
                "UPDATE sessions SET revoked_at = CURRENT_TIMESTAMP WHERE user_id = ? AND token_hash != ? AND revoked_at IS NULL",
                params![user_id, current_session_hash],
            )?;
        } else {
            tx.execute(
                "UPDATE sessions SET revoked_at = CURRENT_TIMESTAMP WHERE user_id = ? AND revoked_at IS NULL",
                [user_id],
            )?;
        }
        tx.commit()?;
        Ok(())
    })
    .await
}

pub async fn current_user(
    pool: &SqlitePool,
    headers: &HeaderMap,
) -> anyhow::Result<Option<CurrentUser>> {
    let Some(token) = session_cookie(headers) else {
        return Ok(None);
    };
    let token_hash = hash_token(&token);
    pool.call(move |conn| {
        conn.query_row(
            r#"
        SELECT u.id, u.username, u.display_name, u.is_admin, u.is_suspended, u.theme,
               u.nsfw_blur_enabled, u.must_change_password, u.deletion_scheduled_at
        FROM sessions s
        JOIN users u ON u.id = s.user_id
        WHERE s.token_hash = ? AND s.revoked_at IS NULL
          AND datetime(s.expires_at) > CURRENT_TIMESTAMP
          AND u.is_deleted = 0
        "#,
            [token_hash],
            |row| {
                Ok(CurrentUser {
                    id: row.get(0)?,
                    username: row.get(1)?,
                    display_name: row.get(2)?,
                    is_admin: row.get::<_, i64>(3)? != 0,
                    is_suspended: row.get::<_, i64>(4)? != 0,
                    theme: Theme::from(row.get::<_, String>(5)?.as_str()),
                    nsfw_blur_enabled: row.get::<_, i64>(6)? != 0,
                    must_change_password: row.get::<_, i64>(7)? != 0,
                    deletion_scheduled_at: row.get(8)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    })
    .await
}

pub async fn csrf_hashes_for_cookie(
    pool: &SqlitePool,
    token: &str,
) -> anyhow::Result<Option<Vec<String>>> {
    let token_hash = hash_token(token);
    pool.call(move |conn| {
        conn.query_row(
            "SELECT substr(s.csrf_token_hash, 1, 128), substr(s.previous_csrf_token_hash, 1, 4096) FROM sessions s JOIN users u ON u.id = s.user_id WHERE s.token_hash = ? AND s.revoked_at IS NULL AND datetime(s.expires_at) > CURRENT_TIMESTAMP AND u.is_deleted = 0",
            [token_hash],
            |row| {
                let current = row.get::<_, String>(0)?;
                let previous = row.get::<_, Option<String>>(1)?;
                let mut hashes = Vec::with_capacity(8);
                hashes.push(current);
                if let Some(previous) = previous {
                    hashes.extend(
                        previous
                            .lines()
                            .filter(|hash| !hash.is_empty())
                            .take(31)
                            .map(ToOwned::to_owned),
                    );
                }
                Ok(hashes)
            },
        )
        .optional()
        .map_err(Into::into)
    })
    .await
}

/// Parse all Cookie fields (including HTTP/2 split fields), rejecting duplicate
/// session names rather than letting browser/proxy ordering select an identity.
pub(crate) fn checked_session_cookie(headers: &HeaderMap) -> anyhow::Result<Option<String>> {
    let mut token = None;
    for raw in headers.get_all(axum::http::header::COOKIE) {
        let raw = raw.to_str().context("invalid Cookie header")?;
        for part in raw.split(';') {
            let Some((name, value)) = part.trim().split_once('=') else {
                continue;
            };
            if name != "rustpost_session" {
                continue;
            }
            anyhow::ensure!(token.is_none(), "ambiguous session cookies");
            anyhow::ensure!(
                value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "invalid session cookie"
            );
            token = Some(value.to_owned());
        }
    }
    Ok(token)
}

pub fn session_cookie(headers: &HeaderMap) -> Option<String> {
    checked_session_cookie(headers).ok().flatten()
}

/// A domain-separated token stable for one session. It cannot reveal the
/// `HttpOnly` session credential and survives page loads, tabs and restarts.
pub(crate) fn session_csrf_token(session: &str) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, session.as_bytes());
    let signature = ring::hmac::sign(&key, b"RustPost session CSRF v1");
    base64::Engine::encode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        signature.as_ref(),
    )
}

pub fn set_session_cookie(session: &Session, secure: bool) -> String {
    cookie_value(
        "rustpost_session",
        &session.token,
        secure,
        Some(60 * 60 * 24 * 30),
    )
}

pub fn clear_session_cookie(secure: bool) -> String {
    cookie_value("rustpost_session", "", secure, Some(0))
}

fn cookie_value(name: &str, value: &str, secure: bool, max_age: Option<u64>) -> String {
    let mut cookie = format!("{name}={value}; Path=/; HttpOnly; SameSite=Lax");
    if secure {
        cookie.push_str("; Secure");
    }
    if let Some(max_age) = max_age {
        cookie.push_str(&format!("; Max-Age={max_age}"));
    }
    cookie
}

pub fn hash_password(password: &str) -> anyhow::Result<String> {
    let mut salt_bytes = [0_u8; 16];
    getrandom::fill(&mut salt_bytes).context("generate password salt")?;
    let params = Params::new(19_456, 2, 1, None)
        .map_err(|err| anyhow::anyhow!("invalid argon2 params: {err}"))?;
    Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_with_salt(password.as_bytes(), &salt_bytes)
        .map_err(|err| anyhow::anyhow!("password hashing failed: {err}"))?
        .to_string())
}

pub fn verify_password(password: &str, encoded_hash: &str) -> anyhow::Result<bool> {
    let parsed = PasswordHash::new(encoded_hash)
        .map_err(|err| anyhow::anyhow!("invalid password hash: {err}"))?;
    Ok(Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok())
}

pub fn hash_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, digest)
}

pub fn secure_token() -> String {
    let mut token = Uuid::new_v4().simple().to_string();
    token.push_str(&Uuid::new_v4().simple().to_string());
    token
}
