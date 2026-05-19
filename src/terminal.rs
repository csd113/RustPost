use std::fmt::Write as _;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;

use anyhow::Error;

use crate::{config, ffmpeg, runtime, tor};

const RULE: &str = "------------------------------------------------------------";

#[derive(Debug, Clone, Copy)]
pub enum Status {
    Ok,
    Warn,
    Pending,
    Off,
}

impl Status {
    const fn label(self) -> &'static str {
        match self {
            Self::Ok => "[OK]",
            Self::Warn => "[WARN]",
            Self::Pending => "[PENDING]",
            Self::Off => "[OFF]",
        }
    }
}

#[derive(Debug)]
pub struct StartupDashboard<'a> {
    pub paths: &'a runtime::RuntimePaths,
    pub settings_path: &'a Path,
    pub settings: &'a config::Settings,
    pub admin_count: i64,
    pub user_count: i64,
    pub post_count: i64,
    pub ffmpeg: &'a ffmpeg::FfmpegStatus,
    pub tor_status: &'a tor::TorStatus,
    pub onion_target: Option<SocketAddr>,
}

#[must_use]
pub fn render_startup_dashboard(input: &StartupDashboard<'_>) -> String {
    let mut out = String::with_capacity(1_800);
    push_header(&mut out, "RustPost");
    push_section(
        &mut out,
        "Status",
        &[
            row("Version", env!("CARGO_PKG_VERSION")),
            row(
                "Server",
                status_value(Status::Pending, &bind_address(&input.settings.server)),
            ),
            row(
                "Admin",
                if input.admin_count > 0 {
                    status_value(Status::Ok, "present")
                } else {
                    status_value(Status::Warn, "not created")
                },
            ),
            row(
                "Registration",
                enabled_label(input.settings.accounts.registration_enabled),
            ),
            row(
                "Anonymous posting",
                enabled_label(input.settings.accounts.anonymous_mode_enabled),
            ),
            row("Users", input.user_count.to_string()),
            row("Posts", input.post_count.to_string()),
        ],
    );
    push_section(
        &mut out,
        "Endpoints",
        &endpoint_rows(input.settings, input.tor_status, input.onion_target),
    );
    push_section(
        &mut out,
        "Storage",
        &[
            row("Data directory", display(input.paths.data_dir.as_path())),
            row("Settings", display(input.settings_path)),
            row("Database", display(input.paths.database_path.as_path())),
            row("Uploads", display(input.paths.uploads_originals.as_path())),
            row("Media", display(input.paths.uploads_images.as_path())),
            row("Logs", display(input.paths.logs_dir.as_path())),
            row("Backups", display(input.paths.backups_dir.as_path())),
        ],
    );
    push_section(
        &mut out,
        "Services",
        &[
            row("Tor", tor_status_value(input.settings, input.tor_status)),
            row("FFmpeg", ffmpeg_status_value(input.ffmpeg)),
        ],
    );
    push_section(&mut out, "Next commands", &next_command_rows(input));
    out.push_str(RULE);
    out.push('\n');
    out
}

#[must_use]
pub fn render_check(
    settings: &config::Settings,
    ffmpeg: &ffmpeg::FfmpegStatus,
    tor_status: &tor::TorStatus,
) -> String {
    let mut out = String::with_capacity(512);
    push_header(&mut out, "RustPost configuration check");
    push_section(
        &mut out,
        "Result",
        &[
            row("Configuration", status_value(Status::Ok, "valid")),
            row("Bind address", bind_address(&settings.server)),
            row("Public URL", public_url(settings)),
            row(
                "Registration",
                enabled_label(settings.accounts.registration_enabled),
            ),
            row(
                "Anonymous posting",
                enabled_label(settings.accounts.anonymous_mode_enabled),
            ),
            row("Tor", tor_status_value(settings, tor_status)),
            row("FFmpeg", ffmpeg_status_value(ffmpeg)),
        ],
    );
    push_section(
        &mut out,
        "Next commands",
        &[row("Start server", "rustpost-cli serve")],
    );
    out.push_str(RULE);
    out.push('\n');
    out
}

#[must_use]
pub fn render_init(paths: &runtime::RuntimePaths, settings_path: &Path) -> String {
    let mut out = String::with_capacity(384);
    push_header(&mut out, "RustPost initialized");
    push_section(
        &mut out,
        "Paths",
        &[
            row("Data directory", display(paths.data_dir.as_path())),
            row("Settings", display(settings_path)),
            row("Database", display(paths.database_path.as_path())),
            row("Uploads", display(paths.uploads_originals.as_path())),
            row("Logs", display(paths.logs_dir.as_path())),
            row("Backups", display(paths.backups_dir.as_path())),
        ],
    );
    push_section(
        &mut out,
        "Next commands",
        &[
            row(
                "Create admin",
                format!(
                    "rustpost-cli --data-dir {} create-admin-interactive",
                    display(paths.data_dir.as_path())
                ),
            ),
            row(
                "Start server",
                format!(
                    "rustpost-cli --data-dir {} serve",
                    display(paths.data_dir.as_path())
                ),
            ),
        ],
    );
    out.push_str(RULE);
    out.push('\n');
    out
}

#[must_use]
pub fn render_command_success(title: &str, rows: &[Row]) -> String {
    let mut out = String::with_capacity(512);
    push_header(&mut out, title);
    push_section(&mut out, "Result", rows);
    out.push_str(RULE);
    out.push('\n');
    out
}

#[must_use]
pub fn render_first_admin_setup(paths: &runtime::RuntimePaths) -> String {
    let mut out = String::with_capacity(512);
    push_header(&mut out, "Create admin account");
    push_section(
        &mut out,
        "Setup",
        &[
            row("Why", "no admin account exists"),
            row("Data directory", display(paths.data_dir.as_path())),
            row("Password input", "hidden where supported"),
        ],
    );
    out.push_str(RULE);
    out.push('\n');
    out
}

#[must_use]
pub fn render_first_admin_non_interactive(paths: &runtime::RuntimePaths) -> String {
    let mut out = String::with_capacity(640);
    push_header(&mut out, "RustPost first admin required");
    push_section(
        &mut out,
        "Setup",
        &[
            row("Status", "no admin account exists"),
            row("Input", "stdin is not interactive, so setup was skipped"),
            row(
                "Create admin",
                format!(
                    "rustpost-cli --data-dir {} create-admin-interactive",
                    display(paths.data_dir.as_path())
                ),
            ),
            row(
                "Alternative",
                format!(
                    "rustpost-cli --data-dir {} create-admin <username> <password>",
                    display(paths.data_dir.as_path())
                ),
            ),
        ],
    );
    out.push_str(RULE);
    out.push('\n');
    out
}

#[must_use]
pub fn render_error(error: &Error) -> String {
    let mut out = String::with_capacity(512);
    push_header(&mut out, error_title(error));
    push_section(&mut out, "Error", &[row("What failed", error.to_string())]);
    let mut sources = error.chain().skip(1).peekable();
    if sources.peek().is_some() {
        out.push_str("Caused by\n");
        for source in sources {
            let _ = writeln!(out, "  - {source}");
        }
        out.push('\n');
    }
    if let Some(next_step) = next_step_for_error(error) {
        push_section(&mut out, "Next step", &[row("Try", next_step)]);
    }
    out.push_str(RULE);
    out.push('\n');
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    label: String,
    value: String,
}

#[must_use]
pub fn row(label: impl Into<String>, value: impl Into<String>) -> Row {
    Row {
        label: label.into(),
        value: value.into(),
    }
}

fn push_header(out: &mut String, title: &str) {
    out.push_str(RULE);
    out.push('\n');
    let _ = writeln!(out, " {title}");
    out.push_str(RULE);
    out.push('\n');
}

fn push_section(out: &mut String, title: &str, rows: &[Row]) {
    let label_width = rows.iter().map(|row| row.label.len()).max().unwrap_or(0);
    let _ = writeln!(out, "{title}");
    for row in rows {
        let _ = writeln!(out, "  {:label_width$} : {}", row.label, row.value);
    }
    out.push('\n');
}

fn endpoint_rows(
    settings: &config::Settings,
    tor_status: &tor::TorStatus,
    onion_target: Option<SocketAddr>,
) -> Vec<Row> {
    let mut rows = vec![
        row("Local URL", local_http_url(&settings.server)),
        row("Public URL", public_url(settings)),
    ];
    if let Some(addr) = onion_target {
        rows.push(row("Onion target", format!("http://{addr}")));
    }
    match tor_status.onion_address() {
        Some(onion) => rows.push(row("Onion URL", format!("http://{onion}"))),
        None if settings.tor.enabled => rows.push(row("Onion URL", "pending")),
        None => {}
    }
    rows
}

fn next_command_rows(input: &StartupDashboard<'_>) -> Vec<Row> {
    let mut rows = Vec::with_capacity(3);
    if input.admin_count == 0 {
        rows.push(row(
            "Create admin",
            format!(
                "rustpost-cli --data-dir {} create-admin-interactive",
                display(input.paths.data_dir.as_path())
            ),
        ));
    }
    rows.push(row(
        "Check config",
        format!(
            "rustpost-cli --data-dir {} check",
            display(input.paths.data_dir.as_path())
        ),
    ));
    rows.push(row("Stop server", "Press Ctrl-C"));
    rows
}

fn bind_address(settings: &config::ServerSettings) -> String {
    format!("{}:{}", settings.host, settings.port)
}

fn local_http_url(settings: &config::ServerSettings) -> String {
    let host = settings
        .host
        .parse::<IpAddr>()
        .map_or_else(|_| settings.host.clone(), local_host_for_bind);
    format!("http://{host}:{}", settings.port)
}

fn local_host_for_bind(addr: IpAddr) -> String {
    match addr {
        IpAddr::V4(addr) if addr.is_unspecified() => "127.0.0.1".to_owned(),
        IpAddr::V6(addr) if addr.is_unspecified() => "[::1]".to_owned(),
        IpAddr::V6(addr) => format!("[{addr}]"),
        IpAddr::V4(addr) => addr.to_string(),
    }
}

fn public_url(settings: &config::Settings) -> String {
    if settings.server.public_url.trim().is_empty() {
        "(not configured)".to_owned()
    } else {
        settings.server.public_url.clone()
    }
}

fn enabled_label(enabled: bool) -> String {
    if enabled {
        status_value(Status::Ok, "enabled")
    } else {
        status_value(Status::Off, "disabled")
    }
}

fn tor_status_value(settings: &config::Settings, tor_status: &tor::TorStatus) -> String {
    if !settings.tor.enabled {
        return status_value(Status::Off, "disabled");
    }
    if tor_status.running() {
        return status_value(Status::Ok, &tor_status.summary());
    }
    if let Some(error) = tor_status.error() {
        return status_value(Status::Warn, &error);
    }
    status_value(Status::Pending, &tor_status.summary())
}

fn ffmpeg_status_value(ffmpeg: &ffmpeg::FfmpegStatus) -> String {
    if ffmpeg.available {
        status_value(Status::Ok, &ffmpeg.summary())
    } else {
        status_value(Status::Warn, &ffmpeg.summary())
    }
}

fn status_value(status: Status, value: &str) -> String {
    format!("{} {value}", status.label())
}

fn display(path: &Path) -> String {
    path.display().to_string()
}

fn next_step_for_error(error: &Error) -> Option<&'static str> {
    let text = error_text(error);
    if text.contains("address already in use") || text.contains("os error 48") {
        return Some("stop the process using this port or change server.port in settings.toml");
    }
    if text.contains("permission denied") {
        return Some("check the data directory permissions and run RustPost as the owning user");
    }
    if text.contains("settings") || text.contains("toml") || text.contains("config") {
        return Some("review settings.toml for the field named in the error");
    }
    None
}

fn error_title(error: &Error) -> &'static str {
    let text = error_text(error);
    if text.contains("bind rustpost server") || text.contains("tor onion service startup") {
        "RustPost startup failed"
    } else {
        "RustPost command failed"
    }
}

fn error_text(error: &Error) -> String {
    error
        .chain()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::{config::Settings, ffmpeg::FfmpegStatus, runtime::RuntimePaths};

    use super::*;

    #[test]
    fn startup_dashboard_groups_copyable_urls_paths_and_commands() {
        let paths = RuntimePaths::from_data_dir(PathBuf::from("/tmp/rustpost-data"));
        let settings = Settings::default();
        let ffmpeg = FfmpegStatus {
            available: false,
            version: String::new(),
            supports_webp: false,
            supports_vp9: false,
            error: Some("ffmpeg command not found".to_owned()),
        };
        let tor_status = tor::validate_startup(&settings.tor);
        let output = render_startup_dashboard(&StartupDashboard {
            paths: &paths,
            settings_path: &paths.settings_path,
            settings: &settings,
            admin_count: 0,
            user_count: 2,
            post_count: 3,
            ffmpeg: &ffmpeg,
            tor_status: &tor_status,
            onion_target: None,
        });

        assert!(output.contains("Status"));
        assert!(output.contains("Endpoints"));
        assert!(output.contains("Storage"));
        assert!(output.contains("Next commands"));
        assert!(output.contains("Local URL"));
        assert!(output.contains("http://127.0.0.1:8080"));
        assert!(output.contains("Data directory"));
        assert!(output.contains("/tmp/rustpost-data"));
        assert!(output.contains("/tmp/rustpost-data/db/rustpost.sqlite3"));
        assert!(output.contains("/tmp/rustpost-data/uploads/originals"));
        assert!(output.contains("/tmp/rustpost-data/logs"));
        assert!(output.contains("/tmp/rustpost-data/backups"));
        assert!(
            output.contains("rustpost-cli --data-dir /tmp/rustpost-data create-admin-interactive")
        );
        assert!(output.contains("FFmpeg"));
        assert!(output.contains("[WARN] unavailable"));
    }

    #[test]
    fn local_url_uses_loopback_for_unspecified_bind_addresses() {
        let mut settings = Settings::default();
        settings.server.host = "0.0.0.0".to_owned();
        assert_eq!(local_http_url(&settings.server), "http://127.0.0.1:8080");

        settings.server.host = "::".to_owned();
        assert_eq!(local_http_url(&settings.server), "http://[::1]:8080");
    }

    #[test]
    fn rendered_error_includes_actionable_port_hint() {
        let error = anyhow::anyhow!("bind RustPost server at 127.0.0.1:8080")
            .context("Address already in use");
        let output = render_error(&error);

        assert!(output.contains("RustPost startup failed"));
        assert!(output.contains("Next step"));
        assert!(output.contains("change server.port"));
    }
}
