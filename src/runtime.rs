use std::env;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct RuntimePaths {
    pub data_dir: PathBuf,
    pub settings_path: PathBuf,
    pub database_path: PathBuf,
    pub uploads_originals: PathBuf,
    pub uploads_images: PathBuf,
    pub uploads_videos: PathBuf,
    pub uploads_thumbs: PathBuf,
    pub backups_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub tor_dir: PathBuf,
    pub tor_onion_service_dir: PathBuf,
}

impl RuntimePaths {
    pub fn discover(configured_data_dir: Option<&Path>) -> anyhow::Result<Self> {
        let data_dir = match configured_data_dir {
            Some(path) => path.to_path_buf(),
            None => exe_dir()?.join("rustpost-data"),
        };
        Ok(Self::from_data_dir(data_dir))
    }

    #[must_use]
    pub fn from_data_dir(data_dir: PathBuf) -> Self {
        Self {
            settings_path: data_dir.join("settings.toml"),
            database_path: data_dir.join("app.sqlite3"),
            uploads_originals: data_dir.join("uploads/originals"),
            uploads_images: data_dir.join("uploads/images"),
            uploads_videos: data_dir.join("uploads/videos"),
            uploads_thumbs: data_dir.join("uploads/thumbs"),
            backups_dir: data_dir.join("backups"),
            logs_dir: data_dir.join("logs"),
            tor_dir: data_dir.join("tor"),
            tor_onion_service_dir: data_dir.join("tor/onion-service"),
            data_dir,
        }
    }

    #[must_use]
    pub fn with_tor_data_dir(mut self, tor_data_dir: &str) -> Self {
        self.tor_dir = self.data_dir.join(tor_data_dir);
        self.tor_onion_service_dir = self.tor_dir.join("onion-service");
        self
    }

    pub fn ensure(&self) -> anyhow::Result<()> {
        for path in [
            &self.data_dir,
            &self.uploads_originals,
            &self.uploads_images,
            &self.uploads_videos,
            &self.uploads_thumbs,
            &self.backups_dir,
            &self.logs_dir,
            &self.tor_dir,
            &self.tor_onion_service_dir,
        ] {
            fs::create_dir_all(path)?;
        }
        restrict_dir(&self.tor_dir)?;
        restrict_dir(&self.tor_onion_service_dir)?;
        Ok(())
    }
}

#[cfg(unix)]
fn restrict_dir(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_dir(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

fn exe_dir() -> anyhow::Result<PathBuf> {
    let exe = env::current_exe()?;
    exe.parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow::anyhow!("cannot resolve executable directory"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tor_data_dir_is_resolved_under_runtime_data() {
        let paths = RuntimePaths::from_data_dir(PathBuf::from("/tmp/rustpost-data"))
            .with_tor_data_dir("privacy/tor");
        assert_eq!(
            paths.tor_dir,
            PathBuf::from("/tmp/rustpost-data/privacy/tor")
        );
        assert_eq!(
            paths.tor_onion_service_dir,
            PathBuf::from("/tmp/rustpost-data/privacy/tor/onion-service")
        );
    }
}
