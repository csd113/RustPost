# Configuration reference

Most site settings can be changed in **Admin → Deep server settings**. Change a value, review the summary, then confirm or discard it. Start with the [operator guide](operator-guide.md#configuration) for everyday changes; this page lists every setting and explains how the editor saves them.

[All guides](README.md) · [Settings inventory](#complete-toml-inventory) · [Editor behavior](#how-the-editor-saves-changes) · [Deployment](deployment.md)

## Choose where to change a setting

The model contains **65 TOML options**: 61 appear in the web editor, and four remain deployment-managed. The admin overview also manages four database values (announcement text/enabled and maintenance message/enabled) plus the favicon file. Account preferences are managed from each account's Settings page.

The four file-only fields are `media.ffmpeg_path`, `tor.data_dir`, `backup.backup_dir`, and `tor.include_tor_keys_in_backups_by_default`. Executable selection and moving private storage require access to the host; the last field must remain false because manual key inclusion requires explicit consent.

## Sources and timing

`src/config.rs` defines the model, defaults, validation, and generated commented TOML. `src/config/admin_fields.rs` defines editor fields. Actual runtime consumers determine behavior: some older generated/editor help text still describes proxy trust as unused or a manually supplied onion display address as accepted. **Current code uses trusted proxies and rejects a non-empty onion display address**; the corrected behavior is documented here and in [Deployment](deployment.md#https-and-proxy-settings).

The CLI's `--config` selects a settings file; `--data-dir` selects storage. Neither overrides individual settings. There are no per-setting environment overrides. `RUST_LOG` selects tracing filters at startup. Debug builds also recognize `RUSTPOST_E2E_CAPTCHA_ANSWER` for controlled test fixtures; it is not an operator setting or release-mode CAPTCHA bypass.

Startup settings apply after restart. The editor shows stored values alongside pending differences from the running state. The NSFW blur toggle saved through the editor applies immediately; backup policy is read by manual operations and on the scheduler's next check. Announcements and maintenance mode are live database values. `server.public_url` supplies display information while links remain relative; `posts.allow_hashtags` remains reserved and does not disable hashtag extraction.

Missing backward-compatible fields use serde defaults; required fields remain required. An absent backup table defaults to `backups`, while an existing table omitting `backup_dir` retains the historical empty-path default (the data root). Preserve the distinction when editing old configurations.

## How the editor saves changes

Fields are grouped as Site, Accounts, Posts, Media, Networking & cookies, Rate limiting, Onion service, Backups, and Administration. Lists use one item per line; byte controls display exact MiB values. Category links and review/confirm/discard work without JavaScript; JavaScript adds search.

The editor validates values before writing. It replaces changed TOML value spans while preserving unrelated keys, sections, comments, and unchanged formatting. Comments inside a replaced value are replaced with that value. Missing defaulted fields can be inserted. The writer uses a same-directory temporary file, inherited permissions, file synchronization, and atomic replacement; non-regular destination files are rejected.

Web writers share a lock. Settings forms carry a revision of the file so an intervening edit requires fresh review instead of silently overwriting it. Preview and confirmation revalidate input; running startup settings remain unchanged until restart. Backup settings share the configuration writer.

The typed field contract names actual model members and generates parsing and accessors. Historical coverage described in earlier versions of this page is not a claim about today's test suite; see [current checks](development.md#checks).

## Complete TOML inventory

All entries below live in `settings.toml` under their canonical section/key. None has a value override from environment/CLI. “Optional” means omitted fields/tables are supported through serde defaults; all other fields remain required. None contains a secret. Numeric values retain Rust type ranges unless a smaller bound is listed. Signed rate limits <= 0 block the relevant action. Empty lists disable the corresponding upload formats or proxy trust list. Default byte values are bytes; controls show MiB.

| Canonical key | Rust type | Default | Optional | Consumer | Editing / timing / bounds |
|---|---|---|---|---|---|
| `site.name` | `String` | `"RustPost"` | Yes | compression, server, db | Restart required; Shown in page titles, the header, and the footer. Empty hides the display name. |
| `server.host` | `String` | `"127.0.0.1"` | No | cli | Restart required; IP address to listen on. A restart can disconnect this console. |
| `server.port` | `u16` | `8080` | No | tor, cli | Restart required; TCP port, 0–65535. Port 0 asks the OS to choose a port. Restart required. |
| `server.public_url` | `String` | `""` | No | terminal startup/status dashboard only; generated links remain relative | Restart required; Optional HTTP(S) URL shown in the startup dashboard. RustPost currently generates relative links regardless of this value. |
| `server.cookie_secure` | `bool` | `false` | No | server | Restart required; Enable only when visitors always use HTTPS. |
| `server.trusted_proxy_cidrs` | `Vec<String>` | `["127.0.0.1/32", "::1/128"]` | No | request provenance and client-IP resolution | Restart required; One IP/CIDR per line. Only listed direct peers can supply forwarded scheme and client-IP metadata; see the deployment guide. |
| `accounts.registration_enabled` | `bool` | `true` | No | server | Restart required; Allow this feature. |
| `accounts.registration_captcha_enabled` | `bool` | `false` | Yes | server | Restart required; Allow this feature. |
| `accounts.anonymous_mode_enabled` | `bool` | `false` | No | social, server | Restart required; Allow this feature. |
| `accounts.min_password_length` | `usize` | `10` | No | server, validation | Restart required; Characters. Recommended default is 10; 0 permits empty passwords. |
| `accounts.max_username_len` | `usize` | `32` | No | portability, auth, identity | Restart required; Characters. |
| `accounts.max_display_name_len` | `usize` | `64` | No | server, validation | Restart required; Characters. |
| `accounts.max_bio_len` | `usize` | `240` | No | server, validation | Restart required; Characters. |
| `accounts.allow_profile_banners` | `bool` | `true` | No | server | Restart required; Allow this feature. |
| `accounts.allow_profile_pictures` | `bool` | `true` | No | server | Restart required; Allow this feature. |
| `accounts.deletion_grace_period_days` | `u64` | `30` | Yes | server, account | Restart required; Days, 0–3650. Set 0 for immediate deletion. |
| `accounts.max_archive_upload_bytes` | `u64` | `314572800` | Yes | portability, server | Restart required; MiB. Applies to imports and exports; 0.0625–2048 MiB. |
| `accounts.max_archive_expanded_bytes` | `u64` | `1073741824` | Yes | portability, server | Restart required; MiB of decompressed media; 0.0625–16384 MiB. |
| `accounts.max_archive_entries` | `usize` | `10000` | Yes | portability | Restart required; Tar entries per import, 1–100000. |
| `posts.max_text_chars` | `usize` | `280` | No | portability, social, server | Restart required; Characters. |
| `posts.post_edit_window_seconds` | `u64` | `15` | Yes | social, server | Restart required; Seconds, 0–300. Set 0 to disable editing. |
| `posts.max_images_per_post` | `usize` | `4` | No | social, server | Restart required; Attachments per post. |
| `posts.max_videos_per_post` | `usize` | `1` | No | social, server | Restart required; Attachments per post. |
| `posts.max_media_per_post` | `usize` | `4` | No | social, server | Restart required; Attachments per post. |
| `posts.allow_reposts` | `bool` | `true` | No | posts runtime | Restart required; Allow this feature. |
| `posts.allow_replies` | `bool` | `true` | No | posts runtime | Restart required; Allow this feature. |
| `posts.allow_likes` | `bool` | `true` | No | posts runtime | Restart required; Allow this feature. |
| `posts.allow_bookmarks` | `bool` | `true` | No | posts runtime | Restart required; Allow this feature. |
| `posts.allow_hashtags` | `bool` | `true` | No | Reserved; hashtag extraction is unconditional | Restart required; Reserved: hashtag extraction currently operates regardless of this flag. |
| `posts.allow_mentions` | `bool` | `true` | No | social | Restart required; Allow this feature. |
| `media.ffmpeg_path` | `String` | `"ffmpeg"` | No | ffmpeg | Deployment executable: arbitrary process selection with server privileges. |
| `media.convert_images_to_webp` | `bool` | `true` | No | media | Restart required; Convert accepted images using the existing image processor. |
| `media.convert_videos_to_webm` | `bool` | `true` | No | media runtime | Restart required; Convert accepted videos using FFmpeg. |
| `media.keep_original_uploads` | `bool` | `false` | No | media | Restart required; Retain original uploads in addition to converted media. |
| `media.nsfw_blur_enabled` | `bool` | `true` | Yes | portability, auth, server | Immediate; Blur flagged media unless a user disables their own blur setting. |
| `media.max_image_size` | `u64` | `52428800` | No | server | Restart required; MiB per image (1 MiB = 1,048,576 bytes). Decimal values are accepted. |
| `media.max_video_size` | `u64` | `157286400` | No | media, server | Restart required; MiB per video (1 MiB = 1,048,576 bytes). Decimal values are accepted. |
| `media.generate_video_thumbnails` | `bool` | `true` | No | media runtime | Restart required; Use FFmpeg to extract video previews. |
| `media.allowed_image_mime_types` | `Vec<String>` | `["image/jpeg", "image/png", "image/gif", "image/webp"]` | No | portability | Restart required; One MIME type per line, for example image/jpeg. Empty disables image uploads. |
| `media.allowed_video_mime_types` | `Vec<String>` | `["video/mp4", "video/webm", "video/quicktime"]` | No | portability | Restart required; One MIME type per line, for example video/mp4. Empty disables video uploads. |
| `media.webp_quality` | `u8` | `82` | No | ffmpeg | Restart required; Image quality, 0–100. Higher values produce larger files. |
| `media.vp9_crf` | `u8` | `32` | No | ffmpeg | Restart required; VP9 constant quality, 0–63. Lower values produce larger files. |
| `media.vp9_deadline` | `String` | `"good"` | No | ffmpeg | Restart required; Best prioritizes quality; good balances speed; realtime prioritizes speed. |
| `tor.enabled` | `bool` | `false` | No | server, backup, tor, cli | Restart required; Start the embedded Arti onion service on restart. |
| `tor.tor_only` | `bool` | `false` | No | tor, cli | Restart required; Disable the public HTTP listener. Requires the onion service; ensure onion access before restarting. |
| `tor.data_dir` | `String` | `"tor"` | No | runtime, backup, cli | Private-key storage relocation needs a filesystem migration; changing it alone can lose onion identity. |
| `tor.onion_service_name` | `String` | `"microblog"` | No | tor | Restart required; Validated local name. Changing it selects a different onion identity on restart; it does not migrate keys. |
| `tor.display_onion_address` | `String` | `""` | Yes | tor runtime | Restart required; Must remain blank by validation. The running Arti service supplies the displayed address. |
| `tor.bootstrap_timeout_secs` | `u64` | `120` | No | tor | Restart required; Seconds allowed for Arti bootstrap. |
| `tor.max_concurrent_streams` | `usize` | `512` | No | tor | Restart required; Positive bound used for Arti streams per circuit and global forwarding concurrency; must fit the runtime limit. |
| `tor.include_tor_keys_in_backups_by_default` | `bool` | `false` | No | config validation forbids true; manual backups use explicit consent | Must remain false by validation; manual private-key inclusion requires explicit consent. |
| `moderation.posts_per_minute` | `i64` | `5` | No | server | Restart required; Posts per account per minute; values of 0 or less block posting. |
| `moderation.replies_per_minute` | `i64` | `10` | No | server | Restart required; Replies per account per minute; values of 0 or less block replies. |
| `moderation.reposts_per_minute` | `i64` | `10` | No | server | Restart required; Reposts per account per minute; values of 0 or less block reposts. |
| `moderation.account_creations_per_ip_per_day` | `i64` | `3` | No | server | Restart required; Registrations per client IP per day; values of 0 or less block registration. |
| `moderation.failed_login_attempts_per_15m` | `i64` | `10` | No | server | Restart required; Failed logins per client IP per 15 minutes; values of 0 or less block login. |
| `moderation.anonymous_posts_per_ip_per_hour` | `i64` | `10` | No | server | Restart required; Anonymous posts per client IP per hour; values of 0 or less block posting. |
| `admin.create_admin_on_first_boot` | `bool` | `true` | No | cli: first-admin interactive bootstrap | Restart required; Prompt for an administrator at startup only when no administrator exists. |
| `backup.enabled` | `bool` | `true` | Yes | server, backup, tor, cli | Live / next scheduler check; Allow manual backups and scheduled backups. |
| `backup.backup_dir` | `String` | `"backups"` | Yes | runtime paths + backup scheduler/restore | Backup storage relocation needs a filesystem migration; startup paths are immutable. |
| `backup.automatic_enabled` | `bool` | `false` | Yes | server, backup | Live / next scheduler check; Run automatic backups using the configured interval. |
| `backup.automatic_interval_minutes` | `u64` | `1440` | Yes | server, backup | Live / next scheduler check; Minutes between backups; minimum 1. Scheduler checks once per minute. |
| `backup.retention_keep_last` | `usize` | `10` | Yes | server, backup | Live / next scheduler check; Keep the newest 1–10000 automatic backups. Manual backups are unaffected. |
| `backup.retention_max_age_days` | `u64` | `30` | Yes | server, backup | Live / next scheduler check; Days, 0–3650. Set 0 to disable age-based pruning. |
| `backup.automatic_include_tor_keys` | `bool` | `false` | Yes | server, backup | Live / next scheduler check; Sensitive: backups will contain private onion identity keys. Protect backup files. |

## Other persisted site options

| Option | Source / default | Validation / runtime | Admin / timing |
|---|---|---|---|
| announcement | SQLite instance_settings / empty | <=280 characters, controls limited to newline/tab; layout | Existing overview, live |
| announcement_enabled | SQLite / false | Canonical stored `1`; empty text disables | Existing overview, live |
| maintenance_mode | SQLite / false | Canonical `1`; maintenance middleware and posting controls | Existing overview, live |
| maintenance_message | SQLite / empty | <=280 characters, validated controls; default maintenance copy when empty | Existing overview, live |
| favicon | assets/favicon.{ico,png,svg} / built-in | <=256 KiB, signature/format and passive SVG validation | Existing overview upload/remove, live |

## What is not a site setting?

Parser safety ceilings, schema versions, search result caps, database worker sizes, fixed cleanup intervals, and archive format rules are implementation limits. They cannot be tuned through this settings file. There is no SMTP service, federation protocol, external database connection string, or built-in TLS certificate configuration.

Runtime directory names and database/private-key relocation require an offline filesystem procedure. Account preferences, moderation records, and import/export history belong to their dedicated account or administration screens. Demo credentials are confined to the explicit demo-seed operation.
