use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

pub const DEFAULT_POST_EDIT_WINDOW_SECONDS: u64 = 15;
pub const MAX_POST_EDIT_WINDOW_SECONDS: u64 = 300;

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
    #[serde(default)]
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
    #[serde(default)]
    pub registration_captcha_enabled: bool,
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
    #[serde(default = "default_post_edit_window_seconds")]
    pub post_edit_window_seconds: u64,
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
    #[serde(default = "default_true")]
    pub nsfw_blur_enabled: bool,
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
    #[serde(default)]
    pub display_onion_address: String,
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
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub backup_dir: String,
    #[serde(default)]
    pub automatic_enabled: bool,
    #[serde(default = "default_automatic_interval_minutes")]
    pub automatic_interval_minutes: u64,
    #[serde(default = "default_retention_keep_last")]
    pub retention_keep_last: usize,
    #[serde(default = "default_retention_max_age_days")]
    pub retention_max_age_days: u64,
    #[serde(default)]
    pub automatic_include_tor_keys: bool,
}

impl Default for BackupSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            backup_dir: "backups".to_owned(),
            automatic_enabled: false,
            automatic_interval_minutes: default_automatic_interval_minutes(),
            retention_keep_last: default_retention_keep_last(),
            retention_max_age_days: default_retention_max_age_days(),
            automatic_include_tor_keys: false,
        }
    }
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
                registration_captcha_enabled: false,
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
                post_edit_window_seconds: DEFAULT_POST_EDIT_WINDOW_SECONDS,
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
                nsfw_blur_enabled: true,
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
                display_onion_address: String::new(),
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
            backup: BackupSettings::default(),
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
        if site_name != self.site.name {
            anyhow::bail!("site.name must not contain surrounding whitespace");
        }
        if self.site.name.chars().any(char::is_control) {
            anyhow::bail!("site.name must not contain control characters");
        }
        validate_relative_path(&self.tor.data_dir, "tor.data_dir")?;
        validate_relative_path(&self.backup.backup_dir, "backup.backup_dir")?;
        if self.backup.automatic_interval_minutes == 0 {
            anyhow::bail!("backup.automatic_interval_minutes must be at least 1");
        }
        if self.backup.retention_keep_last == 0 {
            anyhow::bail!("backup.retention_keep_last must be at least 1");
        }
        if self.backup.retention_keep_last > 10_000 {
            anyhow::bail!("backup.retention_keep_last is too large");
        }
        if self.backup.retention_max_age_days > 3_650 {
            anyhow::bail!("backup.retention_max_age_days must be 3650 days or less");
        }
        if self.tor.tor_only && !self.tor.enabled {
            anyhow::bail!("tor.tor_only requires tor.enabled");
        }
        validate_onion_service_name(&self.tor.onion_service_name)?;
        validate_display_onion_address(&self.tor.display_onion_address)?;
        validate_post_edit_window(self.posts.post_edit_window_seconds)?;
        Ok(())
    }
}

const fn default_post_edit_window_seconds() -> u64 {
    DEFAULT_POST_EDIT_WINDOW_SECONDS
}

fn validate_post_edit_window(seconds: u64) -> anyhow::Result<()> {
    if seconds > MAX_POST_EDIT_WINDOW_SECONDS {
        anyhow::bail!(
            "posts.post_edit_window_seconds must be {MAX_POST_EDIT_WINDOW_SECONDS} seconds or less"
        );
    }
    Ok(())
}

pub fn write_default_if_missing(path: &Path) -> anyhow::Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let settings = default_settings_toml();
    fs::write(path, settings)?;
    Ok(())
}

#[expect(
    clippy::too_many_lines,
    reason = "the generated settings template is intentionally kept in one readable TOML block"
)]
fn default_settings_toml() -> String {
    let settings = Settings::default();

    format!(
        r#"# RustPost settings
# Generated with conservative defaults. Edit values as needed, keep key names intact,
# and restart RustPost after changing this file.

# Site identity

[site]
# Display name shown in the page title, header, footer, and user-facing copy.
name = {site_name}


# HTTP listener and public access

[server]
# Address RustPost listens on. 127.0.0.1 is safest behind a reverse proxy.
host = {server_host}

# HTTP port for the RustPost listener.
port = {server_port}

# Public base URL used for absolute links. Leave empty for local or proxy-only setups.
public_url = {public_url}

# SECURITY: Set true only when users always reach RustPost over HTTPS.
# This marks session cookies as Secure.
cookie_secure = {cookie_secure}

# SECURITY: Reverse-proxy CIDRs allowed to provide forwarded client IP headers.
# Keep this list narrow; do not add public networks.
trusted_proxy_cidrs = {trusted_proxy_cidrs}


# Accounts and profiles

[accounts]
# Allow visitors to create accounts from the web UI.
registration_enabled = {registration_enabled}

# Require a CAPTCHA challenge on account creation. Login is not affected.
registration_captcha_enabled = {registration_captcha_enabled}

# Allow posting without accounts. Rate limits are keyed by client IP.
anonymous_mode_enabled = {anonymous_mode_enabled}

# Minimum account password length. Recommended default is 10.
min_password_length = {min_password_length}

# Maximum username length in bytes. Usernames are also format-validated.
max_username_len = {max_username_len}

# Maximum display-name length in bytes.
max_display_name_len = {max_display_name_len}

# Maximum profile bio length in bytes.
max_bio_len = {max_bio_len}

# Allow users to upload profile banner images.
allow_profile_banners = {allow_profile_banners}

# Allow users to upload profile picture images.
allow_profile_pictures = {allow_profile_pictures}


# Posts and interactions

[posts]
# Maximum post text length in characters.
max_text_chars = {max_text_chars}

# Seconds after creation that a user may edit their own post. Set to 0 to disable editing.
post_edit_window_seconds = {post_edit_window_seconds}

# Maximum image attachments allowed on one post.
max_images_per_post = {max_images_per_post}

# Maximum video attachments allowed on one post.
max_videos_per_post = {max_videos_per_post}

# Maximum total media attachments allowed on one post.
max_media_per_post = {max_media_per_post}

# Allow users to repost posts.
allow_reposts = {allow_reposts}

# Allow replies to posts.
allow_replies = {allow_replies}

# Allow likes on posts.
allow_likes = {allow_likes}

# Allow users to bookmark posts.
allow_bookmarks = {allow_bookmarks}

# Link hashtag text to hashtag feeds.
allow_hashtags = {allow_hashtags}

# Link @mentions to user profiles.
allow_mentions = {allow_mentions}


# Uploads and media processing

[media]
# ffmpeg executable name or path used for media conversion and thumbnails.
ffmpeg_path = {ffmpeg_path}

# Convert uploaded images to WebP when ffmpeg supports the input.
convert_images_to_webp = {convert_images_to_webp}

# Convert uploaded videos to WebM/VP9 when ffmpeg supports the input.
convert_videos_to_webm = {convert_videos_to_webm}

# SECURITY: Keep original uploaded files after conversion.
# false reduces stored untrusted file formats.
keep_original_uploads = {keep_original_uploads}

# Blur media that users or admins mark as NSFW. Safe default is true.
nsfw_blur_enabled = {nsfw_blur_enabled}

# SECURITY: Maximum accepted image upload size in bytes. Default is 50 MiB.
max_image_size = {max_image_size}

# SECURITY: Maximum accepted video upload size in bytes. Default is 150 MiB.
max_video_size = {max_video_size}

# Generate thumbnail images for uploaded videos.
generate_video_thumbnails = {generate_video_thumbnails}

# SECURITY: Accepted image MIME types after content sniffing.
# Do not add wildcard or scriptable formats.
allowed_image_mime_types = {allowed_image_mime_types}

# SECURITY: Accepted video MIME types after content sniffing.
allowed_video_mime_types = {allowed_video_mime_types}

# WebP image quality. Recommended default is 82.
webp_quality = {webp_quality}

# VP9 quality. Recommended default is 32; lower means higher quality and larger files.
vp9_crf = {vp9_crf}

# VP9 encoder deadline. Typical values are "good", "best", or "realtime".
vp9_deadline = {vp9_deadline}


# Tor onion service

[tor]
# SECURITY: Start the embedded Arti onion service.
enabled = {tor_enabled}

# SECURITY: Serve only through Tor. Requires tor.enabled = true.
tor_only = {tor_only}

# Relative directory for Tor state under the RustPost data directory.
data_dir = {tor_data_dir}

# Local name for the onion service state directory.
onion_service_name = {onion_service_name}

# Optional stable onion address to show in the site header before Arti has
# reported the active service address. Leave blank to use the runtime address.
display_onion_address = {display_onion_address}

# Seconds to wait for Tor bootstrap during Tor-only startup.
bootstrap_timeout_secs = {bootstrap_timeout_secs}

# Maximum concurrent onion streams accepted by the service.
max_concurrent_streams = {max_concurrent_streams}

# SECURITY: Include onion service keys when backups are created by default.
# Keep false unless backups are encrypted and access-controlled.
include_tor_keys_in_backups_by_default = {include_tor_keys_in_backups_by_default}


# Rate limits

[moderation]
# Posts allowed per authenticated user per minute.
posts_per_minute = {posts_per_minute}

# Replies allowed per authenticated user per minute.
replies_per_minute = {replies_per_minute}

# Reposts allowed per authenticated user per minute.
reposts_per_minute = {reposts_per_minute}

# Account creations allowed per client IP per day.
account_creations_per_ip_per_day = {account_creations_per_ip_per_day}

# Failed login attempts allowed per client IP per 15 minutes.
failed_login_attempts_per_15m = {failed_login_attempts_per_15m}

# Anonymous posts allowed per client IP per hour.
anonymous_posts_per_ip_per_hour = {anonymous_posts_per_ip_per_hour}


# Administration

[admin]
# SECURITY: Create the first admin account during first boot.
# Disable after initial setup if you want manual admin provisioning only.
create_admin_on_first_boot = {create_admin_on_first_boot}


# Backups

[backup]
# Enable the built-in tar backup command.
enabled = {backup_enabled}

# SECURITY: Relative directory for backup archives under the RustPost data directory.
backup_dir = {backup_dir}

# Create scheduled automatic backups. Manual backups remain available when backup.enabled is true.
automatic_enabled = {backup_automatic_enabled}

# Interval between automatic backup attempts. The scheduler checks periodically and runs when due.
automatic_interval_minutes = {backup_automatic_interval_minutes}

# Safe retention for automatic backups. Manual and pre-restore backups are never pruned.
retention_keep_last = {backup_retention_keep_last}

# Delete automatic backups older than this many days after retaining the newest backups.
# Set 0 to disable age-based cleanup.
retention_max_age_days = {backup_retention_max_age_days}

# SECURITY: Include onion service keys in automatic backups.
# Keep false unless automatic backup storage is encrypted and access-controlled.
automatic_include_tor_keys = {backup_automatic_include_tor_keys}
"#,
        site_name = toml_string(&settings.site.name),
        server_host = toml_string(&settings.server.host),
        server_port = settings.server.port,
        public_url = toml_string(&settings.server.public_url),
        cookie_secure = settings.server.cookie_secure,
        trusted_proxy_cidrs = toml_string_array(&settings.server.trusted_proxy_cidrs),
        registration_enabled = settings.accounts.registration_enabled,
        registration_captcha_enabled = settings.accounts.registration_captcha_enabled,
        anonymous_mode_enabled = settings.accounts.anonymous_mode_enabled,
        min_password_length = settings.accounts.min_password_length,
        max_username_len = settings.accounts.max_username_len,
        max_display_name_len = settings.accounts.max_display_name_len,
        max_bio_len = settings.accounts.max_bio_len,
        allow_profile_banners = settings.accounts.allow_profile_banners,
        allow_profile_pictures = settings.accounts.allow_profile_pictures,
        max_text_chars = settings.posts.max_text_chars,
        post_edit_window_seconds = settings.posts.post_edit_window_seconds,
        max_images_per_post = settings.posts.max_images_per_post,
        max_videos_per_post = settings.posts.max_videos_per_post,
        max_media_per_post = settings.posts.max_media_per_post,
        allow_reposts = settings.posts.allow_reposts,
        allow_replies = settings.posts.allow_replies,
        allow_likes = settings.posts.allow_likes,
        allow_bookmarks = settings.posts.allow_bookmarks,
        allow_hashtags = settings.posts.allow_hashtags,
        allow_mentions = settings.posts.allow_mentions,
        ffmpeg_path = toml_string(&settings.media.ffmpeg_path),
        convert_images_to_webp = settings.media.convert_images_to_webp,
        convert_videos_to_webm = settings.media.convert_videos_to_webm,
        keep_original_uploads = settings.media.keep_original_uploads,
        nsfw_blur_enabled = settings.media.nsfw_blur_enabled,
        max_image_size = settings.media.max_image_size,
        max_video_size = settings.media.max_video_size,
        generate_video_thumbnails = settings.media.generate_video_thumbnails,
        allowed_image_mime_types = toml_string_array(&settings.media.allowed_image_mime_types),
        allowed_video_mime_types = toml_string_array(&settings.media.allowed_video_mime_types),
        webp_quality = settings.media.webp_quality,
        vp9_crf = settings.media.vp9_crf,
        vp9_deadline = toml_string(&settings.media.vp9_deadline),
        tor_enabled = settings.tor.enabled,
        tor_only = settings.tor.tor_only,
        tor_data_dir = toml_string(&settings.tor.data_dir),
        onion_service_name = toml_string(&settings.tor.onion_service_name),
        display_onion_address = toml_string(&settings.tor.display_onion_address),
        bootstrap_timeout_secs = settings.tor.bootstrap_timeout_secs,
        max_concurrent_streams = settings.tor.max_concurrent_streams,
        include_tor_keys_in_backups_by_default =
            settings.tor.include_tor_keys_in_backups_by_default,
        posts_per_minute = settings.moderation.posts_per_minute,
        replies_per_minute = settings.moderation.replies_per_minute,
        reposts_per_minute = settings.moderation.reposts_per_minute,
        account_creations_per_ip_per_day = settings.moderation.account_creations_per_ip_per_day,
        failed_login_attempts_per_15m = settings.moderation.failed_login_attempts_per_15m,
        anonymous_posts_per_ip_per_hour = settings.moderation.anonymous_posts_per_ip_per_hour,
        create_admin_on_first_boot = settings.admin.create_admin_on_first_boot,
        backup_enabled = settings.backup.enabled,
        backup_dir = toml_string(&settings.backup.backup_dir),
        backup_automatic_enabled = settings.backup.automatic_enabled,
        backup_automatic_interval_minutes = settings.backup.automatic_interval_minutes,
        backup_retention_keep_last = settings.backup.retention_keep_last,
        backup_retention_max_age_days = settings.backup.retention_max_age_days,
        backup_automatic_include_tor_keys = settings.backup.automatic_include_tor_keys,
    )
}

fn toml_string(value: &str) -> String {
    toml::Value::String(value.to_owned()).to_string()
}

fn toml_string_array(values: &[String]) -> String {
    if values.is_empty() {
        return "[]".to_owned();
    }

    let mut output = "[\n".to_owned();
    for value in values {
        output.push_str("    ");
        output.push_str(&toml_string(value));
        output.push_str(",\n");
    }
    output.push(']');
    output
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

fn validate_display_onion_address(value: &str) -> anyhow::Result<()> {
    if value.is_empty() {
        return Ok(());
    }
    if value.trim() != value {
        anyhow::bail!("tor.display_onion_address must not contain surrounding whitespace");
    }
    let Some(service_id) = value.strip_suffix(".onion") else {
        anyhow::bail!("tor.display_onion_address must end with .onion");
    };
    if service_id.len() != 56 {
        anyhow::bail!("tor.display_onion_address must be a v3 onion address");
    }
    if !service_id
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || (b'2'..=b'7').contains(&byte))
    {
        anyhow::bail!("tor.display_onion_address must contain only lowercase base32 characters");
    }
    Ok(())
}

const fn default_true() -> bool {
    true
}

const fn default_automatic_interval_minutes() -> u64 {
    1_440
}

const fn default_retention_keep_last() -> usize {
    10
}

const fn default_retention_max_age_days() -> u64 {
    30
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_settings_validate() {
        Settings::default().validate().expect("default config");
    }

    #[test]
    fn generated_default_settings_toml_parses_and_preserves_defaults() {
        let generated = default_settings_toml();
        let parsed: Settings = toml::from_str(&generated).expect("generated config parses");

        assert_eq!(
            toml::to_string(&parsed).expect("parsed settings serialize"),
            toml::to_string(&Settings::default()).expect("default settings serialize")
        );
        parsed.validate().expect("generated config validates");
    }

    #[test]
    fn missing_nsfw_blur_setting_defaults_to_safe_enabled() {
        let raw = toml::to_string(&Settings::default())
            .expect("settings toml")
            .lines()
            .filter(|line| !line.trim_start().starts_with("nsfw_blur_enabled"))
            .collect::<Vec<_>>()
            .join("\n");
        let parsed: Settings = toml::from_str(&raw).expect("legacy settings parse");

        assert!(parsed.media.nsfw_blur_enabled);
    }

    #[test]
    fn missing_display_onion_address_defaults_to_blank() {
        let raw = toml::to_string(&Settings::default())
            .expect("settings toml")
            .lines()
            .filter(|line| !line.trim_start().starts_with("display_onion_address"))
            .collect::<Vec<_>>()
            .join("\n");
        let parsed: Settings = toml::from_str(&raw).expect("legacy settings parse");

        assert!(parsed.tor.display_onion_address.is_empty());
    }

    #[test]
    fn missing_post_edit_window_defaults_to_fifteen_seconds() {
        let raw = toml::to_string(&Settings::default())
            .expect("settings toml")
            .lines()
            .filter(|line| !line.trim_start().starts_with("post_edit_window_seconds"))
            .collect::<Vec<_>>()
            .join("\n");
        let parsed: Settings = toml::from_str(&raw).expect("legacy settings parse");

        assert_eq!(
            parsed.posts.post_edit_window_seconds,
            DEFAULT_POST_EDIT_WINDOW_SECONDS
        );
        parsed.validate().expect("defaulted setting validates");
    }

    #[test]
    fn missing_backup_schedule_settings_default_safely_disabled() {
        let raw = toml::to_string(&Settings::default())
            .expect("settings toml")
            .lines()
            .filter(|line| {
                ![
                    "automatic_enabled",
                    "automatic_interval_minutes",
                    "retention_keep_last",
                    "retention_max_age_days",
                    "automatic_include_tor_keys",
                ]
                .iter()
                .any(|key| line.trim_start().starts_with(key))
            })
            .collect::<Vec<_>>()
            .join("\n");
        let parsed: Settings = toml::from_str(&raw).expect("legacy backup settings parse");

        assert!(!parsed.backup.automatic_enabled);
        assert_eq!(parsed.backup.automatic_interval_minutes, 1_440);
        assert_eq!(parsed.backup.retention_keep_last, 10);
        assert_eq!(parsed.backup.retention_max_age_days, 30);
        assert!(!parsed.backup.automatic_include_tor_keys);
        parsed
            .validate()
            .expect("defaulted backup settings validate");
    }

    #[test]
    fn rejects_unsafe_backup_retention_settings() {
        let mut settings = Settings::default();
        settings.backup.automatic_interval_minutes = 0;
        assert!(settings.validate().is_err());

        let mut settings = Settings::default();
        settings.backup.retention_keep_last = 0;
        assert!(settings.validate().is_err());
    }

    #[test]
    fn accepts_zero_post_edit_window_as_disabled() {
        let mut settings = Settings::default();
        settings.posts.post_edit_window_seconds = 0;

        settings.validate().expect("zero disables post editing");
    }

    #[test]
    fn rejects_unsafe_post_edit_windows() {
        let mut settings = Settings::default();

        settings.posts.post_edit_window_seconds = MAX_POST_EDIT_WINDOW_SECONDS + 1;
        assert!(settings.validate().is_err());
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
    }

    #[test]
    fn validates_display_onion_address() {
        let mut settings = Settings::default();
        settings.tor.display_onion_address =
            "abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx.onion".to_owned();
        settings.validate().expect("valid display onion address");

        settings.tor.display_onion_address = "examplehiddenservice.onion".to_owned();
        assert!(settings.validate().is_err());

        settings.tor.display_onion_address =
            "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567ABCDEFGHIJKLMNOPQRSTUVWX.onion".to_owned();
        assert!(settings.validate().is_err());
    }

    #[test]
    fn allows_operator_chosen_numeric_limits() {
        let mut settings = Settings::default();
        settings.accounts.min_password_length = 0;
        settings.accounts.max_username_len = 0;
        settings.posts.max_text_chars = 0;
        settings.posts.max_media_per_post = 99;
        settings.posts.max_images_per_post = 99;
        settings.media.max_image_size = 0;
        settings.media.max_video_size = 0;
        settings.moderation.posts_per_minute = 0;
        settings.tor.bootstrap_timeout_secs = 0;
        settings.tor.max_concurrent_streams = 0;

        settings
            .validate()
            .expect("operator-chosen limits validate");
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
            registration_captcha_enabled = false
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
        assert!(!settings.accounts.registration_captcha_enabled);

        let mut settings = Settings::default();
        settings.site.name = " Custom".to_owned();
        assert!(settings.validate().is_err());
    }

    #[test]
    fn registration_captcha_defaults_disabled_when_missing() {
        let generated = default_settings_toml();
        let without_captcha = generated
            .lines()
            .filter(|line| {
                !line
                    .trim_start()
                    .starts_with("registration_captcha_enabled")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let settings: Settings = toml::from_str(&without_captcha).expect("legacy settings parse");

        assert!(!settings.accounts.registration_captcha_enabled);
    }
}
