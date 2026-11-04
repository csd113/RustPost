//! Bounded event retention with an allowlist at the capture boundary.
//!
//! Arbitrary tracing fields (including errors, URLs and request data) are never
//! retained. Structured operational outcomes supply the actionable task state.
use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};

use tracing::{
    Event, Subscriber,
    field::{Field, Visit},
};
use tracing_subscriber::{EnvFilter, Layer, layer::Context, prelude::*};

const RETAINED: usize = 200;
const FILE_LIMIT: u64 = 1024 * 1024;
static STORE: OnceLock<Logs> = OnceLock::new();

#[derive(Clone, Default)]
pub struct Logs {
    inner: Arc<Mutex<LogState>>,
    interactive: Arc<AtomicBool>,
    output_gate: Arc<Mutex<()>>,
    workers: Arc<Mutex<Vec<std::thread::JoinHandle<()>>>>,
}

#[derive(Default)]
struct LogState {
    lines: VecDeque<String>,
    evicted: u64,
    dropped: u64,
    output_dropped: u64,
    disk: &'static str,
    sender: Option<mpsc::SyncSender<String>>,
    output: Option<mpsc::SyncSender<String>>,
}

pub struct LogSnapshot {
    pub lines: Vec<String>,
    pub evicted: u64,
    pub dropped: u64,
    pub output_dropped: u64,
    pub disk: &'static str,
}

impl Logs {
    pub fn snapshot(&self) -> LogSnapshot {
        self.inner.lock().map_or_else(
            |_| LogSnapshot {
                lines: Vec::new(),
                evicted: 0,
                dropped: 0,
                output_dropped: 0,
                disk: "[UNAVAILABLE] log lock",
            },
            |state| LogSnapshot {
                lines: state.lines.iter().cloned().collect(),
                evicted: state.evicted,
                dropped: state.dropped,
                output_dropped: state.output_dropped,
                disk: if state.disk.is_empty() {
                    "[UNAVAILABLE] persistence not started"
                } else {
                    state.disk
                },
            },
        )
    }

    pub fn set_interactive(&self, value: bool) {
        self.interactive.store(value, Ordering::Relaxed);
        // Wait for any preceding line write before entering the alternate screen.
        drop(self.output_gate.lock());
    }

    fn start_output(&self) -> io::Result<()> {
        let (sender, receiver) = mpsc::sync_channel::<String>(128);
        let interactive = self.interactive.clone();
        let gate = self.output_gate.clone();
        let worker = std::thread::Builder::new()
            .name("rustpost-log-output".to_owned())
            .spawn(move || {
                for line in receiver {
                    if let Ok(_owner) = gate.lock()
                        && !interactive.load(Ordering::Relaxed)
                    {
                        let _ = writeln!(io::stderr().lock(), "{line}");
                    }
                }
            })?;
        if let Ok(mut state) = self.inner.lock() {
            state.output = Some(sender);
        }
        if let Ok(mut workers) = self.workers.lock() {
            workers.push(worker);
        }
        Ok(())
    }

    /// No filesystem work takes place on the renderer/event producer thread.
    pub fn persist(&self, dir: &Path) -> io::Result<()> {
        let (sender, receiver) = mpsc::sync_channel::<String>(128);
        let dir = dir.to_owned();
        let logs = self.clone();
        if let Ok(mut state) = self.inner.lock() {
            state.sender = Some(sender);
            state.disk = "starting";
        }
        let worker = std::thread::Builder::new()
            .name("rustpost-log".to_owned())
            .spawn(move || {
                let mut file = if let Ok(file) = LogFile::open(dir) {
                    logs.disk_state("active; 1 MiB + one previous file");
                    file
                } else {
                    logs.disk_state("[FAILED] cannot open retained event log; memory tail only");
                    return;
                };
                for line in receiver {
                    if file.append(&line).is_err() {
                        logs.disk_state(
                            "[FAILED] retained event log write/rotation; memory tail only",
                        );
                        return;
                    }
                }
            })?;
        if let Ok(mut workers) = self.workers.lock() {
            workers.push(worker);
        }
        Ok(())
    }

    /// Drain bounded queues. Report an unresponsive sink instead of waiting
    /// forever on terminal/filesystem IO at process exit.
    pub async fn finish(&self) {
        if let Ok(mut state) = self.inner.lock() {
            state.sender = None;
            state.output = None;
        }
        let workers = self
            .workers
            .lock()
            .map(|mut workers| std::mem::take(&mut *workers))
            .unwrap_or_default();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        for worker in workers {
            while !worker.is_finished() && std::time::Instant::now() < deadline {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            if worker.is_finished() {
                let _ = worker.join();
            } else {
                self.disk_state(
                    "[STALE] event sink still draining; final records may be unavailable",
                );
            }
        }
    }

    fn disk_state(&self, value: &'static str) {
        if let Ok(mut state) = self.inner.lock() {
            state.disk = value;
        }
    }

    fn record(&self, line: String) {
        if let Ok(mut state) = self.inner.lock() {
            if state.lines.len() == RETAINED {
                state.lines.pop_front();
                state.evicted += 1;
            }
            if let Some(sender) = &state.sender
                && sender.try_send(line.clone()).is_err()
            {
                state.dropped += 1;
            }
            if !self.interactive.load(Ordering::Relaxed)
                && let Some(output) = &state.output
                && output.try_send(line.clone()).is_err()
            {
                state.output_dropped += 1;
            }
            state.lines.push_back(line);
        }
    }
}

struct LogFile {
    dir: PathBuf,
    file: File,
    bytes: u64,
}
impl LogFile {
    fn open(dir: PathBuf) -> io::Result<Self> {
        let path = dir.join("events.log");
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        if fs::symlink_metadata(&path).is_ok_and(|meta| !meta.is_file()) {
            return Err(io::Error::other("event log is not a regular file"));
        }
        let file = options.open(path)?;
        let bytes = file.metadata()?.len();
        Ok(Self { dir, file, bytes })
    }
    fn append(&mut self, line: &str) -> io::Result<()> {
        let size = u64::try_from(line.len()).unwrap_or(FILE_LIMIT) + 1;
        if self.bytes.saturating_add(size) > FILE_LIMIT {
            self.file.flush()?;
            // Close before rename, including on Windows.
            let replacement = self.dir.join("events.next");
            let mut options = OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            let next = options.open(&replacement)?;
            let old = std::mem::replace(&mut self.file, next);
            drop(old);
            let previous = self.dir.join("events.previous.log");
            match fs::remove_file(&previous) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            fs::rename(self.dir.join("events.log"), previous)?;
            fs::rename(replacement, self.dir.join("events.log"))?;
            self.bytes = 0;
        }
        writeln!(self.file, "{line}")?;
        self.file.flush()?;
        self.bytes = self.bytes.saturating_add(size);
        Ok(())
    }
}

#[derive(Default)]
struct SafeMessage {
    message: Option<&'static str>,
    diagnostic: Option<&'static str>,
}
fn safe_diagnostic(value: &str) -> Option<&'static str> {
    match value {
        "permission denied" => Some("permission denied"),
        "required file/directory missing" => Some("required file/directory missing"),
        "storage full" => Some("storage full"),
        "operation timed out" => Some("operation timed out"),
        "filesystem/IO operation failed" => Some("filesystem/IO operation failed"),
        "database operation failed (possibly busy/locked)" => {
            Some("database operation failed (possibly busy/locked)")
        }
        "operation failed; inspect configuration, permissions and event callsite" => {
            Some("operation failed; inspect configuration, permissions and event callsite")
        }
        _ => None,
    }
}
impl Visit for SafeMessage {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "diagnostic" {
            self.diagnostic = safe_diagnostic(value);
        }
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() != "message" {
            return;
        }
        // Compare a bounded temporary, never store arbitrary values. Display
        // formatting can contain secrets, so unknown messages are omitted too.
        let mut bounded = Bounded::default();
        let _ = std::fmt::write(&mut bounded, format_args!("{value:?}"));
        self.message = match bounded.0.as_str() {
            "runtime paths ready" => Some("runtime paths ready"),
            "RustPost listening" => Some("RustPost listening"),
            "RustPost command failed" => Some("RustPost command failed"),
            "pending media deletion recovery failed" => {
                Some("pending media deletion recovery failed")
            }
            "temp file cleanup failed" => Some("temp file cleanup failed"),
            "temp cleanup worker failed" => Some("temp cleanup worker failed"),
            "rate limit event pruning failed" => Some("rate limit event pruning failed"),
            "session pruning failed" => Some("session pruning failed"),
            "scheduled account deletion finalization failed" => {
                Some("scheduled account deletion finalization failed")
            }
            "Tor onion forwarding listener failed" => Some("Tor onion forwarding listener failed"),
            "Tor onion service startup task failed" => {
                Some("Tor onion service startup task failed")
            }
            "HTTP shutdown exceeded 30 seconds; cancelling active requests" => {
                Some("HTTP drain exceeded 30 seconds; active requests cancelled")
            }
            "automatic backup created" => Some("automatic backup created"),
            "automatic backup check failed" => Some("automatic backup check failed"),
            "automatic backup worker failed" => Some("automatic backup worker failed"),
            "Tor onion service startup failed; continuing clearnet service" => {
                Some("Tor startup failed; clearnet continues")
            }
            "Tor onion stream forwarding failed" => Some("Tor forwarding failed"),
            "ffmpeg unavailable; uploads will safely fall back to allowed originals" => {
                Some("FFmpeg unavailable; original-media fallback")
            }
            _ => None,
        };
    }
}
#[derive(Default)]
struct Bounded(zeroize::Zeroizing<String>);
impl std::fmt::Write for Bounded {
    fn write_str(&mut self, value: &str) -> std::fmt::Result {
        if self.0.len().saturating_add(value.len()) > 256 {
            return Err(std::fmt::Error);
        }
        self.0.push_str(value);
        Ok(())
    }
}

struct Capture(Logs);
impl<S: Subscriber> Layer<S> for Capture {
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        let meta = event.metadata();
        let mut message = SafeMessage::default();
        event.record(&mut message);
        self.0.record(format!(
            "{} {} {}:{} {} | {}",
            chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
            meta.level(),
            crate::terminal::display_text(meta.target()),
            meta.line().unwrap_or(0),
            message
                .message
                .unwrap_or("event recorded; diagnostic fields omitted"),
            message.diagnostic.unwrap_or("diagnostic fields omitted")
        ));
    }
}

pub fn init() -> Logs {
    STORE
        .get_or_init(|| {
            let logs = Logs::default();
            let _ = logs.start_output();
            let filter = EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("rustpost=info,tower_http=info"));
            let _ = tracing_subscriber::registry()
                .with(filter)
                .with(Capture(logs.clone()))
                .try_init();
            logs
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retained_logs_omit_secret_fields_and_bound_the_tail() {
        let logs = Logs::default();
        let subscriber = tracing_subscriber::registry().with(Capture(logs.clone()));
        tracing::subscriber::with_default(subscriber, || {
            for _ in 0..250 {
                tracing::warn!(
                    password = "secret-token-ABC",
                    error = "https://name:secret@host/\u{202e}\x1b",
                    "{}",
                    "untrusted dynamic secret-token-ABC"
                );
            }
        });
        let snapshot = logs.snapshot();
        assert_eq!(snapshot.lines.len(), 200);
        assert_eq!(snapshot.evicted, 50);
        for line in snapshot.lines {
            assert!(!line.contains("secret-token"));
            assert!(!line.contains("secret@host"));
            assert!(!line.chars().any(char::is_control));
            assert!(!line.contains('\u{202e}'));
        }
    }

    #[test]
    fn retention_rotation_stays_bounded_and_open_failure_is_explicit() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let mut log = LogFile::open(directory.path().to_owned())?;
        for _ in 0..2100 {
            log.append(&"x".repeat(512))?;
        }
        assert!(std::fs::metadata(directory.path().join("events.log"))?.len() <= FILE_LIMIT);
        assert!(
            std::fs::metadata(directory.path().join("events.previous.log"))?.len() <= FILE_LIMIT
        );
        drop(log);
        std::fs::remove_file(directory.path().join("events.log"))?;
        std::fs::create_dir(directory.path().join("events.log"))?;
        assert!(LogFile::open(directory.path().to_owned()).is_err());
        Ok(())
    }
}
