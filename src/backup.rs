use std::fs::{self, File};
use std::path::{Component, Path, PathBuf};

use chrono::Utc;
use tar::{Archive, Builder, EntryType};
use walkdir::WalkDir;

use crate::runtime::RuntimePaths;

pub fn create_backup(paths: &RuntimePaths, include_tor_keys: bool) -> anyhow::Result<PathBuf> {
    fs::create_dir_all(&paths.backups_dir)?;
    let archive_path = paths.backups_dir.join(format!(
        "rustpost-{}.tar",
        Utc::now().format("%Y%m%d%H%M%S%f")
    ));
    let file = File::create(&archive_path)?;
    let mut builder = Builder::new(file);
    append_file(&mut builder, &paths.database_path, "app.sqlite3")?;
    append_file(&mut builder, &paths.settings_path, "settings.toml")?;
    append_dir(&mut builder, &paths.uploads_originals, "uploads/originals")?;
    append_dir(&mut builder, &paths.uploads_images, "uploads/images")?;
    append_dir(&mut builder, &paths.uploads_videos, "uploads/videos")?;
    append_dir(&mut builder, &paths.uploads_thumbs, "uploads/thumbs")?;
    if include_tor_keys {
        append_dir(
            &mut builder,
            &paths.tor_onion_service_dir,
            "tor/onion-service",
        )?;
    }
    builder.finish()?;
    Ok(archive_path)
}

pub fn restore_backup(
    paths: &RuntimePaths,
    archive_path: &Path,
    include_tor_keys: bool,
) -> anyhow::Result<()> {
    let file = File::open(archive_path)?;
    let mut archive = Archive::new(file);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_path_buf();
        validate_archive_path(&path, include_tor_keys)?;
        let entry_type = entry.header().entry_type();
        if !matches!(entry_type, EntryType::Regular | EntryType::Directory) {
            anyhow::bail!("archive contains unsupported entry type");
        }
        let target = paths.data_dir.join(&path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        entry.unpack(&target)?;
    }
    Ok(())
}

fn append_file(builder: &mut Builder<File>, path: &Path, archive_name: &str) -> anyhow::Result<()> {
    if path.exists() {
        builder.append_path_with_name(path, archive_name)?;
    }
    Ok(())
}

fn append_dir(builder: &mut Builder<File>, dir: &Path, archive_name: &str) -> anyhow::Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in WalkDir::new(dir).follow_links(false) {
        let entry = entry?;
        if entry.file_type().is_symlink() || entry.file_type().is_dir() {
            continue;
        }
        let rel = entry.path().strip_prefix(dir)?;
        builder.append_path_with_name(entry.path(), Path::new(archive_name).join(rel))?;
    }
    Ok(())
}

pub fn validate_archive_path(path: &Path, include_tor_keys: bool) -> anyhow::Result<()> {
    if path.is_absolute() {
        anyhow::bail!("archive path is absolute");
    }
    let as_text = path.to_string_lossy();
    let lower = as_text.to_ascii_lowercase();
    if as_text.contains('\\')
        || as_text.contains(':')
        || lower.contains("%2e")
        || lower.contains("%2f")
        || lower.contains("%5c")
        || lower.contains('\u{2215}')
        || lower.contains('\u{2044}')
    {
        anyhow::bail!("archive path contains unsafe characters");
    }
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        anyhow::bail!("archive path contains traversal");
    }
    if as_text.starts_with("tor/") && !include_tor_keys {
        anyhow::bail!("archive contains Tor keys but restore did not opt in");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_path_safety() {
        assert!(validate_archive_path(Path::new("uploads/images/a.webp"), false).is_ok());
        assert!(validate_archive_path(Path::new("../settings.toml"), false).is_err());
        assert!(validate_archive_path(Path::new("/tmp/settings.toml"), false).is_err());
        assert!(validate_archive_path(Path::new("C:/settings.toml"), false).is_err());
        assert!(validate_archive_path(Path::new("uploads\\x"), false).is_err());
        assert!(validate_archive_path(Path::new("uploads/%2e%2e/settings.toml"), false).is_err());
        assert!(validate_archive_path(Path::new("tor/onion-service/key"), false).is_err());
        assert!(validate_archive_path(Path::new("tor/onion-service/key"), true).is_ok());
    }

    #[test]
    fn backup_excludes_tor_by_default_and_can_include() {
        let temp = tempfile::tempdir().expect("temp dir");
        let paths = RuntimePaths::from_data_dir(temp.path().join("data"));
        paths.ensure().expect("ensure");
        fs::write(&paths.database_path, "db").expect("db");
        fs::write(&paths.settings_path, "settings").expect("settings");
        fs::write(paths.tor_onion_service_dir.join("secret"), "key").expect("key");
        let no_tor = create_backup(&paths, false).expect("backup");
        let names = archive_names(&no_tor);
        assert!(!names.iter().any(|name| name.contains("tor/onion-service")));
        let with_tor = create_backup(&paths, true).expect("backup");
        let names = archive_names(&with_tor);
        assert!(
            names
                .iter()
                .any(|name| name.contains("tor/onion-service/secret"))
        );
    }

    fn archive_names(path: &Path) -> Vec<String> {
        let file = File::open(path).expect("archive");
        Archive::new(file)
            .entries()
            .expect("entries")
            .map(|entry| {
                entry
                    .expect("entry")
                    .path()
                    .expect("path")
                    .to_string_lossy()
                    .to_string()
            })
            .collect()
    }
}
