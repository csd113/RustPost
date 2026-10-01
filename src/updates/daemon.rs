#[cfg(unix)]
use super::transaction::native::{Config, Engine, Service};
#[cfg(unix)]
use anyhow::Context as _;
use std::path::Path;

/// Runs the operator-configured updater. Linux only; never invoked by HTTP.
pub async fn run(config_path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        anyhow::ensure!(cfg!(target_os = "linux"), "rustpost-updater requires Linux");
        linux::run(config_path).await
    }
    #[cfg(not(unix))]
    {
        let _ = config_path;
        anyhow::bail!("rustpost-updater supports managed native Linux installations only")
    }
}

#[cfg(unix)]
mod linux {
    use super::super::{Reply, Request, SOCKET};
    use super::*;
    use std::fs;
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    use std::process::{Command, Stdio};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::{Duration, Instant};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    #[derive(Default)]
    struct Admission {
        recovered: AtomicBool,
        starting: AtomicBool,
        installing: AtomicBool,
    }
    impl Admission {
        fn may_start(&self, phase: super::super::Phase) -> bool {
            (self.recovered.load(Ordering::Acquire) || self.starting.load(Ordering::Acquire))
                && phase.may_start()
        }
    }

    fn configured_engine(config_path: &Path) -> anyhow::Result<Engine> {
        super::super::transaction::native::protected_ancestors(config_path, u32::MAX)?;
        let metadata = fs::symlink_metadata(config_path)?;
        anyhow::ensure!(
            metadata.is_file() && metadata.uid() == 0 && metadata.mode() & 0o022 == 0,
            "updater configuration must be a root-owned non-writable regular file"
        );
        let config: Config = toml::from_str(&fs::read_to_string(config_path)?)
            .map_err(|_| anyhow::anyhow!("invalid updater configuration"))?;
        super::super::transaction::native::protected_ancestors(&config.public_key, config.web_uid)?;
        let metadata = fs::symlink_metadata(&config.public_key)?;
        anyhow::ensure!(
            metadata.is_file() && metadata.uid() == 0 && metadata.mode() & 0o022 == 0,
            "release trust key must be root owned and non-writable"
        );
        let key_text = fs::read_to_string(&config.public_key)?;
        let key_text = key_text.trim();
        anyhow::ensure!(
            key_text.len() == 64 && key_text.is_ascii(),
            "release key must be 32 bytes encoded as hexadecimal"
        );
        let key = (0..32)
            .map(|i| u8::from_str_radix(&key_text[i * 2..i * 2 + 2], 16))
            .collect::<Result<Vec<_>, _>>()?;
        let engine = Engine { config, key };
        engine.validate()?;
        super::super::transaction::native::protected_ancestors(
            Path::new(SOCKET)
                .parent()
                .context("missing socket parent")?,
            engine.config.web_uid,
        )?;
        Ok(engine)
    }

    pub async fn run(config_path: &Path) -> anyhow::Result<()> {
        let engine = Arc::new(configured_engine(config_path)?);
        // Exclusive daemon lock prevents two listeners/recovery workers. File
        // locks release on death, unlike pid files or lock directories.
        let daemon_lock = fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(engine.config.state_dir.join("daemon.lock"))?;
        daemon_lock
            .try_lock()
            .context("updater daemon already running")?;
        match fs::symlink_metadata(SOCKET) {
            Ok(metadata) => {
                use std::os::unix::fs::FileTypeExt as _;
                anyhow::ensure!(
                    metadata.file_type().is_socket(),
                    "updater socket path has unexpected type"
                );
                fs::remove_file(SOCKET)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let listener = tokio::net::UnixListener::bind(SOCKET)?;
        fs::set_permissions(SOCKET, fs::Permissions::from_mode(0o660))?;
        // Listen while recovering: starting the application needs Ready IPC.
        let admission = Arc::new(Admission::default());
        let recovery_admission = Arc::clone(&admission);
        let recovery_engine = Arc::clone(&engine);
        tokio::task::spawn_blocking(move || {
            let service = SystemService {
                port: recovery_engine.config.health_port,
                admission: Arc::clone(&recovery_admission),
            };
            if let Err(error) = recovery_engine.recover(&service) {
                tracing::error!(error = %error, "updater journal recovery failed; application startup is blocked");
            } else {
                recovery_admission.recovered.store(true, Ordering::Release);
            }
        });
        let slots = Arc::new(tokio::sync::Semaphore::new(16));
        loop {
            let (mut socket, _) = listener.accept().await?;
            if socket.peer_cred()?.uid() != engine.config.web_uid {
                continue;
            }
            let Ok(permit) = Arc::clone(&slots).try_acquire_owned() else {
                continue;
            };
            let engine = Arc::clone(&engine);
            let admission = Arc::clone(&admission);
            tokio::spawn(async move {
                let _permit = permit;
                let result = async {
                    let mut bytes = Vec::new();
                    tokio::time::timeout(
                        Duration::from_secs(5),
                        (&mut socket).take(4097).read_to_end(&mut bytes),
                    )
                    .await??;
                    anyhow::ensure!(bytes.len() <= 4096, "updater request exceeds limit");
                    let request: Request = serde_json::from_slice(&bytes)?;
                    let reply =
                        tokio::task::spawn_blocking(move || handle(&engine, &admission, request))
                            .await??;
                    socket.write_all(&serde_json::to_vec(&reply)?).await?;
                    socket.shutdown().await?;
                    Ok::<_, anyhow::Error>(())
                }
                .await;
                if let Err(error) = result {
                    tracing::warn!(error = %error, "updater IPC request rejected");
                }
            });
        }
    }

    fn handle(
        engine: &Arc<Engine>,
        admission: &Arc<Admission>,
        request: Request,
    ) -> anyhow::Result<Reply> {
        let result = match request {
            Request::Status => engine.status().map(|status| {
                let writable = admission.recovered.load(Ordering::Acquire)
                    && !status.phase.active()
                    && status.phase != super::super::Phase::FailedManualIntervention
                    && !admission.installing.load(Ordering::Acquire);
                (status, writable)
            }),
            Request::Ready { version } => engine.status().map(|status| {
                let ready = admission.may_start(status.phase)
                    && engine
                        .current_version()
                        .is_ok_and(|current| current == version);
                (status, ready)
            }),
            Request::Check => {
                anyhow::ensure!(
                    admission.recovered.load(Ordering::Acquire),
                    "recovery is incomplete"
                );
                engine.check().map(|status| (status, false))
            }
            Request::Install {
                approval,
                administrator,
            } => (|| {
                anyhow::ensure!(
                    admission.recovered.load(Ordering::Acquire),
                    "recovery is incomplete"
                );
                let lock = engine.lock()?;
                let (status, manifest) = engine.approve(&approval, administrator)?;
                admission.installing.store(true, Ordering::Release);
                let mut worker_status = status.clone();
                let worker_engine = Arc::clone(engine);
                let worker_admission = Arc::clone(admission);
                let spawned = std::thread::Builder::new().name("rustpost-update".into()).spawn(move || {
                    let worker_lock = lock;
                    let service = SystemService { port: worker_engine.config.health_port, admission: Arc::clone(&worker_admission) };
                    if let Err(error) = worker_engine.install(&mut worker_status, &manifest, &service) {
                        worker_admission.recovered.store(false, Ordering::Release);
                        tracing::error!(error = %error, "failed to persist update result; recovery required");
                    }
                    drop(worker_lock);
                    worker_admission.installing.store(false, Ordering::Release);
                });
                if let Err(error) = spawned {
                    let mut failed = status;
                    engine.save(
                        &mut failed,
                        super::super::Phase::Failed,
                        "Update worker could not start; previous version retained.",
                    )?;
                    admission.installing.store(false, Ordering::Release);
                    return Err(error.into());
                }
                Ok((status, false))
            })(),
        };
        match result {
            Ok((status, ready)) => Ok(Reply {
                status,
                error: None,
                ready,
            }),
            Err(error) => {
                tracing::warn!(error = %error, "updater operation failed");
                Ok(Reply { status: engine.status()?, error: Some("Update operation could not proceed. Another operation may be running, or preflight/verification failed. Check updater logs.".into()), ready: false })
            }
        }
    }

    struct SystemService {
        port: u16,
        admission: Arc<Admission>,
    }
    fn control(verb: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            matches!(verb, "start" | "stop"),
            "unsupported service action"
        );
        bounded_command(
            "/usr/bin/systemctl",
            &["--no-ask-password", verb, "rustpost.service"],
        )
    }
    fn bounded_command(program: &str, arguments: &[&str]) -> anyhow::Result<()> {
        let mut child = Command::new(program)
            .env_clear()
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            if let Some(status) = child.try_wait()? {
                anyhow::ensure!(status.success(), "RustPost service control failed");
                return Ok(());
            }
            if Instant::now() >= deadline {
                child.kill()?;
                child.wait()?;
                anyhow::bail!("RustPost service control timed out");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    impl Service for SystemService {
        fn preflight(&self) -> anyhow::Result<()> {
            let pid = std::process::id().to_string();
            for verb in ["start", "stop"] {
                bounded_command(
                    "/usr/bin/pkcheck",
                    &[
                        "--action-id",
                        "org.freedesktop.systemd1.manage-units",
                        "--process",
                        &pid,
                        "--detail",
                        "unit",
                        "rustpost.service",
                        "--detail",
                        "verb",
                        verb,
                    ],
                )?;
            }
            Ok(())
        }
        fn stop(&self) -> anyhow::Result<()> {
            control("stop")
        }
        fn start(&self) -> anyhow::Result<()> {
            // Only this controlled start may bypass the daemon boot barrier.
            // Recovery reaches it only after restore/reactivation has completed.
            self.admission.starting.store(true, Ordering::Release);
            let result = control("start");
            self.admission.starting.store(false, Ordering::Release);
            result
        }
        fn health(&self, version: &str, schema: i64) -> anyhow::Result<()> {
            let client: ureq::Agent = ureq::Agent::config_builder()
                .max_redirects(0)
                .timeout_global(Some(Duration::from_secs(2)))
                .build()
                .into();
            let deadline = Instant::now() + Duration::from_secs(60);
            while Instant::now() < deadline {
                let checked = (|| {
                    let mut response = client
                        .get(format!(
                            "http://127.0.0.1:{}/internal/update-health",
                            self.port
                        ))
                        .call()?;
                    let bytes = response
                        .body_mut()
                        .with_config()
                        .limit(4096)
                        .read_to_vec()?;
                    let health: crate::server::UpdateHealth = serde_json::from_slice(&bytes)?;
                    anyhow::ensure!(
                        health.version == version && health.schema == schema && health.ready,
                        "RustPost health or version mismatch"
                    );
                    Ok::<_, anyhow::Error>(())
                })();
                if checked.is_ok() {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            anyhow::bail!("RustPost failed the bounded health check window")
        }
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::updates::Phase;
        #[test]
        fn persisted_start_phase_cannot_bypass_boot_recovery() {
            let gate = Admission::default();
            for phase in [
                Phase::Restarting,
                Phase::HealthChecking,
                Phase::RestartingPrevious,
                Phase::Idle,
            ] {
                assert!(!gate.may_start(phase));
            }
            gate.starting.store(true, Ordering::Release);
            assert!(gate.may_start(Phase::RestartingPrevious));
            assert!(!gate.may_start(Phase::RollingBack));
            gate.starting.store(false, Ordering::Release);
            assert!(!gate.may_start(Phase::RestartingPrevious));
            gate.recovered.store(true, Ordering::Release);
            assert!(gate.may_start(Phase::Idle));
        }
    }
}
