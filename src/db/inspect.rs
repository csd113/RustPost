//! Inspect for the db subsystem.

use super::*;

pub async fn schema_report(pool: &Db) -> anyhow::Result<SchemaReport> {
    pool.call(|conn| inspect_schema(conn)).await
}

pub fn schema_report_from_path(path: &Path) -> anyhow::Result<SchemaReport> {
    let conn = open_read_only_connection(path)?;
    inspect_schema(&conn)
}

pub fn adoptable_schema_summary_from_path(path: &Path) -> anyhow::Result<Option<String>> {
    let conn = open_read_only_connection(path)?;
    let state = migration_state(&conn)?;
    let Some(source) = state.source_format() else {
        return Ok(None);
    };

    // The current migration creates these objects without IF NOT EXISTS.
    if user_schema_objects(&conn)?
        .iter()
        .any(|object| schema::V5_ADDITIONS.contains(&object.as_str()))
        || !inspect_migration_candidate(&conn, &state)?.is_empty()
    {
        return Ok(None);
    }
    Ok(Some(format!(
        "{source} detected. Startup will attempt migration to RustPost {CURRENT_RELEASE}."
    )))
}

pub fn validate_schema(conn: &Connection) -> anyhow::Result<()> {
    let report = inspect_schema(conn)?;
    if report.is_compatible() {
        return Ok(());
    }
    anyhow::bail!(
        "database schema is incompatible with RustPost release baseline: {}",
        report.summary()
    );
}

pub fn validate_restorable_schema(conn: &Connection) -> anyhow::Result<()> {
    validate_or_normalize_restorable_schema(conn, false)
}

pub fn normalize_restorable_schema(conn: &Connection) -> anyhow::Result<()> {
    validate_or_normalize_restorable_schema(conn, true)
}

pub fn schema_version_from_connection(conn: &Connection) -> anyhow::Result<i64> {
    if !table_exists(conn, "schema_migrations")? {
        anyhow::bail!("database has no schema_migrations table");
    }
    migration_versions(conn)?
        .into_iter()
        .max()
        .ok_or_else(|| anyhow::anyhow!("database has no schema migration version"))
}

pub(super) fn open_read_only_connection(path: &Path) -> anyhow::Result<Connection> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("failed to open SQLite database {}", path.display()))
}

pub(super) fn validate_or_normalize_restorable_schema(
    conn: &Connection,
    normalize: bool,
) -> anyhow::Result<()> {
    let state = migration_state(conn)?;
    if !normalize
        && !matches!(state, MigrationState::AlphaLatest)
        && let Some(source) = state.source_format()
    {
        anyhow::bail!("{source} requires migration to RustPost {CURRENT_RELEASE}");
    }
    match state {
        MigrationState::Baseline => validate_schema(conn),
        MigrationState::ReleaseVersion4 => {
            migrate_release_v4_to_current(conn)?;
            validate_schema(conn)
        }
        MigrationState::ReleaseVersion3 => {
            migrate_release_v3_to_current(conn)?;
            validate_schema(conn)
        }
        MigrationState::ReleaseVersion2 => {
            migrate_release_v2_to_current(conn)?;
            validate_schema(conn)
        }
        MigrationState::ReleaseVersion1 => {
            migrate_release_v1_to_current(conn)?;
            validate_schema(conn)
        }
        MigrationState::AlphaLatest => {
            let issues = inspect_alpha_schema_objects(conn)?;
            if !issues.is_empty() {
                anyhow::bail!(
                    "pre-release database is not structurally compatible with the release baseline: {}. {}",
                    summarize_issues(&issues),
                    INCOMPATIBLE_DATABASE_HINT
                );
            }
            if normalize {
                reset_schema_migrations_to(conn, RELEASE_V1_SCHEMA_VERSION)?;
                migrate_release_v1_to_current(conn)?;
                validate_schema(conn)?;
            }
            Ok(())
        }
        MigrationState::Empty => {
            anyhow::bail!(
                "database does not contain RustPost schema metadata. {INCOMPATIBLE_DATABASE_HINT}"
            );
        }
        MigrationState::Incompatible(reason) => {
            anyhow::bail!("{reason}. {INCOMPATIBLE_DATABASE_HINT}");
        }
    }
}

pub(super) fn inspect_schema(conn: &Connection) -> anyhow::Result<SchemaReport> {
    let mut issues = Vec::new();
    let mut version = None;

    if table_exists(conn, "schema_migrations")? {
        let versions = migration_versions(conn)?;
        version = versions.iter().max().copied();
        if versions != [CURRENT_SCHEMA_VERSION] {
            issues.push(format!(
                "migration metadata does not match the RustPost {CURRENT_RELEASE} database format"
            ));
        }
    } else {
        issues.push("missing table schema_migrations".to_owned());
    }

    issues.extend(inspect_schema_objects(conn)?);
    Ok(SchemaReport { version, issues })
}

pub(super) fn inspect_schema_objects(conn: &Connection) -> anyhow::Result<Vec<String>> {
    let mut issues = Vec::new();

    for table in REQUIRED_TABLES {
        if !table_exists(conn, table)? {
            issues.push(format!("missing table {table}"));
        }
    }

    for table in REQUIRED_TABLES {
        if table_exists(conn, table)? {
            check_table_columns(conn, table, &mut issues)?;
        }
    }

    for index in REQUIRED_INDEXES {
        check_index(conn, index, &mut issues)?;
    }

    for trigger in REQUIRED_TRIGGERS {
        check_trigger(conn, trigger, &mut issues)?;
    }

    for object in user_schema_objects(conn)? {
        if !is_expected_schema_object(&object) {
            issues.push(format!("unexpected schema object {object}"));
        }
    }

    Ok(issues)
}

pub(super) fn inspect_alpha_schema_objects(conn: &Connection) -> anyhow::Result<Vec<String>> {
    inspect_migration_candidate(conn, &MigrationState::AlphaLatest)
}

fn inspect_migration_candidate(
    conn: &Connection,
    state: &MigrationState,
) -> anyhow::Result<Vec<String>> {
    let add_embeds = matches!(
        state,
        MigrationState::AlphaLatest | MigrationState::ReleaseVersion1
    );
    let add_release_indexes = add_embeds || matches!(state, MigrationState::ReleaseVersion2);
    let add_account_lifecycle =
        add_release_indexes || matches!(state, MigrationState::ReleaseVersion3);
    // Ignore only the objects that the remaining migrations create.
    Ok(inspect_schema_objects(conn)?
        .into_iter()
        .filter(|issue| {
            let new_object = issue
                .strip_prefix("missing table ")
                .or_else(|| issue.strip_prefix("missing index "))
                .is_some_and(|name| {
                    schema::V5_ADDITIONS.contains(&name)
                        || (add_embeds && schema::V2_ADDITIONS.contains(&name))
                        || (add_release_indexes && schema::V3_ADDITIONS.contains(&name))
                        || (add_account_lifecycle && schema::V4_ADDITIONS.contains(&name))
                });
            let new_user_column = add_account_lifecycle
                && (issue.starts_with("table users columns differ")
                    || issue
                        .strip_prefix("missing column users.")
                        .is_some_and(|name| {
                            USER_COLUMNS_ADDED_IN_V4
                                .iter()
                                .any(|(column, _)| name == *column)
                        }));
            !new_object && !new_user_column
        })
        .collect())
}

pub(super) fn check_table_columns(
    conn: &Connection,
    table: &str,
    issues: &mut Vec<String>,
) -> anyhow::Result<()> {
    let columns = table_columns(conn, table)?;
    let expected = required_columns_for(table);
    let actual_names = columns
        .iter()
        .map(|column| column.name.as_str())
        .collect::<Vec<_>>();
    let expected_names = expected
        .iter()
        .map(|column| column.name)
        .collect::<Vec<_>>();
    if actual_names != expected_names {
        issues.push(format!(
            "table {table} columns differ; expected {}, found {}",
            expected_names.join(", "),
            actual_names.join(", ")
        ));
    }

    for expected_column in expected {
        let Some(actual) = columns
            .iter()
            .find(|column| column.name == expected_column.name)
        else {
            issues.push(format!(
                "missing column {}.{}",
                expected_column.table, expected_column.name
            ));
            continue;
        };
        if !actual
            .type_name
            .eq_ignore_ascii_case(expected_column.type_name)
        {
            issues.push(format!(
                "column {}.{} has type {}, expected {}",
                expected_column.table,
                expected_column.name,
                actual.type_name,
                expected_column.type_name
            ));
        }
        if actual.not_null != expected_column.not_null {
            issues.push(format!(
                "column {}.{} nullability differs",
                expected_column.table, expected_column.name
            ));
        }
        if actual.default_value.as_deref() != expected_column.default_value {
            issues.push(format!(
                "column {}.{} default differs",
                expected_column.table, expected_column.name
            ));
        }
        if actual.primary_key_position != expected_column.primary_key_position {
            issues.push(format!(
                "column {}.{} primary key position differs",
                expected_column.table, expected_column.name
            ));
        }
    }
    Ok(())
}

pub(super) fn check_index(
    conn: &Connection,
    expected: &RequiredIndex,
    issues: &mut Vec<String>,
) -> anyhow::Result<()> {
    if !table_exists(conn, expected.table)? {
        return Ok(());
    }
    let indexes = table_indexes(conn, expected.table)?;
    let Some((_name, unique)) = indexes.iter().find(|(name, _unique)| name == expected.name) else {
        issues.push(format!("missing index {}", expected.name));
        return Ok(());
    };
    if *unique != expected.unique {
        issues.push(format!("index {} uniqueness differs", expected.name));
    }
    Ok(())
}

pub(super) fn check_trigger(
    conn: &Connection,
    expected: &RequiredTrigger,
    issues: &mut Vec<String>,
) -> anyhow::Result<()> {
    let trigger_table: Option<String> = conn
        .query_row(
            "SELECT tbl_name FROM sqlite_master WHERE type = 'trigger' AND name = ?",
            [expected.name],
            |row| row.get(0),
        )
        .optional()?;
    match trigger_table {
        Some(table) if table == expected.table => {}
        Some(_) => issues.push(format!("trigger {} table differs", expected.name)),
        None => issues.push(format!("missing trigger {}", expected.name)),
    }
    Ok(())
}

pub(super) fn table_exists(conn: &Connection, table: &str) -> anyhow::Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?",
        [table],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

pub(super) fn migration_versions(conn: &Connection) -> anyhow::Result<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT version FROM schema_migrations ORDER BY version")?;
    let versions = stmt
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<i64>>>()?;
    Ok(versions)
}

pub(super) fn user_schema_objects(conn: &Connection) -> anyhow::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT name
        FROM sqlite_master
        WHERE name NOT LIKE 'sqlite_%'
        ORDER BY name
        "#,
    )?;
    let objects = stmt
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    Ok(objects)
}

pub(super) fn table_columns(conn: &Connection, table: &str) -> anyhow::Result<Vec<ColumnInfo>> {
    let sql = format!("PRAGMA table_info({})", quoted_identifier(table));
    let mut stmt = conn.prepare(&sql)?;
    let columns = stmt
        .query_map([], |row| {
            Ok(ColumnInfo {
                name: row.get(1)?,
                type_name: row.get(2)?,
                not_null: row.get::<_, i64>(3)? != 0,
                default_value: row.get(4)?,
                primary_key_position: row.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(columns)
}

pub(super) fn table_indexes(conn: &Connection, table: &str) -> anyhow::Result<Vec<(String, bool)>> {
    let sql = format!("PRAGMA index_list({})", quoted_identifier(table));
    let mut stmt = conn.prepare(&sql)?;
    let indexes = stmt
        .query_map([], |row| Ok((row.get(1)?, row.get::<_, i64>(2)? != 0)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(indexes)
}

pub(super) fn quoted_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

pub(super) fn required_columns_for(table: &str) -> Vec<RequiredColumn> {
    REQUIRED_COLUMNS
        .iter()
        .copied()
        .filter(|column| column.table == table)
        .collect()
}

pub(super) fn is_expected_schema_object(name: &str) -> bool {
    REQUIRED_TABLES.contains(&name)
        || REQUIRED_INDEXES.iter().any(|index| index.name == name)
        || REQUIRED_TRIGGERS.iter().any(|trigger| trigger.name == name)
        || FTS_SHADOW_TABLES.contains(&name)
}

pub(super) fn summarize_issues(issues: &[String]) -> String {
    const MAX_ISSUES: usize = 6;
    let mut summary = issues
        .iter()
        .take(MAX_ISSUES)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("; ");
    let remaining = issues.len().saturating_sub(MAX_ISSUES);
    if remaining > 0 {
        summary.push_str(&format!("; and {remaining} more issue(s)"));
    }
    summary
}
