use std::io::{self, IsTerminal as _, Write as _};
use std::net::SocketAddr;
use std::path::PathBuf;
#[cfg(unix)]
use std::process::Command as ProcessCommand;

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use tokio::sync::watch;
use tracing::{info, warn};

use crate::{admin, backup, config, db, demo_seed, logging, runtime, server, terminal, tor};

#[derive(Debug, Parser)]
#[command(
    about = "Single-binary self-hosted microblog",
    after_help = "Common first run:\n  rustpost-cli init\n  rustpost-cli create-admin-interactive\n  rustpost-cli serve"
)]
struct Cli {
    /// Path to settings.toml. Defaults to <data-dir>/settings.toml.
    #[arg(long, help = "Path to settings.toml")]
    config: Option<PathBuf>,

    /// Runtime data directory. Defaults to rustpost-data beside the binary.
    #[arg(long, help = "Runtime data directory")]
    data_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create the data directory and default settings file.
    #[command(about = "Create the data directory and default settings file")]
    Init,

    /// Validate settings and report optional service readiness.
    #[command(about = "Validate settings and optional service readiness")]
    Check,

    /// Create an administrator account with arguments from the command line.
    #[command(about = "Create an administrator account")]
    CreateAdmin {
        #[arg(help = "Administrator username")]
        username: String,
        #[arg(help = "Administrator password")]
        password: String,
    },

    /// Create an administrator account with hidden password prompts.
    #[command(about = "Create an administrator account with hidden password prompts")]
    CreateAdminInteractive,

    /// Reset an administrator password.
    #[command(about = "Reset an administrator password")]
    ResetAdminPassword {
        #[arg(help = "Administrator username")]
        username: String,
        #[arg(help = "New administrator password")]
        password: String,
    },

    /// Seed demo data into an explicit target/debug data directory.
    #[command(about = "Seed demo data into an explicit data directory")]
    SeedDemo,

    /// Write a tar backup under the runtime backup directory.
    #[command(about = "Write a tar backup under the backup directory")]
    Backup {
        #[arg(long, help = "Include Tor onion service keys in the backup")]
        include_tor_keys: bool,
    },

    /// Restore a backup tar archive into the runtime data directory.
    #[command(about = "Restore a backup tar archive into the data directory")]
    Restore {
        #[arg(help = "Backup archive to restore")]
        archive: PathBuf,
        #[arg(long, help = "Allow restoring Tor onion service keys")]
        include_tor_keys: bool,
    },

    /// Print only the configured onion address when one is available.
    #[command(about = "Print the onion address when one is available")]
    PrintOnionAddress,

    /// Start the `RustPost` web server.
    #[command(about = "Start the RustPost web server")]
    Serve,
}

pub async fn run() -> anyhow::Result<()> {
    logging::init();
    let cli = Cli::parse();
    let explicit_data_dir = cli.data_dir.clone();
    let mut paths = runtime::RuntimePaths::discover(cli.data_dir.as_deref())?;
    paths.ensure()?;
    let settings_path = cli.config.unwrap_or_else(|| paths.settings_path.clone());
    config::write_default_if_missing(&settings_path)?;
    let settings = config::Settings::load(&settings_path)?;
    settings.validate()?;
    paths = paths.with_tor_data_dir(&settings.tor.data_dir);
    paths.ensure()?;
    info!(data_dir = %paths.data_dir.display(), settings = %settings_path.display(), "runtime paths ready");

    run_command(
        cli.command.unwrap_or(Command::Serve),
        paths,
        settings_path,
        settings,
        explicit_data_dir.is_some(),
    )
    .await
}

async fn run_command(
    command: Command,
    paths: runtime::RuntimePaths,
    settings_path: PathBuf,
    settings: config::Settings,
    has_explicit_data_dir: bool,
) -> anyhow::Result<()> {
    let database = AdminDatabase {
        paths: &paths,
        settings: &settings,
    };
    match command {
        Command::Init => {
            stdout_raw(format_args!(
                "{}",
                terminal::render_init(&paths, &settings_path)
            ))?;
            Ok(())
        }
        Command::Check => {
            let ffmpeg = crate::ffmpeg::probe(&settings.media).await;
            let tor_status = tor::validate_startup(&settings.tor);
            stdout_raw(format_args!(
                "{}",
                terminal::render_check(&settings, &ffmpeg, &tor_status)
            ))?;
            Ok(())
        }
        Command::CreateAdmin { username, password } => {
            create_admin_command(&database, username, password).await
        }
        Command::CreateAdminInteractive => {
            let (username, password) = prompt_admin_credentials()?;
            create_admin_command(&database, username, password).await
        }
        Command::ResetAdminPassword { username, password } => {
            reset_admin_password_command(&database, username, password).await
        }
        Command::SeedDemo => {
            if !has_explicit_data_dir {
                anyhow::bail!("seed-demo requires an explicit --data-dir under target/debug");
            }
            let pool = db::connect(&paths.database_path).await?;
            db::migrate(&pool).await?;
            let report = demo_seed::seed(&pool, &paths, &settings, &settings_path).await?;
            stdout_line(format_args!("{report}"))?;
            Ok(())
        }
        Command::Backup { include_tor_keys } => backup_command(&paths, include_tor_keys),
        Command::Restore {
            archive,
            include_tor_keys,
        } => restore_command(&paths, &archive, include_tor_keys),
        Command::PrintOnionAddress => {
            let status = tor::validate_startup(&settings.tor);
            stdout_line(format_args!(
                "{}",
                status.onion_address().unwrap_or_default()
            ))?;
            Ok(())
        }
        Command::Serve => serve(paths, settings_path, settings).await,
    }
}

struct AdminDatabase<'a> {
    paths: &'a runtime::RuntimePaths,
    settings: &'a config::Settings,
}

async fn create_admin_command(
    database: &AdminDatabase<'_>,
    username: String,
    password: String,
) -> anyhow::Result<()> {
    let pool = db::connect(&database.paths.database_path).await?;
    db::migrate(&pool).await?;
    admin::create_admin(&pool, database.settings, &username, &password).await?;
    stdout_raw(format_args!(
        "{}",
        terminal::render_command_success(
            "RustPost admin created",
            &[
                terminal::row("Username", username),
                terminal::row(
                    "Next command",
                    format!(
                        "rustpost-cli --data-dir {} serve",
                        database.paths.data_dir.display()
                    ),
                ),
            ],
        )
    ))?;
    Ok(())
}

async fn reset_admin_password_command(
    database: &AdminDatabase<'_>,
    username: String,
    password: String,
) -> anyhow::Result<()> {
    let pool = db::connect(&database.paths.database_path).await?;
    db::migrate(&pool).await?;
    admin::reset_admin_password(&pool, database.settings, &username, &password).await?;
    stdout_raw(format_args!(
        "{}",
        terminal::render_command_success(
            "RustPost admin password reset",
            &[
                terminal::row("Username", username),
                terminal::row("Next command", "sign in with the new password"),
            ],
        )
    ))?;
    Ok(())
}

fn backup_command(paths: &runtime::RuntimePaths, include_tor_keys: bool) -> anyhow::Result<()> {
    let archive = backup::create_backup(paths, include_tor_keys)?;
    stdout_raw(format_args!(
        "{}",
        terminal::render_command_success(
            "RustPost backup created",
            &[
                terminal::row("Archive", archive.display().to_string()),
                terminal::row(
                    "Tor keys",
                    if include_tor_keys {
                        "included"
                    } else {
                        "excluded"
                    },
                ),
            ],
        )
    ))?;
    Ok(())
}

fn restore_command(
    paths: &runtime::RuntimePaths,
    archive: &std::path::Path,
    include_tor_keys: bool,
) -> anyhow::Result<()> {
    backup::restore_backup(paths, archive, include_tor_keys)?;
    stdout_raw(format_args!(
        "{}",
        terminal::render_command_success(
            "RustPost restore completed",
            &[
                terminal::row("Archive", archive.display().to_string()),
                terminal::row("Data directory", paths.data_dir.display().to_string()),
            ],
        )
    ))?;
    Ok(())
}

async fn serve(
    paths: runtime::RuntimePaths,
    settings_path: PathBuf,
    settings: config::Settings,
) -> anyhow::Result<()> {
    let pool = db::connect(&paths.database_path).await?;
    db::migrate(&pool).await?;
    let admin_count = ensure_first_admin_interactive(&pool, &settings, &paths).await?;
    let ffmpeg = crate::ffmpeg::probe(&settings.media).await;
    if !ffmpeg.available {
        warn!("ffmpeg unavailable; uploads will safely fall back to allowed originals");
    }
    let onion_listener = bind_onion_listener(&settings.tor).await?;
    let onion_target = onion_listener_target(onion_listener.as_ref());
    let tor_status = initial_tor_status(&settings.tor, &paths, onion_target).await?;
    let tor_status_for_background = tor_status.clone();
    print_startup_dashboard(
        &pool,
        StartupPrintContext {
            paths: &paths,
            settings_path: &settings_path,
            settings: &settings,
            admin_count,
            ffmpeg: &ffmpeg,
            tor_status: &tor_status_for_background,
            onion_target,
        },
    )
    .await?;
    let state = server::AppState::new(pool, settings.clone(), paths.clone(), ffmpeg, tor_status);
    let app = server::router(state);
    let shutdown_rx = shutdown_receiver();

    if settings.tor.tor_only {
        return serve_tor_only(onion_listener, app, shutdown_rx).await;
    }

    if let Some(listener) = onion_listener {
        spawn_onion_forwarding(
            listener,
            app.clone(),
            paths.clone(),
            settings.tor.clone(),
            tor_status_for_background.clone(),
            onion_target,
            shutdown_rx.clone(),
        );
    }

    serve_clearnet(app, &settings.server, shutdown_rx).await
}

async fn ensure_first_admin_interactive(
    pool: &db::SqlitePool,
    settings: &config::Settings,
    paths: &runtime::RuntimePaths,
) -> anyhow::Result<i64> {
    let count = admin::admin_count(pool).await?;
    if count > 0 || !settings.admin.create_admin_on_first_boot {
        return Ok(count);
    }
    if !stdin_is_interactive() {
        stderr_raw(format_args!(
            "{}",
            terminal::render_first_admin_non_interactive(paths)
        ));
        admin::ensure_first_boot_admin_hint(pool).await?;
        return Ok(count);
    }
    stderr_raw(format_args!(
        "{}",
        terminal::render_first_admin_setup(paths)
    ));
    let credentials = prompt_first_admin_credentials(settings)?;
    admin::create_admin_with_display_name(
        pool,
        settings,
        &credentials.username,
        &credentials.password,
        credentials.display_name.as_deref(),
    )
    .await?;
    stdout_raw(format_args!(
        "{}",
        terminal::render_command_success(
            "RustPost admin created",
            &[
                terminal::row("Username", credentials.username),
                terminal::row(
                    "Display name",
                    credentials
                        .display_name
                        .filter(|name| !name.is_empty())
                        .unwrap_or_else(|| "(same as username)".to_owned()),
                ),
                terminal::row("Next step", "starting RustPost"),
            ],
        )
    ))?;
    admin::admin_count(pool).await
}

fn stdin_is_interactive() -> bool {
    io::stdin().is_terminal()
}

async fn bind_onion_listener(
    settings: &config::TorSettings,
) -> anyhow::Result<Option<tokio::net::TcpListener>> {
    if !settings.enabled {
        return Ok(None);
    }
    Ok(Some(tokio::net::TcpListener::bind("127.0.0.1:0").await?))
}

fn onion_listener_target(listener: Option<&tokio::net::TcpListener>) -> Option<SocketAddr> {
    let listener = listener?;
    listener.local_addr().ok()
}

async fn initial_tor_status(
    settings: &config::TorSettings,
    paths: &runtime::RuntimePaths,
    onion_target: Option<SocketAddr>,
) -> anyhow::Result<tor::TorStatus> {
    if settings.tor_only {
        return tor::start(settings, paths, onion_target).await;
    }
    if settings.enabled {
        return Ok(tor::TorStatus::starting());
    }
    Ok(tor::validate_startup(settings))
}

fn shutdown_receiver() -> watch::Receiver<bool> {
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = shutdown_tx.send(true);
    });
    shutdown_rx
}

async fn serve_tor_only(
    onion_listener: Option<tokio::net::TcpListener>,
    app: axum::Router,
    shutdown_rx: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let listener = onion_listener.context("tor_only requires an internal onion listener")?;
    let addr = listener.local_addr()?;
    info!(%addr, "RustPost listening on loopback for Arti onion forwarding");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(wait_for_shutdown(shutdown_rx))
    .await?;
    Ok(())
}

async fn serve_clearnet(
    app: axum::Router,
    settings: &config::ServerSettings,
    shutdown_rx: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let addr: SocketAddr = format!("{}:{}", settings.host, settings.port)
        .parse()
        .with_context(|| "invalid server bind address")?;
    let clearnet_listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind RustPost server at {addr}"))?;
    info!(%addr, "RustPost listening");
    let clearnet = axum::serve(
        clearnet_listener,
        app.clone()
            .into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(wait_for_shutdown(shutdown_rx.clone()));
    clearnet.await?;
    Ok(())
}

fn spawn_onion_forwarding(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    paths: runtime::RuntimePaths,
    settings: config::TorSettings,
    tor_status: tor::TorStatus,
    onion_target: Option<SocketAddr>,
    shutdown_rx: watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        let started = match tor::start(&settings, &paths, onion_target).await {
            Ok(status) => status,
            Err(error) => {
                warn!(error = %error, "Tor onion service startup task failed");
                return;
            }
        };
        tor_status.replace_with(&started);
        if !started.running() {
            return;
        }
        let addr = match listener.local_addr() {
            Ok(addr) => addr,
            Err(error) => {
                warn!(error = %error, "Tor onion listener address unavailable");
                return;
            }
        };
        info!(%addr, "RustPost listening on loopback for Arti onion forwarding");
        if let Err(error) = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(wait_for_shutdown(shutdown_rx))
        .await
        {
            warn!(error = %error, "Tor onion forwarding listener failed");
        }
    });
}

struct StartupPrintContext<'a> {
    paths: &'a runtime::RuntimePaths,
    settings_path: &'a std::path::Path,
    settings: &'a config::Settings,
    admin_count: i64,
    ffmpeg: &'a crate::ffmpeg::FfmpegStatus,
    tor_status: &'a tor::TorStatus,
    onion_target: Option<SocketAddr>,
}

async fn print_startup_dashboard(
    pool: &db::SqlitePool,
    context: StartupPrintContext<'_>,
) -> anyhow::Result<()> {
    let (user_count, post_count) = crate::social::instance_counts(pool).await?;
    stderr_raw(format_args!(
        "{}",
        terminal::render_startup_dashboard(&terminal::StartupDashboard {
            paths: context.paths,
            settings_path: context.settings_path,
            settings: context.settings,
            admin_count: context.admin_count,
            user_count,
            post_count,
            ffmpeg: context.ffmpeg,
            tor_status: context.tor_status,
            onion_target: context.onion_target,
        })
    ));
    Ok(())
}

fn prompt_admin_credentials() -> anyhow::Result<(String, String)> {
    stdout_raw(format_args!("Admin username: "))?;
    let mut username = String::new();
    io::stdin().read_line(&mut username)?;
    let username = username.trim().to_owned();
    let password = read_secret("Admin password: ")?;
    let confirm = read_secret("Confirm admin password: ")?;
    if password != confirm {
        anyhow::bail!("passwords do not match");
    }
    Ok((username, password))
}

#[derive(Debug, PartialEq, Eq)]
struct FirstAdminCredentials {
    username: String,
    display_name: Option<String>,
    password: String,
}

fn prompt_first_admin_credentials(
    settings: &config::Settings,
) -> anyhow::Result<FirstAdminCredentials> {
    stdout_raw(format_args!("Admin username: "))?;
    let mut username = String::new();
    io::stdin().read_line(&mut username)?;
    stdout_raw(format_args!("Admin display name (optional): "))?;
    let mut display_name = String::new();
    io::stdin().read_line(&mut display_name)?;
    let password = read_secret("Admin password: ")?;
    let confirm = read_secret("Confirm admin password: ")?;
    validate_first_admin_credentials(settings, &username, &display_name, password, &confirm)
}

fn validate_first_admin_credentials(
    settings: &config::Settings,
    username: &str,
    display_name: &str,
    password: String,
    confirm: &str,
) -> anyhow::Result<FirstAdminCredentials> {
    let username = username.trim().to_owned();
    if username.is_empty() {
        anyhow::bail!("username cannot be empty");
    }
    if password.is_empty() {
        anyhow::bail!("password cannot be empty");
    }
    if password != confirm {
        anyhow::bail!("passwords do not match");
    }
    let display_name = display_name.trim().to_owned();
    if !display_name.is_empty() {
        crate::validation::validate_profile_text(&display_name, "", settings)?;
    }
    Ok(FirstAdminCredentials {
        username,
        display_name: (!display_name.is_empty()).then_some(display_name),
        password,
    })
}

#[cfg(unix)]
fn read_secret(prompt: &str) -> anyhow::Result<String> {
    stdout_raw(format_args!("{prompt}"))?;
    let _guard = EchoGuard::disable()?;
    let mut value = String::new();
    io::stdin().read_line(&mut value)?;
    stderr_line(format_args!(""));
    Ok(value.trim_end_matches(['\r', '\n']).to_owned())
}

#[cfg(not(unix))]
fn read_secret(_prompt: &str) -> anyhow::Result<String> {
    anyhow::bail!(
        "hidden password prompts are not supported on this platform; use create-admin instead"
    )
}

#[cfg(unix)]
struct EchoGuard;

#[cfg(unix)]
impl EchoGuard {
    fn disable() -> anyhow::Result<Self> {
        let status = ProcessCommand::new("stty").arg("-echo").status()?;
        if !status.success() {
            anyhow::bail!("failed to disable terminal echo");
        }
        Ok(Self)
    }
}

#[cfg(unix)]
impl Drop for EchoGuard {
    fn drop(&mut self) {
        let _ = ProcessCommand::new("stty").arg("echo").status();
    }
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

async fn wait_for_shutdown(mut shutdown_rx: watch::Receiver<bool>) {
    while !*shutdown_rx.borrow() {
        if shutdown_rx.changed().await.is_err() {
            break;
        }
    }
}

fn stdout_line(args: std::fmt::Arguments<'_>) -> anyhow::Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_fmt(args)?;
    stdout.write_all(b"\n")?;
    Ok(())
}

fn stdout_raw(args: std::fmt::Arguments<'_>) -> anyhow::Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_fmt(args)?;
    stdout.flush()?;
    Ok(())
}

#[cfg(unix)]
fn stderr_line(args: std::fmt::Arguments<'_>) {
    let mut stderr = io::stderr().lock();
    let _write_result = stderr.write_fmt(args);
    let _write_newline_result = stderr.write_all(b"\n");
}

fn stderr_raw(args: std::fmt::Arguments<'_>) {
    let mut stderr = io::stderr().lock();
    let _write_result = stderr.write_fmt(args);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_admin_credentials_reject_empty_username() {
        let result = validate_first_admin_credentials(
            &config::Settings::default(),
            "   ",
            "",
            "very secure password".to_owned(),
            "very secure password",
        );

        assert!(result.is_err());
        assert_eq!(
            result.expect_err("empty username").to_string(),
            "username cannot be empty"
        );
    }

    #[test]
    fn first_admin_credentials_reject_empty_password() {
        let result = validate_first_admin_credentials(
            &config::Settings::default(),
            "admin-user",
            "",
            String::new(),
            "",
        );

        assert!(result.is_err());
        assert_eq!(
            result.expect_err("empty password").to_string(),
            "password cannot be empty"
        );
    }

    #[test]
    fn first_admin_credentials_reject_password_mismatch() {
        let result = validate_first_admin_credentials(
            &config::Settings::default(),
            "admin-user",
            "",
            "very secure password".to_owned(),
            "very different password",
        );

        assert!(result.is_err());
        assert_eq!(
            result.expect_err("mismatch").to_string(),
            "passwords do not match"
        );
    }

    #[test]
    fn first_admin_credentials_accept_optional_display_name() {
        let credentials = validate_first_admin_credentials(
            &config::Settings::default(),
            " Admin-User ",
            " Ada Admin ",
            "very secure password".to_owned(),
            "very secure password",
        )
        .expect("valid credentials");

        assert_eq!(
            credentials,
            FirstAdminCredentials {
                username: "Admin-User".to_owned(),
                display_name: Some("Ada Admin".to_owned()),
                password: "very secure password".to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn first_admin_creation_sets_optional_display_name() {
        let temp = tempfile::tempdir().expect("temp dir");
        let pool = db::connect(&temp.path().join("test.sqlite3"))
            .await
            .expect("connect");
        db::migrate(&pool).await.expect("migrate");
        let settings = config::Settings::default();

        let user_id = admin::create_admin_with_display_name(
            &pool,
            &settings,
            "admin-user",
            "very secure password",
            Some("Ada Admin"),
        )
        .await
        .expect("create admin");

        let row: (String, bool, String) = pool
            .call(move |conn| {
                Ok(conn.query_row(
                    "SELECT username, is_admin, display_name FROM users WHERE id = ?",
                    [user_id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)? != 0,
                            row.get::<_, String>(2)?,
                        ))
                    },
                )?)
            })
            .await
            .expect("load admin");

        assert_eq!(row, ("admin-user".to_owned(), true, "Ada Admin".to_owned()));
    }
}
