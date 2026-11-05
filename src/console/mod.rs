//! Read-only operational console and bounded line-mode fallback.
pub mod metrics;
pub mod operations;
pub mod requests;
mod view;

use std::io::{self, IsTerminal as _, Write as _};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::ValueEnum;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::sync::{Notify, watch};

use crate::{config, ffmpeg, logging, runtime, terminal, tor};

#[derive(Debug, Clone, Copy, Default, ValueEnum)]
pub enum Mode {
    #[default]
    Auto,
    Tui,
    Plain,
    Off,
}
impl Mode {
    pub fn interactive(self) -> bool {
        !matches!(self, Self::Plain | Self::Off)
            && io::stdin().is_terminal()
            && io::stdout().is_terminal()
            && !matches!(std::env::var("TERM").as_deref(), Ok("dumb"))
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Phase {
    Starting(&'static str),
    Listening,
    Stopping,
    Stopped,
    Failed(&'static str),
}
impl Phase {
    pub fn label(self) -> String {
        match self {
            Self::Starting(stage) => format!("[STARTING] {stage}"),
            Self::Listening => "[READY] listener bound; external health unverified".to_owned(),
            Self::Stopping => {
                "[STOPPING] draining HTTP and admitted backup/maintenance work".to_owned()
            }
            Self::Stopped => "[STOPPED]".to_owned(),
            Self::Failed(reason) => format!("[FAILED] {reason}"),
        }
    }
}
#[derive(Debug, Clone)]
pub struct Lifecycle {
    pub phase: Phase,
    pub clearnet: Option<SocketAddr>,
    pub onion_target: Option<SocketAddr>,
}
impl Default for Lifecycle {
    fn default() -> Self {
        Self {
            phase: Phase::Starting("opening database"),
            clearnet: None,
            onion_target: None,
        }
    }
}
pub type Live = Arc<Mutex<Lifecycle>>;

#[derive(Clone)]
pub struct Context {
    pub paths: runtime::RuntimePaths,
    pub settings: config::Settings,
    pub ffmpeg: Arc<Mutex<Option<ffmpeg::FfmpegStatus>>>,
    pub tor: tor::TorStatus,
    pub live: Live,
    pub operations: operations::Operations,
    pub metrics: metrics::Shared,
    pub logs: logging::Logs,
    pub started: Instant,
    pub refresh: Arc<Notify>,
    pub requests: Arc<requests::Requests>,
    pub ascii: bool,
}

pub struct Console {
    handle: tokio::task::JoinHandle<io::Result<()>>,
    done: Arc<std::sync::atomic::AtomicBool>,
}
impl Console {
    pub fn start(context: Context, mode: Mode, shutdown: watch::Sender<bool>) -> Self {
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopped = done.clone();
        let handle = tokio::task::spawn_blocking(move || {
            let result = if mode.interactive() {
                context.logs.set_interactive(true);
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    interactive(&context, &shutdown, &stopped)
                }))
                .unwrap_or_else(|_| Err(io::Error::other("console panicked")));
                context.logs.set_interactive(false);
                if result.is_err() {
                    let _ = writeln!(
                        io::stderr(),
                        "[UNAVAILABLE] terminal console; continuing in plain mode"
                    );
                    plain(&context, mode, &stopped)
                } else if *shutdown.borrow() && !stopped.load(std::sync::atomic::Ordering::Relaxed)
                {
                    plain(&context, mode, &stopped)
                } else {
                    result
                }
            } else {
                plain(&context, mode, &stopped)
            };
            context.logs.set_interactive(false);
            result
        });
        Self { handle, done }
    }
    pub async fn finish(self) {
        self.done.store(true, std::sync::atomic::Ordering::Relaxed);
        // Poll is bounded to 50ms; admitted workers have already been drained.
        let _ = self.handle.await;
    }
}

fn interactive(
    context: &Context,
    shutdown: &watch::Sender<bool>,
    done: &std::sync::atomic::AtomicBool,
) -> io::Result<()> {
    let _session = terminal::session::Session::enter(true)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut state = view::State::default();
    let mut draw = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .unwrap_or_else(Instant::now);
    while !done.load(std::sync::atomic::Ordering::Relaxed) {
        if *shutdown.borrow() {
            state.stopping = true;
            terminal.draw(|frame| view::render(frame, context, &mut state))?;
            return Ok(());
        }
        if draw.elapsed() >= Duration::from_secs(1) {
            terminal.draw(|frame| view::render(frame, context, &mut state))?;
            draw = Instant::now();
        }
        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        if let Event::Key(key) = event::read()? {
            if key.kind == KeyEventKind::Release {
                continue;
            }
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('c' | 'd'))
            {
                shutdown.send_replace(true);
                state.stopping = true;
            } else {
                state.key(key.code, context, shutdown);
            }
            draw = Instant::now()
                .checked_sub(Duration::from_secs(1))
                .unwrap_or_else(Instant::now);
        }
    }
    Ok(())
}

fn plain(context: &Context, mode: Mode, done: &std::sync::atomic::AtomicBool) -> io::Result<()> {
    if matches!(mode, Mode::Off) {
        return Ok(());
    }
    let mut previous = String::new();
    let mut last_report = Instant::now();
    let mut broken_pipe = false;
    while !done.load(std::sync::atomic::Ordering::Relaxed) {
        let line = view::plain_status(context);
        // State changes are prompt; changing counts are bounded to one report/3s.
        if !broken_pipe
            && (previous.is_empty()
                || line != previous && last_report.elapsed() >= Duration::from_secs(3))
        {
            if let Err(error) = writeln!(io::stderr().lock(), "{line}") {
                if matches!(
                    error.kind(),
                    io::ErrorKind::BrokenPipe | io::ErrorKind::NotConnected
                ) {
                    broken_pipe = true;
                } else {
                    return Err(error);
                }
            }
            previous = line;
            last_report = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if !broken_pipe {
        let _ = writeln!(io::stderr().lock(), "{}", view::plain_status(context));
    }
    Ok(())
}

pub fn phase(live: &Live, phase: Phase) {
    if let Ok(mut state) = live.lock() {
        state.phase = phase;
    }
}
