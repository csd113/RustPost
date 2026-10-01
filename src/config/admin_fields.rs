//! Typed contract between the configuration model and web administration.
//! Every accessor names a real Settings field; the inventory test detects additions.
use super::Settings;
use anyhow::Context as _;
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeepSettingsInputKind {
    Text,
    Url,
    Number,
    Size,
    Boolean,
    List,
    Encoding,
}

pub const NON_WEB_SETTINGS: [(&str, &str); 4] = [
    (
        "media.ffmpeg_path",
        "Executable selection permits process execution with the server's privileges; deployment managed.",
    ),
    (
        "tor.data_dir",
        "Moving private onion identity storage requires a coordinated filesystem migration; changing a path alone can lose the identity.",
    ),
    (
        "backup.backup_dir",
        "Moving backup storage requires a coordinated filesystem migration; runtime paths are fixed at startup.",
    ),
    (
        "tor.include_tor_keys_in_backups_by_default",
        "Validation requires false. Private keys require explicit consent on each manual backup or the separately configured automatic-backup policy.",
    ),
];

macro_rules! admin_fields {
    ($( $variant:ident, $name:ident: $ty:ty, $section:ident.$key:ident, $label:literal, $helper:literal, $kind:ident, [$($attr:meta),*]; )*) => {
        #[derive(Debug, Clone, Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct DeepSettingsForm {
            pub csrf: String,
            pub intent: Option<String>,
            pub revision: Option<String>,
            $( $(#[$attr])* pub $name: String,)*
        }
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum DeepSettingsField { $($variant,)* }
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct DeepSettingsValues { $(pub $name: $ty,)* }
        impl DeepSettingsField {
            pub const ALL: [Self; 61] = [$(Self::$variant,)*];
            #[must_use]
            pub const fn toml_section(self) -> &'static str { match self { $(Self::$variant => stringify!($section),)* } }
            #[must_use]
            pub const fn toml_key(self) -> &'static str { match self { $(Self::$variant => stringify!($key),)* } }
            #[must_use]
            pub const fn form_name(self) -> &'static str { match self { $(Self::$variant => stringify!($name),)* } }
            #[must_use]
            pub const fn label(self) -> &'static str { match self { $(Self::$variant => $label,)* } }
            #[must_use]
            pub const fn helper(self) -> Option<&'static str> { match self { $(Self::$variant => Some($helper),)* } }
            #[must_use]
            pub const fn input_kind(self) -> DeepSettingsInputKind { match self { $(Self::$variant => DeepSettingsInputKind::$kind,)* } }
        }
        impl DeepSettingsValues {
            #[must_use]
            pub fn from_settings(settings: &Settings) -> Self { Self { $($name: SettingsValue::copy_value(&settings.$section.$key),)* } }
            #[must_use]
            pub fn apply_to(&self, current: &Settings) -> Settings {
                let mut updated = current.clone();
                $(updated.$section.$key = SettingsValue::copy_value(&self.$name);)*
                updated
            }
            #[must_use]
            pub fn form_value(&self, field: DeepSettingsField) -> String { match field { $(DeepSettingsField::$variant => self.$name.formatted(field.input_kind()),)* } }
        }
        impl DeepSettingsForm {
            #[must_use]
            pub fn from_settings(settings: &Settings) -> Self {
                let values = DeepSettingsValues::from_settings(settings);
                Self { csrf: String::new(), revision: None, intent: Some("preview".to_owned()), $($name: values.form_value(DeepSettingsField::$variant),)* }
            }
            #[must_use]
            pub fn submitted_value(&self, field: DeepSettingsField) -> &str { match field { $(DeepSettingsField::$variant => &self.$name,)* } }
        }
        pub fn parse_deep_settings_form(form: &DeepSettingsForm, current: &Settings) -> anyhow::Result<DeepSettingsValues> {
            let values = DeepSettingsValues { $($name: SettingsValue::parse(&form.$name, DeepSettingsField::$variant)?,)* };
            values.apply_to(current).validate().map_err(|error| {
                let message = error.to_string();
                for field in DeepSettingsField::ALL {
                    let key = format!("{}.{}", field.toml_section(), field.toml_key());
                    if let Some(rest) = message.strip_prefix(&key) {
                        return anyhow::anyhow!("{}{rest}", field.label());
                    }
                }
                error
            })?;
            Ok(values)
        }
    }
}

admin_fields! {
    SiteName, site_name: String, site.name, "Site name", "Shown in page titles, the header, and the footer. Empty hides the display name.", Text, [];
    RegistrationEnabled, registration_enabled: bool, accounts.registration_enabled, "Registration enabled", "Allow this feature.", Boolean, [serde(default = "unchecked")];
    RegistrationCaptchaEnabled, registration_captcha_enabled: bool, accounts.registration_captcha_enabled, "Registration CAPTCHA enabled", "Allow this feature.", Boolean, [serde(default = "unchecked")];
    AnonymousModeEnabled, anonymous_mode_enabled: bool, accounts.anonymous_mode_enabled, "Anonymous posting enabled", "Allow this feature.", Boolean, [serde(default = "unchecked")];
    MinPasswordLength, min_password_length: usize, accounts.min_password_length, "Minimum password length", "Characters. Recommended default is 10; 0 permits empty passwords.", Number, [];
    MaxUsernameLen, max_username_len: usize, accounts.max_username_len, "Maximum username length", "Characters.", Number, [];
    MaxDisplayNameLen, max_display_name_len: usize, accounts.max_display_name_len, "Maximum display name length", "Characters.", Number, [];
    MaxBioLen, max_bio_len: usize, accounts.max_bio_len, "Maximum bio length", "Characters.", Number, [];
    AllowProfileBanners, allow_profile_banners: bool, accounts.allow_profile_banners, "Allow profile banners", "Allow this feature.", Boolean, [serde(default = "unchecked")];
    AllowProfilePictures, allow_profile_pictures: bool, accounts.allow_profile_pictures, "Allow profile pictures", "Allow this feature.", Boolean, [serde(default = "unchecked")];
    DeletionGracePeriodDays, deletion_grace_period_days: u64, accounts.deletion_grace_period_days, "Account deletion grace period", "Days, 0–3650. Set 0 for immediate deletion.", Number, [];
    MaxArchiveUploadBytes, max_archive_upload_bytes: u64, accounts.max_archive_upload_bytes, "Maximum compressed account archive", "MiB. Applies to imports and exports; 0.0625–2048 MiB.", Size, [];
    MaxArchiveExpandedBytes, max_archive_expanded_bytes: u64, accounts.max_archive_expanded_bytes, "Maximum expanded account archive", "MiB of decompressed media; 0.0625–16384 MiB.", Size, [];
    MaxArchiveEntries, max_archive_entries: usize, accounts.max_archive_entries, "Maximum archive entries", "Tar entries per import, 1–100000.", Number, [];
    MaxTextChars, max_text_chars: usize, posts.max_text_chars, "Maximum post text length", "Characters.", Number, [];
    PostEditWindowSeconds, post_edit_window_seconds: u64, posts.post_edit_window_seconds, "Post edit window", "Seconds, 0–300. Set 0 to disable editing.", Number, [];
    MaxImagesPerPost, max_images_per_post: usize, posts.max_images_per_post, "Maximum images per post", "Attachments per post.", Number, [];
    MaxVideosPerPost, max_videos_per_post: usize, posts.max_videos_per_post, "Maximum videos per post", "Attachments per post.", Number, [];
    MaxMediaPerPost, max_media_per_post: usize, posts.max_media_per_post, "Maximum total media per post", "Attachments per post.", Number, [];
    AllowReposts, allow_reposts: bool, posts.allow_reposts, "Allow reposts", "Allow this feature.", Boolean, [serde(default = "unchecked")];
    AllowReplies, allow_replies: bool, posts.allow_replies, "Allow replies", "Allow this feature.", Boolean, [serde(default = "unchecked")];
    AllowLikes, allow_likes: bool, posts.allow_likes, "Allow likes", "Allow this feature.", Boolean, [serde(default = "unchecked")];
    AllowBookmarks, allow_bookmarks: bool, posts.allow_bookmarks, "Allow bookmarks", "Allow this feature.", Boolean, [serde(default = "unchecked")];
    AllowHashtags, allow_hashtags: bool, posts.allow_hashtags, "Allow hashtags", "Reserved: hashtag extraction currently operates regardless of this flag.", Boolean, [serde(default = "unchecked")];
    AllowMentions, allow_mentions: bool, posts.allow_mentions, "Allow mentions", "Allow this feature.", Boolean, [serde(default = "unchecked")];
    NsfwBlurEnabled, nsfw_blur_enabled: bool, media.nsfw_blur_enabled, "Blur NSFW media", "Blur flagged media unless a user disables their own blur setting.", Boolean, [serde(default = "unchecked")];
    MaxImageSizeMb, max_image_size_mb: u64, media.max_image_size, "Maximum image size", "MiB per image (1 MiB = 1,048,576 bytes). Decimal values are accepted.", Size, [];
    MaxVideoSizeMb, max_video_size_mb: u64, media.max_video_size, "Maximum video size", "MiB per video (1 MiB = 1,048,576 bytes). Decimal values are accepted.", Size, [];
    ConvertImagesToWebp, convert_images_to_webp: bool, media.convert_images_to_webp, "Convert images to WebP", "Convert accepted images using the existing image processor.", Boolean, [serde(default = "unchecked")];
    ConvertVideosToWebm, convert_videos_to_webm: bool, media.convert_videos_to_webm, "Convert videos to WebM", "Convert accepted videos using FFmpeg.", Boolean, [serde(default = "unchecked")];
    KeepOriginalUploads, keep_original_uploads: bool, media.keep_original_uploads, "Keep original uploads", "Retain original uploads in addition to converted media.", Boolean, [serde(default = "unchecked")];
    GenerateVideoThumbnails, generate_video_thumbnails: bool, media.generate_video_thumbnails, "Generate video thumbnails", "Use FFmpeg to extract video previews.", Boolean, [serde(default = "unchecked")];
    AllowedImageMimeTypes, allowed_image_mime_types: Vec<String>, media.allowed_image_mime_types, "Allowed image formats", "One MIME type per line, for example image/jpeg. Empty disables image uploads.", List, [];
    AllowedVideoMimeTypes, allowed_video_mime_types: Vec<String>, media.allowed_video_mime_types, "Allowed video formats", "One MIME type per line, for example video/mp4. Empty disables video uploads.", List, [];
    WebpQuality, webp_quality: u8, media.webp_quality, "WebP quality", "Image quality, 0–100. Higher values produce larger files.", Number, [];
    Vp9Crf, vp9_crf: u8, media.vp9_crf, "Video quality", "VP9 constant quality, 0–63. Lower values produce larger files.", Number, [];
    Vp9Deadline, vp9_deadline: String, media.vp9_deadline, "Video encoding speed", "Best prioritizes quality; good balances speed; realtime prioritizes speed.", Encoding, [];
    Host, host: String, server.host, "Listener address", "IP address to listen on. A restart can disconnect this console.", Text, [];
    Port, port: u16, server.port, "Listener port", "TCP port, 0–65535. Port 0 asks the OS to choose a port. Restart required.", Number, [];
    PublicUrl, public_url: String, server.public_url, "Public URL", "Optional HTTP(S) URL shown in the startup dashboard. RustPost currently generates relative links regardless of this value.", Url, [];
    CookieSecure, cookie_secure: bool, server.cookie_secure, "Secure session cookies", "Enable only when visitors always use HTTPS.", Boolean, [serde(default = "unchecked")];
    TrustedProxyCidrs, trusted_proxy_cidrs: Vec<String>, server.trusted_proxy_cidrs, "Trusted reverse proxies", "One IP/CIDR per line. Reserved: RustPost currently ignores forwarded IP headers and uses the direct connection address.", List, [];
    PostsPerMinute, posts_per_minute: i64, moderation.posts_per_minute, "Post rate limit", "Posts per account per minute; values of 0 or less block posting.", Number, [];
    RepliesPerMinute, replies_per_minute: i64, moderation.replies_per_minute, "Reply rate limit", "Replies per account per minute; values of 0 or less block replies.", Number, [];
    RepostsPerMinute, reposts_per_minute: i64, moderation.reposts_per_minute, "Repost rate limit", "Reposts per account per minute; values of 0 or less block reposts.", Number, [];
    AccountCreationsPerIpPerDay, account_creations_per_ip_per_day: i64, moderation.account_creations_per_ip_per_day, "Registration rate limit", "Registrations per client IP per day; values of 0 or less block registration.", Number, [];
    FailedLoginAttemptsPer15M, failed_login_attempts_per_15m: i64, moderation.failed_login_attempts_per_15m, "Failed login rate limit", "Failed logins per client IP per 15 minutes; values of 0 or less block login.", Number, [];
    AnonymousPostsPerIpPerHour, anonymous_posts_per_ip_per_hour: i64, moderation.anonymous_posts_per_ip_per_hour, "Anonymous posting rate limit", "Anonymous posts per client IP per hour; values of 0 or less block posting.", Number, [];
    TorEnabled, tor_enabled: bool, tor.enabled, "Enable onion service", "Start the embedded Arti onion service on restart.", Boolean, [serde(default = "unchecked")];
    TorOnly, tor_only: bool, tor.tor_only, "Onion access only", "Disable the public HTTP listener. Requires the onion service; ensure onion access before restarting.", Boolean, [serde(default = "unchecked")];
    OnionServiceName, onion_service_name: String, tor.onion_service_name, "Onion service identity name", "Validated local name. Changing it selects a different onion identity on restart; it does not migrate keys.", Text, [];
    DisplayOnionAddress, display_onion_address: String, tor.display_onion_address, "Public onion address", "Optional 56-character v3 onion hostname. Empty uses the running onion service address.", Text, [];
    BootstrapTimeoutSecs, bootstrap_timeout_secs: u64, tor.bootstrap_timeout_secs, "Onion bootstrap timeout", "Seconds allowed for Arti bootstrap.", Number, [];
    MaxConcurrentStreams, max_concurrent_streams: usize, tor.max_concurrent_streams, "Concurrent onion streams", "Streams per circuit; must fit in a 32-bit unsigned integer.", Number, [];
    BackupEnabled, backup_enabled: bool, backup.enabled, "Enable backups", "Allow manual backups and scheduled backups.", Boolean, [serde(default = "unchecked")];
    AutomaticEnabled, automatic_enabled: bool, backup.automatic_enabled, "Schedule automatic backups", "Run automatic backups using the configured interval.", Boolean, [serde(default = "unchecked")];
    AutomaticIntervalMinutes, automatic_interval_minutes: u64, backup.automatic_interval_minutes, "Automatic backup interval", "Minutes between backups; minimum 1. Scheduler checks once per minute.", Number, [];
    RetentionKeepLast, retention_keep_last: usize, backup.retention_keep_last, "Automatic backups to keep", "Keep the newest 1–10000 automatic backups. Manual backups are unaffected.", Number, [];
    RetentionMaxAgeDays, retention_max_age_days: u64, backup.retention_max_age_days, "Maximum automatic backup age", "Days, 0–3650. Set 0 to disable age-based pruning.", Number, [];
    AutomaticIncludeTorKeys, automatic_include_tor_keys: bool, backup.automatic_include_tor_keys, "Include onion keys in automatic backups", "Sensitive: backups will contain private onion identity keys. Protect backup files.", Boolean, [serde(default = "unchecked")];
    CreateAdminOnFirstBoot, create_admin_on_first_boot: bool, admin.create_admin_on_first_boot, "First boot administrator setup", "Prompt for an administrator at startup only when no administrator exists.", Boolean, [serde(default = "unchecked")];
}

impl DeepSettingsField {
    #[must_use]
    pub const fn section(self) -> &'static str {
        match self.toml_section().as_bytes() {
            b"site" => "Site",
            b"accounts" => "Accounts",
            b"posts" => "Posts",
            b"media" => "Media",
            b"server" => "Networking & cookies",
            b"moderation" => "Rate limiting",
            b"tor" => "Onion service",
            b"backup" => "Backups",
            _ => "Administration",
        }
    }

    #[must_use]
    pub const fn applies_live(self) -> bool {
        matches!(self, Self::NsfwBlurEnabled) || matches!(self.toml_section().as_bytes(), b"backup")
    }
}

impl DeepSettingsValues {
    #[must_use]
    pub fn display_value(&self, field: DeepSettingsField) -> String {
        let value = self.form_value(field);
        match field {
            DeepSettingsField::MaxTextChars
            | DeepSettingsField::MinPasswordLength
            | DeepSettingsField::MaxUsernameLen
            | DeepSettingsField::MaxDisplayNameLen
            | DeepSettingsField::MaxBioLen => format!("{value} characters"),
            DeepSettingsField::PostEditWindowSeconds => format!("{value} seconds"),
            _ if field.input_kind() == DeepSettingsInputKind::Size => format!("{value} MB (MiB)"),
            _ => value,
        }
    }
}

trait SettingsValue: Sized {
    fn parse(value: &str, field: DeepSettingsField) -> anyhow::Result<Self>;
    fn formatted(&self, kind: DeepSettingsInputKind) -> String;
    fn copy_value(&self) -> Self;
}

impl SettingsValue for String {
    fn parse(value: &str, _field: DeepSettingsField) -> anyhow::Result<Self> {
        Ok(value.to_owned())
    }
    fn formatted(&self, _kind: DeepSettingsInputKind) -> String {
        self.clone()
    }
    fn copy_value(&self) -> Self {
        self.clone()
    }
}
impl SettingsValue for Vec<String> {
    fn parse(value: &str, _field: DeepSettingsField) -> anyhow::Result<Self> {
        Ok(value
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect())
    }
    fn formatted(&self, _kind: DeepSettingsInputKind) -> String {
        self.join("\n")
    }
    fn copy_value(&self) -> Self {
        self.clone()
    }
}
impl SettingsValue for bool {
    fn parse(value: &str, field: DeepSettingsField) -> anyhow::Result<Self> {
        match value {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => anyhow::bail!("{} must be true or false", field.label()),
        }
    }
    fn formatted(&self, _kind: DeepSettingsInputKind) -> String {
        self.to_string()
    }
    fn copy_value(&self) -> Self {
        *self
    }
}
macro_rules! numeric_value {
    ($($ty:ty),*) => { $(impl SettingsValue for $ty {
        fn parse(value: &str, field: DeepSettingsField) -> anyhow::Result<Self> {
            if field.input_kind() == DeepSettingsInputKind::Size {
                return Self::try_from(parse_mib(value)?).with_context(|| format!("{} is too large", field.label()));
            }
            if !stringify!($ty).starts_with('i') && value.trim().starts_with('-') {
                anyhow::bail!("{} must not be negative", field.label());
            }
            value.trim().parse().with_context(|| format!("{} must be a whole number in range", field.label()))
        }
        fn formatted(&self, kind: DeepSettingsInputKind) -> String {
            if kind == DeepSettingsInputKind::Size {
                format_mib(u128::try_from(*self).unwrap_or_default())
            } else { self.to_string() }
        }
        fn copy_value(&self) -> Self { *self }
    })* }
}
numeric_value!(usize, u64, u16, u8, i64);

fn parse_mib(value: &str) -> anyhow::Result<u64> {
    let value = value.trim();
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty()
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|b| b.is_ascii_digit())
        || fraction.len() > 20
    {
        anyhow::bail!("Size must be a nonnegative number of MiB with at most 20 decimal places");
    }
    let whole: u64 = whole.parse().context("Size is too large")?;
    let scale = 10_u128.pow(u32::try_from(fraction.len())?);
    let numerator = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<u128>()?
    } * 1_048_576;
    if !numerator.is_multiple_of(scale) {
        anyhow::bail!("Size must represent a whole number of bytes");
    }
    let bytes = u128::from(whole) * 1_048_576 + numerator / scale;
    u64::try_from(bytes).context("Size is too large")
}
fn format_mib(bytes: u128) -> String {
    let whole = bytes / 1_048_576;
    let remainder = bytes % 1_048_576;
    if remainder == 0 {
        return whole.to_string();
    }
    let fraction = format!("{:020}", remainder * 5_u128.pow(20));
    format!("{whole}.{}", fraction.trim_end_matches('0'))
}

fn unchecked() -> String {
    "false".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn every_configuration_field_has_exactly_one_classification() {
        let model = toml::Value::try_from(Settings::default()).expect("serialized config");
        let model_keys: BTreeSet<_> = model
            .as_table()
            .expect("config table")
            .iter()
            .flat_map(|(section, value)| {
                value
                    .as_table()
                    .expect("section table")
                    .keys()
                    .map(move |key| format!("{section}.{key}"))
            })
            .collect();
        let editable: BTreeSet<_> = DeepSettingsField::ALL
            .into_iter()
            .map(|field| format!("{}.{}", field.toml_section(), field.toml_key()))
            .collect();
        let excluded: BTreeSet<_> = NON_WEB_SETTINGS
            .iter()
            .map(|(key, reason)| {
                assert!(!reason.is_empty());
                key.to_string()
            })
            .collect();
        assert_eq!(
            editable.len(),
            DeepSettingsField::ALL.len(),
            "Duplicate admin mapping"
        );
        assert!(editable.is_disjoint(&excluded));
        assert_eq!(
            model_keys,
            editable.union(&excluded).cloned().collect(),
            "New config fields must be classified"
        );
        let names: BTreeSet<_> = DeepSettingsField::ALL
            .into_iter()
            .map(DeepSettingsField::form_name)
            .collect();
        assert_eq!(names.len(), editable.len());
    }

    #[test]
    fn defaults_round_trip_through_every_form_field() {
        let settings = Settings::default();
        let form = DeepSettingsForm::from_settings(&settings);
        let values = parse_deep_settings_form(&form, &settings).expect("default form");
        assert_eq!(
            toml::Value::try_from(values.apply_to(&settings)).expect("updated"),
            toml::Value::try_from(settings).expect("original")
        );
    }

    #[test]
    fn exact_byte_limits_round_trip_without_rounding() {
        for bytes in [0, 1, 65_536, 1_048_575, 1_048_577, 52_428_801, u64::MAX] {
            assert_eq!(
                parse_mib(&format_mib(u128::from(bytes))).expect("exact bytes"),
                bytes
            );
        }
        assert!(parse_mib("0.00001").is_err());
        assert!(parse_mib("-1").is_err());
        assert!(parse_mib("NaN").is_err());
    }

    #[test]
    fn enums_lists_optional_values_and_bounds_use_config_validation() {
        let settings = Settings::default();
        let mut form = DeepSettingsForm::from_settings(&settings);
        form.public_url = "https://example.org".to_owned();
        form.trusted_proxy_cidrs = "127.0.0.1/32\n::1/128\n".to_owned();
        form.vp9_deadline = "realtime".to_owned();
        let values = parse_deep_settings_form(&form, &settings).expect("valid options");
        assert_eq!(values.trusted_proxy_cidrs.len(), 2);
        form.public_url.clear();
        form.display_onion_address.clear();
        form.allowed_image_mime_types.clear();
        assert!(parse_deep_settings_form(&form, &settings).is_ok());
        for bad in ["slow", "good;touch /tmp/unsafe"] {
            form.vp9_deadline = bad.to_owned();
            assert!(parse_deep_settings_form(&form, &settings).is_err());
        }
        for speed in super::super::VideoEncodingSpeed::ALL {
            assert_eq!(
                speed
                    .as_str()
                    .parse::<super::super::VideoEncodingSpeed>()
                    .expect("enum"),
                speed
            );
        }
        form.vp9_deadline = "good".to_owned();
        form.webp_quality = "101".to_owned();
        assert!(parse_deep_settings_form(&form, &settings).is_err());
        form.webp_quality = "100".to_owned();
        form.vp9_crf = "64".to_owned();
        assert!(parse_deep_settings_form(&form, &settings).is_err());
        form.vp9_crf = "63".to_owned();
        form.tor_only = "true".to_owned();
        assert!(parse_deep_settings_form(&form, &settings).is_err());
        form.tor_enabled = "true".to_owned();
        assert!(parse_deep_settings_form(&form, &settings).is_ok());
    }

    #[test]
    fn malformed_hosts_urls_and_lists_are_rejected() {
        let mut settings = Settings::default();
        for host in ["localhost; rm", "", "127.0.0.999"] {
            settings.server.host = host.to_owned();
            assert!(settings.validate().is_err());
        }
        settings.server.host = "127.0.0.1".to_owned();
        for url in [
            "javascript:alert(1)",
            "https://user:password@example.org",
            "/relative",
        ] {
            settings.server.public_url = url.to_owned();
            assert!(settings.validate().is_err());
        }
        settings.server.public_url.clear();
        for cidr in ["10.0.0.0/33", "::/129", "127.0.0.1/"] {
            settings.server.trusted_proxy_cidrs = vec![cidr.to_owned()];
            assert!(settings.validate().is_err());
        }
    }

    #[test]
    fn runtime_classification_is_explicit() {
        for field in DeepSettingsField::ALL {
            assert_eq!(
                field.applies_live(),
                field == DeepSettingsField::NsfwBlurEnabled || field.toml_section() == "backup"
            );
        }
    }
    #[test]
    fn listener_addresses_preserve_bracketed_ipv6_and_support_bare_ipv6() {
        let mut settings = Settings::default();
        for host in ["127.0.0.1", "[::1]", "::1"] {
            settings.server.host = host.to_owned();
            settings.validate().expect("valid listener");
            assert_eq!(
                settings.server.listener_address().expect("listener").port(),
                8080
            );
        }
    }
}
