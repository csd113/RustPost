use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub site: SiteSettings,
    pub server: ServerSettings,
    pub accounts: AccountSettings,
    pub posts: PostSettings,
    pub media: MediaSettings,
    pub tor: TorSettings,
    pub moderation: ModerationSettings,
    pub admin: AdminSettings,
    pub backup: BackupSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SiteSettings {
    pub name: String,
}

impl Default for SiteSettings {
    fn default() -> Self {
        Self {
            name: "RustPost".to_owned(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerSettings {
    pub host: String,
    pub port: u16,
    pub public_url: String,
    pub cookie_secure: bool,
    pub trusted_proxy_cidrs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountSettings {
    pub registration_enabled: bool,
    pub anonymous_mode_enabled: bool,
    pub min_password_length: usize,
    pub max_username_len: usize,
    pub max_display_name_len: usize,
    pub max_bio_len: usize,
    pub allow_profile_banners: bool,
    pub allow_profile_pictures: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostSettings {
    pub max_text_chars: usize,
    pub max_images_per_post: usize,
    pub max_videos_per_post: usize,
    pub max_media_per_post: usize,
    pub allow_reposts: bool,
    pub allow_replies: bool,
    pub allow_likes: bool,
    pub allow_bookmarks: bool,
    pub allow_hashtags: bool,
    pub allow_mentions: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaSettings {
    pub ffmpeg_path: String,
    pub convert_images_to_webp: bool,
    pub convert_videos_to_webm: bool,
    pub keep_original_uploads: bool,
    pub max_image_size: u64,
    pub max_video_size: u64,
    pub generate_video_thumbnails: bool,
    pub allowed_image_mime_types: Vec<String>,
    pub allowed_video_mime_types: Vec<String>,
    pub webp_quality: u8,
    pub vp9_crf: u8,
    pub vp9_deadline: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorSettings {
    pub enabled: bool,
    pub tor_only: bool,
    pub data_dir: String,
    pub onion_service_name: String,
    pub bootstrap_timeout_secs: u64,
    pub max_concurrent_streams: usize,
    pub include_tor_keys_in_backups_by_default: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModerationSettings {
    pub posts_per_minute: i64,
    pub replies_per_minute: i64,
    pub reposts_per_minute: i64,
    pub account_creations_per_ip_per_day: i64,
    pub failed_login_attempts_per_15m: i64,
    pub anonymous_posts_per_ip_per_hour: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminSettings {
    pub create_admin_on_first_boot: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupSettings {
    pub enabled: bool,
    pub backup_dir: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            site: SiteSettings::default(),
            server: ServerSettings {
                host: "127.0.0.1".to_owned(),
                port: 8080,
                public_url: String::new(),
                cookie_secure: false,
                trusted_proxy_cidrs: vec!["127.0.0.1/32".to_owned(), "::1/128".to_owned()],
            },
            accounts: AccountSettings {
                registration_enabled: true,
                anonymous_mode_enabled: false,
                min_password_length: 10,
                max_username_len: 32,
                max_display_name_len: 64,
                max_bio_len: 240,
                allow_profile_banners: true,
                allow_profile_pictures: true,
            },
            posts: PostSettings {
                max_text_chars: 280,
                max_images_per_post: 4,
                max_videos_per_post: 1,
                max_media_per_post: 4,
                allow_reposts: true,
                allow_replies: true,
                allow_likes: true,
                allow_bookmarks: true,
                allow_hashtags: true,
                allow_mentions: true,
            },
            media: MediaSettings {
                ffmpeg_path: "ffmpeg".to_owned(),
                convert_images_to_webp: true,
                convert_videos_to_webm: true,
                keep_original_uploads: false,
                max_image_size: 52_428_800,
                max_video_size: 157_286_400,
                generate_video_thumbnails: true,
                allowed_image_mime_types: vec![
                    "image/jpeg".to_owned(),
                    "image/png".to_owned(),
                    "image/gif".to_owned(),
                    "image/webp".to_owned(),
                ],
                allowed_video_mime_types: vec![
                    "video/mp4".to_owned(),
                    "video/webm".to_owned(),
                    "video/quicktime".to_owned(),
                ],
                webp_quality: 82,
                vp9_crf: 32,
                vp9_deadline: "good".to_owned(),
            },
            tor: TorSettings {
                enabled: false,
                tor_only: false,
                data_dir: "tor".to_owned(),
                onion_service_name: "microblog".to_owned(),
                bootstrap_timeout_secs: 120,
                max_concurrent_streams: 512,
                include_tor_keys_in_backups_by_default: false,
            },
            moderation: ModerationSettings {
                posts_per_minute: 5,
                replies_per_minute: 10,
                reposts_per_minute: 10,
                account_creations_per_ip_per_day: 3,
                failed_login_attempts_per_15m: 10,
                anonymous_posts_per_ip_per_hour: 10,
            },
            admin: AdminSettings {
                create_admin_on_first_boot: true,
            },
            backup: BackupSettings {
                enabled: true,
                backup_dir: "backups".to_owned(),
            },
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = fs::read_to_string(path)?;
        Ok(toml::from_str(&raw)?)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        let site_name = self.site.name.trim();
        if site_name.is_empty() || site_name.len() > 80 {
            anyhow::bail!("site.name must be between 1 and 80 bytes");
        }
        if site_name != self.site.name {
            anyhow::bail!("site.name must not contain surrounding whitespace");
        }
        if self
            .site
            .name
            .chars()
            .any(|character| character.is_control())
        {
            anyhow::bail!("site.name must not contain control characters");
        }
        if self.accounts.min_password_length < 10 {
            anyhow::bail!("accounts.min_password_length must be at least 10");
        }
        if self.accounts.max_username_len == 0 || self.accounts.max_username_len > 64 {
            anyhow::bail!("accounts.max_username_len must be between 1 and 64");
        }
        if self.posts.max_text_chars == 0 || self.posts.max_text_chars > 280 {
            anyhow::bail!("posts.max_text_chars must be between 1 and 280");
        }
        if self.posts.max_media_per_post > 4 {
            anyhow::bail!("posts.max_media_per_post must be at most 4");
        }
        if self.posts.max_images_per_post > self.posts.max_media_per_post {
            anyhow::bail!("posts.max_images_per_post cannot exceed max_media_per_post");
        }
        if self.media.max_image_size > 52_428_800 {
            anyhow::bail!("media.max_image_size cannot exceed 50 MiB");
        }
        if self.media.max_video_size > 157_286_400 {
            anyhow::bail!("media.max_video_size cannot exceed 150 MiB");
        }
        if self.media.webp_quality > 100 {
            anyhow::bail!("media.webp_quality must be 0..=100");
        }
        if self.media.vp9_crf > 63 {
            anyhow::bail!("media.vp9_crf must be 0..=63");
        }
        if [
            self.moderation.posts_per_minute,
            self.moderation.replies_per_minute,
            self.moderation.reposts_per_minute,
            self.moderation.account_creations_per_ip_per_day,
            self.moderation.failed_login_attempts_per_15m,
            self.moderation.anonymous_posts_per_ip_per_hour,
        ]
        .iter()
        .any(|limit| *limit <= 0)
        {
            anyhow::bail!("moderation rate limits must be positive");
        }
        validate_relative_path(&self.tor.data_dir, "tor.data_dir")?;
        validate_relative_path(&self.backup.backup_dir, "backup.backup_dir")?;
        if self.tor.tor_only && !self.tor.enabled {
            anyhow::bail!("tor.tor_only requires tor.enabled");
        }
        validate_onion_service_name(&self.tor.onion_service_name)?;
        if !(5..=600).contains(&self.tor.bootstrap_timeout_secs) {
            anyhow::bail!("tor.bootstrap_timeout_secs must be between 5 and 600");
        }
        if self.tor.max_concurrent_streams == 0 || self.tor.max_concurrent_streams > 65_535 {
            anyhow::bail!("tor.max_concurrent_streams must be between 1 and 65535");
        }
        Ok(())
    }
}

pub fn write_default_if_missing(path: &Path) -> anyhow::Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let settings = toml::to_string_pretty(&Settings::default())?;
    fs::write(path, settings)?;
    Ok(())
}

pub fn validate_relative_path(value: &str, field: &str) -> anyhow::Result<()> {
    let path = Path::new(value);
    if value.is_empty() || path.is_absolute() {
        anyhow::bail!("{field} must be a relative path");
    }
    if path
        .components()
        .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        anyhow::bail!("{field} must not contain traversal or prefixes");
    }
    Ok(())
}

fn validate_onion_service_name(value: &str) -> anyhow::Result<()> {
    let name = value.trim();
    if name.is_empty() || name.len() > 63 {
        anyhow::bail!("tor.onion_service_name must be between 1 and 63 characters");
    }
    if name != value {
        anyhow::bail!("tor.onion_service_name must not contain surrounding whitespace");
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        anyhow::bail!(
            "tor.onion_service_name must contain only ASCII letters, numbers, '-' or '_'"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_settings_validate() {
        Settings::default().validate().expect("default config");
    }

    #[test]
    fn tor_only_requires_tor_enabled() {
        let mut settings = Settings::default();
        settings.tor.tor_only = true;
        assert!(settings.validate().is_err());
    }

    #[test]
    fn rejects_unsafe_relative_paths() {
        assert!(validate_relative_path("../tor", "x").is_err());
        assert!(validate_relative_path("/tmp/tor", "x").is_err());
        assert!(validate_relative_path("tor/onion-service", "x").is_ok());
        assert!(validate_relative_path("tor", "x").is_ok());
    }

    #[test]
    fn rejects_invalid_tor_settings() {
        let mut settings = Settings::default();
        settings.tor.onion_service_name = "bad/name".to_owned();
        assert!(settings.validate().is_err());

        let mut settings = Settings::default();
        settings.tor.bootstrap_timeout_secs = 0;
        assert!(settings.validate().is_err());

        let mut settings = Settings::default();
        settings.tor.max_concurrent_streams = 0;
        assert!(settings.validate().is_err());
    }

    #[test]
    fn site_name_is_validated_and_defaults_when_missing() {
        let settings: Settings = toml::from_str(
            r#"
            [server]
            host = "127.0.0.1"
            port = 8080
            public_url = ""
            cookie_secure = false
            trusted_proxy_cidrs = ["127.0.0.1/32"]

            [accounts]
            registration_enabled = true
            anonymous_mode_enabled = false
            min_password_length = 10
            max_username_len = 32
            max_display_name_len = 64
            max_bio_len = 240
            allow_profile_banners = true
            allow_profile_pictures = true

            [posts]
            max_text_chars = 280
            max_images_per_post = 4
            max_videos_per_post = 1
            max_media_per_post = 4
            allow_reposts = true
            allow_replies = true
            allow_likes = true
            allow_bookmarks = true
            allow_hashtags = true
            allow_mentions = true

            [media]
            ffmpeg_path = "ffmpeg"
            convert_images_to_webp = true
            convert_videos_to_webm = true
            keep_original_uploads = false
            max_image_size = 52428800
            max_video_size = 157286400
            generate_video_thumbnails = true
            allowed_image_mime_types = ["image/png"]
            allowed_video_mime_types = ["video/webm"]
            webp_quality = 82
            vp9_crf = 32
            vp9_deadline = "good"

            [tor]
            enabled = false
            tor_only = false
            data_dir = "tor"
            onion_service_name = "microblog"
            bootstrap_timeout_secs = 120
            max_concurrent_streams = 512
            include_tor_keys_in_backups_by_default = false

            [moderation]
            posts_per_minute = 5
            replies_per_minute = 10
            reposts_per_minute = 10
            account_creations_per_ip_per_day = 3
            failed_login_attempts_per_15m = 10
            anonymous_posts_per_ip_per_hour = 10

            [admin]
            create_admin_on_first_boot = true

            [backup]
            enabled = true
            backup_dir = "backups"
            "#,
        )
        .expect("settings without site");
        assert_eq!(settings.site.name, "RustPost");

        let mut settings = Settings::default();
        settings.site.name = " Custom".to_owned();
        assert!(settings.validate().is_err());
    }
}
