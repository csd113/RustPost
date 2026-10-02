# Configuration audit and administration contract

The source of truth is `Settings` in `src/config.rs`, its generated commented TOML template, validation, and actual runtime consumers. The inventory below covers **65 TOML options**. **30** were previously editable (24 in deep settings, six in backups); **31** are newly exposed. **61** are now editable and four are intentionally deployment-managed. Five additional site controls (four SQLite instance values and a favicon file) were already administered through the overview, bringing persisted site configuration to **70 options** and prior web coverage to **35**.

## Sources and precedence

`Settings::default()` generates first-run settings. Existing TOML deserializes typed required fields, with serde defaults for backward-compatible additions listed below. The CLI validates the result before using it. CLI `--config` selects a file (the editor uses the same resolved `RuntimePaths.settings_path`); `--data-dir` selects storage. Neither overrides field values. There are **no per-setting environment or CLI overrides**. Admin displays the resolved file and saved/running distinctions; ordinary startup settings apply after restart.

There are four additional startup/test controls outside those 70 persisted options: `RUST_LOG` → tracing `EnvFilter` at startup (default `rustpost=info,tower_http=info`), CLI `--config`, CLI `--data-dir`, and debug-only `RUSTPOST_E2E_CAPTCHA_ANSWER` → registration challenge generation (five allowed CAPTCHA characters; not compiled into release behavior). These remain process/deployment/test controls: a web editor cannot change process environment or relocate its configuration/storage safely. Changing log filters can disclose operational detail or overwhelm output and must follow the deployment's logging policy. One-off backup/restore `--include-tor-keys` and `--allow-tor-keys` are operation consent flags, not persistent settings; existing backup UI provides equivalent explicit consent.

`Settings::load` parses; callers validate before mutation or startup. TOML required fields stay required. Defaults and key names are unchanged. An absent backup table defaults to `backups`; an existing backup table omitting `backup_dir` retains its existing serde default (empty path, the data root). The editor preserves that historical distinction.

## Model contract and safety

`src/config/admin_fields.rs` defines one typed field contract. Its macro generates form fields, enums, metadata, parsing, exact display values and typed getters/setters that name the actual configuration members. Defaults come directly from `Settings::default`. TOML and form submissions use the same `Settings::validate`. An exhaustive serialized-model coverage test requires the union of editable fields and documented exclusions to equal every model leaf, prohibits duplicate keys/form names, and round-trips every default field. A model addition or serde rename therefore fails coverage instead of silently disappearing from admin.

The writer replaces only changed, approved TOML value spans, including multiline values and quoted keys; unrelated keys, sections and adjacent comments survive. Comments inside a replaced array/string are part of that replaced value. Unchanged fields preserve their original spelling and formatting. Missing defaulted fields/sections are inserted. Both configuration and backup pages use this writer. A same-directory unique tempfile, inherited permissions, file sync and atomic replace prevent truncated files. Unix directory sync follows the replace. Non-regular destination files are rejected; temporary files clean themselves up on failure. All values validate before any write.

Web writers share a mutex. Settings forms carry a SHA-256 revision covering the original file and require fresh review if another save changed it. External file edits are detected at form submission, but operators should still avoid editing the file concurrently during the brief write itself; no portable filesystem compare-and-swap exists here. Authorization and CSRF precede writes; unknown/duplicate fields and incomplete required values fail deserialization. Normal unchecked checkboxes deserialize as false. Invalid values preserve the other submitted values in an escaped, accessible form.

There are no editable secret-valued configuration fields in this model: no SMTP, database credentials, API credentials, encryption keys or signing keys. The editor never reads onion private-key contents or database password hashes. Unknown TOML extension values are preserved and never displayed. The automatic-backup private-key flag controls inclusion, not secret disclosure; its help explains that archives contain sensitive private keys.

## Timing and interface

Nine sections: Site, Accounts, Posts, Media, Networking & cookies, Rate limiting, Onion service, Backups, Administration. All share admin navigation with overview, health, users, media jobs and backups. Settings have labels, defaults, help, units and timing; list textareas use one entry per line, optional strings clear by empty input, byte limits use exact decimal MiB without rounding. Server-rendered category anchors and preview/confirm/discard work without JavaScript. JavaScript only adds search. Existing routes, IDs and field names are preserved.

`media.nsfw_blur_enabled` uses the existing runtime atomic and applies immediately. Backup policy is read for manual operations and by the scheduler on its next 60-second check. Every other active setting is startup-only; saved differences show the current running value and restart-pending status. Reserved fields explicitly describe their lack of runtime effect. `server.public_url` is only shown by the startup dashboard; trusted proxies are unused (forwarded headers are ignored); `posts.allow_hashtags` does not gate current hashtag extraction. These behaviors are preserved rather than silently changed during an admin upgrade.

## Complete TOML inventory

All entries below live in `settings.toml` under their canonical section/key. None has a value override from environment/CLI. “Optional” means omitted fields/tables are supported through serde defaults; all other fields remain required. None contains a secret. Numeric values retain Rust type ranges unless a smaller bound is listed. Signed rate limits <= 0 block the relevant action. Empty lists disable the corresponding upload formats or proxy trust list. Default byte values are bytes; controls show MiB.

| Canonical key | Rust type | Default | Optional | Consumer | Prior admin | Current editing / timing / bounds |
|---|---|---|---|---|---|---|
| `site.name` | `String` | `"RustPost"` | Yes | compression, server, db | Yes | Restart required; Shown in page titles, the header, and the footer. Empty hides the display name. |
| `server.host` | `String` | `"127.0.0.1"` | No | cli | No | Restart required; IP address to listen on. A restart can disconnect this console. |
| `server.port` | `u16` | `8080` | No | tor, cli | No | Restart required; TCP port, 0–65535. Port 0 asks the OS to choose a port. Restart required. |
| `server.public_url` | `String` | `""` | No | terminal startup/status dashboard only; generated links remain relative | No | Restart required; Optional HTTP(S) URL shown in the startup dashboard. RustPost currently generates relative links regardless of this value. |
| `server.cookie_secure` | `bool` | `false` | No | server | No | Restart required; Enable only when visitors always use HTTPS. |
| `server.trusted_proxy_cidrs` | `Vec<String>` | `["127.0.0.1/32", "::1/128"]` | No | Reserved; client IP uses direct ConnectInfo, ignores forwarded headers | No | Restart required; One IP/CIDR per line. Reserved: RustPost currently ignores forwarded IP headers and uses the direct connection address. |
| `accounts.registration_enabled` | `bool` | `true` | No | server | Yes | Restart required; Allow this feature. |
| `accounts.registration_captcha_enabled` | `bool` | `false` | Yes | server | Yes | Restart required; Allow this feature. |
| `accounts.anonymous_mode_enabled` | `bool` | `false` | No | social, server | Yes | Restart required; Allow this feature. |
| `accounts.min_password_length` | `usize` | `10` | No | server, validation | Yes | Restart required; Characters. Recommended default is 10; 0 permits empty passwords. |
| `accounts.max_username_len` | `usize` | `32` | No | portability, auth, identity | Yes | Restart required; Characters. |
| `accounts.max_display_name_len` | `usize` | `64` | No | server, validation | Yes | Restart required; Characters. |
| `accounts.max_bio_len` | `usize` | `240` | No | server, validation | Yes | Restart required; Characters. |
| `accounts.allow_profile_banners` | `bool` | `true` | No | server | Yes | Restart required; Allow this feature. |
| `accounts.allow_profile_pictures` | `bool` | `true` | No | server | Yes | Restart required; Allow this feature. |
| `accounts.deletion_grace_period_days` | `u64` | `30` | Yes | server, account | No | Restart required; Days, 0–3650. Set 0 for immediate deletion. |
| `accounts.max_archive_upload_bytes` | `u64` | `314572800` | Yes | portability, server | No | Restart required; MiB. Applies to imports and exports; 0.0625–2048 MiB. |
| `accounts.max_archive_expanded_bytes` | `u64` | `1073741824` | Yes | portability, server | No | Restart required; MiB of decompressed media; 0.0625–16384 MiB. |
| `accounts.max_archive_entries` | `usize` | `10000` | Yes | portability | No | Restart required; Tar entries per import, 1–100000. |
| `posts.max_text_chars` | `usize` | `280` | No | portability, social, server | Yes | Restart required; Characters. |
| `posts.post_edit_window_seconds` | `u64` | `15` | Yes | social, server | Yes | Restart required; Seconds, 0–300. Set 0 to disable editing. |
| `posts.max_images_per_post` | `usize` | `4` | No | social, server | Yes | Restart required; Attachments per post. |
| `posts.max_videos_per_post` | `usize` | `1` | No | social, server | Yes | Restart required; Attachments per post. |
| `posts.max_media_per_post` | `usize` | `4` | No | social, server | Yes | Restart required; Attachments per post. |
| `posts.allow_reposts` | `bool` | `true` | No | posts runtime | Yes | Restart required; Allow this feature. |
| `posts.allow_replies` | `bool` | `true` | No | posts runtime | Yes | Restart required; Allow this feature. |
| `posts.allow_likes` | `bool` | `true` | No | posts runtime | Yes | Restart required; Allow this feature. |
| `posts.allow_bookmarks` | `bool` | `true` | No | posts runtime | Yes | Restart required; Allow this feature. |
| `posts.allow_hashtags` | `bool` | `true` | No | Reserved; hashtag extraction is unconditional | Yes | Restart required; Reserved: hashtag extraction currently operates regardless of this flag. |
| `posts.allow_mentions` | `bool` | `true` | No | social | Yes | Restart required; Allow this feature. |
| `media.ffmpeg_path` | `String` | `"ffmpeg"` | No | ffmpeg | No | Deployment executable: arbitrary process selection with server privileges. |
| `media.convert_images_to_webp` | `bool` | `true` | No | media | No | Restart required; Convert accepted images using the existing image processor. |
| `media.convert_videos_to_webm` | `bool` | `true` | No | media runtime | No | Restart required; Convert accepted videos using FFmpeg. |
| `media.keep_original_uploads` | `bool` | `false` | No | media | No | Restart required; Retain original uploads in addition to converted media. |
| `media.nsfw_blur_enabled` | `bool` | `true` | Yes | portability, auth, server | Yes | Immediate; Blur flagged media unless a user disables their own blur setting. |
| `media.max_image_size` | `u64` | `52428800` | No | server | Yes | Restart required; MiB per image (1 MiB = 1,048,576 bytes). Decimal values are accepted. |
| `media.max_video_size` | `u64` | `157286400` | No | media, server | Yes | Restart required; MiB per video (1 MiB = 1,048,576 bytes). Decimal values are accepted. |
| `media.generate_video_thumbnails` | `bool` | `true` | No | media runtime | No | Restart required; Use FFmpeg to extract video previews. |
| `media.allowed_image_mime_types` | `Vec<String>` | `["image/jpeg", "image/png", "image/gif", "image/webp"]` | No | portability | No | Restart required; One MIME type per line, for example image/jpeg. Empty disables image uploads. |
| `media.allowed_video_mime_types` | `Vec<String>` | `["video/mp4", "video/webm", "video/quicktime"]` | No | portability | No | Restart required; One MIME type per line, for example video/mp4. Empty disables video uploads. |
| `media.webp_quality` | `u8` | `82` | No | ffmpeg | No | Restart required; Image quality, 0–100. Higher values produce larger files. |
| `media.vp9_crf` | `u8` | `32` | No | ffmpeg | No | Restart required; VP9 constant quality, 0–63. Lower values produce larger files. |
| `media.vp9_deadline` | `String` | `"good"` | No | ffmpeg | No | Restart required; Best prioritizes quality; good balances speed; realtime prioritizes speed. |
| `tor.enabled` | `bool` | `false` | No | server, backup, tor, cli | No | Restart required; Start the embedded Arti onion service on restart. |
| `tor.tor_only` | `bool` | `false` | No | tor, cli | No | Restart required; Disable the public HTTP listener. Requires the onion service; ensure onion access before restarting. |
| `tor.data_dir` | `String` | `"tor"` | No | runtime, backup, cli | No | Private-key storage relocation needs a filesystem migration; changing it alone can lose onion identity. |
| `tor.onion_service_name` | `String` | `"microblog"` | No | tor | No | Restart required; Validated local name. Changing it selects a different onion identity on restart; it does not migrate keys. |
| `tor.display_onion_address` | `String` | `""` | Yes | tor runtime | No | Restart required; Optional 56-character v3 onion hostname. Empty uses the running onion service address. |
| `tor.bootstrap_timeout_secs` | `u64` | `120` | No | tor | No | Restart required; Seconds allowed for Arti bootstrap. |
| `tor.max_concurrent_streams` | `usize` | `512` | No | tor | No | Restart required; Streams per circuit; must fit in a 32-bit unsigned integer. |
| `tor.include_tor_keys_in_backups_by_default` | `bool` | `false` | No | config validation forbids true; manual backups use explicit consent | No | Must remain false by validation; manual private-key inclusion requires explicit consent. |
| `moderation.posts_per_minute` | `i64` | `5` | No | server | No | Restart required; Posts per account per minute; values of 0 or less block posting. |
| `moderation.replies_per_minute` | `i64` | `10` | No | server | No | Restart required; Replies per account per minute; values of 0 or less block replies. |
| `moderation.reposts_per_minute` | `i64` | `10` | No | server | No | Restart required; Reposts per account per minute; values of 0 or less block reposts. |
| `moderation.account_creations_per_ip_per_day` | `i64` | `3` | No | server | No | Restart required; Registrations per client IP per day; values of 0 or less block registration. |
| `moderation.failed_login_attempts_per_15m` | `i64` | `10` | No | server | No | Restart required; Failed logins per client IP per 15 minutes; values of 0 or less block login. |
| `moderation.anonymous_posts_per_ip_per_hour` | `i64` | `10` | No | server | No | Restart required; Anonymous posts per client IP per hour; values of 0 or less block posting. |
| `admin.create_admin_on_first_boot` | `bool` | `true` | No | cli: first-admin interactive bootstrap | No | Restart required; Prompt for an administrator at startup only when no administrator exists. |
| `backup.enabled` | `bool` | `true` | Yes | server, backup, tor, cli | Yes | Live / next scheduler check; Allow manual backups and scheduled backups. |
| `backup.backup_dir` | `String` | `"backups"` | Yes | runtime paths + backup scheduler/restore | No | Backup storage relocation needs a filesystem migration; startup paths are immutable. |
| `backup.automatic_enabled` | `bool` | `false` | Yes | server, backup | Yes | Live / next scheduler check; Run automatic backups using the configured interval. |
| `backup.automatic_interval_minutes` | `u64` | `1440` | Yes | server, backup | Yes | Live / next scheduler check; Minutes between backups; minimum 1. Scheduler checks once per minute. |
| `backup.retention_keep_last` | `usize` | `10` | Yes | server, backup | Yes | Live / next scheduler check; Keep the newest 1–10000 automatic backups. Manual backups are unaffected. |
| `backup.retention_max_age_days` | `u64` | `30` | Yes | server, backup | Yes | Live / next scheduler check; Days, 0–3650. Set 0 to disable age-based pruning. |
| `backup.automatic_include_tor_keys` | `bool` | `false` | Yes | server, backup | Yes | Live / next scheduler check; Sensitive: backups will contain private onion identity keys. Protect backup files. |

## Other persisted site options

| Option | Source / default | Validation / runtime | Admin / timing |
|---|---|---|---|
| announcement | SQLite instance_settings / empty | <=280 characters, controls limited to newline/tab; layout | Existing overview, live |
| announcement_enabled | SQLite / false | Canonical stored `1`; empty text disables | Existing overview, live |
| maintenance_mode | SQLite / false | Canonical `1`; maintenance middleware and posting controls | Existing overview, live |
| maintenance_message | SQLite / empty | <=280 characters, validated controls; default maintenance copy when empty | Existing overview, live |
| favicon | assets/favicon.{ico,png,svg} / built-in | <=256 KiB, signature/format and passive SVG validation | Existing overview upload/remove, live |

## Constants and unexposed subsystems

Repository-wide searches covered TOML, serde/defaults, CLI, environment, `DEFAULT_`, `MAX_`, `MIN_`, durations, paths, constructor arguments, feature flags and runtime reads. There is no email/SMTP subsystem, external federation protocol, configurable DB DSN/credentials, TLS/master-key configuration, or configurable cache layer to expose.

Fixed limits are implementation/security invariants, not operator settings: 100-million-pixel image decoding ceiling, media sniff buffers/thumbnail geometry; favicon parser/size limits; CAPTCHA alphabet/length/10-minute expiry and bounded store; archive manifest/document/count/text/hash/path-validation safety ceilings; ZIP/tar format versions; username reserved routes and released-username tombstone caps; SQL schema versions/required indexes/triggers; search/pagination/suggestion caps; backup lock names and once-per-minute polling; temp cleanup scan caps and active-operation guards; YouTube endpoint/ID rules, response ceilings and subsecond/network deadlines; compression threshold/buffering limits; CSRF token history; hashing/Argon2 policy and session lifecycle; scheduler cadence, temp-file age, seven-day session cleanup and two-hour rate-event retention. None was previously configurable through an admin-oriented source. Exposing parser guardrails or internal batching knobs would broaden the product/config surface and weaken safety without a corresponding existing configuration contract. This upgrade does not invent new keys for them.

Runtime directory names, SQLite busy timeouts/WAL settings and worker sizes are fixed implementation choices; relocating databases and private-key storage requires an offline filesystem operation, not a field edit. Cargo features and optional Tor dependencies are build-time choices. Demo credentials are confined to the explicit demo-seed CLI operation, never exposed as settings. User profile/theme/protection/NSFW preferences, import/export metadata and moderation database state are account/operational data, not instance configuration; their existing dedicated UI remains authoritative.

## Verification

Rust coverage includes complete model classification, defaults, enum parsing, valid/invalid limits and URLs/CIDRs/MIME lists, empty optional values, exact byte round trips, multi-line/quoted TOML persistence, missing-field insertion, unrelated value preservation, file-write failure and restart classification. HTTP coverage verifies admin authorization, CSRF, malformed/duplicate/incomplete submissions, stale revisions, preserved submissions, preview/discard/confirm and unchanged running settings.

Run `cargo fmt --all --check`, `cargo check --workspace --all-targets --all-features`, strict Clippy and `cargo test --workspace --all-features`. Browser suite: first `cargo build --workspace --all-features --bin rustpost-cli`, then `npx playwright test -c tests/playwright/configuration-admin.config.mjs` (uses the existing @playwright/test installation). It self-hosts disposable runtimes in Chromium, WebKit and Chromium with JavaScript disabled, plus a Firefox no-JS project, checks every category/type, error retention, save/reload/restart, unknown-secret nondisclosure, category navigation, search, keyboard focus/label/help associations, controls and page overflow at 320/375/768/1024/1440/1920px. Screenshots/traces stay in ignored `output/playwright`. Test fixture extensions contain dummy values only.

Validation on this host: Chromium, Chromium no-JS and WebKit pass, including all six responsive widths and shared navigation across all six administration pages. Firefox's bundled Nightly fails before test execution with a macOS sandbox-extension/framebuffer error; the Firefox project remains available for compatible hosts. Accessibility checks assert label/help associations, field error associations, status roles, visible keyboard focus and tab order; this is not a full assistive-technology certification. No external/live Tor network tests were needed for this configuration/editor change.
