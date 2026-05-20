use rusqlite::params;

use crate::auth;
use crate::config::Settings;
use crate::db::SqlitePool;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MediaJobsReport {
    pub total: i64,
    pub pending: i64,
    pub running: i64,
    pub succeeded: i64,
    pub failed: i64,
    pub newest_pending_age_seconds: Option<i64>,
    pub oldest_pending_age_seconds: Option<i64>,
    pub recent_failures: Vec<MediaJobFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaJobFailure {
    pub id: i64,
    pub media_id: Option<i64>,
    pub media_path: Option<String>,
    pub job_kind: Option<String>,
    pub age_seconds: Option<i64>,
    pub error_summary: String,
}

pub async fn create_admin(
    pool: &SqlitePool,
    settings: &Settings,
    username: &str,
    password: &str,
) -> anyhow::Result<i64> {
    auth::register_user(pool, settings, username, password, true).await
}

pub async fn create_admin_with_display_name(
    pool: &SqlitePool,
    settings: &Settings,
    username: &str,
    password: &str,
    display_name: Option<&str>,
) -> anyhow::Result<i64> {
    let display_name = display_name.map(str::trim).filter(|name| !name.is_empty());
    if let Some(display_name) = display_name {
        crate::validation::validate_profile_text(display_name, "", settings)?;
    }
    let user_id = create_admin(pool, settings, username, password).await?;
    if let Some(display_name) = display_name {
        let display_name = display_name.to_owned();
        pool.call(move |conn| {
            conn.execute(
                "UPDATE users SET display_name = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
                params![display_name, user_id],
            )?;
            Ok(())
        })
        .await?;
    }
    Ok(user_id)
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

pub async fn media_jobs_report(pool: &SqlitePool) -> anyhow::Result<MediaJobsReport> {
    pool.call(|conn| {
        let (total, pending, running, succeeded, failed, newest_pending_age, oldest_pending_age) =
            conn.query_row(
                r"
                SELECT
                    COUNT(*),
                    COALESCE(SUM(CASE WHEN status IN ('pending', 'queued') THEN 1 ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN status = 'running' THEN 1 ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN status IN ('succeeded', 'success', 'converted') THEN 1 ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN status IN ('failed', 'error', 'fallback') THEN 1 ELSE 0 END), 0),
                    MIN(CASE WHEN status IN ('pending', 'queued') THEN CAST(strftime('%s', 'now') - strftime('%s', created_at) AS INTEGER) END),
                    MAX(CASE WHEN status IN ('pending', 'queued') THEN CAST(strftime('%s', 'now') - strftime('%s', created_at) AS INTEGER) END)
                FROM media_jobs
                ",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )?;

        let mut stmt = conn.prepare(
            r"
            SELECT
                j.id,
                j.media_id,
                COALESCE(NULLIF(m.public_path, ''), NULLIF(m.original_filename, ''), NULLIF(m.stored_path, '')),
                NULLIF(m.media_kind, ''),
                CAST(strftime('%s', 'now') - strftime('%s', COALESCE(j.finished_at, j.created_at)) AS INTEGER),
                j.stderr_summary
            FROM media_jobs j
            LEFT JOIN media m ON m.id = j.media_id
            WHERE j.status IN ('failed', 'error', 'fallback')
            ORDER BY j.id DESC
            LIMIT 5
            ",
        )?;
        let recent_failures = stmt
            .query_map([], |row| {
                Ok(MediaJobFailure {
                    id: row.get(0)?,
                    media_id: row.get(1)?,
                    media_path: row.get(2)?,
                    job_kind: row.get(3)?,
                    age_seconds: row.get(4)?,
                    error_summary: row.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(MediaJobsReport {
            total,
            pending,
            running,
            succeeded,
            failed,
            newest_pending_age_seconds: newest_pending_age,
            oldest_pending_age_seconds: oldest_pending_age,
            recent_failures,
        })
    })
    .await
}
