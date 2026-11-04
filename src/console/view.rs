use std::time::Duration;

use crossterm::event::KeyCode;
use ratatui::{
    Frame,
    layout::{Constraint, Layout},
    style::{Modifier, Style},
    text::{Line, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use tokio::sync::watch;

use super::{Context, metrics};
use crate::terminal::display_text;

const VIEWS: [&str; 6] = [
    "Overview",
    "Tasks",
    "Content",
    "Logs",
    "System",
    "Configuration",
];

#[derive(Default)]
pub(super) struct State {
    tab: usize,
    scroll: [u16; 7],
    help: bool,
    confirm: bool,
    pub stopping: bool,
    max_scroll: u16,
    page: u16,
}
impl State {
    pub fn key(&mut self, key: KeyCode, context: &Context, shutdown: &watch::Sender<bool>) {
        if self.confirm {
            match key {
                KeyCode::Char('y' | 'Y') | KeyCode::Enter => {
                    shutdown.send_replace(true);
                    self.stopping = true;
                    self.confirm = false;
                }
                KeyCode::Esc | KeyCode::Char('n' | 'N' | 'q' | 'Q') => self.confirm = false,
                _ => {}
            }
            return;
        }
        if matches!(key, KeyCode::Char('?' | 'h' | 'H')) {
            self.help = !self.help;
            return;
        }
        if self.help && key == KeyCode::Esc {
            self.help = false;
            return;
        }
        if self.help
            && !matches!(
                key,
                KeyCode::Up
                    | KeyCode::Down
                    | KeyCode::PageUp
                    | KeyCode::PageDown
                    | KeyCode::Home
                    | KeyCode::End
                    | KeyCode::Char('j' | 'k')
            )
        {
            return;
        }
        let slot = if self.help { 6 } else { self.tab };
        match key {
            KeyCode::Char('q' | 'Q') if !self.stopping => self.confirm = true,
            KeyCode::Char('1'..='6') => {
                if let KeyCode::Char(ch) = key {
                    self.tab = (ch as usize) - ('1' as usize);
                }
            }
            KeyCode::Tab | KeyCode::Right => self.tab = (self.tab + 1) % VIEWS.len(),
            KeyCode::BackTab | KeyCode::Left => {
                self.tab = (self.tab + VIEWS.len() - 1) % VIEWS.len()
            }
            KeyCode::Esc => self.tab = 0,
            KeyCode::Char('r' | 'R') => context.refresh.notify_one(),
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll[slot] = self.scroll[slot].saturating_add(1).min(self.max_scroll)
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll[slot] = self.scroll[slot].saturating_sub(1)
            }
            KeyCode::PageDown => {
                self.scroll[slot] = self.scroll[slot]
                    .saturating_add(self.page)
                    .min(self.max_scroll)
            }
            KeyCode::PageUp => self.scroll[slot] = self.scroll[slot].saturating_sub(self.page),
            KeyCode::Home => self.scroll[slot] = 0,
            KeyCode::End => self.scroll[slot] = self.max_scroll,
            _ => {}
        }
    }
}

pub(super) fn render(frame: &mut Frame<'_>, context: &Context, state: &mut State) {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let areas = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(if area.height >= 14 { 2 } else { 1 }),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(format!(
            "RustPost {} | {} | uptime {}s",
            env!("CARGO_PKG_VERSION"),
            VIEWS[state.tab],
            context.started.elapsed().as_secs()
        )),
        areas[0],
    );
    let navigation = if area.width >= 80 {
        "1 Overview  2 Tasks  3 Content  4 Logs  5 System  6 Configuration".to_owned()
    } else {
        format!("{} {} | Tab: next | 1-6", state.tab + 1, VIEWS[state.tab])
    };
    frame.render_widget(
        Paragraph::new(navigation).wrap(Wrap { trim: false }),
        areas[1],
    );
    let lines = if state.confirm {
        vec![
            "Stop RustPost? Y/Enter: graceful shutdown; N/Esc: return".to_owned(),
            "Admitted backups and maintenance finish before exit.".to_owned(),
        ]
    } else if state.help {
        help()
    } else {
        content(context, state.tab)
    };
    let lines = if context.ascii {
        lines
            .into_iter()
            .map(|line| crate::terminal::ascii_text(&line))
            .collect()
    } else {
        lines
    };
    let text = Text::from(lines.into_iter().map(Line::from).collect::<Vec<_>>());
    let bordered = area.width >= 44 && area.height >= 14;
    let block = if bordered {
        Block::default()
            .borders(Borders::ALL)
            .border_set(ratatui::symbols::border::Set {
                top_left: "+",
                top_right: "+",
                bottom_left: "+",
                bottom_right: "+",
                vertical_left: "|",
                vertical_right: "|",
                horizontal_top: "-",
                horizontal_bottom: "-",
            })
            .title(VIEWS[state.tab])
    } else {
        Block::default()
    };
    let inner = block.inner(areas[2]);
    let paragraph = Paragraph::new(text).wrap(Wrap { trim: false });
    let rows = paragraph.line_count(inner.width);
    let max_scroll =
        u16::try_from(rows.saturating_sub(usize::from(inner.height))).unwrap_or(u16::MAX);
    state.page = inner.height.saturating_sub(1).max(1);
    state.max_scroll = max_scroll;
    let slot = if state.help { 6 } else { state.tab };
    state.scroll[slot] = state.scroll[slot].min(max_scroll);
    let offset = if state.confirm { 0 } else { state.scroll[slot] };
    frame.render_widget(paragraph.block(block).scroll((offset, 0)), areas[2]);
    let footer = if state.stopping {
        "STOPPING: draining workers; Ctrl-C remains graceful".to_owned()
    } else if state.confirm {
        "Y: stop | Esc: return | Ctrl-C: graceful stop".to_owned()
    } else {
        format!(
            "?: help Q: stop R: refresh | Up/Down PgUp/Dn | {}/{}",
            offset.saturating_add(1),
            max_scroll.saturating_add(1)
        )
    };
    frame.render_widget(
        Paragraph::new(footer).style(Style::default().add_modifier(Modifier::BOLD)),
        areas[3],
    );
}

fn help() -> Vec<String> {
    [
        "Navigation: 1-6 or Tab/Shift-Tab; Left/Right.",
        "Up/Down or J/K scroll; PgUp/PgDn; Home/End.",
        "? or H toggles help; Esc returns to Overview.",
        "R requests one coalesced DB refresh. Storage cadence stays bounded.",
        "Q asks before stopping; Y/Enter confirms; N/Esc cancels.",
        "Ctrl-C / Ctrl-D requests the same graceful drain without confirmation.",
        "SIGINT/SIGTERM also drain. An admitted backup is not force-aborted.",
        "Health and freshness are separate: LIVE / STALE / UNAVAILABLE.",
        "Counts: every 3s, 250ms SQL budget. File sizes: every 3s.",
        "Upload scan: every 30s, 10,000 entries / 200ms cooperative limit.",
        "At small sizes, use scrolling or resize. No operation requires a hidden button.",
        "Console operations are read-only. Use the web admin/CLI for changes.",
        "--console plain/off works with redirected streams and TERM=dumb.",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn content(context: &Context, tab: usize) -> Vec<String> {
    let metrics = context
        .metrics
        .lock()
        .map(|value| value.clone())
        .unwrap_or_default();
    match tab {
        0 => overview(context, &metrics),
        1 => task_rows(context, &metrics),
        2 => content_rows(context, &metrics),
        3 => log_rows(context),
        4 => system_rows(context, &metrics),
        _ => configuration_rows(context),
    }
}

fn overview(context: &Context, metrics: &metrics::Metrics) -> Vec<String> {
    let live = context.live.lock().ok().map(|value| value.clone());
    let mut rows = Vec::new();

    rows.push(live.as_ref().map_or_else(
        || "[UNAVAILABLE] lifecycle lock".to_owned(),
        |live| live.phase.label(),
    ));
    rows.extend(endpoints(context));
    rows.push(context.requests.summary());
    rows.push(format!(
        "Counts: {}",
        metrics.counts.freshness(Duration::from_secs(10))
    ));
    if let Some(counts) = &metrics.counts.value {
        rows.push(format!(
            "Active users: {} | active posts: {} | admins: {}",
            counts.users, counts.posts, counts.admins
        ));
        rows.push(format!(
            "Media failures/fallbacks: {} | pending: {} | running: {}",
            counts.failed, counts.pending, counts.running
        ));
        rows.push(format!(
            "Maintenance mode: {}",
            if counts.maintenance {
                "enabled"
            } else {
                "disabled"
            }
        ));
    }
    rows.extend(storage(metrics));
    rows.extend(tasks(context));
    let logs = context.logs.snapshot();
    if logs.disk.contains("[FAILED]") || logs.disk.contains("[STALE]") {
        rows.push(format!("Event persistence: {}; see 4 Logs", logs.disk));
    }
    rows
}

fn task_rows(context: &Context, metrics: &metrics::Metrics) -> Vec<String> {
    let mut rows = Vec::new();

    rows.extend(tasks(context));
    rows.push(format!(
        "Media jobs: {}",
        metrics.counts.freshness(Duration::from_secs(10))
    ));
    if let Some(counts) = &metrics.counts.value {
        rows.push(format!(
            "Pending {} | running {} | succeeded {} | failed/fallback {}",
            counts.pending, counts.running, counts.succeeded, counts.failed
        ));
        rows.push(format!("Due account deletions: {}", counts.due_deletions));
        for (id, at) in &counts.recent_failures {
            rows.push(format!(
                "[FAILED/FALLBACK] media job {id} | finished {} | details: web admin Media",
                at.as_deref()
                    .map_or_else(|| "unknown".to_owned(), display_text)
            ));
        }
    }
    rows.push(
        "Task outcomes are observed in this process; prior-run outcomes are unknown.".to_owned(),
    );
    rows
}

fn content_rows(_context: &Context, metrics: &metrics::Metrics) -> Vec<String> {
    let mut rows = Vec::new();

    rows.push(format!(
        "Content: {}",
        metrics.counts.freshness(Duration::from_secs(10))
    ));
    if let Some(counts) = &metrics.counts.value {
        rows.extend([
            format!("Active users: {}", counts.users),
            format!("Active posts: {}", counts.posts),
            format!("Active administrators: {}", counts.admins),
            format!("Media records (including retained): {}", counts.media),
        ]);
    }
    rows.push(
        "Counts exclude deleted users/posts. Post content and private account data are omitted."
            .to_owned(),
    );
    rows
}

fn log_rows(context: &Context) -> Vec<String> {
    let mut rows = Vec::new();

    let logs = context.logs.snapshot();
    rows.push(format!(
        "Retained safe events: last 200 | evicted {} | disk drops {} | output drops {}",
        logs.evicted, logs.dropped, logs.output_dropped
    ));
    rows.push(format!("Persistence: {}", logs.disk));
    rows.push(format!(
        "Destination: {}",
        display_text(
            &context
                .paths
                .logs_dir
                .join("events.log")
                .display()
                .to_string()
        )
    ));
    rows.push("Only allowlisted messages and callsite metadata; diagnostic fields omitted for credential safety.".to_owned());
    rows.extend(logs.lines);
    rows
}

fn system_rows(context: &Context, metrics: &metrics::Metrics) -> Vec<String> {
    let mut rows = Vec::new();

    rows.extend(endpoints(context));
    rows.extend(storage(metrics));
    rows.push(format!(
        "Data: {}",
        display_text(&context.paths.data_dir.display().to_string())
    ));
    rows.push(format!(
        "Settings: {}",
        display_text(&context.paths.settings_path.display().to_string())
    ));
    rows.push(format!(
        "Backups: {}",
        display_text(&context.paths.backups_dir.display().to_string())
    ));
    rows.push("RSS/CPU: [UNAVAILABLE] not collected".to_owned());
    rows.push(
        context
            .ffmpeg
            .lock()
            .ok()
            .and_then(|value| {
                value.as_ref().map(|ffmpeg| {
                    format!(
                        "FFmpeg (startup probe): {}",
                        display_text(&ffmpeg.summary())
                    )
                })
            })
            .unwrap_or_else(|| "FFmpeg: [STARTING] probing".to_owned()),
    );
    rows
}

fn configuration_rows(context: &Context) -> Vec<String> {
    let mut rows = Vec::new();

    rows.push(
        "Allowlisted settings loaded at startup; listener/config changes require restart."
            .to_owned(),
    );
    rows.extend([
        format!(
            "Registration: {}",
            context.settings.accounts.registration_enabled
        ),
        format!(
            "Anonymous posting: {}",
            context.settings.accounts.anonymous_mode_enabled
        ),
        format!(
            "Tor configured: {} | Tor-only: {}",
            context.settings.tor.enabled, context.settings.tor.tor_only
        ),
        format!(
            "Automatic backup configured: {}",
            context.settings.backup.enabled && context.settings.backup.automatic_enabled
        ),
    ]);
    rows.push("Maintenance mode is read live from the database (Overview).".to_owned());
    rows.push(format!(
        "Check: {}",
        crate::terminal::instance_command(&context.paths, "check")
    ));
    rows.push("Suggested commands use POSIX sh on Unix, PowerShell on Windows.".to_owned());
    rows
}

fn endpoint_label(phase: super::Phase) -> &'static str {
    match phase {
        super::Phase::Starting(_) => "BOUND; awaiting service start",
        super::Phase::Listening => "BOUND",
        super::Phase::Stopping => "STOPPING; last bound address",
        super::Phase::Stopped => "STOPPED; last bound address",
        super::Phase::Failed(_) => "FAILED; last bound address",
    }
}

fn endpoints(context: &Context) -> Vec<String> {
    let mut rows = Vec::new();
    let live = context.live.lock().ok().map(|value| value.clone());
    if context.settings.tor.tor_only {
        rows.push("Clearnet: [DISABLED] Tor-only".to_owned());
    } else {
        rows.push(live.as_ref().map_or_else(
            || "Clearnet: [UNAVAILABLE] lifecycle".to_owned(),
            |live| {
                live.clearnet.map_or_else(
                    || format!("Clearnet: {} (listener not bound)", live.phase.label()),
                    |addr| format!("Clearnet: [{}] http://{addr}", endpoint_label(live.phase)),
                )
            },
        ));
    }
    if context.settings.tor.enabled {
        rows.push(
            live.as_ref()
                .and_then(|live| live.onion_target.map(|addr| (live.phase, addr)))
                .map_or_else(
                    || "Onion target: [UNAVAILABLE] not bound".to_owned(),
                    |(phase, target)| {
                        format!(
                            "Onion target: [{}] http://{target} (internal loopback)",
                            endpoint_label(phase)
                        )
                    },
                ),
        );
    }
    rows.push(format!(
        "Tor: {}",
        display_text(&context.tor.operational_summary())
    ));
    if context.settings.tor.enabled {
        rows.push(format!(
            "Onion address: {}",
            context.tor.onion_address().map_or_else(
                || "[UNAVAILABLE] not assigned".to_owned(),
                |addr| format!("http://{}", display_text(&addr))
            )
        ));
    }
    rows
}

fn tasks(context: &Context) -> Vec<String> {
    let tasks = context.operations.snapshot();
    if tasks.is_empty() {
        return vec!["Tasks: [UNAVAILABLE] state lock".to_owned()];
    }
    tasks
        .iter()
        .map(|task| {
            format!(
                "{}: [{}] {}; {}s ago | failures {} | last failure {}",
                task.name,
                task.state.label(),
                task.detail,
                task.changed.elapsed().as_secs(),
                task.failures,
                task.last_failure.map_or_else(
                    || "unknown/none in this process".to_owned(),
                    |at| format!("{}s ago", at.elapsed().as_secs())
                )
            )
        })
        .collect()
}

fn storage(metrics: &metrics::Metrics) -> Vec<String> {
    let mut rows = vec![format!(
        "DB file footprint: {}",
        metrics.db.freshness(Duration::from_secs(10))
    )];
    if let Some(db) = &metrics.db.value {
        rows.push(format!(
            "Main {} | WAL {} | SHM {}",
            metrics::bytes(db.main),
            metrics::bytes(db.wal),
            metrics::bytes(db.shm)
        ));
        rows.push(format!(
            "DB main+WAL+SHM: {} (file lengths; not allocated disk blocks)",
            metrics::bytes(db.main.saturating_add(db.wal).saturating_add(db.shm))
        ));
    }
    rows.push(format!(
        "Uploads: {} | {}",
        metrics
            .uploads
            .value
            .map_or_else(|| "unknown".to_owned(), metrics::bytes),
        metrics.uploads.freshness(Duration::from_secs(65))
    ));
    rows
}

pub(super) fn plain_status(context: &Context) -> String {
    let metrics = context
        .metrics
        .lock()
        .map(|value| value.clone())
        .unwrap_or_default();
    let lifecycle = context.live.lock().map_or_else(
        |_| "[UNAVAILABLE] lifecycle".to_owned(),
        |live| live.phase.label(),
    );
    let counts = metrics.counts.value.as_ref().map_or_else(
        || "users/posts/media failures unknown".to_owned(),
        |counts| {
            format!(
                "active users {} posts {} media failed/fallback {} maintenance {}",
                counts.users, counts.posts, counts.failed, counts.maintenance
            )
        },
    );
    let logs = context.logs.snapshot();
    // No ANSI, terminal control bytes, or arbitrary diagnostic fields.
    let report = format!(
        "RustPost {lifecycle} | {counts} | {}\n{}\n{}\n{}",
        metrics.counts.freshness(Duration::from_secs(10)),
        endpoints(context).join(" | "),
        storage(&metrics).join(" | "),
        tasks(context).join(" | ")
    );
    let report = format!(
        "{report}\n{} | Event sink: {} | disk drops {} | output drops {}",
        context.requests.summary(),
        logs.disk,
        logs.dropped,
        logs.output_dropped
    );
    if context.ascii {
        crate::terminal::ascii_text(&report)
    } else {
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    fn context() -> Context {
        let settings = crate::config::Settings::default();
        let mut counts = metrics::Sample::default();
        counts.update(Ok(metrics::Counts {
            users: 4,
            posts: 7,
            admins: 1,
            media: 6,
            pending: 1,
            running: 2,
            failed: 37,
            succeeded: 3,
            due_deletions: 0,
            maintenance: false,
            recent_failures: vec![(99, None)],
        }));
        Context {
            paths: crate::runtime::RuntimePaths::from_data_dir("/tmp/長い path 😀".into()),
            tor: crate::tor::validate_startup(&settings.tor),
            settings,
            ffmpeg: Arc::new(Mutex::new(None)),
            live: super::super::Live::default(),
            operations: super::super::operations::Operations::default(),
            metrics: Arc::new(Mutex::new(metrics::Metrics {
                counts,
                ..Default::default()
            })),
            logs: crate::logging::Logs::default(),
            started: Instant::now(),
            refresh: Arc::default(),
            requests: Arc::default(),
            ascii: false,
        }
    }
    fn screen(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    #[test]
    fn operational_rows_and_help_are_reachable_in_small_viewports() -> anyhow::Result<()> {
        let context = context();
        let (shutdown, _) = watch::channel(false);
        for (width, height) in [(120, 40), (80, 24), (60, 20), (44, 14), (40, 10), (20, 6)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height))?;
            let mut state = State::default();
            let mut seen = String::new();
            loop {
                terminal.draw(|frame| render(frame, &context, &mut state))?;
                seen.push_str(&screen(&terminal));
                if state.scroll[0] == state.max_scroll {
                    break;
                }
                state.key(KeyCode::Down, &context, &shutdown);
            }
            assert!(
                seen.contains("37"),
                "media failure count unreachable at {width}x{height}"
            );
            state.key(KeyCode::Char('H'), &context, &shutdown);
            terminal.draw(|frame| render(frame, &context, &mut state))?;
            state.key(KeyCode::End, &context, &shutdown);
            terminal.draw(|frame| render(frame, &context, &mut state))?;
            assert!(
                screen(&terminal).contains("TERM=dumb"),
                "help tail unreachable at {width}x{height}"
            );
            state.key(KeyCode::Char('H'), &context, &shutdown);
            assert!(!state.help);
            assert_eq!(state.tab, 0);
        }
        Ok(())
    }

    #[test]
    fn ascii_mode_contains_only_ascii_cells_in_every_destination() -> anyhow::Result<()> {
        let mut context = context();
        context.ascii = true;
        let mut terminal = Terminal::new(TestBackend::new(80, 24))?;
        for tab in 0..6 {
            let mut state = State {
                tab,
                ..Default::default()
            };
            terminal.draw(|frame| render(frame, &context, &mut state))?;
            assert!(
                screen(&terminal).is_ascii(),
                "non-ASCII cells in destination {tab}"
            );
        }
        Ok(())
    }

    #[test]
    fn stopped_endpoint_is_labeled_as_a_previous_address() -> anyhow::Result<()> {
        let context = context();
        {
            let mut live = context
                .live
                .lock()
                .map_err(|_| anyhow::anyhow!("test lifecycle lock"))?;
            live.phase = super::super::Phase::Stopped;
            live.clearnet = Some("[::1]:43210".parse()?);
        }
        let text = endpoints(&context).join(" ");
        assert!(text.contains("[STOPPED; last bound address]"));
        assert!(text.contains("http://[::1]:43210"));
        assert!(!text.contains("[BOUND]"));
        Ok(())
    }

    #[test]
    fn resizing_and_zero_dimensions_do_not_panic_or_trap_navigation() -> anyhow::Result<()> {
        let context = context();
        let (shutdown, _) = watch::channel(false);
        for width in [0, 1, 20, 40, 44, 60, 80, 120] {
            for height in [0, 1, 2, 6, 10, 14, 24, 40] {
                let mut terminal = Terminal::new(TestBackend::new(width, height))?;
                let mut state = State::default();
                for tab in 0..6 {
                    state.tab = tab;
                    terminal.draw(|frame| render(frame, &context, &mut state))?;
                    state.key(KeyCode::End, &context, &shutdown);
                    terminal.draw(|frame| render(frame, &context, &mut state))?;
                    state.key(KeyCode::Char('?'), &context, &shutdown);
                    terminal.draw(|frame| render(frame, &context, &mut state))?;
                    state.key(KeyCode::Esc, &context, &shutdown);
                    assert!(!state.help);
                }
                state.key(KeyCode::Char('Q'), &context, &shutdown);
                terminal.draw(|frame| render(frame, &context, &mut state))?;
                state.key(KeyCode::Esc, &context, &shutdown);
                assert!(!state.confirm);
            }
        }
        Ok(())
    }
}
