use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Context as _;
use arti_client::{TorClient, config::TorClientConfigBuilder};
use futures_util::StreamExt as _;
use safelog::DisplayRedacted as _;
use tokio::net::TcpStream;
use tor_cell::relaycell::msg::{Connected, End, EndReason};
use tor_hsservice::{HsNickname, RunningOnionService};
use tor_proto::client::stream::IncomingStreamRequest;
use tracing::{info, warn};

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
}

struct TorRuntime {
    _client: Arc<TorClient<tor_rtcompat::PreferredRuntime>>,
    _service: Arc<RunningOnionService>,
    stream_task: tokio::task::JoinHandle<()>,
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

    fn record_error(&self, message: impl Into<String>) {
        if let Ok(mut snapshot) = self.inner.write() {
            snapshot.error = Some(message.into());
        }
    }

    fn record_reachable(&self) {
        if let Ok(mut snapshot) = self.inner.write() {
            "reachable".clone_into(&mut snapshot.state);
        }
    }

    fn snapshot(&self) -> TorStatusSnapshot {
        self.inner
            .read()
            .map_or_else(|_| TorStatusSnapshot::poisoned(), |guard| guard.clone())
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
    }));
    let runtime_slot = Arc::new(RwLock::new(None));
    let status = TorStatus {
        inner: Arc::clone(&inner),
        runtime: Arc::clone(&runtime_slot),
    };
    let task_status = status;
    let mut stream_requests = tor_hsservice::handle_rend_requests(rend_requests);
    let stream_task = tokio::spawn(async move {
        while let Some(request) = stream_requests.next().await {
            let task_status = task_status.clone();
            tokio::spawn(async move {
                match forward_stream_request(request, onion_target).await {
                    Ok(()) => task_status.record_reachable(),
                    Err(error) => {
                        let message = format!("{error:#}");
                        task_status.record_error(message.clone());
                        warn!(error = %message, "Tor onion stream forwarding failed");
                    }
                }
            });
        }
    });
    let runtime = Arc::new(TorRuntime {
        _client: client,
        _service: service,
        stream_task,
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

async fn forward_stream_request(
    request: tor_hsservice::StreamRequest,
    target: SocketAddr,
) -> anyhow::Result<()> {
    if !is_supported_http_begin(request.request()) {
        request
            .reject(End::new_with_reason(EndReason::DONE))
            .await
            .context("reject unsupported onion stream")?;
        return Ok(());
    }
    let mut local = match TcpStream::connect(target).await {
        Ok(stream) => stream,
        Err(error) => {
            request
                .reject(End::new_with_reason(EndReason::CONNECTREFUSED))
                .await
                .context("reject onion stream after local connect failure")?;
            anyhow::bail!("connect to onion target {target}: {error}");
        }
    };
    let mut onion = request
        .accept(Connected::new_empty())
        .await
        .context("accept onion stream")?;
    match tokio::io::copy_bidirectional(&mut onion, &mut local).await {
        Ok(_) => {}
        Err(error) if is_clean_stream_close(&error) => {}
        Err(error) => return Err(error).context("proxy onion stream to local RustPost listener"),
    }
    Ok(())
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
mod tests {
    use super::*;

    fn settings(enabled: bool, tor_only: bool) -> TorSettings {
        TorSettings {
            enabled,
            tor_only,
            data_dir: "tor".to_owned(),
            onion_service_name: "microblog".to_owned(),
            display_onion_address: String::new(),
            bootstrap_timeout_secs: 120,
            max_concurrent_streams: 512,
            include_tor_keys_in_backups_by_default: false,
        }
    }

    #[test]
    fn tor_disabled_is_ok() {
        let status = validate_startup(&settings(false, false));
        assert!(!status.enabled());
        assert!(!status.running());
        assert_eq!(status.summary(), "disabled");
    }

    #[tokio::test]
    async fn tor_disabled_start_does_not_need_internal_target() {
        let temp = tempfile::tempdir().expect("temp dir");
        let paths = RuntimePaths::from_data_dir(temp.path().join("data"));
        let status = start(&settings(false, false), &paths, None)
            .await
            .expect("disabled Tor startup");
        assert!(!status.enabled());
        assert!(!status.running());
        assert_eq!(status.summary(), "disabled");
    }

    #[test]
    fn tor_enabled_reports_pending_before_runtime_start() {
        let status = validate_startup(&settings(true, false));
        assert!(status.enabled());
        assert!(!status.running());
        assert!(status.error().is_some());
    }

    #[tokio::test]
    async fn tor_only_fails_clearly_without_internal_target() {
        let temp = tempfile::tempdir().expect("temp dir");
        let paths = RuntimePaths::from_data_dir(temp.path().join("data"));
        let err = start(&settings(true, true), &paths, None)
            .await
            .expect_err("tor_only failure");
        assert!(err.to_string().contains("Tor onion service startup failed"));
    }

    #[tokio::test]
    async fn dual_mode_reports_error_without_internal_target() {
        let temp = tempfile::tempdir().expect("temp dir");
        let paths = RuntimePaths::from_data_dir(temp.path().join("data"));
        let status = start(&settings(true, false), &paths, None)
            .await
            .expect("dual mode continues");
        assert!(status.enabled());
        assert!(!status.running());
        assert!(status.error().is_some());
    }

    #[test]
    fn status_does_not_expose_private_key_material() {
        let status = TorStatus::startup_error(
            "bootstrap failed while connecting to network".to_owned(),
            Some("BootstrapStatus".to_owned()),
        );
        let rendered = format!(
            "{} {:?} {:?}",
            status.summary(),
            status.bootstrap_status(),
            status.error()
        );
        assert!(!rendered.contains("keystore"));
        assert!(!rendered.contains("private"));
        assert!(!rendered.contains("secret"));
    }

    #[test]
    fn status_snapshot_sync_copies_later_runtime_errors() {
        let visible = TorStatus::starting();
        let running = TorStatus::new(TorStatusSnapshot {
            enabled: true,
            running: true,
            onion_address: Some("example.onion".to_owned()),
            state: "service active; external reachability unverified".to_owned(),
            bootstrap_status: Some("ready".to_owned()),
            error: None,
        });

        visible.replace_with(&running);
        running.record_error("forwarding failed");
        visible.replace_snapshot_with(&running);

        assert_eq!(visible.error().as_deref(), Some("forwarding failed"));
    }

    #[test]
    fn successful_stream_marks_status_reachable() {
        let status = TorStatus::starting();

        status.record_reachable();

        assert_eq!(status.state(), "reachable");
    }

    #[test]
    fn common_http_stream_close_errors_are_clean() {
        for kind in [
            io::ErrorKind::BrokenPipe,
            io::ErrorKind::ConnectionAborted,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::NotConnected,
            io::ErrorKind::TimedOut,
            io::ErrorKind::UnexpectedEof,
        ] {
            let error = io::Error::from(kind);
            assert!(is_clean_stream_close(&error));
        }
        assert!(is_clean_stream_close(&io::Error::other(
            "Received an END cell with reason MISC"
        )));
    }
}
