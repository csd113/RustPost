use rusqlite::params;

use crate::db::SqlitePool;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Post,
    Reply,
    Repost,
    FailedLogin,
    Registration,
    AnonymousPost,
    UpdateCheck,
}

impl Scope {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Post => "post",
            Self::Reply => "reply",
            Self::Repost => "repost",
            Self::FailedLogin => "failed_login",
            Self::Registration => "registration",
            Self::AnonymousPost => "anonymous_post",
            Self::UpdateCheck => "update_check",
        }
    }
}

/// Atomically checks the limit and records the event.
///
/// The check and the insert happen inside one database transaction so that
/// concurrent requests from the same actor cannot all observe an under-limit
/// state and slip past the limit together.
pub async fn check_and_record(
    pool: &SqlitePool,
    scope: Scope,
    actor: &str,
    max_events: i64,
    window_secs: i64,
) -> anyhow::Result<()> {
    if max_events <= 0 {
        anyhow::bail!("rate limit exceeded; try again later");
    }
    let cutoff = format!("-{window_secs} seconds");
    let actor = actor.to_owned();
    pool.call(move |conn| {
        let tx = conn.transaction()?;
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM rate_limit_events WHERE scope = ? AND actor = ? AND created_at >= datetime('now', ?)",
            params![scope.as_str(), actor, cutoff],
            |row| row.get(0),
        )?;
        if count >= max_events {
            anyhow::bail!("rate limit exceeded; try again later");
        }
        tx.execute(
            "INSERT INTO rate_limit_events (scope, actor) VALUES (?, ?)",
            params![scope.as_str(), actor],
        )?;
        tx.commit()?;
        Ok(())
    })
    .await
}

pub async fn ensure_under_limit(
    pool: &SqlitePool,
    scope: Scope,
    actor: &str,
    max_events: i64,
    window_secs: i64,
) -> anyhow::Result<()> {
    if max_events <= 0 {
        anyhow::bail!("rate limit exceeded; try again later");
    }
    let cutoff = format!("-{window_secs} seconds");
    let actor = actor.to_owned();
    let count: i64 = pool
        .call(move |conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM rate_limit_events WHERE scope = ? AND actor = ? AND created_at >= datetime('now', ?)",
                params![scope.as_str(), actor, cutoff],
                |row| row.get(0),
            )?)
        })
        .await?;
    if count >= max_events {
        anyhow::bail!("rate limit exceeded; try again later");
    }
    Ok(())
}

pub async fn record(pool: &SqlitePool, scope: Scope, actor: &str) -> anyhow::Result<()> {
    let actor = actor.to_owned();
    pool.call(move |conn| {
        conn.execute(
            "INSERT INTO rate_limit_events (scope, actor) VALUES (?, ?)",
            params![scope.as_str(), actor],
        )?;
        Ok(())
    })
    .await
}

pub async fn prune_old(pool: &SqlitePool, oldest_window_secs: i64) -> anyhow::Result<()> {
    let cutoff = format!("-{oldest_window_secs} seconds");
    pool.call(move |conn| {
        conn.execute(
            "DELETE FROM rate_limit_events WHERE created_at < datetime('now', ?)",
            [cutoff],
        )?;
        Ok(())
    })
    .await
}
