//! Narrow regressions for externally observable audit failures.
use std::io::Write as _;
use std::process::{Command, Stdio};

use rustpost::{config, runtime::RuntimePaths, terminal};

#[test]
fn terminal_text_is_bounded_and_controls_remain_visible() {
    let input = "path\x1b]0;title\x07\r\n\t\u{0085}\u{202e}終😀e\u{301}";
    let output = terminal::display_text(input);
    assert!(!output.chars().any(char::is_control));
    assert!(output.contains("<U+001B>"));
    assert!(output.contains("<U+202E>"));
    assert!(output.contains("終😀e\u{301}"));
    assert!(terminal::display_text(&"😀".repeat(1_000_000)).ends_with(" [truncated]"));
}

#[test]
fn suggestions_quote_instance_paths_and_reject_controls() {
    let mut paths =
        RuntimePaths::from_data_dir("/tmp/path with 'quotes' $dollars `ticks`;終".into());
    paths.settings_path = "/tmp/custom settings.toml".into();
    let command = terminal::instance_command(&paths, "check");
    assert!(command.contains("--config"));
    assert!(command.contains("custom settings.toml"));
    #[cfg(unix)]
    assert!(command.contains("'\"'\"'"));
    paths.data_dir = "/tmp/unsafe\x1bpath".into();
    assert!(terminal::instance_command(&paths, "check").starts_with("[UNAVAILABLE]"));
}

fn cli(path: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rustpost-cli"));
    command.arg("--data-dir").arg(path);
    command.env("TERM", "dumb").env_remove("NO_COLOR");
    command
}
fn fixture() -> anyhow::Result<(tempfile::TempDir, RuntimePaths)> {
    let dir = tempfile::tempdir()?;
    let paths = RuntimePaths::from_data_dir(dir.path().join("data with spaces"));
    assert!(cli(&paths.data_dir).arg("init").output()?.status.success());
    let mut settings = config::Settings::load(&paths.settings_path)?;
    "rustpost-deliberately-missing-ffmpeg".clone_into(&mut settings.media.ffmpeg_path);
    std::fs::write(&paths.settings_path, toml::to_string(&settings)?)?;
    Ok((dir, paths))
}

#[test]
fn corrupt_check_is_failure_and_missing_database_is_read_only() -> anyhow::Result<()> {
    let (_dir, paths) = fixture()?;
    let fresh = cli(&paths.data_dir).arg("check").output()?;
    assert!(fresh.status.success());
    assert!(!paths.database_path.exists());
    std::fs::write(&paths.database_path, b"not sqlite")?;
    let before = std::fs::read(&paths.database_path)?;
    let corrupt = cli(&paths.data_dir).arg("check").output()?;
    assert!(!corrupt.status.success());
    assert_eq!(std::fs::read(&paths.database_path)?, before);
    assert!(String::from_utf8_lossy(&corrupt.stdout).contains("unreadable"));
    assert!(!corrupt.stdout.contains(&0x1b));
    assert!(!corrupt.stderr.contains(&0x1b));
    Ok(())
}

#[test]
fn check_and_migration_identify_the_shared_release_format() -> anyhow::Result<()> {
    let (_dir, paths) = fixture()?;
    let open = || rusqlite::Connection::open(&paths.database_path);
    open()?.execute_batch(include_str!("fixtures/schema-v4.sql"))?;
    let before = std::fs::read(&paths.database_path)?;
    let release = concat!("v", env!("CARGO_PKG_VERSION"));

    let pending = cli(&paths.data_dir).arg("check").output()?;
    let summary = String::from_utf8_lossy(&pending.stdout);
    assert!(pending.status.success(), "{summary}");
    assert!(summary.contains("[PENDING] RustPost v1.0.0/v1.1.0 database format"));
    assert!(summary.contains(&format!("migration to RustPost {release}")));
    assert_eq!(std::fs::read(&paths.database_path)?, before);

    let migrated = cli(&paths.data_dir)
        .args(["create-admin", "auditadmin", "fixture-only-password"])
        .output()?;
    let diagnostics = String::from_utf8_lossy(&migrated.stderr);
    assert!(migrated.status.success(), "{diagnostics}");
    assert!(diagnostics.contains(&format!(
        "Migrated database from RustPost v1.0.0/v1.1.0 database format to RustPost {release}."
    )));

    let current = cli(&paths.data_dir).arg("check").output()?;
    assert!(current.status.success());
    assert!(
        String::from_utf8_lossy(&current.stdout)
            .contains(&format!("[OK] compatible with RustPost {release}"))
    );
    assert_eq!(
        rustpost::db::schema_version_from_connection(&open()?)?,
        rustpost::db::CURRENT_SCHEMA_VERSION
    );

    // An older ledger with newer tables is inconsistent, not a pending migration.
    open()?.execute("UPDATE schema_migrations SET version = 4", [])?;
    let inconsistent = cli(&paths.data_dir).arg("check").output()?;
    assert!(!inconsistent.status.success());
    Ok(())
}

#[test]
fn piped_secret_creates_admin_without_echo_or_argv_secret() -> anyhow::Result<()> {
    let (_dir, paths) = fixture()?;
    let secret = "audit-only-secret-382717";
    let mut child = cli(&paths.data_dir)
        .args(["create-admin", "auditadmin", "--password-stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        writeln!(stdin, "{secret}")?;
    }
    let output = child.wait_with_output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(secret));
    assert!(cli(&paths.data_dir).arg("check").output()?.status.success());
    Ok(())
}

#[test]
fn redirected_interactive_setup_fails_cleanly_without_controls() -> anyhow::Result<()> {
    let (_dir, paths) = fixture()?;
    let output = cli(&paths.data_dir)
        .arg("create-admin-interactive")
        .stdin(Stdio::null())
        .output()?;
    assert!(!output.status.success());
    assert!(!output.stdout.contains(&0x1b));
    assert!(!output.stderr.contains(&0x1b));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--password-stdin"));
    Ok(())
}

#[cfg(unix)]
#[test]
fn exact_suggestion_executes_with_custom_config_and_shell_metacharacters() -> anyhow::Result<()> {
    let (_dir, mut paths) = fixture()?;
    let original_settings = std::fs::read(&paths.settings_path)?;
    paths.settings_path = paths
        .data_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("fixture parent"))?
        .join("custom ' $ ` ; 終.toml");
    std::fs::write(&paths.settings_path, original_settings)?;
    let command = terminal::instance_command(&paths, "check");
    // current_exe() in this test is the test harness, so replace only the
    // executable's separately quoted argument with the actual CLI binary.
    let harness = std::env::current_exe()?;
    let harness = format!(
        "'{}'",
        harness.display().to_string().replace('\'', "'\"'\"'")
    );
    let executable = format!(
        "'{}'",
        env!("CARGO_BIN_EXE_rustpost-cli").replace('\'', "'\"'\"'")
    );
    let command = command.replacen(&harness, &executable, 1);
    let output = Command::new("/bin/sh").args(["-c", &command]).output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.stdout.contains(&0x1b));
    Ok(())
}

#[cfg(windows)]
#[test]
fn powershell_suggestion_executes_with_typographic_quotes() -> anyhow::Result<()> {
    use base64::Engine as _;

    let (dir, fixture_paths) = fixture()?;
    let paths = RuntimePaths::from_data_dir(dir.path().join("data ’ ‘ ‚ ‛ ' $ ` ; 終"));
    std::fs::rename(&fixture_paths.data_dir, &paths.data_dir)?;
    let command = terminal::instance_command(&paths, "check");
    // Keep the exact suggested arguments and swap the harness executable for the CLI.
    let (_, arguments) = command
        .split_once(" --data-dir ")
        .ok_or_else(|| anyhow::anyhow!("missing PowerShell suggestion: {command}"))?;
    let exe = env!("CARGO_BIN_EXE_rustpost-cli");
    let command = format!("& $env:RUSTPOST_TEST_EXE --data-dir {arguments}; exit $LASTEXITCODE");
    let encoded = base64::engine::general_purpose::STANDARD.encode(
        command
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>(),
    );
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-EncodedCommand", &encoded])
        .env("RUSTPOST_TEST_EXE", exe)
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.stdout.contains(&0x1b));
    Ok(())
}

#[test]
fn closed_result_pipe_exits_cleanly() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let mut child = cli(directory.path())
        .arg("init")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    drop(child.stdout.take());
    let output = child.wait_with_output()?;
    assert!(output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("command failed"));
    Ok(())
}
