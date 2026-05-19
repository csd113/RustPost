use axum::http::HeaderMap;

use crate::auth::{csrf_hashes_for_cookie, hash_token, session_cookie};
use crate::db::SqlitePool;

pub async fn validate(
    pool: &SqlitePool,
    headers: &HeaderMap,
    submitted: &str,
) -> anyhow::Result<()> {
    let Some(token) = session_cookie(headers) else {
        anyhow::bail!("missing session");
    };
    let Some(accepted_hashes) = csrf_hashes_for_cookie(pool, &token).await? else {
        anyhow::bail!("missing csrf session");
    };
    let submitted = hash_token(submitted);
    if !accepted_hashes.iter().any(|hash| hash == &submitted) {
        anyhow::bail!("invalid csrf token");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::http::{HeaderMap, HeaderValue};

    use crate::{auth, db};

    #[tokio::test]
    async fn csrf_rejects_and_accepts() {
        let temp = tempfile::tempdir().expect("temp dir");
        let pool = db::connect(&temp.path().join("x.sqlite3"))
            .await
            .expect("connect");
        db::migrate(&pool).await.expect("migrate");
        let user_id = crate::auth::register_user(
            &pool,
            &crate::config::Settings::default(),
            "alice",
            "very secure password",
            false,
        )
        .await
        .expect("user");
        let session = auth::create_session(&pool, user_id).await.expect("session");
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::COOKIE,
            HeaderValue::from_str(&format!("rustpost_session={}", session.token)).expect("header"),
        );
        assert!(validate(&pool, &headers, "wrong").await.is_err());
        validate(&pool, &headers, &session.csrf_token)
            .await
            .expect("valid csrf");
    }
}
