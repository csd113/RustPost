use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use arti_client::{TorClient, config::TorClientConfigBuilder};
use futures_util::StreamExt as _;
use safelog::DisplayRedacted as _;
use tokio::net::TcpStream;
use tor_cell::relaycell::msg::{Connected, End, EndReason};
use tor_hsservice::{HsNickname, RunningOnionService};
use tor_proto::stream::IncomingStreamRequest;
use tracing::{info, warn};

mod activity;
use activity::ObservedIo;

use crate::config::TorSettings;
use crate::runtime::RuntimePaths;

#[derive(Clone)]
pub struct TorStatus {
    inner: Arc<RwLock<TorStatusSnapshot>>,
    runtime: Arc<RwLock<Option<Arc<TorRuntime>>>>,
}

#[derive(Debug, Clone)]
struct TorStatusSnapshot {
    enabled: bool,
    running: bool,
    onion_address: Option<String>,
    state: String,
    bootstrap_status: Option<String>,
    error: Option<String>,
    last_traffic: Option<Instant>,
}

struct TorRuntime {
    _client: Arc<TorClient<tor_rtcompat::PreferredRuntime>>,
    service: Arc<RunningOnionService>,
    stream_task: tokio::task::JoinHandle<()>,
    status: Arc<RwLock<TorStatusSnapshot>>,
}

impl Drop for TorRuntime {
    fn drop(&mut self) {
        self.stream_task.abort();
    }
}

impl fmt::Debug for TorStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TorStatus")
            .field("inner", &self.snapshot())
            .field("runtime", &self.runtime_is_running())
            .finish()
    }
}

impl TorStatus {
    #[must_use]
    pub fn summary(&self) -> String {
        let snapshot = self.snapshot();
        if !snapshot.enabled {
            return "disabled".to_owned();
        }
        match snapshot.onion_address {
            Some(onion) => format!("{} {}", snapshot.state, onion),
            None => snapshot.state,
        }
    }

    #[must_use]
    pub fn enabled(&self) -> bool {
        self.snapshot().enabled
    }

    #[must_use]
    pub fn running(&self) -> bool {
        self.snapshot().running
    }

    #[must_use]
    pub fn onion_address(&self) -> Option<String> {
        self.snapshot().onion_address
    }

    #[must_use]
    pub fn state(&self) -> String {
        self.snapshot().state
    }

    #[must_use]
    pub fn bootstrap_status(&self) -> Option<String> {
        self.snapshot().bootstrap_status
    }

    #[must_use]
    pub fn error(&self) -> Option<String> {
        self.snapshot().error
    }

    fn disabled() -> Self {
        Self::new(TorStatusSnapshot {
            enabled: false,
            running: false,
            onion_address: None,
            state: "disabled".to_owned(),
            bootstrap_status: None,
            error: None,
            last_traffic: None,
        })
    }

    #[must_use]
    pub fn starting() -> Self {
        Self::new(TorStatusSnapshot {
            enabled: true,
            running: false,
            onion_address: None,
            state: "starting".to_owned(),
            bootstrap_status: None,
            error: None,
            last_traffic: None,
        })
    }

    fn startup_error(message: String, bootstrap_status: Option<String>) -> Self {
        Self::new(TorStatusSnapshot {
            enabled: true,
            running: false,
            onion_address: None,
            state: "error".to_owned(),
            bootstrap_status,
            error: Some(message),
            last_traffic: None,
        })
    }

    fn new(snapshot: TorStatusSnapshot) -> Self {
        Self {
            inner: Arc::new(RwLock::new(snapshot)),
            runtime: Arc::new(RwLock::new(None)),
        }
    }

    pub(crate) fn replace_with(&self, other: &Self) {
        self.replace_snapshot_with(other);
        if let (Ok(mut runtime), Ok(other_runtime)) = (self.runtime.write(), other.runtime.read()) {
            runtime.clone_from(&other_runtime);
        }
    }

    pub(crate) fn replace_snapshot_with(&self, other: &Self) {
        if let (Ok(mut snapshot), Ok(other_snapshot)) = (self.inner.write(), other.inner.read()) {
            *snapshot = other_snapshot.clone();
        }
    }

    fn source(&self) -> Arc<RwLock<TorStatusSnapshot>> {
        self.runtime
            .read()
            .ok()
            .and_then(|runtime| runtime.as_ref().map(|runtime| runtime.status.clone()))
            .unwrap_or_else(|| self.inner.clone())
    }

    fn snapshot(&self) -> TorStatusSnapshot {
        self.source()
            .read()
            .map_or_else(|_| TorStatusSnapshot::poisoned(), |guard| guard.clone())
    }

    pub(crate) fn fail(&self, reason: &str) {
        if let Ok(mut snapshot) = self.source().write() {
            snapshot.running = false;
            snapshot.error = Some(reason.to_owned());
            "failed".clone_into(&mut snapshot.state);
        }
    }

    pub(crate) fn stop(&self) {
        let mut final_status = self.snapshot();
        final_status.running = false;
        "stopped".clone_into(&mut final_status.state);
        if let Ok(mut snapshot) = self.inner.write() {
            *snapshot = final_status;
        }
        if let Ok(mut runtime) = self.runtime.write() {
            *runtime = None;
        }
    }

    /// Arti publication state and observed traffic are independent of address assignment.
    #[must_use]
    pub fn operational_summary(&self) -> String {
        use tor_hsservice::status::State;
        let snapshot = self.snapshot();
        if !snapshot.enabled {
            return "[DISABLED]".to_owned();
        }
        if let Some(error) = &snapshot.error {
            return format!(
                "[{}] {}",
                if snapshot.running {
                    "DEGRADED"
                } else {
                    "FAILED"
                },
                crate::terminal::display_text(error)
            );
        }
        let publication = self.runtime.read().ok().and_then(|runtime| {
            runtime
                .as_ref()
                .map(|runtime| runtime.service.status().state())
        });
        let state = match publication {
            Some(State::Running) => {
                "[READY] Arti reports introduction points and descriptor current"
            }
            Some(State::DegradedReachable) => {
                "[DEGRADED] Arti reports reachable with reduced introduction points"
            }
            Some(State::DegradedUnreachable) => "[DEGRADED] descriptor publication incomplete",
            Some(State::Recovering) => "[DEGRADED] recovering introduction points/publication",
            Some(State::Broken) => "[FAILED] Arti service broken",
            Some(State::Shutdown) => "[STOPPED] Arti service stopped",
            Some(State::Bootstrapping) => "[STARTING] introduction points / descriptor publication",
            Some(_) => "[UNAVAILABLE] unrecognized Arti service state",
            None if snapshot.state == "stopped" => "[STOPPED]",
            None => "[STARTING] bootstrap; service not launched",
        };
        format!(
            "{state}; current external reachability unverified; last traffic {}",
            snapshot.last_traffic.map_or_else(
                || "never observed in this process".to_owned(),
                |at| format!("{}s ago", at.elapsed().as_secs())
            )
        )
    }

    fn runtime_is_running(&self) -> bool {
        self.runtime
            .read()
            .is_ok_and(|runtime| runtime.as_ref().is_some())
    }
}

impl TorStatusSnapshot {
    fn poisoned() -> Self {
        Self {
            enabled: true,
            running: false,
            onion_address: None,
            state: "error".to_owned(),
            bootstrap_status: None,
            error: Some("Tor status lock was poisoned".to_owned()),
            last_traffic: None,
        }
    }
}

pub fn validate_startup(settings: &TorSettings) -> TorStatus {
    if !settings.enabled {
        return TorStatus::disabled();
    }
    TorStatus::startup_error(
        "Tor is enabled but startup has not been attempted in this process".to_owned(),
        None,
    )
}

pub async fn start(
    settings: &TorSettings,
    paths: &RuntimePaths,
    onion_target: Option<SocketAddr>,
) -> anyhow::Result<TorStatus> {
    if !settings.enabled {
        return Ok(TorStatus::disabled());
    }
    ensure_rustls_crypto_provider();
    let Some(onion_target) = onion_target else {
        return tor_startup_error(
            settings,
            "internal onion target listener is unavailable".to_owned(),
            None,
        );
    };

    match start_inner(settings, paths, onion_target).await {
        Ok(status) => Ok(status),
        Err(error) => tor_startup_error(settings, error.to_string(), None),
    }
}

fn tor_startup_error(
    settings: &TorSettings,
    message: String,
    bootstrap_status: Option<String>,
) -> anyhow::Result<TorStatus> {
    if settings.tor_only {
        anyhow::bail!("Tor onion service startup failed: {message}");
    }
    warn!(error = %message, "Tor onion service startup failed; continuing clearnet service");
    Ok(TorStatus::startup_error(message, bootstrap_status))
}

async fn start_inner(
    settings: &TorSettings,
    paths: &RuntimePaths,
    onion_target: SocketAddr,
) -> anyhow::Result<TorStatus> {
    let state_dir = paths.tor_onion_service_dir.join("state");
    let cache_dir = paths.tor_dir.join("cache");
    create_private_dir(&paths.tor_dir)?;
    create_private_dir(&paths.tor_onion_service_dir)?;
    create_private_dir(&state_dir)?;
    create_private_dir(&cache_dir)?;

    let client_config = TorClientConfigBuilder::from_directories(&state_dir, &cache_dir)
        .build()
        .context("build Arti client configuration")?;
    let bootstrap_timeout = Duration::from_secs(settings.bootstrap_timeout_secs);
    let client = tokio::time::timeout(
        bootstrap_timeout,
        TorClient::create_bootstrapped(client_config),
    )
    .await
    .with_context(|| {
        format!(
            "Arti bootstrap timed out after {} seconds",
            settings.bootstrap_timeout_secs
        )
    })?
    .context("bootstrap Arti client")?;
    let bootstrap_status = Some(format!("{:?}", client.bootstrap_status()));

    let nickname = HsNickname::new(settings.onion_service_name.clone())
        .context("validate Arti onion service nickname")?;
    let max_streams = u32::try_from(settings.max_concurrent_streams)
        .context("tor.max_concurrent_streams does not fit in Arti stream limit")?;
    let service_config = tor_hsservice::config::OnionServiceConfigBuilder::new()
        .nickname(nickname)
        .enabled(true)
        .max_concurrent_streams_per_circuit(max_streams)
        .build()
        .context("build Arti onion service configuration")?;
    let Some((service, rend_requests)) = client
        .launch_onion_service(service_config)
        .context("launch Arti onion service")?
    else {
        anyhow::bail!("Arti onion service was disabled by configuration");
    };
    let onion_address = service
        .onion_address()
        .map(|address| address.display_unredacted().to_string());

    let inner = Arc::new(RwLock::new(TorStatusSnapshot {
        enabled: true,
        running: true,
        onion_address: onion_address.clone(),
        state: "service active; external reachability unverified".to_owned(),
        bootstrap_status,
        error: None,
        last_traffic: None,
    }));
    let runtime_slot = Arc::new(RwLock::new(None));
    let stream_task = spawn_stream_forwarder(
        tor_hsservice::handle_rend_requests(rend_requests),
        onion_target,
        Arc::downgrade(&inner),
        settings.max_concurrent_streams,
    );
    let runtime = Arc::new(TorRuntime {
        _client: client,
        service,
        stream_task,
        status: inner.clone(),
    });
    if let Ok(mut slot) = runtime_slot.write() {
        *slot = Some(runtime);
    }
    info!(
        onion = onion_address.as_deref().unwrap_or("unavailable"),
        "Arti onion service active; external reachability unverified"
    );
    Ok(TorStatus {
        inner,
        runtime: runtime_slot,
    })
}

/// Bound total onion forwarding tasks across circuits without retaining the
/// owning runtime through a status cycle.
fn spawn_stream_forwarder(
    mut stream_requests: impl futures_util::Stream<Item = tor_hsservice::StreamRequest>
    + Send
    + Unpin
    + 'static,
    onion_target: SocketAddr,
    task_status: std::sync::Weak<RwLock<TorStatusSnapshot>>,
    max_streams: usize,
) -> tokio::task::JoinHandle<()> {
    let stream_permits = Arc::new(tokio::sync::Semaphore::new(max_streams));
    tokio::spawn(async move {
        while let Some(request) = stream_requests.next().await {
            let Ok(permit) = Arc::clone(&stream_permits).try_acquire_owned() else {
                if let Err(error) = request
                    .reject(End::new_with_reason(EndReason::RESOURCELIMIT))
                    .await
                {
                    warn!(error = %error, "reject busy onion stream failed");
                }
                continue;
            };
            let task_status = task_status.clone();
            tokio::spawn(async move {
                let _permit = permit;
                match forward_stream_request(request, onion_target).await {
                    Ok(true) => {
                        if let Some(status) = task_status.upgrade()
                            && let Ok(mut snapshot) = status.write()
                        {
                            "traffic observed; current external reachability unverified"
                                .clone_into(&mut snapshot.state);
                            snapshot.last_traffic = Some(Instant::now());
                            snapshot.error = None;
                        }
                    }
                    Ok(false) => {}
                    Err(error) => {
                        let message = format!("{error:#}");
                        if let Some(status) = task_status.upgrade()
                            && let Ok(mut snapshot) = status.write()
                        {
                            snapshot.error = Some(message.clone());
                        }
                        warn!(error = %message, "Tor onion stream forwarding failed");
                    }
                }
            });
        }
        if let Some(status) = task_status.upgrade()
            && let Ok(mut snapshot) = status.write()
        {
            snapshot.running = false;
            "stopped".clone_into(&mut snapshot.state);
            snapshot.error = Some("onion stream handler closed".to_owned());
        }
    })
}

async fn forward_stream_request(
    request: tor_hsservice::StreamRequest,
    target: SocketAddr,
) -> anyhow::Result<bool> {
    if !is_supported_http_begin(request.request()) {
        request
            .reject(End::new_with_reason(EndReason::DONE))
            .await
            .context("reject unsupported onion stream")?;
        return Ok(false);
    }
    let connection = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(target))
        .await
        .unwrap_or_else(|_| {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "onion target connect timed out",
            ))
        });
    let mut local = match connection {
        Ok(stream) => stream,
        Err(error) => {
            request
                .reject(End::new_with_reason(EndReason::CONNECTREFUSED))
                .await
                .context("reject onion stream after local connect failure")?;
            anyhow::bail!("connect to onion target {target}: {error}");
        }
    };
    let mut onion = ObservedIo {
        inner: request
            .accept(Connected::new_empty())
            .await
            .context("accept onion stream")?,
        transferred: false,
    };
    match tokio::time::timeout(
        Duration::from_hours(1),
        tokio::io::copy_bidirectional(&mut onion, &mut local),
    )
    .await
    {
        Ok(Ok((received, sent))) => Ok(received > 0 || sent > 0),
        // Arti may end a successfully used stream with NotConnected/MISC
        // rather than TCP EOF. Keep the observed traffic for reachability.
        Ok(Err(error)) if is_clean_stream_close(&error) => Ok(onion.transferred),
        Ok(Err(error)) => Err(error).context("proxy onion stream to local RustPost listener"),
        Err(_) => {
            tracing::debug!("onion forwarding connection lifetime expired");
            Ok(false)
        }
    }
}

fn is_supported_http_begin(request: &IncomingStreamRequest) -> bool {
    matches!(request, IncomingStreamRequest::Begin(begin) if begin.port() == 80)
}

fn is_clean_stream_close(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::NotConnected
            | io::ErrorKind::TimedOut
            | io::ErrorKind::UnexpectedEof
    ) || error.to_string() == "Received an END cell with reason MISC"
}

fn create_private_dir(path: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(path)?;
    restrict_dir(path)?;
    Ok(())
}

fn ensure_rustls_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[cfg(unix)]
fn restrict_dir(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_dir(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod terminal_tests {
    use super::*;
    #[test]
    fn assigned_address_is_not_publication_and_forwarding_errors_take_precedence()
    -> anyhow::Result<()> {
        let status = TorStatus::starting();
        {
            let mut snapshot = status
                .inner
                .write()
                .map_err(|_| anyhow::anyhow!("test lock"))?;
            snapshot.onion_address = Some("fixture.onion".to_owned());
            snapshot.running = true;
            snapshot.error = Some("forwarding connection failed".to_owned());
        }
        let text = status.operational_summary();
        assert!(text.contains("[DEGRADED]"));
        assert!(text.contains("forwarding connection failed"));
        {
            let mut snapshot = status
                .inner
                .write()
                .map_err(|_| anyhow::anyhow!("test lock"))?;
            snapshot.error = None;
        }
        assert!(!status.operational_summary().contains("[READY]"));
        assert!(
            status
                .operational_summary()
                .contains("external reachability unverified")
        );
        Ok(())
    }
}
