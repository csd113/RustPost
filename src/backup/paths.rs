//! Paths for the backup subsystem.

use super::*;

pub(super) fn validate_entry_path(
    entry: &tar::Entry<'_, File>,
    include_tor_keys: bool,
) -> anyhow::Result<String> {
    let entry_type = entry.header().entry_type();
    if !entry_type.is_file() && !entry_type.is_dir() {
        anyhow::bail!("archive contains unsupported entry type");
    }
    let raw = entry.path_bytes();
    let raw = std::str::from_utf8(raw.as_ref())
        .map_err(|_utf8| anyhow::anyhow!("archive path is not valid UTF-8"))?;
    validate_archive_path_text(raw, entry_type.is_dir(), include_tor_keys)
}

pub fn validate_archive_path(path: &Path, include_tor_keys: bool) -> anyhow::Result<()> {
    let text = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("archive path is not valid UTF-8"))?;
    validate_archive_path_text(text, false, include_tor_keys).map(|_| ())
}

pub(super) fn validate_archive_path_text(
    raw: &str,
    is_directory: bool,
    include_tor_keys: bool,
) -> anyhow::Result<String> {
    if raw.is_empty() || raw.starts_with('/') {
        anyhow::bail!("archive path is absolute or empty");
    }
    if raw.contains('\\')
        || raw.contains(':')
        || raw.contains("//")
        || raw.contains('\u{2215}')
        || raw.contains('\u{2044}')
        || raw.contains('\u{29f8}')
        || raw.contains('\u{ff0f}')
    {
        anyhow::bail!("archive path contains unsafe characters");
    }
    let lower = raw.to_ascii_lowercase();
    if lower.contains("%2e") || lower.contains("%2f") || lower.contains("%5c") {
        anyhow::bail!("archive path contains unsafe characters");
    }
    let normalized = if is_directory {
        raw.strip_suffix('/').unwrap_or(raw)
    } else {
        raw
    };
    if normalized.is_empty() || normalized.contains("//") || normalized.ends_with('/') {
        anyhow::bail!("archive path contains unsafe separators");
    }
    let path = Path::new(normalized);
    if normalized.split('/').any(|part| matches!(part, "." | ".."))
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        anyhow::bail!("archive path contains traversal");
    }
    if normalized.starts_with("tor/") && !include_tor_keys {
        anyhow::bail!("archive contains Tor keys but restore did not opt in");
    }
    Ok(normalized.to_owned())
}

pub(super) fn validate_allowed_archive_path(
    path: &str,
    include_tor_keys: bool,
) -> anyhow::Result<()> {
    if !include_tor_keys && (path == "tor" || path.starts_with("tor/")) {
        anyhow::bail!("archive contains Tor keys but restore did not opt in");
    }
    if path == MANIFEST_PATH
        || path == "settings.toml"
        || path == "db"
        || path == "db/rustpost.sqlite3"
        || path == "uploads"
        || path == "uploads/originals"
        || path.starts_with("uploads/originals/")
        || path == "uploads/images"
        || path.starts_with("uploads/images/")
        || path == "uploads/videos"
        || path.starts_with("uploads/videos/")
        || path == "uploads/thumbs"
        || path.starts_with("uploads/thumbs/")
        || path == "assets"
        || path.starts_with("assets/")
        || (include_tor_keys
            && (path == "tor"
                || path == "tor/onion-service"
                || path.starts_with("tor/onion-service/")))
    {
        return Ok(());
    }
    anyhow::bail!("archive path is outside approved runtime roots");
}

pub(super) fn format_archive_path(root: &str, rel: &Path) -> anyhow::Result<String> {
    let rel = rel
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("runtime path is not valid UTF-8"))?
        .replace(std::path::MAIN_SEPARATOR, "/");
    let archive_path = format!("{root}/{rel}");
    validate_archive_path_text(&archive_path, false, true)
}

pub(super) fn ensure_under_root(root: &Path, target: &Path) -> anyhow::Result<()> {
    let mut normalized = PathBuf::from(root);
    for component in target
        .strip_prefix(root)
        .with_context(|| "restore target is outside staging root")?
        .components()
    {
        match component {
            Component::Normal(part) => normalized.push(part),
            _ => anyhow::bail!("restore target contains traversal"),
        }
    }
    if normalized != target {
        anyhow::bail!("restore target normalization failed");
    }
    Ok(())
}

pub(super) fn required_archive_dirs(include_tor_keys: bool) -> Vec<&'static str> {
    let mut dirs = vec![
        "db",
        "uploads",
        "uploads/originals",
        "uploads/images",
        "uploads/videos",
        "uploads/thumbs",
        "assets",
    ];
    if include_tor_keys {
        dirs.push("tor");
        dirs.push("tor/onion-service");
    }
    dirs
}

pub(super) fn component_for_archive_path(path: &str) -> &str {
    if path == "db" || path.starts_with("db/") {
        "database"
    } else if path == "settings.toml" {
        "settings"
    } else if path == "assets" || path.starts_with("assets/") {
        "assets"
    } else if path == "tor" || path.starts_with("tor/") {
        "tor_keys"
    } else {
        "media"
    }
}

pub(super) fn excluded_runtime_artifact(path: &Path) -> bool {
    path.components().any(|component| {
        let Component::Normal(value) = component else {
            return true;
        };
        let name = value.to_string_lossy();
        matches!(
            name.as_ref(),
            ".DS_Store"
                | "node_modules"
                | "playwright-report"
                | "test-results"
                | ".cache"
                | "trace.zip"
        ) || name.ends_with(".tmp")
            || name.ends_with(".upload")
            || name.ends_with(".log")
            || name.ends_with(".trace")
            || name.ends_with(".har")
    })
}
