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
        }
    }
}

pub async fn check_and_record(
    pool: &SqlitePool,
    scope: Scope,
    actor: &str,
    max_events: i64,
    window_secs: i64,
) -> anyhow::Result<()> {
    ensure_under_limit(pool, scope, actor, max_events, window_secs).await?;
    record(pool, scope, actor).await
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    #[tokio::test]
    async fn limit_blocks_only_matching_actor_and_scope() {
        let temp = tempfile::tempdir().expect("temp dir");
        let pool = db::connect(&temp.path().join("test.sqlite3"))
            .await
            .expect("connect");
        db::migrate(&pool).await.expect("migrate");

        check_and_record(&pool, Scope::Post, "user:1", 1, 60)
            .await
            .expect("first post");
        assert!(
            check_and_record(&pool, Scope::Post, "user:1", 1, 60)
                .await
                .is_err()
        );
        check_and_record(&pool, Scope::Reply, "user:1", 1, 60)
            .await
            .expect("different scope");
        check_and_record(&pool, Scope::Post, "user:2", 1, 60)
            .await
            .expect("different actor");
    }

    #[tokio::test]
    async fn failed_login_limit_can_be_checked_before_recording() {
        let temp = tempfile::tempdir().expect("temp dir");
        let pool = db::connect(&temp.path().join("test.sqlite3"))
            .await
            .expect("connect");
        db::migrate(&pool).await.expect("migrate");

        ensure_under_limit(&pool, Scope::FailedLogin, "ip:127.0.0.1", 1, 900)
            .await
            .expect("under limit");
        record(&pool, Scope::FailedLogin, "ip:127.0.0.1")
            .await
            .expect("record");
        assert!(
            ensure_under_limit(&pool, Scope::FailedLogin, "ip:127.0.0.1", 1, 900)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn configured_action_scopes_can_each_rate_limit() {
        let temp = tempfile::tempdir().expect("temp dir");
        let pool = db::connect(&temp.path().join("test.sqlite3"))
            .await
            .expect("connect");
        db::migrate(&pool).await.expect("migrate");

        for scope in [
            Scope::Post,
            Scope::Reply,
            Scope::Repost,
            Scope::Registration,
            Scope::AnonymousPost,
        ] {
            let actor = format!("actor:{}", scope.as_str());
            check_and_record(&pool, scope, &actor, 1, 60)
                .await
                .expect("first action");
            assert!(check_and_record(&pool, scope, &actor, 1, 60).await.is_err());
        }
    }
}
