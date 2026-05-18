use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use tokio::sync::watch;
use tracing::{info, warn};

use crate::{admin, backup, config, db, logging, runtime, server, tor};

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
    ResetAdminPassword {
        username: String,
        password: String,
    },
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
        Command::ResetAdminPassword { username, password } => {
            let pool = db::connect(&paths.database_path).await?;
            db::migrate(&pool).await?;
            admin::reset_admin_password(&pool, &settings, &username, &password).await?;
            println!("admin password reset");
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
