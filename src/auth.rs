use argon2::password_hash::{PasswordHash, PasswordHasher as _, PasswordVerifier as _, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use axum::http::HeaderMap;
use chrono::{Duration, Utc};
use rand_core::OsRng;
use rusqlite::{OptionalExtension as _, params};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use crate::config::Settings;
use crate::db::SqlitePool;
use crate::validation::{normalize_username, validate_password};

#[derive(Debug, Clone)]
pub struct CurrentUser {
    pub id: i64,
    pub username: String,
    pub display_name: String,
    pub is_admin: bool,
    pub is_suspended: bool,
}

#[derive(Debug, Clone)]
pub struct Session {
    pub token: String,
    pub csrf_token: String,
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
    let hash = hash_password(password)?;
    let username = username.trim().to_owned();
    pool.call(move |conn| {
        let existing = conn
            .query_row(
                "SELECT 1 FROM users WHERE normalized_username = ?",
                [&normalized],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if existing {
            anyhow::bail!("username is already taken");
        }
        conn.execute(
            "INSERT INTO users (username, normalized_username, password_hash, display_name, is_admin) VALUES (?, ?, ?, ?, ?)",
            params![username, normalized, hash, username, i64::from(is_admin)],
        )?;
        Ok(conn.last_insert_rowid())
    })
    .await
}

pub async fn login(
    pool: &SqlitePool,
    username: &str,
    password: &str,
) -> anyhow::Result<Option<Session>> {
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
        return Ok(None);
    };
    if row.2 != 0 || row.3 != 0 {
        return Ok(None);
    }
    if !verify_password(password, &row.1)? {
        return Ok(None);
    }
    create_session(pool, row.0).await.map(Some)
}

pub async fn create_session(pool: &SqlitePool, user_id: i64) -> anyhow::Result<Session> {
    let token = secure_token();
    let csrf_token = secure_token();
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
        SELECT u.id, u.username, u.display_name, u.is_admin, u.is_suspended
        FROM sessions s
        JOIN users u ON u.id = s.user_id
        WHERE s.token_hash = ? AND s.revoked_at IS NULL AND s.expires_at > CURRENT_TIMESTAMP AND u.is_deleted = 0
        "#,
            [token_hash],
            |row| {
                Ok(CurrentUser {
                    id: row.get(0)?,
                    username: row.get(1)?,
                    display_name: row.get(2)?,
                    is_admin: row.get::<_, i64>(3)? != 0,
                    is_suspended: row.get::<_, i64>(4)? != 0,
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
            "SELECT csrf_token_hash, previous_csrf_token_hash FROM sessions WHERE token_hash = ? AND revoked_at IS NULL",
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

pub fn session_cookie(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    raw.split(';').find_map(|part| {
        let part = part.trim();
        part.strip_prefix("rustpost_session=")
            .map(ToOwned::to_owned)
    })
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
    let salt = SaltString::generate(&mut OsRng);
    let params = Params::new(19_456, 2, 1, None)
        .map_err(|err| anyhow::anyhow!("invalid argon2 params: {err}"))?;
    Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password(password.as_bytes(), &salt)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hash_verifies() {
        let hash = hash_password("correct horse battery").expect("hash");
        assert!(verify_password("correct horse battery", &hash).expect("verify"));
        assert!(!verify_password("wrong horse battery", &hash).expect("verify"));
        assert!(!hash.contains("correct horse battery"));
    }

    #[tokio::test]
    async fn duplicate_username_is_rejected() {
        let temp = tempfile::tempdir().expect("temp dir");
        let pool = crate::db::connect(&temp.path().join("test.sqlite3"))
            .await
            .expect("connect");
        crate::db::migrate(&pool).await.expect("migrate");
        let settings = Settings::default();

        register_user(&pool, &settings, "Alice", "very secure password", false)
            .await
            .expect("first user");
        assert!(
            register_user(&pool, &settings, "alice", "very secure password", false)
                .await
                .is_err()
        );
    }
}
