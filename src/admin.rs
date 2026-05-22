use std::fs::{self, File};
use std::io::Write as _;
use std::path::Path;

use anyhow::Context as _;
use rusqlite::{Row, params, params_from_iter};
use serde::Deserialize;

use crate::auth;
use crate::config::Settings;
use crate::db::SqlitePool;

const MIB: u64 = 1024 * 1024;

#[derive(Debug, Clone, Deserialize)]
pub struct DeepSettingsForm {
    pub csrf: String,
    pub intent: Option<String>,
    pub site_name: String,
    pub max_text_chars: String,
    pub max_images_per_post: String,
    pub max_videos_per_post: String,
    pub max_media_per_post: String,
    pub allow_reposts: String,
    pub allow_replies: String,
    pub allow_likes: String,
    pub allow_bookmarks: String,
    pub allow_hashtags: String,
    pub allow_mentions: String,
    pub registration_enabled: String,
    pub anonymous_mode_enabled: String,
    pub min_password_length: String,
    pub max_username_len: String,
    pub max_display_name_len: String,
    pub max_bio_len: String,
    pub allow_profile_banners: String,
    pub allow_profile_pictures: String,
    pub max_image_size_mb: String,
    pub max_video_size_mb: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeepSettingsField {
    SiteName,
    MaxTextChars,
    MaxImagesPerPost,
    MaxVideosPerPost,
    MaxMediaPerPost,
    AllowReposts,
    AllowReplies,
    AllowLikes,
    AllowBookmarks,
    AllowHashtags,
    AllowMentions,
    RegistrationEnabled,
    AnonymousModeEnabled,
    MinPasswordLength,
    MaxUsernameLen,
    MaxDisplayNameLen,
    MaxBioLen,
    AllowProfileBanners,
    AllowProfilePictures,
    MaxImageSizeMb,
    MaxVideoSizeMb,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeepSettingsInputKind {
    Text,
    Number,
    Boolean,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeepSettingsValues {
    pub site_name: String,
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
    pub registration_enabled: bool,
    pub anonymous_mode_enabled: bool,
    pub min_password_length: usize,
    pub max_username_len: usize,
    pub max_display_name_len: usize,
    pub max_bio_len: usize,
    pub allow_profile_banners: bool,
    pub allow_profile_pictures: bool,
    pub max_image_size_mb: u64,
    pub max_video_size_mb: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeepSettingsChange {
    pub label: &'static str,
    pub old_value: String,
    pub new_value: String,
}

impl DeepSettingsField {
    pub const ALL: [Self; 21] = [
        Self::SiteName,
        Self::MaxTextChars,
        Self::MaxImagesPerPost,
        Self::MaxVideosPerPost,
        Self::MaxMediaPerPost,
        Self::AllowReposts,
        Self::AllowReplies,
        Self::AllowLikes,
        Self::AllowBookmarks,
        Self::AllowHashtags,
        Self::AllowMentions,
        Self::RegistrationEnabled,
        Self::AnonymousModeEnabled,
        Self::MinPasswordLength,
        Self::MaxUsernameLen,
        Self::MaxDisplayNameLen,
        Self::MaxBioLen,
        Self::AllowProfileBanners,
        Self::AllowProfilePictures,
        Self::MaxImageSizeMb,
        Self::MaxVideoSizeMb,
    ];

    #[must_use]
    pub const fn section(self) -> &'static str {
        match self {
            Self::SiteName => "Site",
            Self::MaxTextChars
            | Self::MaxImagesPerPost
            | Self::MaxVideosPerPost
            | Self::MaxMediaPerPost
            | Self::AllowReposts
            | Self::AllowReplies
            | Self::AllowLikes
            | Self::AllowBookmarks
            | Self::AllowHashtags
            | Self::AllowMentions => "Posts",
            Self::RegistrationEnabled
            | Self::AnonymousModeEnabled
            | Self::MinPasswordLength
            | Self::MaxUsernameLen
            | Self::MaxDisplayNameLen
            | Self::MaxBioLen
            | Self::AllowProfileBanners
            | Self::AllowProfilePictures => "Accounts",
            Self::MaxImageSizeMb | Self::MaxVideoSizeMb => "Media limits",
        }
    }

    #[must_use]
    pub const fn toml_section(self) -> &'static str {
        match self {
            Self::SiteName => "site",
            Self::MaxTextChars
            | Self::MaxImagesPerPost
            | Self::MaxVideosPerPost
            | Self::MaxMediaPerPost
            | Self::AllowReposts
            | Self::AllowReplies
            | Self::AllowLikes
            | Self::AllowBookmarks
            | Self::AllowHashtags
            | Self::AllowMentions => "posts",
            Self::RegistrationEnabled
            | Self::AnonymousModeEnabled
            | Self::MinPasswordLength
            | Self::MaxUsernameLen
            | Self::MaxDisplayNameLen
            | Self::MaxBioLen
            | Self::AllowProfileBanners
            | Self::AllowProfilePictures => "accounts",
            Self::MaxImageSizeMb | Self::MaxVideoSizeMb => "media",
        }
    }

    #[must_use]
    pub const fn toml_key(self) -> &'static str {
        match self {
            Self::SiteName => "name",
            Self::MaxTextChars => "max_text_chars",
            Self::MaxImagesPerPost => "max_images_per_post",
            Self::MaxVideosPerPost => "max_videos_per_post",
            Self::MaxMediaPerPost => "max_media_per_post",
            Self::AllowReposts => "allow_reposts",
            Self::AllowReplies => "allow_replies",
            Self::AllowLikes => "allow_likes",
            Self::AllowBookmarks => "allow_bookmarks",
            Self::AllowHashtags => "allow_hashtags",
            Self::AllowMentions => "allow_mentions",
            Self::RegistrationEnabled => "registration_enabled",
            Self::AnonymousModeEnabled => "anonymous_mode_enabled",
            Self::MinPasswordLength => "min_password_length",
            Self::MaxUsernameLen => "max_username_len",
            Self::MaxDisplayNameLen => "max_display_name_len",
            Self::MaxBioLen => "max_bio_len",
            Self::AllowProfileBanners => "allow_profile_banners",
            Self::AllowProfilePictures => "allow_profile_pictures",
            Self::MaxImageSizeMb => "max_image_size",
            Self::MaxVideoSizeMb => "max_video_size",
        }
    }

    #[must_use]
    pub const fn form_name(self) -> &'static str {
        match self {
            Self::SiteName => "site_name",
            Self::MaxTextChars => "max_text_chars",
            Self::MaxImagesPerPost => "max_images_per_post",
            Self::MaxVideosPerPost => "max_videos_per_post",
            Self::MaxMediaPerPost => "max_media_per_post",
            Self::AllowReposts => "allow_reposts",
            Self::AllowReplies => "allow_replies",
            Self::AllowLikes => "allow_likes",
            Self::AllowBookmarks => "allow_bookmarks",
            Self::AllowHashtags => "allow_hashtags",
            Self::AllowMentions => "allow_mentions",
            Self::RegistrationEnabled => "registration_enabled",
            Self::AnonymousModeEnabled => "anonymous_mode_enabled",
            Self::MinPasswordLength => "min_password_length",
            Self::MaxUsernameLen => "max_username_len",
            Self::MaxDisplayNameLen => "max_display_name_len",
            Self::MaxBioLen => "max_bio_len",
            Self::AllowProfileBanners => "allow_profile_banners",
            Self::AllowProfilePictures => "allow_profile_pictures",
            Self::MaxImageSizeMb => "max_image_size_mb",
            Self::MaxVideoSizeMb => "max_video_size_mb",
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::SiteName => "Site name",
            Self::MaxTextChars => "Maximum post text length",
            Self::MaxImagesPerPost => "Maximum images per post",
            Self::MaxVideosPerPost => "Maximum videos per post",
            Self::MaxMediaPerPost => "Maximum total media per post",
            Self::AllowReposts => "Allow reposts",
            Self::AllowReplies => "Allow replies",
            Self::AllowLikes => "Allow likes",
            Self::AllowBookmarks => "Allow bookmarks",
            Self::AllowHashtags => "Allow hashtags",
            Self::AllowMentions => "Allow mentions",
            Self::RegistrationEnabled => "Registration enabled",
            Self::AnonymousModeEnabled => "Anonymous posting enabled",
            Self::MinPasswordLength => "Minimum password length",
            Self::MaxUsernameLen => "Maximum username length",
            Self::MaxDisplayNameLen => "Maximum display name length",
            Self::MaxBioLen => "Maximum bio length",
            Self::AllowProfileBanners => "Allow profile banners",
            Self::AllowProfilePictures => "Allow profile pictures",
            Self::MaxImageSizeMb => "Maximum image size",
            Self::MaxVideoSizeMb => "Maximum video size",
        }
    }

    #[must_use]
    pub const fn helper(self) -> Option<&'static str> {
        match self {
            Self::SiteName => Some("Shown in page titles, the header, and the footer."),
            Self::MaxTextChars
            | Self::MaxUsernameLen
            | Self::MaxDisplayNameLen
            | Self::MaxBioLen => Some("Characters."),
            Self::MaxImagesPerPost | Self::MaxVideosPerPost | Self::MaxMediaPerPost => {
                Some("Attachments per post.")
            }
            Self::MinPasswordLength => Some("Characters. Recommended default is 10."),
            Self::MaxImageSizeMb | Self::MaxVideoSizeMb => Some("MB."),
            _ => None,
        }
    }

    #[must_use]
    pub const fn input_kind(self) -> DeepSettingsInputKind {
        match self {
            Self::SiteName => DeepSettingsInputKind::Text,
            Self::AllowReposts
            | Self::AllowReplies
            | Self::AllowLikes
            | Self::AllowBookmarks
            | Self::AllowHashtags
            | Self::AllowMentions
            | Self::RegistrationEnabled
            | Self::AnonymousModeEnabled
            | Self::AllowProfileBanners
            | Self::AllowProfilePictures => DeepSettingsInputKind::Boolean,
            _ => DeepSettingsInputKind::Number,
        }
    }
}

impl DeepSettingsValues {
    #[must_use]
    pub fn from_settings(settings: &Settings) -> Self {
        Self {
            site_name: settings.site.name.clone(),
            max_text_chars: settings.posts.max_text_chars,
            max_images_per_post: settings.posts.max_images_per_post,
            max_videos_per_post: settings.posts.max_videos_per_post,
            max_media_per_post: settings.posts.max_media_per_post,
            allow_reposts: settings.posts.allow_reposts,
            allow_replies: settings.posts.allow_replies,
            allow_likes: settings.posts.allow_likes,
            allow_bookmarks: settings.posts.allow_bookmarks,
            allow_hashtags: settings.posts.allow_hashtags,
            allow_mentions: settings.posts.allow_mentions,
            registration_enabled: settings.accounts.registration_enabled,
            anonymous_mode_enabled: settings.accounts.anonymous_mode_enabled,
            min_password_length: settings.accounts.min_password_length,
            max_username_len: settings.accounts.max_username_len,
            max_display_name_len: settings.accounts.max_display_name_len,
            max_bio_len: settings.accounts.max_bio_len,
            allow_profile_banners: settings.accounts.allow_profile_banners,
            allow_profile_pictures: settings.accounts.allow_profile_pictures,
            max_image_size_mb: bytes_to_mb(settings.media.max_image_size),
            max_video_size_mb: bytes_to_mb(settings.media.max_video_size),
        }
    }

    #[must_use]
    pub fn form_value(&self, field: DeepSettingsField) -> String {
        match field {
            DeepSettingsField::SiteName => self.site_name.clone(),
            DeepSettingsField::MaxTextChars => self.max_text_chars.to_string(),
            DeepSettingsField::MaxImagesPerPost => self.max_images_per_post.to_string(),
            DeepSettingsField::MaxVideosPerPost => self.max_videos_per_post.to_string(),
            DeepSettingsField::MaxMediaPerPost => self.max_media_per_post.to_string(),
            DeepSettingsField::AllowReposts => self.allow_reposts.to_string(),
            DeepSettingsField::AllowReplies => self.allow_replies.to_string(),
            DeepSettingsField::AllowLikes => self.allow_likes.to_string(),
            DeepSettingsField::AllowBookmarks => self.allow_bookmarks.to_string(),
            DeepSettingsField::AllowHashtags => self.allow_hashtags.to_string(),
            DeepSettingsField::AllowMentions => self.allow_mentions.to_string(),
            DeepSettingsField::RegistrationEnabled => self.registration_enabled.to_string(),
            DeepSettingsField::AnonymousModeEnabled => self.anonymous_mode_enabled.to_string(),
            DeepSettingsField::MinPasswordLength => self.min_password_length.to_string(),
            DeepSettingsField::MaxUsernameLen => self.max_username_len.to_string(),
            DeepSettingsField::MaxDisplayNameLen => self.max_display_name_len.to_string(),
            DeepSettingsField::MaxBioLen => self.max_bio_len.to_string(),
            DeepSettingsField::AllowProfileBanners => self.allow_profile_banners.to_string(),
            DeepSettingsField::AllowProfilePictures => self.allow_profile_pictures.to_string(),
            DeepSettingsField::MaxImageSizeMb => self.max_image_size_mb.to_string(),
            DeepSettingsField::MaxVideoSizeMb => self.max_video_size_mb.to_string(),
        }
    }

    #[must_use]
    pub fn display_value(&self, field: DeepSettingsField) -> String {
        let value = self.form_value(field);
        match field {
            DeepSettingsField::MaxTextChars
            | DeepSettingsField::MinPasswordLength
            | DeepSettingsField::MaxUsernameLen
            | DeepSettingsField::MaxDisplayNameLen
            | DeepSettingsField::MaxBioLen => format!("{value} characters"),
            DeepSettingsField::MaxImageSizeMb | DeepSettingsField::MaxVideoSizeMb => {
                format!("{value} MB")
            }
            _ => value,
        }
    }

    #[must_use]
    pub fn apply_to(&self, current: &Settings) -> Settings {
        let mut updated = current.clone();
        updated.site.name.clone_from(&self.site_name);
        updated.posts.max_text_chars = self.max_text_chars;
        updated.posts.max_images_per_post = self.max_images_per_post;
        updated.posts.max_videos_per_post = self.max_videos_per_post;
        updated.posts.max_media_per_post = self.max_media_per_post;
        updated.posts.allow_reposts = self.allow_reposts;
        updated.posts.allow_replies = self.allow_replies;
        updated.posts.allow_likes = self.allow_likes;
        updated.posts.allow_bookmarks = self.allow_bookmarks;
        updated.posts.allow_hashtags = self.allow_hashtags;
        updated.posts.allow_mentions = self.allow_mentions;
        updated.accounts.registration_enabled = self.registration_enabled;
        updated.accounts.anonymous_mode_enabled = self.anonymous_mode_enabled;
        updated.accounts.min_password_length = self.min_password_length;
        updated.accounts.max_username_len = self.max_username_len;
        updated.accounts.max_display_name_len = self.max_display_name_len;
        updated.accounts.max_bio_len = self.max_bio_len;
        updated.accounts.allow_profile_banners = self.allow_profile_banners;
        updated.accounts.allow_profile_pictures = self.allow_profile_pictures;
        updated.media.max_image_size = self.max_image_size_mb * MIB;
        updated.media.max_video_size = self.max_video_size_mb * MIB;
        updated
    }
}

pub fn parse_deep_settings_form(
    form: &DeepSettingsForm,
    current: &Settings,
) -> anyhow::Result<DeepSettingsValues> {
    let values = DeepSettingsValues {
        site_name: form.site_name.clone(),
        max_text_chars: parse_usize(&form.max_text_chars, DeepSettingsField::MaxTextChars)?,
        max_images_per_post: parse_usize(
            &form.max_images_per_post,
            DeepSettingsField::MaxImagesPerPost,
        )?,
        max_videos_per_post: parse_usize(
            &form.max_videos_per_post,
            DeepSettingsField::MaxVideosPerPost,
        )?,
        max_media_per_post: parse_usize(
            &form.max_media_per_post,
            DeepSettingsField::MaxMediaPerPost,
        )?,
        allow_reposts: parse_bool(&form.allow_reposts, DeepSettingsField::AllowReposts)?,
        allow_replies: parse_bool(&form.allow_replies, DeepSettingsField::AllowReplies)?,
        allow_likes: parse_bool(&form.allow_likes, DeepSettingsField::AllowLikes)?,
        allow_bookmarks: parse_bool(&form.allow_bookmarks, DeepSettingsField::AllowBookmarks)?,
        allow_hashtags: parse_bool(&form.allow_hashtags, DeepSettingsField::AllowHashtags)?,
        allow_mentions: parse_bool(&form.allow_mentions, DeepSettingsField::AllowMentions)?,
        registration_enabled: parse_bool(
            &form.registration_enabled,
            DeepSettingsField::RegistrationEnabled,
        )?,
        anonymous_mode_enabled: parse_bool(
            &form.anonymous_mode_enabled,
            DeepSettingsField::AnonymousModeEnabled,
        )?,
        min_password_length: parse_usize(
            &form.min_password_length,
            DeepSettingsField::MinPasswordLength,
        )?,
        max_username_len: parse_usize(&form.max_username_len, DeepSettingsField::MaxUsernameLen)?,
        max_display_name_len: parse_usize(
            &form.max_display_name_len,
            DeepSettingsField::MaxDisplayNameLen,
        )?,
        max_bio_len: parse_usize(&form.max_bio_len, DeepSettingsField::MaxBioLen)?,
        allow_profile_banners: parse_bool(
            &form.allow_profile_banners,
            DeepSettingsField::AllowProfileBanners,
        )?,
        allow_profile_pictures: parse_bool(
            &form.allow_profile_pictures,
            DeepSettingsField::AllowProfilePictures,
        )?,
        max_image_size_mb: parse_mb(&form.max_image_size_mb, DeepSettingsField::MaxImageSizeMb)?,
        max_video_size_mb: parse_mb(&form.max_video_size_mb, DeepSettingsField::MaxVideoSizeMb)?,
    };
    values.apply_to(current).validate()?;
    Ok(values)
}

#[must_use]
pub fn diff_deep_settings(
    current: &Settings,
    values: &DeepSettingsValues,
) -> Vec<DeepSettingsChange> {
    let old_values = DeepSettingsValues::from_settings(current);
    DeepSettingsField::ALL
        .iter()
        .copied()
        .filter(|field| old_values.form_value(*field) != values.form_value(*field))
        .map(|field| DeepSettingsChange {
            label: field.label(),
            old_value: old_values.display_value(field),
            new_value: values.display_value(field),
        })
        .collect()
}

pub fn write_deep_settings(path: &Path, updated: &Settings) -> anyhow::Result<()> {
    updated.validate()?;
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read settings file {}", path.display()))?;
    let rewritten = rewrite_deep_settings_toml(&raw, updated);
    let parsed: Settings = toml::from_str(&rewritten)
        .with_context(|| "rewritten settings.toml did not parse as settings")?;
    parsed.validate()?;
    write_atomic(path, rewritten.as_bytes())
}

fn parse_bool(value: &str, field: DeepSettingsField) -> anyhow::Result<bool> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => anyhow::bail!("{} must be true or false", field.label()),
    }
}

fn parse_usize(value: &str, field: DeepSettingsField) -> anyhow::Result<usize> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        anyhow::bail!("{} is required", field.label());
    }
    if trimmed.starts_with('-') {
        anyhow::bail!("{} must not be negative", field.label());
    }
    trimmed
        .parse::<usize>()
        .with_context(|| format!("{} must be a whole number", field.label()))
}

fn parse_mb(value: &str, field: DeepSettingsField) -> anyhow::Result<u64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        anyhow::bail!("{} is required and must be entered in MB", field.label());
    }
    if trimmed.starts_with('-') {
        anyhow::bail!("{} must not be negative", field.label());
    }
    let mb = trimmed
        .parse::<u64>()
        .with_context(|| format!("{} must be a whole number of MB", field.label()))?;
    mb.checked_mul(MIB)
        .with_context(|| format!("{} is too large to convert from MB", field.label()))?;
    Ok(mb)
}

fn bytes_to_mb(bytes: u64) -> u64 {
    bytes / MIB
}

fn rewrite_deep_settings_toml(raw: &str, settings: &Settings) -> String {
    let mut output = Vec::new();
    let mut current_section: Option<&str> = None;
    let mut found = vec![false; DeepSettingsField::ALL.len()];
    let mut section_seen = Vec::new();

    for line in raw.lines() {
        if let Some(section) = parse_section_header(line) {
            append_missing_for_section(&mut output, current_section, &mut found, settings);
            if DeepSettingsField::ALL
                .iter()
                .any(|field| field.toml_section() == section)
            {
                section_seen.push(section.to_owned());
            }
            current_section = Some(section);
            output.push(line.to_owned());
            continue;
        }

        if let Some(section) = current_section
            && let Some((index, field)) =
                DeepSettingsField::ALL
                    .iter()
                    .copied()
                    .enumerate()
                    .find(|(_, field)| {
                        field.toml_section() == section && line_assigns_key(line, field.toml_key())
                    })
        {
            found[index] = true;
            output.push(format!(
                "{}{} = {}",
                leading_whitespace(line),
                field.toml_key(),
                toml_value(field, settings)
            ));
            continue;
        }

        output.push(line.to_owned());
    }

    append_missing_for_section(&mut output, current_section, &mut found, settings);
    for section in ["site", "posts", "accounts", "media"] {
        if !section_seen.iter().any(|seen| seen == section) {
            output.push(String::new());
            output.push(format!("[{section}]"));
            append_missing_for_section(&mut output, Some(section), &mut found, settings);
        }
    }

    let mut rewritten = output.join("\n");
    if raw.ends_with('\n') {
        rewritten.push('\n');
    }
    rewritten
}

fn append_missing_for_section(
    output: &mut Vec<String>,
    section: Option<&str>,
    found: &mut [bool],
    settings: &Settings,
) {
    let Some(section) = section else {
        return;
    };
    for (index, field) in DeepSettingsField::ALL.iter().copied().enumerate() {
        if field.toml_section() == section && !found[index] {
            found[index] = true;
            output.push(format!(
                "{} = {}",
                field.toml_key(),
                toml_value(field, settings)
            ));
        }
    }
}

fn parse_section_header(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    trimmed.strip_prefix('[')?.strip_suffix(']')
}

fn line_assigns_key(line: &str, key: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') {
        return false;
    }
    let Some(rest) = trimmed.strip_prefix(key) else {
        return false;
    };
    rest.trim_start().starts_with('=')
}

fn leading_whitespace(line: &str) -> &str {
    let end = line
        .char_indices()
        .find_map(|(index, ch)| (!ch.is_whitespace()).then_some(index))
        .unwrap_or(line.len());
    &line[..end]
}

fn toml_value(field: DeepSettingsField, settings: &Settings) -> String {
    match field {
        DeepSettingsField::SiteName => toml::Value::String(settings.site.name.clone()).to_string(),
        DeepSettingsField::MaxTextChars => settings.posts.max_text_chars.to_string(),
        DeepSettingsField::MaxImagesPerPost => settings.posts.max_images_per_post.to_string(),
        DeepSettingsField::MaxVideosPerPost => settings.posts.max_videos_per_post.to_string(),
        DeepSettingsField::MaxMediaPerPost => settings.posts.max_media_per_post.to_string(),
        DeepSettingsField::AllowReposts => settings.posts.allow_reposts.to_string(),
        DeepSettingsField::AllowReplies => settings.posts.allow_replies.to_string(),
        DeepSettingsField::AllowLikes => settings.posts.allow_likes.to_string(),
        DeepSettingsField::AllowBookmarks => settings.posts.allow_bookmarks.to_string(),
        DeepSettingsField::AllowHashtags => settings.posts.allow_hashtags.to_string(),
        DeepSettingsField::AllowMentions => settings.posts.allow_mentions.to_string(),
        DeepSettingsField::RegistrationEnabled => {
            settings.accounts.registration_enabled.to_string()
        }
        DeepSettingsField::AnonymousModeEnabled => {
            settings.accounts.anonymous_mode_enabled.to_string()
        }
        DeepSettingsField::MinPasswordLength => settings.accounts.min_password_length.to_string(),
        DeepSettingsField::MaxUsernameLen => settings.accounts.max_username_len.to_string(),
        DeepSettingsField::MaxDisplayNameLen => settings.accounts.max_display_name_len.to_string(),
        DeepSettingsField::MaxBioLen => settings.accounts.max_bio_len.to_string(),
        DeepSettingsField::AllowProfileBanners => {
            settings.accounts.allow_profile_banners.to_string()
        }
        DeepSettingsField::AllowProfilePictures => {
            settings.accounts.allow_profile_pictures.to_string()
        }
        DeepSettingsField::MaxImageSizeMb => settings.media.max_image_size.to_string(),
        DeepSettingsField::MaxVideoSizeMb => settings.media.max_video_size.to_string(),
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("settings path must have a valid file name")?;
    let tmp_path = parent.join(format!(".{file_name}.tmp"));
    {
        let mut tmp = File::create(&tmp_path).with_context(|| {
            format!(
                "failed to create temporary settings file {}",
                tmp_path.display()
            )
        })?;
        tmp.write_all(bytes).with_context(|| {
            format!(
                "failed to write temporary settings file {}",
                tmp_path.display()
            )
        })?;
        tmp.sync_all().with_context(|| {
            format!(
                "failed to sync temporary settings file {}",
                tmp_path.display()
            )
        })?;
    }
    fs::rename(&tmp_path, path)
        .with_context(|| format!("failed to replace settings file {}", path.display()))?;
    if let Ok(parent_dir) = File::open(parent) {
        let _sync_result = parent_dir.sync_all();
    }
    Ok(())
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MediaJobsReport {
    pub total: i64,
    pub pending: i64,
    pub running: i64,
    pub succeeded: i64,
    pub failed: i64,
    pub newest_pending_age_seconds: Option<i64>,
    pub oldest_pending_age_seconds: Option<i64>,
    pub recent_failures: Vec<MediaJobFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaJobFailure {
    pub id: i64,
    pub media_id: Option<i64>,
    pub media_path: Option<String>,
    pub job_kind: Option<String>,
    pub age_seconds: Option<i64>,
    pub error_summary: String,
}

const ADMIN_USERS_LIMIT: i64 = 100;
const ADMIN_POST_SEARCH_TERM_LIMIT: usize = 8;
const ADMIN_USERS_SQL: &str = r"
WITH name_matches AS (
    __NAME_MATCH_SQL__
),
matching_posts AS (
    SELECT p.id, p.user_id, p.text, p.created_at
    FROM posts p
    WHERE p.user_id IS NOT NULL
      AND p.is_deleted = 0
      AND (__POST_MATCH_SQL__)
),
post_match_counts AS (
    SELECT user_id, COUNT(*) AS matching_post_count
    FROM matching_posts
    GROUP BY user_id
),
ranked_post_matches AS (
    SELECT user_id, text,
        ROW_NUMBER() OVER (
            PARTITION BY user_id
            ORDER BY created_at DESC, id DESC
        ) AS rank
    FROM matching_posts
),
post_match_previews AS (
    SELECT user_id, text
    FROM ranked_post_matches
    WHERE rank = 1
),
post_stats AS (
    SELECT user_id, COUNT(*) AS total_posts, MAX(created_at) AS last_post_at
    FROM posts
    WHERE user_id IS NOT NULL AND is_deleted = 0
    GROUP BY user_id
),
media_stats AS (
    SELECT owner_user_id AS user_id, COUNT(*) AS uploaded_media_count
    FROM media
    WHERE owner_user_id IS NOT NULL
    GROUP BY owner_user_id
),
report_stats AS (
    SELECT p.user_id, COUNT(r.id) AS reports_on_posts_count
    FROM reports r
    JOIN posts p ON p.id = r.post_id
    WHERE p.user_id IS NOT NULL
    GROUP BY p.user_id
),
session_stats AS (
    SELECT user_id, MAX(created_at) AS last_session_at
    FROM sessions
    GROUP BY user_id
),
audit_stats AS (
    SELECT target, COUNT(*) AS moderation_action_count
    FROM admin_audit_log
    WHERE target LIKE 'user:%'
    GROUP BY target
)
SELECT
    u.id,
    u.username,
    u.display_name,
    u.is_admin,
    u.is_suspended,
    u.is_deleted,
    u.created_at,
    u.updated_at,
    session_stats.last_session_at,
    post_stats.last_post_at,
    COALESCE(post_stats.total_posts, 0),
    COALESCE(media_stats.uploaded_media_count, 0),
    COALESCE(report_stats.reports_on_posts_count, 0),
    COALESCE(audit_stats.moderation_action_count, 0),
    COALESCE(post_match_counts.matching_post_count, 0),
    post_match_previews.text,
    name_matches.id IS NOT NULL
FROM users u
LEFT JOIN name_matches ON name_matches.id = u.id
LEFT JOIN post_match_counts ON post_match_counts.user_id = u.id
LEFT JOIN post_match_previews ON post_match_previews.user_id = u.id
LEFT JOIN post_stats ON post_stats.user_id = u.id
LEFT JOIN media_stats ON media_stats.user_id = u.id
LEFT JOIN report_stats ON report_stats.user_id = u.id
LEFT JOIN session_stats ON session_stats.user_id = u.id
LEFT JOIN audit_stats ON audit_stats.target = ('user:' || u.id)
__FILTER_SQL__
ORDER BY
    CASE WHEN name_matches.id IS NOT NULL THEN 0 ELSE 1 END,
    COALESCE(post_match_counts.matching_post_count, 0) DESC,
    u.id DESC
LIMIT __LIMIT__
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminUserInvestigation {
    pub id: i64,
    pub username: String,
    pub display_name: String,
    pub is_admin: bool,
    pub is_suspended: bool,
    pub is_deleted: bool,
    pub created_at: String,
    pub updated_at: String,
    pub last_session_at: Option<String>,
    pub last_post_at: Option<String>,
    pub total_posts: i64,
    pub uploaded_media_count: i64,
    pub reports_on_posts_count: i64,
    pub moderation_action_count: i64,
    pub matching_post_count: i64,
    pub post_match_preview: Option<String>,
    pub matched_name: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminUserSearch {
    pub user_query: Option<String>,
    pub post_search: ParsedAdminPostSearch,
}

impl AdminUserSearch {
    #[must_use]
    pub fn new(user_query: &str, post_query: &str) -> Self {
        Self {
            user_query: normalize_admin_user_search(user_query),
            post_search: parse_admin_post_search(post_query),
        }
    }

    #[must_use]
    pub fn has_filter(&self) -> bool {
        self.user_query.is_some() || self.post_search.mode.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedAdminPostSearch {
    pub mode: Option<AdminPostSearchMode>,
    pub malformed_quotes: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminPostSearchMode {
    Keywords(Vec<String>),
    ExactPhrase(String),
}

pub async fn create_admin(
    pool: &SqlitePool,
    settings: &Settings,
    username: &str,
    password: &str,
) -> anyhow::Result<i64> {
    auth::register_user(pool, settings, username, password, true).await
}

pub async fn create_admin_with_display_name(
    pool: &SqlitePool,
    settings: &Settings,
    username: &str,
    password: &str,
    display_name: Option<&str>,
) -> anyhow::Result<i64> {
    let display_name = display_name.map(str::trim).filter(|name| !name.is_empty());
    if let Some(display_name) = display_name {
        crate::validation::validate_profile_text(display_name, "", settings)?;
    }
    let user_id = create_admin(pool, settings, username, password).await?;
    if let Some(display_name) = display_name {
        let display_name = display_name.to_owned();
        pool.call(move |conn| {
            conn.execute(
                "UPDATE users SET display_name = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
                params![display_name, user_id],
            )?;
            Ok(())
        })
        .await?;
    }
    Ok(user_id)
}

pub async fn reset_admin_password(
    pool: &SqlitePool,
    settings: &Settings,
    username: &str,
    password: &str,
) -> anyhow::Result<()> {
    crate::validation::validate_password(password, settings)?;
    let hash = auth::hash_password(password)?;
    let username = username.trim().to_ascii_lowercase();
    let changed = pool
        .call(move |conn| {
            Ok(conn.execute(
                "UPDATE users SET password_hash = ?, updated_at = CURRENT_TIMESTAMP WHERE normalized_username = ? AND is_admin = 1",
                params![hash, username],
            )?)
        })
        .await?;
    if changed == 0 {
        anyhow::bail!("admin user not found");
    }
    Ok(())
}

pub async fn ensure_first_boot_admin_hint(pool: &SqlitePool) -> anyhow::Result<()> {
    let count = admin_count(pool).await?;
    if count == 0 {
        tracing::warn!(
            "no admin account exists; run `rustpost-cli create-admin-interactive` or `rustpost-cli create-admin <username> <password>`"
        );
    }
    Ok(())
}

pub async fn admin_count(pool: &SqlitePool) -> anyhow::Result<i64> {
    pool.call(|conn| {
        Ok(
            conn.query_row("SELECT COUNT(*) FROM users WHERE is_admin = 1", [], |row| {
                row.get(0)
            })?,
        )
    })
    .await
}

pub async fn set_user_suspended(
    pool: &SqlitePool,
    admin_id: i64,
    user_id: i64,
    suspended: bool,
) -> anyhow::Result<()> {
    pool.call(move |conn| {
        conn.execute(
            "UPDATE users SET is_suspended = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
            params![i64::from(suspended), user_id],
        )?;
        Ok(())
    })
    .await?;
    audit(
        pool,
        admin_id,
        if suspended {
            "suspend_user"
        } else {
            "unsuspend_user"
        },
        &format!("user:{user_id}"),
    )
    .await?;
    Ok(())
}

pub async fn audit(
    pool: &SqlitePool,
    admin_id: i64,
    action: &str,
    target: &str,
) -> anyhow::Result<()> {
    let action = action.to_owned();
    let target = target.to_owned();
    pool.call(move |conn| {
        conn.execute(
            "INSERT INTO admin_audit_log (admin_user_id, action, target) VALUES (?, ?, ?)",
            params![admin_id, action, target],
        )?;
        Ok(())
    })
    .await
}

#[must_use]
pub fn normalize_admin_user_search(query: &str) -> Option<String> {
    let trimmed = query.trim().trim_start_matches('@').trim();
    (!trimmed.is_empty()).then(|| trimmed.to_ascii_lowercase())
}

#[must_use]
pub fn parse_admin_post_search(query: &str) -> ParsedAdminPostSearch {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return ParsedAdminPostSearch {
            mode: None,
            malformed_quotes: false,
        };
    }

    if let Some(quoted) = trimmed
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    {
        let phrase = quoted.trim();
        return ParsedAdminPostSearch {
            mode: (!phrase.is_empty()).then(|| AdminPostSearchMode::ExactPhrase(phrase.to_owned())),
            malformed_quotes: false,
        };
    }

    let malformed_quotes = trimmed.contains('"');
    let cleaned = trimmed.replace('"', " ");
    let terms = cleaned
        .split_whitespace()
        .take(ADMIN_POST_SEARCH_TERM_LIMIT)
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();

    ParsedAdminPostSearch {
        mode: (!terms.is_empty()).then_some(AdminPostSearchMode::Keywords(terms)),
        malformed_quotes,
    }
}

pub async fn users(
    pool: &SqlitePool,
    search: AdminUserSearch,
) -> anyhow::Result<Vec<AdminUserInvestigation>> {
    pool.call(move |conn| {
        let (sql, sql_params) = admin_users_sql(&search);
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params_from_iter(sql_params.iter()), admin_user_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}

fn admin_users_sql(search: &AdminUserSearch) -> (String, Vec<String>) {
    let mut sql_params = Vec::new();
    let name_match_sql = name_match_sql(search.user_query.as_deref(), &mut sql_params);
    let post_match_sql = search.post_search.mode.as_ref().map_or_else(
        || "0".to_owned(),
        |mode| post_match_condition(mode, &mut sql_params),
    );
    let filter_sql = if search.has_filter() {
        "WHERE name_matches.id IS NOT NULL OR post_match_counts.matching_post_count IS NOT NULL"
    } else {
        ""
    };
    let sql = ADMIN_USERS_SQL
        .replace("__NAME_MATCH_SQL__", name_match_sql)
        .replace("__POST_MATCH_SQL__", &post_match_sql)
        .replace("__FILTER_SQL__", filter_sql)
        .replace("__LIMIT__", &ADMIN_USERS_LIMIT.to_string());
    (sql, sql_params)
}

fn name_match_sql(user_query: Option<&str>, sql_params: &mut Vec<String>) -> &'static str {
    if let Some(user_query) = user_query {
        let like = format!("%{}%", escape_like(user_query));
        sql_params.extend([like.clone(), like.clone(), like]);
        r"
        SELECT id
        FROM users
        WHERE normalized_username LIKE ? ESCAPE '\'
           OR lower(username) LIKE ? ESCAPE '\'
           OR lower(display_name) LIKE ? ESCAPE '\'
        "
    } else {
        "SELECT id FROM users WHERE 0"
    }
}

fn admin_user_from_row(row: &Row<'_>) -> rusqlite::Result<AdminUserInvestigation> {
    Ok(AdminUserInvestigation {
        id: row.get(0)?,
        username: row.get(1)?,
        display_name: row.get(2)?,
        is_admin: row.get::<_, i64>(3)? != 0,
        is_suspended: row.get::<_, i64>(4)? != 0,
        is_deleted: row.get::<_, i64>(5)? != 0,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
        last_session_at: row.get(8)?,
        last_post_at: row.get(9)?,
        total_posts: row.get(10)?,
        uploaded_media_count: row.get(11)?,
        reports_on_posts_count: row.get(12)?,
        moderation_action_count: row.get(13)?,
        matching_post_count: row.get(14)?,
        post_match_preview: row.get(15)?,
        matched_name: row.get::<_, i64>(16)? != 0,
    })
}

fn post_match_condition(mode: &AdminPostSearchMode, sql_params: &mut Vec<String>) -> String {
    match mode {
        AdminPostSearchMode::ExactPhrase(phrase) => {
            sql_params.push(format!("%{}%", escape_like(&phrase.to_ascii_lowercase())));
            "lower(p.text) LIKE ? ESCAPE '\\'".to_owned()
        }
        AdminPostSearchMode::Keywords(terms) => {
            sql_params.extend(terms.iter().map(|term| format!("%{}%", escape_like(term))));
            terms
                .iter()
                .map(|_| "lower(p.text) LIKE ? ESCAPE '\\'")
                .collect::<Vec<_>>()
                .join(" AND ")
        }
    }
}

fn escape_like(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '%' | '_' | '\\' => {
                escaped.push('\\');
                escaped.push(character);
            }
            _ => escaped.push(character),
        }
    }
    escaped
}

pub async fn recent_media_jobs(pool: &SqlitePool) -> anyhow::Result<Vec<(i64, String, String)>> {
    pool.call(|conn| {
        let mut stmt = conn.prepare(
            "SELECT id, status, stderr_summary FROM media_jobs ORDER BY id DESC LIMIT 50",
        )?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}

pub async fn media_jobs_report(pool: &SqlitePool) -> anyhow::Result<MediaJobsReport> {
    pool.call(|conn| {
        let (total, pending, running, succeeded, failed, newest_pending_age, oldest_pending_age) =
            conn.query_row(
                r"
                SELECT
                    COUNT(*),
                    COALESCE(SUM(CASE WHEN status IN ('pending', 'queued') THEN 1 ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN status = 'running' THEN 1 ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN status IN ('succeeded', 'success', 'converted') THEN 1 ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN status IN ('failed', 'error', 'fallback') THEN 1 ELSE 0 END), 0),
                    MIN(CASE WHEN status IN ('pending', 'queued') THEN CAST(strftime('%s', 'now') - strftime('%s', created_at) AS INTEGER) END),
                    MAX(CASE WHEN status IN ('pending', 'queued') THEN CAST(strftime('%s', 'now') - strftime('%s', created_at) AS INTEGER) END)
                FROM media_jobs
                ",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )?;

        let mut stmt = conn.prepare(
            r"
            SELECT
                j.id,
                j.media_id,
                COALESCE(NULLIF(m.public_path, ''), NULLIF(m.original_filename, ''), NULLIF(m.stored_path, '')),
                NULLIF(m.media_kind, ''),
                CAST(strftime('%s', 'now') - strftime('%s', COALESCE(j.finished_at, j.created_at)) AS INTEGER),
                j.stderr_summary
            FROM media_jobs j
            LEFT JOIN media m ON m.id = j.media_id
            WHERE j.status IN ('failed', 'error', 'fallback')
            ORDER BY j.id DESC
            LIMIT 5
            ",
        )?;
        let recent_failures = stmt
            .query_map([], |row| {
                Ok(MediaJobFailure {
                    id: row.get(0)?,
                    media_id: row.get(1)?,
                    media_path: row.get(2)?,
                    job_kind: row.get(3)?,
                    age_seconds: row.get(4)?,
                    error_summary: row.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(MediaJobsReport {
            total,
            pending,
            running,
            succeeded,
            failed,
            newest_pending_age_seconds: newest_pending_age,
            oldest_pending_age_seconds: oldest_pending_age,
            recent_failures,
        })
    })
    .await
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    async fn test_pool() -> (tempfile::TempDir, SqlitePool, Settings) {
        let temp = tempdir().expect("temp dir");
        let pool = crate::db::connect(&temp.path().join("test.sqlite3"))
            .await
            .expect("connect");
        crate::db::migrate(&pool).await.expect("migrate");
        (temp, pool, Settings::default())
    }

    async fn register(pool: &SqlitePool, settings: &Settings, username: &str) -> i64 {
        auth::register_user(pool, settings, username, "very secure password", false)
            .await
            .expect("register user")
    }

    async fn create_post(pool: &SqlitePool, settings: &Settings, user_id: i64, text: &str) -> i64 {
        crate::social::create_post(pool, settings, Some(user_id), text, None, &[])
            .await
            .expect("create post")
    }

    fn usernames(rows: &[AdminUserInvestigation]) -> Vec<&str> {
        rows.iter().map(|row| row.username.as_str()).collect()
    }

    fn form_from_settings(settings: &Settings) -> DeepSettingsForm {
        let values = DeepSettingsValues::from_settings(settings);
        DeepSettingsForm {
            csrf: "csrf".to_owned(),
            intent: Some("preview".to_owned()),
            site_name: values.site_name,
            max_text_chars: values.max_text_chars.to_string(),
            max_images_per_post: values.max_images_per_post.to_string(),
            max_videos_per_post: values.max_videos_per_post.to_string(),
            max_media_per_post: values.max_media_per_post.to_string(),
            allow_reposts: values.allow_reposts.to_string(),
            allow_replies: values.allow_replies.to_string(),
            allow_likes: values.allow_likes.to_string(),
            allow_bookmarks: values.allow_bookmarks.to_string(),
            allow_hashtags: values.allow_hashtags.to_string(),
            allow_mentions: values.allow_mentions.to_string(),
            registration_enabled: values.registration_enabled.to_string(),
            anonymous_mode_enabled: values.anonymous_mode_enabled.to_string(),
            min_password_length: values.min_password_length.to_string(),
            max_username_len: values.max_username_len.to_string(),
            max_display_name_len: values.max_display_name_len.to_string(),
            max_bio_len: values.max_bio_len.to_string(),
            allow_profile_banners: values.allow_profile_banners.to_string(),
            allow_profile_pictures: values.allow_profile_pictures.to_string(),
            max_image_size_mb: values.max_image_size_mb.to_string(),
            max_video_size_mb: values.max_video_size_mb.to_string(),
        }
    }

    #[test]
    fn admin_post_search_parses_keywords_quotes_and_malformed_quotes() {
        assert_eq!(
            parse_admin_post_search("rust admin"),
            ParsedAdminPostSearch {
                mode: Some(AdminPostSearchMode::Keywords(vec![
                    "rust".to_owned(),
                    "admin".to_owned()
                ])),
                malformed_quotes: false,
            }
        );
        assert_eq!(
            parse_admin_post_search(r#""hello world""#),
            ParsedAdminPostSearch {
                mode: Some(AdminPostSearchMode::ExactPhrase("hello world".to_owned())),
                malformed_quotes: false,
            }
        );
        assert_eq!(
            parse_admin_post_search(r#""hello world"#),
            ParsedAdminPostSearch {
                mode: Some(AdminPostSearchMode::Keywords(vec![
                    "hello".to_owned(),
                    "world".to_owned()
                ])),
                malformed_quotes: true,
            }
        );
    }

    #[tokio::test]
    async fn admin_username_search_matches_username_handle_and_display_name() {
        let (_temp, pool, settings) = test_pool().await;
        let alice = register(&pool, &settings, "alice").await;
        register(&pool, &settings, "bob").await;
        pool.call(move |conn| {
            conn.execute(
                "UPDATE users SET display_name = 'Ada Admin' WHERE id = ?",
                [alice],
            )?;
            Ok(())
        })
        .await
        .expect("display name");

        let by_handle = users(&pool, AdminUserSearch::new("@ali", ""))
            .await
            .expect("search by handle");
        let by_display = users(&pool, AdminUserSearch::new("ada", ""))
            .await
            .expect("search by display");

        assert_eq!(usernames(&by_handle), vec!["alice"]);
        assert!(by_handle[0].matched_name);
        assert_eq!(usernames(&by_display), vec!["alice"]);
    }

    #[tokio::test]
    async fn admin_plain_post_keyword_search_matches_accounts_with_all_terms() {
        let (_temp, pool, settings) = test_pool().await;
        let alice = register(&pool, &settings, "alice").await;
        let bob = register(&pool, &settings, "bob").await;
        create_post(&pool, &settings, alice, "Rust admin investigation notes").await;
        create_post(&pool, &settings, bob, "Rust release notes").await;

        let rows = users(&pool, AdminUserSearch::new("", "rust investigation"))
            .await
            .expect("post search");

        assert_eq!(usernames(&rows), vec!["alice"]);
        assert_eq!(rows[0].matching_post_count, 1);
        assert_eq!(
            rows[0].post_match_preview.as_deref(),
            Some("Rust admin investigation notes")
        );
    }

    #[tokio::test]
    async fn admin_quoted_post_search_matches_exact_substring_phrase() {
        let (_temp, pool, settings) = test_pool().await;
        let alice = register(&pool, &settings, "alice").await;
        let bob = register(&pool, &settings, "bob").await;
        create_post(&pool, &settings, alice, "hello world from alice").await;
        create_post(&pool, &settings, bob, "hello careful world from bob").await;

        let rows = users(&pool, AdminUserSearch::new("", r#""hello world""#))
            .await
            .expect("phrase search");

        assert_eq!(usernames(&rows), vec!["alice"]);
    }

    #[tokio::test]
    async fn admin_post_search_escapes_like_special_characters() {
        let (_temp, pool, settings) = test_pool().await;
        let alice = register(&pool, &settings, "alice").await;
        let bob = register(&pool, &settings, "bob").await;
        create_post(&pool, &settings, alice, "token 100%_safe").await;
        create_post(&pool, &settings, bob, "token 1000 safe").await;

        let rows = users(&pool, AdminUserSearch::new("", "100%_safe"))
            .await
            .expect("escaped search");

        assert_eq!(usernames(&rows), vec!["alice"]);
    }

    #[test]
    fn deep_settings_form_parsing_accepts_valid_values() {
        let settings = Settings::default();
        let mut form = form_from_settings(&settings);
        form.site_name = "Custom Site".to_owned();
        form.max_bio_len = "300".to_owned();
        form.allow_profile_pictures = "false".to_owned();

        let parsed = parse_deep_settings_form(&form, &settings).expect("valid form");

        assert_eq!(parsed.site_name, "Custom Site");
        assert_eq!(parsed.max_bio_len, 300);
        assert!(!parsed.allow_profile_pictures);
    }

    #[test]
    fn deep_settings_form_parsing_rejects_invalid_numbers() {
        let settings = Settings::default();
        let mut form = form_from_settings(&settings);
        form.max_bio_len.clear();
        assert!(parse_deep_settings_form(&form, &settings).is_err());

        let mut form = form_from_settings(&settings);
        form.max_bio_len = "nope".to_owned();
        assert!(parse_deep_settings_form(&form, &settings).is_err());

        let mut form = form_from_settings(&settings);
        form.max_bio_len = "-1".to_owned();
        assert!(parse_deep_settings_form(&form, &settings).is_err());
    }

    #[test]
    fn deep_settings_form_parsing_rejects_invalid_boolean_values() {
        let settings = Settings::default();
        let mut form = form_from_settings(&settings);
        form.allow_likes = "yes".to_owned();

        let err = parse_deep_settings_form(&form, &settings).expect_err("invalid bool");

        assert!(
            err.to_string()
                .contains("Allow likes must be true or false")
        );
    }

    #[test]
    fn unchanged_deep_settings_form_has_no_diff() {
        let settings = Settings::default();
        let form = form_from_settings(&settings);
        let parsed = parse_deep_settings_form(&form, &settings).expect("valid form");

        assert!(diff_deep_settings(&settings, &parsed).is_empty());
    }

    #[test]
    fn changed_deep_settings_diff_uses_friendly_labels_and_units() {
        let settings = Settings::default();
        let mut form = form_from_settings(&settings);
        form.max_bio_len = "300".to_owned();
        form.allow_profile_pictures = "false".to_owned();
        let parsed = parse_deep_settings_form(&form, &settings).expect("valid form");

        let diff = diff_deep_settings(&settings, &parsed);

        assert_eq!(
            diff,
            vec![
                DeepSettingsChange {
                    label: "Maximum bio length",
                    old_value: "240 characters".to_owned(),
                    new_value: "300 characters".to_owned(),
                },
                DeepSettingsChange {
                    label: "Allow profile pictures",
                    old_value: "true".to_owned(),
                    new_value: "false".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn deep_settings_media_mb_values_convert_to_bytes() {
        let settings = Settings::default();
        let mut form = form_from_settings(&settings);
        form.max_image_size_mb = "8".to_owned();
        form.max_video_size_mb = "50".to_owned();
        let parsed = parse_deep_settings_form(&form, &settings).expect("valid form");
        let updated = parsed.apply_to(&settings);

        assert_eq!(updated.media.max_image_size, 8 * MIB);
        assert_eq!(updated.media.max_video_size, 50 * MIB);
    }

    #[test]
    fn deep_settings_accepts_operator_chosen_minimum_password_length() {
        let settings = Settings::default();
        let mut form = form_from_settings(&settings);
        form.min_password_length = "5".to_owned();

        let parsed = parse_deep_settings_form(&form, &settings).expect("valid form");
        let updated = parsed.apply_to(&settings);

        assert_eq!(updated.accounts.min_password_length, 5);
    }

    #[test]
    fn deep_settings_writeback_preserves_unrelated_values_and_comments() {
        let temp = tempdir().expect("temp dir");
        let path = temp.path().join("settings.toml");
        crate::config::write_default_if_missing(&path).expect("default settings");
        let mut settings = Settings::load(&path).expect("load settings");
        settings.site.name = "Written Site".to_owned();
        settings.accounts.max_bio_len = 300;
        settings.media.max_image_size = 8 * MIB;

        write_deep_settings(&path, &settings).expect("write settings");

        let raw = fs::read_to_string(&path).expect("settings raw");
        let parsed = Settings::load(&path).expect("reload settings");
        assert!(raw.contains("# RustPost settings"));
        assert_eq!(parsed.site.name, "Written Site");
        assert_eq!(parsed.accounts.max_bio_len, 300);
        assert_eq!(parsed.media.max_image_size, 8 * MIB);
        assert_eq!(parsed.server.port, Settings::default().server.port);
        assert_eq!(
            parsed.media.ffmpeg_path,
            Settings::default().media.ffmpeg_path
        );
    }
}
