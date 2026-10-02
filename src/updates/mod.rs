//! Stable release discovery and the deliberately small local updater protocol.
//! No request can specify a URL, path, command, or service name.
mod daemon;
mod release;
mod transaction;

pub use daemon::run;
pub use release::{Discovery, Release, discover, platform_target};
pub use transaction::{BackupInfo, Phase, Status};

use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::time::Duration;

pub const SOCKET: &str = "/run/rustpost-updater/control.sock";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, tag = "operation", rename_all = "snake_case")]
pub enum Request {
    Status,
    Check,
    Install {
        approval: String,
        administrator: i64,
    },
    Ready {
        version: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub status: Status,
    pub error: Option<String>,
    pub ready: bool,
}

/// The deployment supplies this environment variable; the web settings editor
/// cannot select arbitrary socket endpoints.
#[must_use]
pub fn managed() -> bool {
    cfg!(target_os = "linux")
        && std::env::var("RUSTPOST_MANAGED").is_ok_and(|value| value == "1")
        && !container_managed()
}

#[must_use]
pub fn container_managed() -> bool {
    std::env::var("RUSTPOST_CONTAINER").is_ok_and(|value| value == "1")
        || std::path::Path::new("/.dockerenv").exists()
        || std::path::Path::new("/run/.containerenv").exists()
}

pub async fn request(request: &Request) -> anyhow::Result<Reply> {
    anyhow::ensure!(
        managed(),
        "installation is managed by the deployment environment"
    );
    request_to(std::path::Path::new(SOCKET), request).await
}

/// The application constructor supplies only the fixed deployment socket.
/// Kept internal so temporary IPC sockets can test authorization without
/// changing global process environment or touching a host's actual updater.
pub(crate) async fn request_to(
    socket_path: &std::path::Path,
    request: &Request,
) -> anyhow::Result<Reply> {
    #[cfg(unix)]
    {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        tokio::time::timeout(Duration::from_secs(45), async {
            let mut socket = tokio::net::UnixStream::connect(socket_path).await?;
            let data = serde_json::to_vec(request)?;
            anyhow::ensure!(data.len() < 4096, "updater request is too large");
            socket.write_all(&data).await?;
            socket.shutdown().await?;
            let mut data = Vec::new();
            socket.take(512 * 1024 + 1).read_to_end(&mut data).await?;
            anyhow::ensure!(data.len() <= 512 * 1024, "updater response is too large");
            serde_json::from_slice(&data).map_err(Into::into)
        })
        .await?
    }
    #[cfg(not(unix))]
    {
        let _ = (socket_path, request);
        anyhow::bail!("self-update requires a managed Linux deployment")
    }
}

/// True only after the daemon has recovered and durably committed its terminal
/// journal, with no update/check worker holding the transaction lock.
pub(crate) async fn mutations_allowed(socket: Option<&std::path::Path>) -> bool {
    let Some(socket) = socket else {
        return true;
    };
    request_to(socket, &Request::Status)
        .await
        .is_ok_and(|reply| reply.error.is_none() && reply.ready)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[tokio::test]
    async fn mutation_admission_fails_closed_during_terminal_commit_or_ipc_loss() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let dir = tempfile::tempdir().expect("socket fixture");
        let socket = dir.path().join("control.sock");
        assert!(!mutations_allowed(Some(&socket)).await);
        let listener = tokio::net::UnixListener::bind(&socket).expect("socket");
        let task = tokio::spawn(async move {
            for ready in [false, true] {
                let (mut client, _) = listener.accept().await.expect("client");
                let mut input = Vec::new();
                client.read_to_end(&mut input).await.expect("request");
                let request: Request = serde_json::from_slice(&input).expect("closed request");
                assert!(matches!(request, Request::Status));
                let reply = Reply {
                    status: Status {
                        phase: Phase::Succeeded,
                        ..Status::default()
                    },
                    ready,
                    error: None,
                };
                client
                    .write_all(&serde_json::to_vec(&reply).expect("reply"))
                    .await
                    .expect("write");
            }
        });
        assert!(!mutations_allowed(Some(&socket)).await);
        assert!(mutations_allowed(Some(&socket)).await);
        task.await.expect("IPC task");
        assert!(mutations_allowed(None).await);
    }
}
