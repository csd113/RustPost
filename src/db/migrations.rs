//! Migrations for the db subsystem.

use super::*;

pub async fn migrate(pool: &Db) -> anyhow::Result<()> {
    migrate_from(pool).await.map(|_| ())
}

pub async fn migrate_from(pool: &Db) -> anyhow::Result<Option<&'static str>> {
    pool.call(|conn| {
        let tx = conn.transaction()?;
        let state = migration_state(&tx)?;
        let source = state.source_format();
        ensure_release_baseline(&tx, state)?;
        tx.commit()?;
        Ok(source)
    })
    .await
}

fn ensure_release_baseline(conn: &Connection, state: MigrationState) -> anyhow::Result<()> {
    match state {
        MigrationState::Empty => initialize_release_baseline(conn)?,
        MigrationState::Baseline => {}
        MigrationState::ReleaseVersion4 => migrate_release_v4_to_current(conn)?,
        MigrationState::ReleaseVersion3 => migrate_release_v3_to_current(conn)?,
        MigrationState::ReleaseVersion2 => migrate_release_v2_to_current(conn)?,
        MigrationState::ReleaseVersion1 => migrate_release_v1_to_current(conn)?,
        MigrationState::AlphaLatest => adopt_latest_alpha_schema(conn)?,
        MigrationState::Incompatible(reason) => {
            anyhow::bail!("{reason}. {INCOMPATIBLE_DATABASE_HINT}");
        }
    }

    validate_schema(conn).with_context(|| INCOMPATIBLE_DATABASE_HINT)
}

pub(super) fn migration_state(conn: &Connection) -> anyhow::Result<MigrationState> {
    let schema_objects = user_schema_objects(conn)?;
    if !table_exists(conn, "schema_migrations")? {
        if schema_objects.is_empty() {
            return Ok(MigrationState::Empty);
        }
        return Ok(MigrationState::Incompatible(format!(
            "database has schema objects but no migration metadata: {}",
            summarize_issues(&schema_objects)
        )));
    }

    let versions = migration_versions(conn)?;
    if versions.is_empty() {
        if schema_objects
            .iter()
            .all(|name| name == "schema_migrations")
        {
            return Ok(MigrationState::Empty);
        }
        return Ok(MigrationState::Incompatible(
            "database has schema objects but no migration version".to_owned(),
        ));
    }

    let Some(latest) = versions.last().copied() else {
        return Ok(MigrationState::Incompatible(
            "database has no migration version".to_owned(),
        ));
    };
    if versions == [CURRENT_SCHEMA_VERSION] {
        return Ok(MigrationState::Baseline);
    }
    if versions == [RELEASE_V4_SCHEMA_VERSION] {
        return Ok(MigrationState::ReleaseVersion4);
    }
    if versions == [RELEASE_V3_SCHEMA_VERSION] {
        return Ok(MigrationState::ReleaseVersion3);
    }
    if versions == [RELEASE_V2_SCHEMA_VERSION] {
        return Ok(MigrationState::ReleaseVersion2);
    }
    if versions == [RELEASE_V1_SCHEMA_VERSION] {
        return Ok(MigrationState::ReleaseVersion1);
    }
    if latest == OLD_ALPHA_SCHEMA_VERSION {
        return Ok(MigrationState::AlphaLatest);
    }
    Ok(MigrationState::Incompatible(
        "database migration metadata is not compatible with this RustPost release baseline"
            .to_owned(),
    ))
}

pub(super) fn initialize_release_baseline(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        r#"
DROP TABLE IF EXISTS schema_migrations;
"#,
    )?;
    conn.execute_batch(BASELINE_SCHEMA)?;
    Ok(())
}

pub(super) fn adopt_latest_alpha_schema(conn: &Connection) -> anyhow::Result<()> {
    let issues = inspect_alpha_schema_objects(conn)?;
    if !issues.is_empty() {
        anyhow::bail!(
            "pre-release database is not safe to adopt as release baseline: {}. {}",
            summarize_issues(&issues),
            INCOMPATIBLE_DATABASE_HINT
        );
    }
    reset_schema_migrations_to(conn, RELEASE_V1_SCHEMA_VERSION)?;
    migrate_release_v1_to_current(conn)
}

pub(super) fn reset_schema_migrations(conn: &Connection) -> anyhow::Result<()> {
    reset_schema_migrations_to(conn, CURRENT_SCHEMA_VERSION)
}

pub(super) fn reset_schema_migrations_to(conn: &Connection, version: i64) -> anyhow::Result<()> {
    conn.execute_batch(
        r#"
DROP TABLE IF EXISTS schema_migrations;
CREATE TABLE schema_migrations (
    version INTEGER PRIMARY KEY,
    applied_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
"#,
    )?;
    conn.execute(
        "INSERT INTO schema_migrations (version) VALUES (?)",
        [version],
    )?;
    Ok(())
}

pub(super) fn migrate_release_v1_to_current(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(YOUTUBE_EMBEDS_SCHEMA)?;
    reset_schema_migrations_to(conn, RELEASE_V2_SCHEMA_VERSION)?;
    migrate_release_v2_to_current(conn)
}

/// Adds the release 1.0.x indexes used by thread assembly, media ownership,
/// session revocation, media reference checks, and rate-limit pruning.
pub(super) fn migrate_release_v2_to_current(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(RELEASE_V3_INDEXES)?;
    reset_schema_migrations_to(conn, RELEASE_V3_SCHEMA_VERSION)?;
    migrate_release_v3_to_current(conn)
}

/// Adds the account lifecycle columns and tables used by protected accounts,
/// forced password resets, deletion grace periods, username history, instance
/// settings, and account imports.
pub(super) fn migrate_release_v3_to_current(conn: &Connection) -> anyhow::Result<()> {
    for (column, definition) in USER_COLUMNS_ADDED_IN_V4 {
        add_column_if_missing(conn, "users", column, definition)?;
    }
    conn.execute_batch(RELEASE_V4_SCHEMA)?;
    reset_schema_migrations_to(conn, RELEASE_V4_SCHEMA_VERSION)?;
    migrate_release_v4_to_current(conn)
}

/// `ALTER TABLE ... ADD COLUMN` is not idempotent, so the migration checks the
/// current table shape first. `PRAGMA table_info` output is safe to inspect
/// because the table names here are compile-time constants.
pub(super) fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> anyhow::Result<()> {
    let exists = table_columns(conn, table)?
        .iter()
        .any(|existing| existing.name == column);
    if exists {
        return Ok(());
    }
    conn.execute_batch(&format!(
        "ALTER TABLE {} ADD COLUMN {} {};",
        quoted_identifier(table),
        quoted_identifier(column),
        definition
    ))?;
    Ok(())
}

/// Adds durable drafts, polls and background media work without rewriting existing content.
pub(super) fn migrate_release_v4_to_current(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(RELEASE_V5_SCHEMA)?;
    reset_schema_migrations(conn)
}
