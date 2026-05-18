use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Command as ProcessCommand;

use anyhow::Context;
use clap::{Parser, Subcommand};
use tokio::sync::watch;
use tracing::{info, warn};

use crate::{admin, backup, config, db, demo_seed, logging, runtime, server, tor};

#[derive(Debug, Parser)]
#[command(about = "Single-binary self-hosted microblog")]
struct Cli {
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long)]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    Init,
    Check,
    CreateAdmin {
        username: String,
        password: String,
    },
    CreateAdminInteractive,
    ResetAdminPassword {
        username: String,
        password: String,
    },
    SeedDemo,
    Backup {
        #[arg(long)]
        include_tor_keys: bool,
    },
    Restore {
        archive: PathBuf,
        #[arg(long)]
        include_tor_keys: bool,
    },
    PrintOnionAddress,
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

    match cli.command.unwrap_or(Command::Serve) {
        Command::Init => {
            println!("initialized {}", paths.data_dir.display());
            Ok(())
        }
        Command::Check => {
            let ffmpeg = crate::ffmpeg::probe(&settings.media).await;
            let tor_status = tor::validate_startup(&settings.tor);
            println!("configuration ok");
            println!("ffmpeg: {}", ffmpeg.summary());
            println!("tor: {}", tor_status.summary());
            Ok(())
        }
        Command::CreateAdmin { username, password } => {
            let pool = db::connect(&paths.database_path).await?;
            db::migrate(&pool).await?;
            admin::create_admin(&pool, &settings, &username, &password).await?;
            println!("admin account created");
            Ok(())
        }
        Command::CreateAdminInteractive => {
            let pool = db::connect(&paths.database_path).await?;
            db::migrate(&pool).await?;
            let (username, password) = prompt_admin_credentials()?;
            admin::create_admin(&pool, &settings, &username, &password).await?;
            println!("admin account created");
            Ok(())
        }
        Command::ResetAdminPassword { username, password } => {
            let pool = db::connect(&paths.database_path).await?;
            db::migrate(&pool).await?;
            admin::reset_admin_password(&pool, &settings, &username, &password).await?;
            println!("admin password reset");
            Ok(())
        }
        Command::SeedDemo => {
            if explicit_data_dir.is_none() {
                anyhow::bail!("seed-demo requires an explicit --data-dir under target/debug");
            }
            let pool = db::connect(&paths.database_path).await?;
            db::migrate(&pool).await?;
            let report = demo_seed::seed(&pool, &paths, &settings, &settings_path).await?;
            println!("{report}");
            Ok(())
        }
        Command::Backup { include_tor_keys } => {
            let archive = backup::create_backup(&paths, include_tor_keys)?;
            println!("{}", archive.display());
            Ok(())
        }
        Command::Restore {
            archive,
            include_tor_keys,
        } => {
            backup::restore_backup(&paths, &archive, include_tor_keys)?;
            println!("restore completed");
            Ok(())
        }
        Command::PrintOnionAddress => {
            let status = tor::validate_startup(&settings.tor);
            println!("{}", status.onion_address().unwrap_or_default());
            Ok(())
        }
        Command::Serve => serve(paths, settings).await,
    }
}

async fn serve(paths: runtime::RuntimePaths, settings: config::Settings) -> anyhow::Result<()> {
    let pool = db::connect(&paths.database_path).await?;
    db::migrate(&pool).await?;
    let admin_count = admin::admin_count(&pool).await?;
    let ffmpeg = crate::ffmpeg::probe(&settings.media).await;
    if !ffmpeg.available {
        warn!("ffmpeg unavailable; uploads will safely fall back to allowed originals");
    }
    let onion_listener = if settings.tor.enabled {
        Some(tokio::net::TcpListener::bind("127.0.0.1:0").await?)
    } else {
        None
    };
    let onion_target = onion_listener
        .as_ref()
        .and_then(|listener| listener.local_addr().ok());
    let tor_status = if settings.tor.tor_only {
        tor::start(&settings.tor, &paths, onion_target).await?
    } else if settings.tor.enabled {
        tor::TorStatus::starting()
    } else {
        tor::validate_startup(&settings.tor)
    };
    let tor_status_for_background = tor_status.clone();
    if settings.admin.create_admin_on_first_boot {
        admin::ensure_first_boot_admin_hint(&pool).await?;
    }
    print_startup_summary(&paths, &settings, admin_count);
    let state = server::AppState::new(pool, settings.clone(), paths.clone(), ffmpeg, tor_status);
    let app = server::router(state);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = shutdown_tx.send(true);
    });

    if settings.tor.tor_only {
        let listener = onion_listener.context("tor_only requires an internal onion listener")?;
        let addr = listener.local_addr()?;
        info!(%addr, "RustPost listening on loopback for Arti onion forwarding");
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(wait_for_shutdown(shutdown_rx))
        .await?;
        return Ok(());
    }

    if let Some(listener) = onion_listener {
        let task_app = app.clone();
        let task_paths = paths.clone();
        let task_tor_settings = settings.tor.clone();
        let task_tor_status = tor_status_for_background.clone();
        let task_shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            let started = match tor::start(&task_tor_settings, &task_paths, onion_target).await {
                Ok(status) => status,
                Err(error) => {
                    warn!(error = %error, "Tor onion service startup task failed");
                    return;
                }
            };
            task_tor_status.replace_with(&started);
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
                task_app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(wait_for_shutdown(task_shutdown_rx))
            .await
            {
                warn!(error = %error, "Tor onion forwarding listener failed");
            }
        });
    }

    let addr: SocketAddr = format!("{}:{}", settings.server.host, settings.server.port)
        .parse()
        .with_context(|| "invalid server bind address")?;
    let clearnet_listener = tokio::net::TcpListener::bind(addr).await?;
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

fn print_startup_summary(
    paths: &runtime::RuntimePaths,
    settings: &config::Settings,
    admin_count: i64,
) {
    eprintln!("RustPost startup");
    eprintln!("  data dir: {}", paths.data_dir.display());
    eprintln!("  settings: {}", paths.settings_path.display());
    eprintln!("  database: {}", paths.database_path.display());
    eprintln!("  uploads: {}", paths.uploads_originals.display());
    eprintln!(
        "  serving: http://{}:{}",
        settings.server.host, settings.server.port
    );
    if admin_count > 0 {
        eprintln!("  admin: present");
    } else {
        eprintln!("  admin: none found");
        eprintln!(
            "  setup: rustpost-cli --data-dir {} create-admin-interactive",
            paths.data_dir.display()
        );
        eprintln!(
            "  non-interactive: rustpost-cli --data-dir {} create-admin <username> <password>",
            paths.data_dir.display()
        );
    }
}

fn prompt_admin_credentials() -> anyhow::Result<(String, String)> {
    print!("Admin username: ");
    io::stdout().flush()?;
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

#[cfg(unix)]
fn read_secret(prompt: &str) -> anyhow::Result<String> {
    print!("{prompt}");
    io::stdout().flush()?;
    let _guard = EchoGuard::disable()?;
    let mut value = String::new();
    io::stdin().read_line(&mut value)?;
    eprintln!();
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
