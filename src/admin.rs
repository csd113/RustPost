use rusqlite::params;

use crate::auth;
use crate::config::Settings;
use crate::db::SqlitePool;

pub async fn create_admin(
    pool: &SqlitePool,
    settings: &Settings,
    username: &str,
    password: &str,
) -> anyhow::Result<i64> {
    auth::register_user(pool, settings, username, password, true).await
}

pub async fn reset_admin_password(
    pool: &SqlitePool,
    settings: &Settings,
    username: &str,
    password: &str,
) -> anyhow::Result<()> {
    crate::validation::validate_password(password, settings)?;
    let hash = auth::hash_password(password)?;
    let username = username.trim().to_ascii_lowercase();
    let changed = pool
        .call(move |conn| {
            Ok(conn.execute(
                "UPDATE users SET password_hash = ?, updated_at = CURRENT_TIMESTAMP WHERE normalized_username = ? AND is_admin = 1",
                params![hash, username],
            )?)
        })
        .await?;
    if changed == 0 {
        anyhow::bail!("admin user not found");
    }
    Ok(())
}

pub async fn ensure_first_boot_admin_hint(pool: &SqlitePool) -> anyhow::Result<()> {
    let count = admin_count(pool).await?;
    if count == 0 {
        tracing::warn!(
            "no admin account exists; run `rustpost-cli create-admin-interactive` or `rustpost-cli create-admin <username> <password>`"
        );
    }
    Ok(())
}

pub async fn admin_count(pool: &SqlitePool) -> anyhow::Result<i64> {
    pool.call(|conn| {
        Ok(
            conn.query_row("SELECT COUNT(*) FROM users WHERE is_admin = 1", [], |row| {
                row.get(0)
            })?,
        )
    })
    .await
}

pub async fn set_user_suspended(
    pool: &SqlitePool,
    admin_id: i64,
    user_id: i64,
    suspended: bool,
) -> anyhow::Result<()> {
    pool.call(move |conn| {
        conn.execute(
            "UPDATE users SET is_suspended = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
            params![i64::from(suspended), user_id],
        )?;
        Ok(())
    })
    .await?;
    audit(
        pool,
        admin_id,
        if suspended {
            "suspend_user"
        } else {
            "unsuspend_user"
        },
        &format!("user:{user_id}"),
    )
    .await?;
    Ok(())
}

pub async fn audit(
    pool: &SqlitePool,
    admin_id: i64,
    action: &str,
    target: &str,
) -> anyhow::Result<()> {
    let action = action.to_owned();
    let target = target.to_owned();
    pool.call(move |conn| {
        conn.execute(
            "INSERT INTO admin_audit_log (admin_user_id, action, target) VALUES (?, ?, ?)",
            params![admin_id, action, target],
        )?;
        Ok(())
    })
    .await
}

pub async fn users(pool: &SqlitePool) -> anyhow::Result<Vec<(i64, String, bool, bool)>> {
    pool.call(|conn| {
        let mut stmt = conn.prepare("SELECT id, username, is_admin, is_suspended FROM users WHERE is_deleted = 0 ORDER BY id DESC LIMIT 100")?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get::<_, i64>(2)? != 0, row.get::<_, i64>(3)? != 0)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}

pub async fn recent_media_jobs(pool: &SqlitePool) -> anyhow::Result<Vec<(i64, String, String)>> {
    pool.call(|conn| {
        let mut stmt = conn.prepare(
            "SELECT id, status, stderr_summary FROM media_jobs ORDER BY id DESC LIMIT 50",
        )?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}
