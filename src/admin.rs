use std::fs;
#[cfg(unix)]
use std::fs::File;
use std::io::Write as _;
use std::path::Path;

use anyhow::Context as _;
use rusqlite::{Row, params, params_from_iter};
use serde::Deserialize;

use crate::auth;
use crate::config::{BackupSettings, Settings};
use crate::db::SqlitePool;

#[cfg(test)]
const MIB: u64 = 1024 * 1024;

pub use crate::config::admin_fields::{
    DeepSettingsField, DeepSettingsForm, DeepSettingsInputKind, DeepSettingsValues,
    parse_deep_settings_form,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeepSettingsChange {
    pub label: &'static str,
    pub old_value: String,
    pub new_value: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupSettingsForm {
    pub csrf: String,
    #[serde(default = "unchecked_backup")]
    pub enabled: String,
    #[serde(default = "unchecked_backup")]
    pub automatic_enabled: String,
    pub automatic_interval_minutes: String,
    pub retention_keep_last: String,
    pub retention_max_age_days: String,
    #[serde(default = "unchecked_backup")]
    pub automatic_include_tor_keys: String,
}

fn unchecked_backup() -> String {
    "false".to_owned()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupSettingsValues {
    pub enabled: bool,
    pub automatic_enabled: bool,
    pub automatic_interval_minutes: u64,
    pub retention_keep_last: usize,
    pub retention_max_age_days: u64,
    pub automatic_include_tor_keys: bool,
}

impl BackupSettingsValues {
    #[must_use]
    pub fn from_settings(settings: &Settings) -> Self {
        Self {
            enabled: settings.backup.enabled,
            automatic_enabled: settings.backup.automatic_enabled,
            automatic_interval_minutes: settings.backup.automatic_interval_minutes,
            retention_keep_last: settings.backup.retention_keep_last,
            retention_max_age_days: settings.backup.retention_max_age_days,
            automatic_include_tor_keys: settings.backup.automatic_include_tor_keys,
        }
    }

    #[must_use]
    pub fn apply_to(&self, current: &Settings) -> Settings {
        let mut updated = current.clone();
        updated.backup = BackupSettings {
            enabled: self.enabled,
            backup_dir: current.backup.backup_dir.clone(),
            automatic_enabled: self.automatic_enabled,
            automatic_interval_minutes: self.automatic_interval_minutes,
            retention_keep_last: self.retention_keep_last,
            retention_max_age_days: self.retention_max_age_days,
            automatic_include_tor_keys: self.automatic_include_tor_keys,
        };
        updated
    }
}

pub fn parse_backup_settings_form(
    form: &BackupSettingsForm,
    current: &Settings,
) -> anyhow::Result<BackupSettingsValues> {
    let mut shared = DeepSettingsForm::from_settings(current);
    shared.backup_enabled.clone_from(&form.enabled);
    shared.automatic_enabled.clone_from(&form.automatic_enabled);
    shared
        .automatic_interval_minutes
        .clone_from(&form.automatic_interval_minutes);
    shared
        .retention_keep_last
        .clone_from(&form.retention_keep_last);
    shared
        .retention_max_age_days
        .clone_from(&form.retention_max_age_days);
    shared
        .automatic_include_tor_keys
        .clone_from(&form.automatic_include_tor_keys);
    let values = parse_deep_settings_form(&shared, current)?;
    Ok(BackupSettingsValues::from_settings(
        &values.apply_to(current),
    ))
}

#[must_use]
pub fn diff_deep_settings(
    current: &Settings,
    values: &DeepSettingsValues,
) -> Vec<DeepSettingsChange> {
    let old_values = DeepSettingsValues::from_settings(current);
    DeepSettingsField::ALL
        .iter()
        .copied()
        .filter(|field| old_values.form_value(*field) != values.form_value(*field))
        .map(|field| DeepSettingsChange {
            label: field.label(),
            old_value: old_values.display_value(field),
            new_value: values.display_value(field),
        })
        .collect()
}

pub fn write_deep_settings(path: &Path, updated: &Settings) -> anyhow::Result<()> {
    updated.validate()?;
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read settings file {}", path.display()))?;
    let rewritten = rewrite_settings_toml(&raw, updated, DeepSettingsField::ALL.into_iter())?;
    let parsed: Settings = toml::from_str(&rewritten)
        .with_context(|| "rewritten settings.toml did not parse as settings")?;
    parsed.validate()?;
    write_atomic(path, rewritten.as_bytes())
}

pub fn write_backup_settings(path: &Path, updated: &Settings) -> anyhow::Result<()> {
    updated.validate()?;
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read settings file {}", path.display()))?;
    let rewritten = rewrite_settings_toml(
        &raw,
        updated,
        DeepSettingsField::ALL
            .into_iter()
            .filter(|field| field.toml_section() == "backup"),
    )?;
    let parsed: Settings = toml::from_str(&rewritten)
        .with_context(|| "rewritten settings.toml did not parse as settings")?;
    parsed.validate()?;
    write_atomic(path, rewritten.as_bytes())
}

type SpannedFields = std::collections::BTreeMap<String, toml::Spanned<toml::Value>>;

// Deserialize only known tables so unrelated scalar or nested extension values
// do not constrain the writer. Unknown data stays untouched in the raw TOML.
#[derive(Deserialize)]
struct ConfigurationSpans {
    site: Option<toml::Spanned<SpannedFields>>,
    server: Option<toml::Spanned<SpannedFields>>,
    accounts: Option<toml::Spanned<SpannedFields>>,
    posts: Option<toml::Spanned<SpannedFields>>,
    media: Option<toml::Spanned<SpannedFields>>,
    tor: Option<toml::Spanned<SpannedFields>>,
    moderation: Option<toml::Spanned<SpannedFields>>,
    admin: Option<toml::Spanned<SpannedFields>>,
    backup: Option<toml::Spanned<SpannedFields>>,
}
impl ConfigurationSpans {
    fn get(&self, section: &str) -> Option<&toml::Spanned<SpannedFields>> {
        match section {
            "site" => self.site.as_ref(),
            "server" => self.server.as_ref(),
            "accounts" => self.accounts.as_ref(),
            "posts" => self.posts.as_ref(),
            "media" => self.media.as_ref(),
            "tor" => self.tor.as_ref(),
            "moderation" => self.moderation.as_ref(),
            "admin" => self.admin.as_ref(),
            "backup" => self.backup.as_ref(),
            _ => None,
        }
    }
}

/// Replace only typed, approved settings. TOML value spans handle multiline
/// strings/arrays, quoted keys and inline tables without touching adjacent text.
fn rewrite_settings_toml(
    raw: &str,
    settings: &Settings,
    fields: impl Iterator<Item = DeepSettingsField>,
) -> anyhow::Result<String> {
    use std::collections::BTreeMap;
    let spans: ConfigurationSpans = toml::from_str(raw)?;
    let raw_values: toml::Value = toml::from_str(raw)?;
    let fields = fields.collect::<Vec<_>>();
    let mut inline_sections = std::collections::BTreeSet::new();
    let before: Settings = toml::from_str(raw)?;
    let before_values = toml::Value::try_from(&before)?;
    let after_values = toml::Value::try_from(settings)?;
    let mut edits = Vec::new();
    let mut missing: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for field in &fields {
        let section = field.toml_section();
        let key = field.toml_key();
        let value = &after_values[section][key];
        if value == &before_values[section][key] {
            continue;
        }
        if let Some(table) = spans.get(section)
            && raw[table.span()].trim_start().starts_with('{')
        {
            if inline_sections.insert(section) {
                let mut replacement = raw_values[section].clone();
                for field in &fields {
                    if field.toml_section() == section {
                        replacement[field.toml_key()] =
                            after_values[section][field.toml_key()].clone();
                    }
                }
                edits.push((table.span(), replacement.to_string()));
            }
            continue;
        }
        if let Some(existing) = spans
            .get(section)
            .and_then(|table| table.get_ref().get(key))
        {
            edits.push((existing.span(), value.to_string()));
        } else {
            missing
                .entry(section)
                .or_default()
                .push(format!("{key} = {value}\n"));
        }
    }
    for (section, lines) in missing {
        if let Some(table) = spans.get(section) {
            let end = table.span().end;
            // Missing fields can only be inserted in a normal table. Refuse
            // unsupported shapes instead of silently producing corrupt TOML.
            if raw[..end].trim_end().ends_with('}') {
                anyhow::bail!(
                    "Missing setting in inline {section} table; expand the table before editing"
                );
            }
            edits.push((end..end, format!("\n{}", lines.join(""))));
        } else {
            edits.push((
                raw.len()..raw.len(),
                format!("\n[{section}]\n{}", lines.join("")),
            ));
        }
    }
    edits.sort_by_key(|(left, _)| std::cmp::Reverse(left.start));
    let mut rewritten = raw.to_owned();
    for (span, value) in edits {
        rewritten.replace_range(span, &value);
    }
    let parsed: Settings =
        toml::from_str(&rewritten).context("Updated TOML could not be parsed")?;
    parsed.validate()?;
    Ok(rewritten)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        anyhow::bail!("Settings must be a regular file, not a symbolic link");
    }
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary
        .as_file()
        .set_permissions(metadata.permissions())?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

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

const ADMIN_USERS_LIMIT: i64 = 100;
const ADMIN_POST_SEARCH_TERM_LIMIT: usize = 8;
const ADMIN_USERS_SQL: &str = r"
WITH name_matches AS (
    __NAME_MATCH_SQL__
),
matching_posts AS (
    SELECT p.id, p.user_id, p.text, p.created_at
    FROM posts p
    WHERE p.user_id IS NOT NULL
      AND p.is_deleted = 0
      AND (__POST_MATCH_SQL__)
),
post_match_counts AS (
    SELECT user_id, COUNT(*) AS matching_post_count
    FROM matching_posts
    GROUP BY user_id
),
ranked_post_matches AS (
    SELECT user_id, text,
        ROW_NUMBER() OVER (
            PARTITION BY user_id
            ORDER BY created_at DESC, id DESC
        ) AS rank
    FROM matching_posts
),
post_match_previews AS (
    SELECT user_id, text
    FROM ranked_post_matches
    WHERE rank = 1
),
post_stats AS (
    SELECT user_id, COUNT(*) AS total_posts, MAX(created_at) AS last_post_at
    FROM posts
    WHERE user_id IS NOT NULL AND is_deleted = 0
    GROUP BY user_id
),
media_stats AS (
    SELECT owner_user_id AS user_id, COUNT(*) AS uploaded_media_count
    FROM media
    WHERE owner_user_id IS NOT NULL
    GROUP BY owner_user_id
),
report_stats AS (
    SELECT p.user_id, COUNT(r.id) AS reports_on_posts_count
    FROM reports r
    JOIN posts p ON p.id = r.post_id
    WHERE p.user_id IS NOT NULL
    GROUP BY p.user_id
),
session_stats AS (
    SELECT user_id, MAX(created_at) AS last_session_at
    FROM sessions
    GROUP BY user_id
),
audit_stats AS (
    SELECT target, COUNT(*) AS moderation_action_count
    FROM admin_audit_log
    WHERE target LIKE 'user:%'
    GROUP BY target
)
SELECT
    u.id,
    u.username,
    u.display_name,
    u.is_admin,
    u.is_suspended,
    u.is_deleted,
    u.created_at,
    u.updated_at,
    session_stats.last_session_at,
    post_stats.last_post_at,
    COALESCE(post_stats.total_posts, 0),
    COALESCE(media_stats.uploaded_media_count, 0),
    COALESCE(report_stats.reports_on_posts_count, 0),
    COALESCE(audit_stats.moderation_action_count, 0),
    COALESCE(post_match_counts.matching_post_count, 0),
    post_match_previews.text,
    name_matches.id IS NOT NULL
FROM users u
LEFT JOIN name_matches ON name_matches.id = u.id
LEFT JOIN post_match_counts ON post_match_counts.user_id = u.id
LEFT JOIN post_match_previews ON post_match_previews.user_id = u.id
LEFT JOIN post_stats ON post_stats.user_id = u.id
LEFT JOIN media_stats ON media_stats.user_id = u.id
LEFT JOIN report_stats ON report_stats.user_id = u.id
LEFT JOIN session_stats ON session_stats.user_id = u.id
LEFT JOIN audit_stats ON audit_stats.target = ('user:' || u.id)
__FILTER_SQL__
ORDER BY
    CASE WHEN name_matches.id IS NOT NULL THEN 0 ELSE 1 END,
    COALESCE(post_match_counts.matching_post_count, 0) DESC,
    u.id DESC
LIMIT __LIMIT__
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminUserInvestigation {
    pub id: i64,
    pub username: String,
    pub display_name: String,
    pub is_admin: bool,
    pub is_suspended: bool,
    pub is_deleted: bool,
    pub created_at: String,
    pub updated_at: String,
    pub last_session_at: Option<String>,
    pub last_post_at: Option<String>,
    pub total_posts: i64,
    pub uploaded_media_count: i64,
    pub reports_on_posts_count: i64,
    pub moderation_action_count: i64,
    pub matching_post_count: i64,
    pub post_match_preview: Option<String>,
    pub matched_name: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminUserSearch {
    pub user_query: Option<String>,
    pub post_search: ParsedAdminPostSearch,
}

impl AdminUserSearch {
    #[must_use]
    pub fn new(user_query: &str, post_query: &str) -> Self {
        Self {
            user_query: normalize_admin_user_search(user_query),
            post_search: parse_admin_post_search(post_query),
        }
    }

    #[must_use]
    pub fn has_filter(&self) -> bool {
        self.user_query.is_some() || self.post_search.mode.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedAdminPostSearch {
    pub mode: Option<AdminPostSearchMode>,
    pub malformed_quotes: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminPostSearchMode {
    Keywords(Vec<String>),
    ExactPhrase(String),
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
    let hash = auth::hash_password_async(password.to_owned()).await?;
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

#[must_use]
pub fn normalize_admin_user_search(query: &str) -> Option<String> {
    let trimmed = query.trim().trim_start_matches('@').trim();
    (!trimmed.is_empty()).then(|| trimmed.to_ascii_lowercase())
}

#[must_use]
pub fn parse_admin_post_search(query: &str) -> ParsedAdminPostSearch {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return ParsedAdminPostSearch {
            mode: None,
            malformed_quotes: false,
        };
    }

    if let Some(quoted) = trimmed
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    {
        let phrase = quoted.trim();
        return ParsedAdminPostSearch {
            mode: (!phrase.is_empty()).then(|| AdminPostSearchMode::ExactPhrase(phrase.to_owned())),
            malformed_quotes: false,
        };
    }

    let malformed_quotes = trimmed.contains('"');
    let cleaned = trimmed.replace('"', " ");
    let terms = cleaned
        .split_whitespace()
        .take(ADMIN_POST_SEARCH_TERM_LIMIT)
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();

    ParsedAdminPostSearch {
        mode: (!terms.is_empty()).then_some(AdminPostSearchMode::Keywords(terms)),
        malformed_quotes,
    }
}

pub async fn users(
    pool: &SqlitePool,
    search: AdminUserSearch,
) -> anyhow::Result<Vec<AdminUserInvestigation>> {
    pool.call(move |conn| {
        let (sql, sql_params) = admin_users_sql(&search);
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params_from_iter(sql_params.iter()), admin_user_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}

fn admin_users_sql(search: &AdminUserSearch) -> (String, Vec<String>) {
    let mut sql_params = Vec::new();
    let name_match_sql = name_match_sql(search.user_query.as_deref(), &mut sql_params);
    let post_match_sql = search.post_search.mode.as_ref().map_or_else(
        || "0".to_owned(),
        |mode| post_match_condition(mode, &mut sql_params),
    );
    let filter_sql = if search.has_filter() {
        "WHERE name_matches.id IS NOT NULL OR post_match_counts.matching_post_count IS NOT NULL"
    } else {
        ""
    };
    let sql = ADMIN_USERS_SQL
        .replace("__NAME_MATCH_SQL__", name_match_sql)
        .replace("__POST_MATCH_SQL__", &post_match_sql)
        .replace("__FILTER_SQL__", filter_sql)
        .replace("__LIMIT__", &ADMIN_USERS_LIMIT.to_string());
    (sql, sql_params)
}

fn name_match_sql(user_query: Option<&str>, sql_params: &mut Vec<String>) -> &'static str {
    if let Some(user_query) = user_query {
        let like = format!("%{}%", escape_like(user_query));
        sql_params.extend([like.clone(), like.clone(), like]);
        r"
        SELECT id
        FROM users
        WHERE normalized_username LIKE ? ESCAPE '\'
           OR lower(username) LIKE ? ESCAPE '\'
           OR lower(display_name) LIKE ? ESCAPE '\'
        "
    } else {
        "SELECT id FROM users WHERE 0"
    }
}

fn admin_user_from_row(row: &Row<'_>) -> rusqlite::Result<AdminUserInvestigation> {
    Ok(AdminUserInvestigation {
        id: row.get(0)?,
        username: row.get(1)?,
        display_name: row.get(2)?,
        is_admin: row.get::<_, i64>(3)? != 0,
        is_suspended: row.get::<_, i64>(4)? != 0,
        is_deleted: row.get::<_, i64>(5)? != 0,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
        last_session_at: row.get(8)?,
        last_post_at: row.get(9)?,
        total_posts: row.get(10)?,
        uploaded_media_count: row.get(11)?,
        reports_on_posts_count: row.get(12)?,
        moderation_action_count: row.get(13)?,
        matching_post_count: row.get(14)?,
        post_match_preview: row.get(15)?,
        matched_name: row.get::<_, i64>(16)? != 0,
    })
}

fn post_match_condition(mode: &AdminPostSearchMode, sql_params: &mut Vec<String>) -> String {
    match mode {
        AdminPostSearchMode::ExactPhrase(phrase) => {
            sql_params.push(format!("%{}%", escape_like(&phrase.to_ascii_lowercase())));
            "lower(p.text) LIKE ? ESCAPE '\\'".to_owned()
        }
        AdminPostSearchMode::Keywords(terms) => {
            sql_params.extend(terms.iter().map(|term| format!("%{}%", escape_like(term))));
            terms
                .iter()
                .map(|_| "lower(p.text) LIKE ? ESCAPE '\\'")
                .collect::<Vec<_>>()
                .join(" AND ")
        }
    }
}

fn escape_like(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '%' | '_' | '\\' => {
                escaped.push('\\');
                escaped.push(character);
            }
            _ => escaped.push(character),
        }
    }
    escaped
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

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    async fn test_pool() -> (tempfile::TempDir, SqlitePool, Settings) {
        let temp = tempdir().expect("temp dir");
        let pool = crate::db::connect(&temp.path().join("test.sqlite3"))
            .await
            .expect("connect");
        crate::db::migrate(&pool).await.expect("migrate");
        (temp, pool, Settings::default())
    }

    async fn register(pool: &SqlitePool, settings: &Settings, username: &str) -> i64 {
        auth::register_user(pool, settings, username, "very secure password", false)
            .await
            .expect("register user")
    }

    async fn create_post(pool: &SqlitePool, settings: &Settings, user_id: i64, text: &str) -> i64 {
        crate::social::create_post(pool, settings, Some(user_id), text, None, &[])
            .await
            .expect("create post")
    }

    fn usernames(rows: &[AdminUserInvestigation]) -> Vec<&str> {
        rows.iter().map(|row| row.username.as_str()).collect()
    }

    fn form_from_settings(settings: &Settings) -> DeepSettingsForm {
        DeepSettingsForm::from_settings(settings)
    }

    #[test]
    fn admin_post_search_parses_keywords_quotes_and_malformed_quotes() {
        assert_eq!(
            parse_admin_post_search("rust admin"),
            ParsedAdminPostSearch {
                mode: Some(AdminPostSearchMode::Keywords(vec![
                    "rust".to_owned(),
                    "admin".to_owned()
                ])),
                malformed_quotes: false,
            }
        );
        assert_eq!(
            parse_admin_post_search(r#""hello world""#),
            ParsedAdminPostSearch {
                mode: Some(AdminPostSearchMode::ExactPhrase("hello world".to_owned())),
                malformed_quotes: false,
            }
        );
        assert_eq!(
            parse_admin_post_search(r#""hello world"#),
            ParsedAdminPostSearch {
                mode: Some(AdminPostSearchMode::Keywords(vec![
                    "hello".to_owned(),
                    "world".to_owned()
                ])),
                malformed_quotes: true,
            }
        );
    }

    #[tokio::test]
    async fn admin_username_search_matches_username_handle_and_display_name() {
        let (_temp, pool, settings) = test_pool().await;
        let alice = register(&pool, &settings, "alice").await;
        register(&pool, &settings, "bob").await;
        pool.call(move |conn| {
            conn.execute(
                "UPDATE users SET display_name = 'Ada Admin' WHERE id = ?",
                [alice],
            )?;
            Ok(())
        })
        .await
        .expect("display name");

        let by_handle = users(&pool, AdminUserSearch::new("@ali", ""))
            .await
            .expect("search by handle");
        let by_display = users(&pool, AdminUserSearch::new("ada", ""))
            .await
            .expect("search by display");

        assert_eq!(usernames(&by_handle), vec!["alice"]);
        assert!(by_handle[0].matched_name);
        assert_eq!(usernames(&by_display), vec!["alice"]);
    }

    #[tokio::test]
    async fn admin_plain_post_keyword_search_matches_accounts_with_all_terms() {
        let (_temp, pool, settings) = test_pool().await;
        let alice = register(&pool, &settings, "alice").await;
        let bob = register(&pool, &settings, "bob").await;
        create_post(&pool, &settings, alice, "Rust admin investigation notes").await;
        create_post(&pool, &settings, bob, "Rust release notes").await;

        let rows = users(&pool, AdminUserSearch::new("", "rust investigation"))
            .await
            .expect("post search");

        assert_eq!(usernames(&rows), vec!["alice"]);
        assert_eq!(rows[0].matching_post_count, 1);
        assert_eq!(
            rows[0].post_match_preview.as_deref(),
            Some("Rust admin investigation notes")
        );
    }

    #[tokio::test]
    async fn admin_quoted_post_search_matches_exact_substring_phrase() {
        let (_temp, pool, settings) = test_pool().await;
        let alice = register(&pool, &settings, "alice").await;
        let bob = register(&pool, &settings, "bob").await;
        create_post(&pool, &settings, alice, "hello world from alice").await;
        create_post(&pool, &settings, bob, "hello careful world from bob").await;

        let rows = users(&pool, AdminUserSearch::new("", r#""hello world""#))
            .await
            .expect("phrase search");

        assert_eq!(usernames(&rows), vec!["alice"]);
    }

    #[tokio::test]
    async fn admin_post_search_escapes_like_special_characters() {
        let (_temp, pool, settings) = test_pool().await;
        let alice = register(&pool, &settings, "alice").await;
        let bob = register(&pool, &settings, "bob").await;
        create_post(&pool, &settings, alice, "token 100%_safe").await;
        create_post(&pool, &settings, bob, "token 1000 safe").await;

        let rows = users(&pool, AdminUserSearch::new("", "100%_safe"))
            .await
            .expect("escaped search");

        assert_eq!(usernames(&rows), vec!["alice"]);
    }

    #[test]
    fn deep_settings_form_parsing_accepts_valid_values() {
        let settings = Settings::default();
        let mut form = form_from_settings(&settings);
        form.site_name = "Custom Site".to_owned();
        form.max_bio_len = "300".to_owned();
        form.allow_profile_pictures = "false".to_owned();
        form.nsfw_blur_enabled = "false".to_owned();

        let parsed = parse_deep_settings_form(&form, &settings).expect("valid form");

        assert_eq!(parsed.site_name, "Custom Site");
        assert_eq!(parsed.max_bio_len, 300);
        assert!(!parsed.allow_profile_pictures);
        assert!(!parsed.nsfw_blur_enabled);
    }

    #[test]
    fn deep_settings_form_parsing_rejects_invalid_numbers() {
        let settings = Settings::default();
        let mut form = form_from_settings(&settings);
        form.max_bio_len.clear();
        assert!(parse_deep_settings_form(&form, &settings).is_err());

        let mut form = form_from_settings(&settings);
        form.max_bio_len = "nope".to_owned();
        assert!(parse_deep_settings_form(&form, &settings).is_err());

        let mut form = form_from_settings(&settings);
        form.max_bio_len = "-1".to_owned();
        assert!(parse_deep_settings_form(&form, &settings).is_err());
    }

    #[test]
    fn deep_settings_form_parsing_rejects_invalid_boolean_values() {
        let settings = Settings::default();
        let mut form = form_from_settings(&settings);
        form.allow_likes = "yes".to_owned();

        let err = parse_deep_settings_form(&form, &settings).expect_err("invalid bool");

        assert!(
            err.to_string()
                .contains("Allow likes must be true or false")
        );
    }

    #[test]
    fn unchanged_deep_settings_form_has_no_diff() {
        let settings = Settings::default();
        let form = form_from_settings(&settings);
        let parsed = parse_deep_settings_form(&form, &settings).expect("valid form");

        assert_eq!(diff_deep_settings(&settings, &parsed).len(), 0);
    }

    #[test]
    fn changed_deep_settings_diff_uses_friendly_labels_and_units() {
        let settings = Settings::default();
        let mut form = form_from_settings(&settings);
        form.max_bio_len = "300".to_owned();
        form.allow_profile_pictures = "false".to_owned();
        form.nsfw_blur_enabled = "false".to_owned();
        let parsed = parse_deep_settings_form(&form, &settings).expect("valid form");

        let diff = diff_deep_settings(&settings, &parsed);

        assert_eq!(
            diff,
            vec![
                DeepSettingsChange {
                    label: "Maximum bio length",
                    old_value: "240 characters".to_owned(),
                    new_value: "300 characters".to_owned(),
                },
                DeepSettingsChange {
                    label: "Allow profile pictures",
                    old_value: "true".to_owned(),
                    new_value: "false".to_owned(),
                },
                DeepSettingsChange {
                    label: "Blur NSFW media",
                    old_value: "true".to_owned(),
                    new_value: "false".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn deep_settings_media_mb_values_convert_to_bytes() {
        let settings = Settings::default();
        let mut form = form_from_settings(&settings);
        form.max_image_size_mb = "8".to_owned();
        form.max_video_size_mb = "50".to_owned();
        let parsed = parse_deep_settings_form(&form, &settings).expect("valid form");
        let updated = parsed.apply_to(&settings);

        assert_eq!(updated.media.max_image_size, 8 * MIB);
        assert_eq!(updated.media.max_video_size, 50 * MIB);
    }

    #[test]
    fn deep_settings_accepts_operator_chosen_minimum_password_length() {
        let settings = Settings::default();
        let mut form = form_from_settings(&settings);
        form.min_password_length = "5".to_owned();

        let parsed = parse_deep_settings_form(&form, &settings).expect("valid form");
        let updated = parsed.apply_to(&settings);

        assert_eq!(updated.accounts.min_password_length, 5);
    }

    #[test]
    fn deep_settings_accepts_zero_post_edit_window_as_disabled() {
        let settings = Settings::default();
        let mut form = form_from_settings(&settings);
        form.post_edit_window_seconds = "0".to_owned();

        let parsed = parse_deep_settings_form(&form, &settings).expect("valid form");
        let updated = parsed.apply_to(&settings);

        assert_eq!(updated.posts.post_edit_window_seconds, 0);
    }

    #[test]
    fn deep_settings_writeback_preserves_unrelated_values_and_comments() {
        let temp = tempdir().expect("temp dir");
        let path = temp.path().join("settings.toml");
        crate::config::write_default_if_missing(&path).expect("default settings");
        let mut settings = Settings::load(&path).expect("load settings");
        settings.site.name = "Written Site".to_owned();
        settings.accounts.max_bio_len = 300;
        settings.media.nsfw_blur_enabled = false;
        settings.media.max_image_size = 8 * MIB;

        write_deep_settings(&path, &settings).expect("write settings");

        let raw = fs::read_to_string(&path).expect("settings raw");
        let parsed = Settings::load(&path).expect("reload settings");
        assert!(raw.contains("# RustPost settings"));
        assert_eq!(parsed.site.name, "Written Site");
        assert_eq!(parsed.accounts.max_bio_len, 300);
        assert!(!parsed.media.nsfw_blur_enabled);
        assert_eq!(parsed.media.max_image_size, 8 * MIB);
        assert_eq!(parsed.server.port, Settings::default().server.port);
        assert_eq!(
            parsed.media.ffmpeg_path,
            Settings::default().media.ffmpeg_path
        );
    }
    #[test]
    fn persistence_handles_multiline_values_and_preserves_unknown_settings() {
        let temp = tempdir().expect("temp");
        let path = temp.path().join("settings.toml");
        crate::config::write_default_if_missing(&path).expect("defaults");
        let raw = fs::read_to_string(&path).expect("read")
            .replace("name = \"RustPost\"", "name = \"\"\"RustPost\"\"\" # inline comment")
            .replace("allowed_image_mime_types = [\"image/jpeg\", \"image/png\", \"image/gif\", \"image/webp\"]", "allowed_image_mime_types = [\n  \"image/jpeg\", # keep comment\n  \"image/png\",\n]");
        let raw = format!("{raw}\n[custom]\nprivate_key = \"never-display-this\"\n");
        fs::write(&path, &raw).expect("fixture");
        let mut updated = Settings::load(&path).expect("load");
        updated.site.name = r#"Quotes " and \"#.to_owned();
        updated.media.allowed_image_mime_types = vec!["image/webp".to_owned()];
        write_deep_settings(&path, &updated).expect("persist");
        let reloaded = Settings::load(&path).expect("reload");
        assert_eq!(reloaded.site.name, updated.site.name);
        assert_eq!(
            reloaded.media.allowed_image_mime_types,
            updated.media.allowed_image_mime_types
        );
        let persisted = fs::read_to_string(&path).expect("read");
        assert!(persisted.contains("# inline comment"));
        assert!(persisted.contains("private_key = \"never-display-this\""));
    }

    #[test]
    fn persistence_adds_optional_defaulted_fields_and_keeps_unrelated_values() {
        let temp = tempdir().expect("temp");
        let path = temp.path().join("settings.toml");
        crate::config::write_default_if_missing(&path).expect("defaults");
        let raw = fs::read_to_string(&path).expect("read");
        let raw = raw
            .lines()
            .filter(|line| !line.starts_with("deletion_grace_period_days ="))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&path, &raw).expect("fixture");
        let mut updated = Settings::load(&path).expect("load");
        updated.accounts.deletion_grace_period_days = 7;
        updated.media.ffmpeg_path = "must-not-change".to_owned();
        write_deep_settings(&path, &updated).expect("persist");
        let reloaded = Settings::load(&path).expect("reload");
        assert_eq!(reloaded.accounts.deletion_grace_period_days, 7);
        assert_eq!(reloaded.media.ffmpeg_path, "ffmpeg");
    }

    #[test]
    fn invalid_config_never_changes_the_existing_file() {
        let temp = tempdir().expect("temp");
        let path = temp.path().join("settings.toml");
        crate::config::write_default_if_missing(&path).expect("defaults");
        let before = fs::read(&path).expect("read");
        let mut updated = Settings::default();
        updated.media.vp9_crf = 255;
        assert!(write_deep_settings(&path, &updated).is_err());
        assert_eq!(fs::read(&path).expect("read"), before);
    }

    #[test]
    fn atomic_write_failure_preserves_config_and_cleans_temporary_files() {
        let temp = tempdir().expect("temp");
        let path = temp.path().join("settings.toml");
        fs::create_dir(&path).expect("directory at destination");
        assert!(write_atomic(&path, b"invalid").is_err());
        assert!(path.is_dir());
        assert_eq!(fs::read_dir(temp.path()).expect("entries").count(), 1);
    }
    #[test]
    fn persistence_preserves_root_extensions_and_updates_inline_tables() {
        let temp = tempdir().expect("temp");
        let path = temp.path().join("settings.toml");
        crate::config::write_default_if_missing(&path).expect("defaults");
        let raw = fs::read_to_string(&path).expect("read");
        let raw = format!("extension = 42\nsite = {{ name = \"RustPost\" }}\n{raw}");
        let raw = raw.replace("[site]", "# site already defined").replacen(
            "name = \"RustPost\"\n",
            "",
            1,
        );
        fs::write(&path, raw).expect("fixture");
        let mut updated = Settings::load(&path).expect("load");
        updated.site.name = "Inline site".to_owned();
        write_deep_settings(&path, &updated).expect("save");
        assert_eq!(
            Settings::load(&path).expect("reload").site.name,
            "Inline site"
        );
        assert!(
            fs::read_to_string(&path)
                .expect("read")
                .contains("extension = 42")
        );
    }
}
