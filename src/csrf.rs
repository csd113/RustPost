//! Session-bound form tokens and request provenance guards.

use axum::http::HeaderMap;
use rusqlite::OptionalExtension as _;
use subtle::{Choice, ConstantTimeEq as _};

use crate::auth::{csrf_hashes_for_cookie, hash_token, session_cookie};
use crate::db::SqlitePool;

mod origin;
pub(crate) use origin::{OnionTransport, RequestSecurity, request_guard};

const TOKEN_HISTORY_LIMIT: usize = 32;

#[derive(Debug, thiserror::Error)]
#[error("missing, expired or mismatched CSRF session/token")]
pub(crate) struct Rejected;

pub async fn validate(
    pool: &SqlitePool,
    headers: &HeaderMap,
    submitted: &str,
) -> anyhow::Result<()> {
    if submitted.is_empty() || submitted.len() > 128 || !submitted.is_ascii() {
        return Err(Rejected.into());
    }
    let token = session_cookie(headers).ok_or(Rejected)?;
    let accepted_hashes = csrf_hashes_for_cookie(pool, &token)
        .await?
        .ok_or(Rejected)?;
    let submitted = hash_token(submitted);
    let matched = accepted_hashes
        .iter()
        .fold(Choice::from(0), |matched, hash| {
            matched | hash.as_bytes().ct_eq(submitted.as_bytes())
        });
    if matched.unwrap_u8() == 0 {
        return Err(Rejected.into());
    }
    Ok(())
}

/// Upgrade legacy rotating sessions once in the serialized DB worker. Keep
/// their bounded history so already-open forms continue to work after upgrade.
pub(crate) async fn issue(pool: &SqlitePool, session: &str) -> anyhow::Result<Option<String>> {
    let plain = crate::auth::session_csrf_token(session);
    let new_hash = hash_token(&plain);
    let token_hash = hash_token(session);
    let valid = pool.call(move |conn| {
        let row: Option<(String, Option<String>)> = conn.query_row(
            "SELECT substr(s.csrf_token_hash, 1, 128), substr(s.previous_csrf_token_hash, 1, 4096) FROM sessions s JOIN users u ON u.id = s.user_id WHERE s.token_hash = ? AND s.revoked_at IS NULL AND datetime(s.expires_at) > CURRENT_TIMESTAMP AND u.is_deleted = 0",
            [&token_hash], |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?;
        let Some((current, previous)) = row else { return Ok(false); };
        if current != new_hash {
            let history = std::iter::once(current.as_str()).chain(previous.as_deref().into_iter().flat_map(str::lines))
                .filter(|hash| !hash.is_empty() && *hash != new_hash)
                .take(TOKEN_HISTORY_LIMIT - 1).collect::<Vec<_>>().join("\n");
            let changed = conn.execute("UPDATE sessions SET csrf_token_hash = ?, previous_csrf_token_hash = ? WHERE token_hash = ? AND revoked_at IS NULL AND datetime(expires_at) > CURRENT_TIMESTAMP", rusqlite::params![new_hash, history, token_hash])?;
            return Ok(changed == 1);
        }
        Ok(true)
    }).await?;
    Ok(valid.then_some(plain))
}
