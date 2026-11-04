use std::io::{self, Write as _};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use tokio::sync::watch;
use tracing::{info, warn};

use crate::{
    admin, backup, config, console, db, demo_seed, logging, runtime, server, terminal, tor,
};
use zeroize::Zeroizing;

#[derive(Debug, Parser)]
#[command(
    name = "rustpost-cli",
    version,
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

    /// Operational console: auto, tui, plain, or off. Redirected/dumb terminals use plain.
    #[arg(long, global = true, value_enum, default_value = "auto")]
    console: console::Mode,

    /// Render Unicode data as explicit ASCII code points in the console.
    #[arg(long, global = true)]
    ascii: bool,

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
        #[arg(
            help = "Administrator password (prefer --password-stdin)",
            required_unless_present = "password_stdin"
        )]
        password: Option<String>,
        /// Read one password line from redirected stdin instead of argv.
        #[arg(long, conflicts_with = "password")]
        password_stdin: bool,
    },

    /// Create an administrator account with hidden password prompts.
    #[command(about = "Create an administrator account with hidden password prompts")]
    CreateAdminInteractive,

    /// Reset an administrator password.
    #[command(about = "Reset an administrator password")]
    ResetAdminPassword {
        #[arg(help = "Administrator username")]
        username: String,
        #[arg(
            help = "New password (prefer --password-stdin)",
            required_unless_present = "password_stdin"
        )]
        password: Option<String>,
        /// Read one password line from redirected stdin instead of argv.
        #[arg(long, conflicts_with = "password")]
        password_stdin: bool,
    },

    /// Reset a password with protected, cancellable prompts.
    ResetAdminPasswordInteractive { username: String },

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

    /// Start the `RustPost` web server.
    #[command(about = "Start the RustPost web server")]
    Serve,
}

pub async fn run() -> anyhow::Result<()> {
    let logs = logging::init();
    let cli = Cli::parse();
    // Register OS handlers synchronously, before any prompt or raw mode.
    let shutdown = shutdown_sender()?;
    let command = cli.command.unwrap_or(Command::Serve);
    let explicit_data_dir = cli.data_dir.clone();
    let mut paths = runtime::RuntimePaths::discover(cli.data_dir.as_deref())?;
    let check_only = matches!(command, Command::Check);
    if !check_only {
        paths.ensure()?;
    }
    runtime::check_restore_recovery(&paths)?;
    let settings_path = cli.config.unwrap_or_else(|| paths.settings_path.clone());
    if !check_only {
        config::write_default_if_missing(&settings_path)?;
    }
    let settings = config::Settings::load(&settings_path)?;
    settings.validate()?;
    paths = paths
        .with_tor_data_dir(&settings.tor.data_dir)
        .with_backup_dir(&settings.backup.backup_dir);
    // Keep backup and restore pointed at the settings file the operator
    // actually loaded, including an explicit --config path.
    paths.settings_path.clone_from(&settings_path);
    if !check_only {
        paths.ensure()?;
    }
    info!(data_dir = %paths.data_dir.display(), settings = %settings_path.display(), "runtime paths ready");

    let result = run_command(
        command,
        paths,
        settings_path,
        settings,
        Options {
            has_explicit_data_dir: explicit_data_dir.is_some(),
            console: cli.console,
            ascii: cli.ascii,
            shutdown,
            logs: logs.clone(),
        },
    )
    .await;
    if let Err(error) = &result
        && !error.chain().any(|source| {
            source
                .downcast_ref::<io::Error>()
                .is_some_and(|error| error.kind() == io::ErrorKind::BrokenPipe)
        })
    {
        tracing::error!(
            diagnostic = console::operations::failure_reason(error),
            "RustPost command failed"
        );
    }
    logs.finish().await;
    result
}

struct Options {
    has_explicit_data_dir: bool,
    console: console::Mode,
    ascii: bool,
    shutdown: watch::Sender<bool>,
    logs: logging::Logs,
}

async fn run_command(
    command: Command,
    paths: runtime::RuntimePaths,
    settings_path: PathBuf,
    settings: config::Settings,
    options: Options,
) -> anyhow::Result<()> {
    let _snapshot_lease = if matches!(
        command,
        Command::CreateAdmin { .. }
            | Command::CreateAdminInteractive
            | Command::ResetAdminPassword { .. }
            | Command::ResetAdminPasswordInteractive { .. }
            | Command::SeedDemo
    ) {
        Some(runtime::snapshot_lease(&paths, false).context("a backup snapshot is in progress")?)
    } else {
        None
    };
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
        Command::Check => check_command(&paths, &settings).await,
        Command::CreateAdmin {
            username,
            password,
            password_stdin,
        } => {
            create_admin_command(
                &database,
                username,
                command_password(password, password_stdin, options.shutdown.subscribe()).await?,
            )
            .await
        }
        Command::CreateAdminInteractive => {
            let settings = settings.clone();
            let stop = options.shutdown.subscribe();
            let credentials =
                tokio::task::spawn_blocking(move || prompt_credentials(&settings, stop, false))
                    .await??;
            create_admin_command(&database, credentials.username, credentials.password).await
        }
        Command::ResetAdminPassword {
            username,
            password,
            password_stdin,
        } => {
            reset_admin_password_command(
                &database,
                username,
                command_password(password, password_stdin, options.shutdown.subscribe()).await?,
            )
            .await
        }
        Command::ResetAdminPasswordInteractive { username } => {
            let settings = settings.clone();
            let stop = options.shutdown.subscribe();
            let password = tokio::task::spawn_blocking(move || {
                let input = terminal::input::Input::open(stop)?;
                prompt_password(&input, &settings)
            })
            .await??;
            reset_admin_password_command(&database, username, password).await
        }
        Command::SeedDemo => {
            if !options.has_explicit_data_dir {
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
        Command::Serve => serve(paths, settings, options).await,
    }
}

async fn check_command(
    paths: &runtime::RuntimePaths,
    settings: &config::Settings,
) -> anyhow::Result<()> {
    let ffmpeg = crate::ffmpeg::probe(&settings.media).await;
    let tor_status = tor::validate_startup(&settings.tor);
    let legacy = paths.data_dir.join("app.sqlite3");
    let legacy_present = match std::fs::metadata(&legacy) {
        Ok(_) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!(
        !(legacy_present && paths.database_path.try_exists()?),
        "database layout conflict: legacy and current database both exist"
    );
    let database_schema = if legacy_present {
        stderr_raw(format_args!(
            "[INFO] legacy database checked in place; serve will relocate it into db/\n"
        ));
        database_schema_status(&legacy)
    } else {
        database_schema_status(&paths.database_path)
    };
    stdout_raw(format_args!(
        "{}",
        terminal::render_check(paths, settings, &ffmpeg, &tor_status, &database_schema)
    ))?;
    if matches!(
        database_schema,
        terminal::DatabaseSchemaStatus::Unreadable { .. }
            | terminal::DatabaseSchemaStatus::Present {
                compatible: false,
                ..
            }
    ) {
        anyhow::bail!("database check failed; database is unreadable or incompatible");
    }
    Ok(())
}

fn database_schema_status(path: &std::path::Path) -> terminal::DatabaseSchemaStatus {
    match std::fs::metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return terminal::DatabaseSchemaStatus::Missing;
        }
        Err(error) => {
            return terminal::DatabaseSchemaStatus::Unreadable {
                error: error.to_string(),
            };
        }
        Ok(meta) if !meta.is_file() => {
            return terminal::DatabaseSchemaStatus::Unreadable {
                error: "database is not a regular file".to_owned(),
            };
        }
        Ok(_) => {}
    }
    match db::schema_report_from_path(path) {
        Ok(report) if report.is_compatible() => terminal::DatabaseSchemaStatus::Present {
            version: report.version(),
            compatible: true,
            summary: report.summary(),
        },
        Ok(report) => match db::adoptable_schema_summary_from_path(path) {
            Ok(Some(summary)) => terminal::DatabaseSchemaStatus::Adoptable { summary },
            Ok(None) => terminal::DatabaseSchemaStatus::Present {
                version: report.version(),
                compatible: false,
                summary: report.summary(),
            },
            Err(error) => terminal::DatabaseSchemaStatus::Unreadable {
                error: error.to_string(),
            },
        },
        Err(error) => terminal::DatabaseSchemaStatus::Unreadable {
            error: error.to_string(),
        },
    }
}

struct AdminDatabase<'a> {
    paths: &'a runtime::RuntimePaths,
    settings: &'a config::Settings,
}

async fn create_admin_command(
    database: &AdminDatabase<'_>,
    username: String,
    password: Zeroizing<String>,
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
                    terminal::instance_command(database.paths, "serve"),
                ),
            ],
        )
    ))?;
    Ok(())
}

async fn reset_admin_password_command(
    database: &AdminDatabase<'_>,
    username: String,
    password: Zeroizing<String>,
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
    let report = backup::restore_backup(paths, archive, include_tor_keys)?;
    stdout_raw(format_args!(
        "{}",
        terminal::render_command_success(
            "RustPost restore completed",
            &[
                terminal::row("Archive", archive.display().to_string()),
                terminal::row("Data directory", paths.data_dir.display().to_string()),
                terminal::row(
                    "Pre-restore safety backup",
                    report.pre_restore_backup.map_or_else(
                        || "not created".to_owned(),
                        |path| path.display().to_string()
                    ),
                ),
                terminal::row(
                    "Tor keys",
                    if report.tor_keys_restored {
                        "restored"
                    } else {
                        "not restored"
                    },
                ),
            ],
        )
    ))?;
    Ok(())
}

async fn serve(
    paths: runtime::RuntimePaths,
    settings: config::Settings,
    options: Options,
) -> anyhow::Result<()> {
    use std::sync::{Arc, Mutex};
    let _server_lease = runtime::acquire_server_lease(&paths)?;
    options.logs.persist(&paths.logs_dir)?;
    stderr_raw(format_args!(
        "[STARTING] opening database and validating schema\n"
    ));
    let pool = db::connect(&paths.database_path).await?;
    db::migrate(&pool).await?;
    ensure_first_admin_interactive(&pool, &settings, &paths, options.shutdown.subscribe()).await?;
    if *options.shutdown.borrow() {
        anyhow::bail!("startup cancelled");
    }
    let live = console::Live::default();
    let refresh = Arc::new(tokio::sync::Notify::new());
    let operations = console::operations::Operations::default();
    let (metrics, collectors) = if matches!(options.console, console::Mode::Off) {
        (console::metrics::Shared::default(), Vec::new())
    } else {
        console::metrics::spawn(&paths, options.shutdown.subscribe(), refresh.clone())
    };
    let ffmpeg_status = Arc::new(Mutex::new(None));
    let tor_status = if settings.tor.enabled {
        tor::TorStatus::starting()
    } else {
        tor::validate_startup(&settings.tor)
    };
    let context = console::Context {
        paths: paths.clone(),
        settings: settings.clone(),
        ffmpeg: ffmpeg_status.clone(),
        tor: tor_status.clone(),
        live: live.clone(),
        operations: operations.clone(),
        metrics,
        logs: options.logs.clone(),
        started: std::time::Instant::now(),
        refresh,
        requests: Arc::default(),
        ascii: options.ascii,
    };
    let display =
        console::Console::start(context.clone(), options.console, options.shutdown.clone());
    let result = serve_runtime(&context, &pool, &options.shutdown).await;
    if result.is_err() {
        console::phase(
            &live,
            console::Phase::Failed("server/startup failed; see command error"),
        );
    }
    options.shutdown.send_replace(true);
    for collector in collectors {
        let _ = collector.await;
    }
    if result.is_ok() {
        console::phase(&live, console::Phase::Stopped);
    }
    display.finish().await;
    result
}

async fn serve_runtime(
    context: &console::Context,
    pool: &db::SqlitePool,
    shutdown: &watch::Sender<bool>,
) -> anyhow::Result<()> {
    let paths = &context.paths;
    let settings = &context.settings;
    let live = &context.live;
    let tor_status = &context.tor;
    let operations = &context.operations;
    let mut stop = shutdown.subscribe();
    let ffmpeg = initialize_media(context, &stop).await?;
    let mut onion_listener = bind_onion_listener(&settings.tor).await?;
    let onion_target = onion_listener_target(onion_listener.as_ref());
    if let Ok(mut state) = live.lock() {
        state.onion_target = onion_target;
    }
    initialize_tor(context, onion_target, &stop).await?;
    let state = server::AppState::new(
        pool.clone(),
        settings.clone(),
        paths.clone(),
        ffmpeg,
        tor_status.clone(),
    );
    let app = server::router(state).layer(axum::middleware::from_fn_with_state(
        context.requests.clone(),
        console::requests::observe,
    ));
    let listener = if settings.tor.tor_only {
        onion_listener
            .take()
            .context("Tor-only requires loopback listener")?
    } else {
        let addr = settings.server.listener_address()?;
        tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("bind RustPost server at {addr}"))?
    };
    if !settings.tor.tor_only
        && let Ok(mut state) = live.lock()
    {
        state.clearnet = Some(listener.local_addr()?);
    }
    console::phase(live, console::Phase::Listening);
    let backup = backup::spawn_automatic_scheduler_observed(
        paths.clone(),
        paths.settings_path.clone(),
        stop.clone(),
        operations.clone(),
    );
    let maintenance = server::spawn_maintenance_scheduler_observed(
        pool.clone(),
        paths.clone(),
        stop.clone(),
        operations.clone(),
    );
    let onion = if settings.tor.enabled && !settings.tor.tor_only {
        Some(spawn_onion_forwarding(
            onion_listener,
            app.clone(),
            paths.clone(),
            settings.tor.clone(),
            tor_status.clone(),
            onion_target,
            stop.clone(),
            live.clone(),
        ))
    } else {
        None
    };
    let app = if settings.tor.tor_only {
        app.layer(axum::Extension(crate::csrf::OnionTransport))
    } else {
        app
    };
    let service = serve_until_shutdown(
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(wait_for_shutdown(stop.clone())),
        stop.clone(),
    );
    tokio::pin!(service);
    let result = tokio::select! {
        result = &mut service => result,
        _ = stop.changed() => {
            console::phase(live, console::Phase::Stopping);
            service.await
        }
    };
    console::phase(live, console::Phase::Stopping);
    shutdown.send_replace(true);
    let drained = drain_workers(backup, maintenance, onion, operations).await;
    tor_status.stop();
    result.and(drained)
}

async fn initialize_tor(
    context: &console::Context,
    target: Option<SocketAddr>,
    shutdown: &watch::Receiver<bool>,
) -> anyhow::Result<()> {
    if context.settings.tor.tor_only {
        console::phase(
            &context.live,
            console::Phase::Starting("bootstrapping Tor; publication and reachability unverified"),
        );
        let started = tokio::select! {
            result = tor::start(&context.settings.tor, &context.paths, target) => result?,
            () = wait_for_shutdown(shutdown.clone()) => anyhow::bail!("startup cancelled"),
        };
        context.tor.replace_with(&started);
    }
    Ok(())
}

async fn initialize_media(
    context: &console::Context,
    shutdown: &watch::Receiver<bool>,
) -> anyhow::Result<crate::ffmpeg::FfmpegStatus> {
    console::phase(
        &context.live,
        console::Phase::Starting("probing FFmpeg (up to 20s)"),
    );
    let ffmpeg = tokio::select! {
        value = crate::ffmpeg::probe(&context.settings.media) => value,
        () = wait_for_shutdown(shutdown.clone()) => anyhow::bail!("startup cancelled"),
    };
    if let Ok(mut visible) = context.ffmpeg.lock() {
        *visible = Some(ffmpeg.clone());
    }
    Ok(ffmpeg)
}

async fn drain_workers(
    backup: tokio::task::JoinHandle<()>,
    maintenance: tokio::task::JoinHandle<()>,
    onion: Option<tokio::task::JoinHandle<()>>,
    operations: &console::operations::Operations,
) -> anyhow::Result<()> {
    let mut failed = false;
    // Admitted transactional work is owned and drained rather than detached.
    for (index, worker) in [backup, maintenance].into_iter().enumerate() {
        if worker.await.is_err() {
            operations.set(
                index,
                console::operations::TaskState::Failed,
                "background worker terminated unexpectedly",
            );
            failed = true;
        }
    }
    if let Some(onion) = onion
        && onion.await.is_err()
    {
        failed = true;
    }
    anyhow::ensure!(
        !failed,
        "a background worker terminated unexpectedly during service"
    );
    Ok(())
}

async fn ensure_first_admin_interactive(
    pool: &db::SqlitePool,
    settings: &config::Settings,
    paths: &runtime::RuntimePaths,
    shutdown: watch::Receiver<bool>,
) -> anyhow::Result<i64> {
    let count = admin::admin_count(pool).await?;
    if count > 0 || !settings.admin.create_admin_on_first_boot {
        return Ok(count);
    }
    if !terminal::input::available() {
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
    let settings_for_prompt = settings.clone();
    let credentials = tokio::task::spawn_blocking(move || {
        prompt_credentials(&settings_for_prompt, shutdown, true)
    })
    .await??;
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

fn shutdown_sender() -> anyhow::Result<watch::Sender<bool>> {
    let (tx, _) = watch::channel(false);
    let sender = tx.clone();
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut interrupt = signal(SignalKind::interrupt())?;
        let mut terminate = signal(SignalKind::terminate())?;
        let mut hangup = signal(SignalKind::hangup())?;
        let mut quit = signal(SignalKind::quit())?;
        tokio::spawn(async move {
            tokio::select! { _ = interrupt.recv() => {}, _ = terminate.recv() => {}, _ = hangup.recv() => {}, _ = quit.recv() => {} }
            sender.send_replace(true);
        });
    }
    #[cfg(windows)]
    {
        let mut interrupt = tokio::signal::windows::ctrl_c()?;
        let mut terminate = tokio::signal::windows::ctrl_break()?;
        tokio::spawn(async move {
            tokio::select! { _ = interrupt.recv() => {}, _ = terminate.recv() => {} }
            sender.send_replace(true);
        });
    }
    #[cfg(not(any(unix, windows)))]
    anyhow::bail!("unsupported terminal platform");
    Ok(tx)
}

async fn serve_until_shutdown<F>(server: F, shutdown: watch::Receiver<bool>) -> anyhow::Result<()>
where
    F: std::future::IntoFuture<Output = io::Result<()>>,
{
    let future = server.into_future();
    tokio::pin!(future);
    tokio::select! {
        result = &mut future => result?,
        () = wait_for_shutdown(shutdown) => {
            if let Ok(result) = tokio::time::timeout(Duration::from_secs(30), &mut future).await { result?; } else { warn!("HTTP shutdown exceeded 30 seconds; cancelling active requests"); }
        }
    }
    Ok(())
}

struct OnionEndpoint(console::Live);
impl Drop for OnionEndpoint {
    fn drop(&mut self) {
        if let Ok(mut live) = self.0.lock() {
            live.onion_target = None;
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "forwarding worker retains explicit service, shutdown and endpoint ownership"
)]
fn spawn_onion_forwarding(
    listener: Option<tokio::net::TcpListener>,
    app: axum::Router,
    paths: runtime::RuntimePaths,
    settings: config::TorSettings,
    tor_status: tor::TorStatus,
    onion_target: Option<SocketAddr>,
    shutdown_rx: watch::Receiver<bool>,
    live: console::Live,
) -> tokio::task::JoinHandle<()> {
    let app = app.layer(axum::Extension(crate::csrf::OnionTransport));
    tokio::spawn(async move {
        let _endpoint = OnionEndpoint(live);
        let Some(listener) = listener else {
            tor_status.fail("onion listener unavailable");
            return;
        };
        let started = match tokio::select! { result = tor::start(&settings, &paths, onion_target) => result, () = wait_for_shutdown(shutdown_rx.clone()) => return }
        {
            Ok(status) => status,
            Err(error) => {
                tor_status.fail("Tor startup task failed");
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
                tor_status.fail("onion listener address unavailable");
                warn!(error = %error, "Tor onion listener address unavailable");
                return;
            }
        };
        info!(%addr, "RustPost listening on loopback for Arti onion forwarding");
        if let Err(error) = serve_until_shutdown(
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(wait_for_shutdown(shutdown_rx.clone())),
            shutdown_rx,
        )
        .await
        {
            tor_status.fail("onion forwarding listener failed");
            warn!(error = %error, "Tor onion forwarding listener failed");
        }
    })
}

async fn command_password(
    password: Option<String>,
    from_stdin: bool,
    shutdown: watch::Receiver<bool>,
) -> anyhow::Result<Zeroizing<String>> {
    if from_stdin {
        // A cancelled pipe read must not pin a Tokio blocking worker at exit.
        // This input-only OS thread ends on EOF or process exit; it owns no DB
        // operation or terminal mutation.
        let (sender, receiver) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name("rustpost-secret-input".to_owned())
            .spawn(move || {
                let _ = sender.send(terminal::input::password_stdin());
            })?;
        return tokio::select! { value = receiver => value.context("password input reader stopped")?, () = wait_for_shutdown(shutdown) => anyhow::bail!("password input cancelled") };
    }
    password
        .map(Zeroizing::new)
        .context("provide --password-stdin or a password argument")
}

struct FirstAdminCredentials {
    username: String,
    display_name: Option<String>,
    password: Zeroizing<String>,
}

fn prompt_password(
    input: &terminal::input::Input,
    settings: &config::Settings,
) -> anyhow::Result<Zeroizing<String>> {
    loop {
        let password = input.line("Admin password: ", true)?;
        let confirm = input.line("Confirm admin password: ", true)?;
        if *password != *confirm {
            stderr_raw(format_args!(
                "Passwords do not match; retry or Esc to cancel.\r\n"
            ));
            continue;
        }
        match crate::validation::validate_password(&password, settings) {
            Ok(()) => return Ok(password),
            Err(error) => stderr_raw(format_args!(
                "{}; retry or Esc to cancel.\r\n",
                terminal::display_text(&error.to_string())
            )),
        }
    }
}

fn prompt_credentials(
    settings: &config::Settings,
    shutdown: watch::Receiver<bool>,
    with_display: bool,
) -> anyhow::Result<FirstAdminCredentials> {
    let input = terminal::input::Input::open(shutdown)?;
    let username = loop {
        let username = input.line("Admin username: ", false)?;
        match crate::validation::normalize_username(&username, settings.accounts.max_username_len) {
            Ok(username) => break username,
            Err(error) => stderr_raw(format_args!(
                "{}; retry or Esc to cancel.\r\n",
                terminal::display_text(&error.to_string())
            )),
        }
    };
    let display = if with_display {
        input.line("Admin display name (optional): ", false)?
    } else {
        Zeroizing::new(String::new())
    };
    let display = display.trim();
    if !display.is_empty() {
        crate::validation::validate_profile_text(display, "", settings)?;
    }
    let password = prompt_password(&input, settings)?;
    Ok(FirstAdminCredentials {
        username,
        display_name: (!display.is_empty()).then(|| display.to_owned()),
        password,
    })
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

fn stderr_raw(args: std::fmt::Arguments<'_>) {
    let mut stderr = io::stderr().lock();
    let _write_result = stderr.write_fmt(args);
}
