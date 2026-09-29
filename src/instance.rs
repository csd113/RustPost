//! Instance-wide settings that administrators can change while the server is
//! running: the top-bar announcement and maintenance mode.
//!
//! Values live in the `instance_settings` key/value table so they survive
//! restarts, are covered by database backups, and do not depend on rewriting
//! `settings.toml` from the web UI.

use rusqlite::{Connection, OptionalExtension as _};

use crate::db::SqlitePool;

/// Maximum announcement length in Unicode characters.
pub const ANNOUNCEMENT_MAX_CHARS: usize = 280;
/// Maximum maintenance message length in Unicode characters.
pub const MAINTENANCE_MESSAGE_MAX_CHARS: usize = 280;
/// Prefix for released-username tombstones in `instance_settings`.
///
/// When an account is permanently deleted its handles become claimable again.
/// A tombstone records that the handle was previously held by an account that
/// no longer exists, so an old profile URL can never silently appear to
/// represent the deleted account when someone new registers the same name.
pub const RELEASED_USERNAME_KEY_PREFIX: &str = "released_username:";
/// Maximum tombstones recorded for one account deletion, bounding storage.
pub const MAX_RELEASED_USERNAMES_PER_ACCOUNT: usize = 50;
/// Message shown while maintenance mode is enabled without a custom message.
pub const DEFAULT_MAINTENANCE_MESSAGE: &str =
    "RustPost is in maintenance mode. Posting and registration are temporarily disabled.";

/// Durable instance state rendered in every page layout and enforced by the
/// maintenance and announcement guards.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InstanceSettings {
    pub announcement: String,
    pub announcement_enabled: bool,
    pub maintenance_mode: bool,
    pub maintenance_message: String,
}

impl InstanceSettings {
    /// Announcement text to render, or `None` when disabled or empty.
    #[must_use]
    pub fn announcement_text(&self) -> Option<&str> {
        if !self.announcement_enabled {
            return None;
        }
        let trimmed = self.announcement.trim();
        if trimmed.is_empty() {
            return None;
        }
        Some(trimmed)
    }

    /// Maintenance notice to render, or `None` when maintenance is off.
    #[must_use]
    pub fn maintenance_notice(&self) -> Option<&str> {
        if !self.maintenance_mode {
            return None;
        }
        let trimmed = self.maintenance_message.trim();
        if trimmed.is_empty() {
            return Some(DEFAULT_MAINTENANCE_MESSAGE);
        }
        Some(trimmed)
    }
}

/// Reads the instance settings from an already-open connection.
///
/// Missing keys fall back to the [`InstanceSettings::default`] values. Unknown
/// keys are ignored. Stored booleans are `"1"`/`"0"`, and any other value is
/// treated as `false`: a corrupt flag must never silently enable maintenance
/// mode or an announcement.
pub fn load_from_connection(conn: &Connection) -> anyhow::Result<InstanceSettings> {
    let mut settings = InstanceSettings::default();
    let mut stmt = conn.prepare("SELECT key, value FROM instance_settings")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (key, value) = row?;
        match key.as_str() {
            "announcement" => settings.announcement = value,
            "announcement_enabled" => settings.announcement_enabled = truthy(&value),
            "maintenance_mode" => settings.maintenance_mode = truthy(&value),
            "maintenance_message" => settings.maintenance_message = value,
            _ => {}
        }
    }
    Ok(settings)
}

/// Interprets a stored boolean. Only the canonical `"1"` written by the save
/// helpers counts as `true`; anything else is `false`.
fn truthy(value: &str) -> bool {
    value == "1"
}

/// Reads the instance settings through the database worker.
pub async fn load(pool: &SqlitePool) -> anyhow::Result<InstanceSettings> {
    pool.call(|conn| load_from_connection(conn)).await
}

/// Stores the announcement text and enabled flag.
///
/// Rejects text longer than [`ANNOUNCEMENT_MAX_CHARS`] and control characters
/// other than newlines and tabs. An empty announcement is stored as disabled.
pub async fn save_announcement(pool: &SqlitePool, text: &str, enabled: bool) -> anyhow::Result<()> {
    validate_setting_text(
        text,
        ANNOUNCEMENT_MAX_CHARS,
        "announcement is too long",
        "announcement contains unsupported control characters",
    )?;
    let text = text.to_owned();
    let enabled = enabled && !text.trim().is_empty();
    pool.call(move |conn| {
        let tx = conn.transaction()?;
        upsert_setting_tx(&tx, "announcement", &text)?;
        upsert_setting_tx(&tx, "announcement_enabled", if enabled { "1" } else { "0" })?;
        tx.commit()?;
        Ok(())
    })
    .await
}

/// Stores the maintenance flag and optional message.
///
/// Rejects messages longer than [`MAINTENANCE_MESSAGE_MAX_CHARS`] and control
/// characters other than newlines and tabs.
pub async fn save_maintenance(
    pool: &SqlitePool,
    enabled: bool,
    message: &str,
) -> anyhow::Result<()> {
    validate_setting_text(
        message,
        MAINTENANCE_MESSAGE_MAX_CHARS,
        "maintenance message is too long",
        "maintenance message contains unsupported control characters",
    )?;
    let message = message.to_owned();
    pool.call(move |conn| {
        let tx = conn.transaction()?;
        upsert_setting_tx(&tx, "maintenance_mode", if enabled { "1" } else { "0" })?;
        upsert_setting_tx(&tx, "maintenance_message", &message)?;
        tx.commit()?;
        Ok(())
    })
    .await
}

fn validate_setting_text(
    text: &str,
    max_chars: usize,
    too_long_error: &str,
    control_chars_error: &str,
) -> anyhow::Result<()> {
    if text.chars().count() > max_chars {
        anyhow::bail!("{too_long_error}");
    }
    if text
        .chars()
        .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
    {
        anyhow::bail!("{control_chars_error}");
    }
    Ok(())
}

fn upsert_setting_tx(tx: &rusqlite::Transaction<'_>, key: &str, value: &str) -> anyhow::Result<()> {
    tx.execute(
        r#"
        INSERT INTO instance_settings (key, value) VALUES (?, ?)
        ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = CURRENT_TIMESTAMP
        "#,
        rusqlite::params![key, value],
    )?;
    Ok(())
}

/// Records release tombstones for handles freed by an account deletion.
///
/// Must run inside the deletion transaction. Only normalized handles are
/// stored; no user identity is retained. Handles are ignored when empty,
/// control-containing, or longer than a username, and at most
/// [`MAX_RELEASED_USERNAMES_PER_ACCOUNT`] handles are recorded.
pub fn record_released_usernames_tx(
    tx: &rusqlite::Transaction<'_>,
    normalized_usernames: &[String],
) -> anyhow::Result<()> {
    let count = normalized_usernames
        .len()
        .min(MAX_RELEASED_USERNAMES_PER_ACCOUNT);
    for normalized in normalized_usernames.iter().take(count) {
        if normalized.is_empty()
            || normalized.chars().count() > 128
            || normalized.chars().any(char::is_control)
        {
            continue;
        }
        upsert_setting_tx(
            tx,
            &released_username_key(normalized),
            RELEASED_USERNAME_TOMBSTONE_VALUE,
        )?;
    }
    Ok(())
}

/// Records one release tombstone on its own. Used by tests and by callers
/// outside a deletion transaction; the deletion path uses
/// [`record_released_usernames_tx`] inside its transaction.
pub async fn record_released_username(
    pool: &SqlitePool,
    normalized_username: &str,
) -> anyhow::Result<()> {
    let normalized = normalized_username.to_owned();
    pool.call(move |conn| {
        let tx = conn.transaction()?;
        record_released_usernames_tx(&tx, std::slice::from_ref(&normalized))?;
        tx.commit()?;
        Ok(())
    })
    .await
}

/// Whether a handle was released by a permanently deleted account.
pub async fn username_was_released(
    pool: &SqlitePool,
    normalized_username: &str,
) -> anyhow::Result<bool> {
    let key = released_username_key(normalized_username);
    pool.call(move |conn| {
        Ok(conn
            .query_row(
                "SELECT 1 FROM instance_settings WHERE key = ?",
                [key],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    })
    .await
}

fn released_username_key(normalized_username: &str) -> String {
    format!("{RELEASED_USERNAME_KEY_PREFIX}{normalized_username}")
}

/// Stored value for a release tombstone. The timestamp lives in `updated_at`.
const RELEASED_USERNAME_TOMBSTONE_VALUE: &str = "1";

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture() -> (tempfile::TempDir, SqlitePool) {
        let temp = tempfile::tempdir().expect("temp dir");
        let pool = crate::db::connect(&temp.path().join("test.sqlite3"))
            .await
            .expect("connect");
        crate::db::migrate(&pool).await.expect("migrate");
        (temp, pool)
    }

    #[tokio::test]
    async fn empty_instance_settings_use_defaults() {
        let (_temp, pool) = fixture().await;

        let settings = load(&pool).await.expect("load");

        assert_eq!(settings, InstanceSettings::default());
        assert_eq!(settings.announcement_text(), None);
        assert_eq!(settings.maintenance_notice(), None);
    }

    #[tokio::test]
    async fn saved_settings_survive_a_fresh_connection() {
        let (temp, pool) = fixture().await;
        save_announcement(&pool, "  Hello\nworld  ", true)
            .await
            .expect("announcement");
        save_maintenance(&pool, true, " back at 20:00 ")
            .await
            .expect("maintenance");

        let reloaded = load(&pool).await.expect("reload");
        assert_eq!(reloaded.announcement, "  Hello\nworld  ");
        assert!(reloaded.announcement_enabled);
        assert_eq!(reloaded.announcement_text(), Some("Hello\nworld"));
        assert!(reloaded.maintenance_mode);
        assert_eq!(reloaded.maintenance_message, " back at 20:00 ");
        assert_eq!(reloaded.maintenance_notice(), Some("back at 20:00"));

        let reopened = crate::db::connect(&temp.path().join("test.sqlite3"))
            .await
            .expect("reopen");
        let persisted = load(&reopened).await.expect("persisted");
        assert_eq!(persisted, reloaded);
    }

    #[tokio::test]
    async fn saves_refresh_updated_at() {
        let (_temp, pool) = fixture().await;
        save_announcement(&pool, "first", true)
            .await
            .expect("announcement");
        pool.call(|conn| {
            conn.execute(
                "UPDATE instance_settings SET updated_at = '2000-01-01 00:00:00'",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("age rows");

        save_announcement(&pool, "second", true)
            .await
            .expect("second announcement");

        let updated_at: String = pool
            .call(|conn| {
                Ok(conn.query_row(
                    "SELECT updated_at FROM instance_settings WHERE key = 'announcement'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await
            .expect("updated_at");
        assert_ne!(updated_at, "2000-01-01 00:00:00");
    }

    #[tokio::test]
    async fn enabled_empty_announcement_renders_nothing() {
        let (_temp, pool) = fixture().await;

        save_announcement(&pool, "   ", true)
            .await
            .expect("empty announcement");

        let settings = load(&pool).await.expect("load");
        assert!(!settings.announcement_enabled);
        assert_eq!(settings.announcement_text(), None);

        save_announcement(&pool, "visible text", false)
            .await
            .expect("disabled announcement");
        let settings = load(&pool).await.expect("load");
        assert!(!settings.announcement_enabled);
        assert_eq!(settings.announcement_text(), None);
    }

    #[tokio::test]
    async fn rejects_long_and_control_character_settings() {
        let (_temp, pool) = fixture().await;

        let max_length = "é".repeat(ANNOUNCEMENT_MAX_CHARS);
        save_announcement(&pool, &max_length, true)
            .await
            .expect("max-length announcement");
        let too_long = save_announcement(&pool, &"x".repeat(ANNOUNCEMENT_MAX_CHARS + 1), true)
            .await
            .expect_err("too-long announcement");
        assert_eq!(too_long.to_string(), "announcement is too long");
        let control = save_announcement(&pool, "bad\u{7}text", true)
            .await
            .expect_err("control characters");
        assert_eq!(
            control.to_string(),
            "announcement contains unsupported control characters"
        );
        save_announcement(&pool, "line\nbreak\ttab", true)
            .await
            .expect("allowed whitespace");

        let too_long =
            save_maintenance(&pool, true, &"m".repeat(MAINTENANCE_MESSAGE_MAX_CHARS + 1))
                .await
                .expect_err("too-long maintenance message");
        assert_eq!(too_long.to_string(), "maintenance message is too long");
        let control = save_maintenance(&pool, true, "bad\u{0}text")
            .await
            .expect_err("control characters");
        assert_eq!(
            control.to_string(),
            "maintenance message contains unsupported control characters"
        );
    }

    #[tokio::test]
    async fn empty_maintenance_message_uses_the_default_notice() {
        let (_temp, pool) = fixture().await;

        save_maintenance(&pool, true, "  ")
            .await
            .expect("maintenance on");
        let settings = load(&pool).await.expect("load");
        assert!(settings.maintenance_mode);
        assert_eq!(
            settings.maintenance_notice(),
            Some(DEFAULT_MAINTENANCE_MESSAGE)
        );

        save_maintenance(&pool, false, "")
            .await
            .expect("maintenance off");
        let settings = load(&pool).await.expect("load");
        assert!(!settings.maintenance_mode);
        assert_eq!(settings.maintenance_notice(), None);
    }

    #[tokio::test]
    async fn corrupt_booleans_and_unknown_keys_default_off() {
        let (_temp, pool) = fixture().await;
        pool.call(|conn| {
            conn.execute(
                "INSERT INTO instance_settings (key, value) VALUES ('maintenance_mode', 'maybe')",
                [],
            )?;
            conn.execute(
                "INSERT INTO instance_settings (key, value) VALUES ('announcement_enabled', 'yes')",
                [],
            )?;
            conn.execute(
                "INSERT INTO instance_settings (key, value) VALUES ('unknown_future_key', 'x')",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("corrupt rows");

        let settings = load(&pool).await.expect("load");

        assert!(!settings.maintenance_mode);
        assert!(!settings.announcement_enabled);
        assert_eq!(settings, InstanceSettings::default());
    }

    #[tokio::test]
    async fn released_usernames_are_recorded_and_queryable() {
        let (_temp, pool) = fixture().await;

        assert!(!username_was_released(&pool, "alice").await.expect("query"));
        record_released_username(&pool, "alice")
            .await
            .expect("record");
        assert!(username_was_released(&pool, "alice").await.expect("query"));
        assert!(!username_was_released(&pool, "bob").await.expect("query"));

        // Recording again refreshes the timestamp without duplicating a key.
        record_released_username(&pool, "alice")
            .await
            .expect("record");
        let rows: i64 = pool
            .call(|conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM instance_settings WHERE key = 'released_username:alice'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await
            .expect("count");
        assert_eq!(rows, 1);

        // Tombstones must not leak into the announcement/maintenance view.
        assert_eq!(
            load(&pool).await.expect("load"),
            InstanceSettings::default()
        );
    }

    #[tokio::test]
    async fn released_usernames_are_bounded_and_screened() {
        let (_temp, pool) = fixture().await;
        let mut handles = (0..MAX_RELEASED_USERNAMES_PER_ACCOUNT + 25)
            .map(|index| format!("handle{index}"))
            .collect::<Vec<_>>();
        handles.push(String::new());
        handles.push("bad\u{7}name".to_owned());

        pool.call(move |conn| {
            let tx = conn.transaction()?;
            record_released_usernames_tx(&tx, &handles)?;
            tx.commit()?;
            Ok(())
        })
        .await
        .expect("record bounded");

        let count: i64 = pool
            .call(|conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM instance_settings WHERE key LIKE 'released_username:%'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await
            .expect("count");
        assert_eq!(
            count,
            i64::try_from(MAX_RELEASED_USERNAMES_PER_ACCOUNT).expect("limit fits")
        );
        assert!(!username_was_released(&pool, "").await.expect("empty"));
        assert!(
            !username_was_released(&pool, "bad\u{7}name")
                .await
                .expect("control")
        );
    }
}
