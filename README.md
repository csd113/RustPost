# RustPost

RustPost is a single-binary, self-hosted microblogging app written in Rust. It is deliberately single-instance and not federated: one operator runs one SQLite-backed site with server-rendered HTML, local accounts, posts, replies, follows, likes, bookmarks, search, uploads, notifications, admin tools, and backup support.

This repository is a production-oriented MVP. The code favors small auditable modules over framework magic, keeps security-sensitive behavior centralized, and avoids a frontend build chain.

Repository: <https://github.com/csd113/RustPost>

## Features

- Local accounts with Argon2id password hashing and server-side sessions.
- Registration, login, logout, profile text/media settings, admin creation/reset commands.
- 280 Unicode-character posts, replies, local timeline, home timeline, profile timelines, and threads.
- Reposts as first-class timeline events, follows, blocks, mutes, likes, private bookmarks, in-app notifications, and search.
- Anonymous posting mode exists but is disabled by default.
- SQLite-backed rate limiting for posts, replies, reposts, failed login attempts, registrations, and anonymous posts.
- Multipart media uploads with content sniffing, generated filenames, size limits, and fixed upload roots.
- Image conversion to WebP and video conversion to WebM VP9 when `ffmpeg` is available.
- Safe upload fallback to original allowed formats when conversion is unavailable or fails.
- SQLite with WAL, foreign keys, migrations, FTS5 post search, and timeline indexes.
- Admin pages for site health, users, media jobs, and backups.
- Tar backup creation including DB, settings, media, and optional Tor onion-service keys.
- Restore path validation that rejects traversal and requires explicit opt-in for Tor keys.
- Tor/Arti configuration boundary and admin health reporting.

## Build And Run

RustPost requires Rust 1.90 or newer.

```sh
cargo build --release
./target/release/rustpost-cli
```

On first run RustPost creates `rustpost-data` next to the executable:

```text
rustpost-data/settings.toml
rustpost-data/app.sqlite3
rustpost-data/uploads/originals/
rustpost-data/uploads/images/
rustpost-data/uploads/videos/
rustpost-data/uploads/thumbs/
rustpost-data/backups/
rustpost-data/logs/
rustpost-data/tor/
rustpost-data/tor/onion-service/
```

Important paths are derived from the executable location unless `--data-dir` or `--config` is provided. The app does not rely on the current working directory for runtime data.

## CLI

```sh
rustpost-cli init
rustpost-cli check
rustpost-cli create-admin <username> <password>
rustpost-cli reset-admin-password <username> <password>
rustpost-cli backup
rustpost-cli backup --include-tor-keys
rustpost-cli restore <archive.tar>
rustpost-cli restore <archive.tar> --include-tor-keys
rustpost-cli print-onion-address
rustpost-cli serve
```

If no subcommand is provided, `serve` is used.

## Configuration

`rustpost-data/settings.toml` is generated with conservative defaults. Clearnet is enabled by default on `127.0.0.1:8080`; Tor is available in config but disabled by default.

Clearnet only:

```toml
[server]
host = "127.0.0.1"
port = 8080
cookie_secure = false

[tor]
enabled = false
tor_only = false
```

Tor only intent:

```toml
[tor]
enabled = true
tor_only = true
data_dir = "tor"
onion_service_name = "microblog"
bootstrap_timeout_secs = 120
max_concurrent_streams = 512
```

Dual mode intent:

```toml
[server]
host = "127.0.0.1"
port = 8080

[tor]
enabled = true
tor_only = false
data_dir = "tor"
onion_service_name = "microblog"
bootstrap_timeout_secs = 120
max_concurrent_streams = 512
```

Rate limits are configured under `[moderation]`:

```toml
posts_per_minute = 5
replies_per_minute = 10
reposts_per_minute = 10
account_creations_per_ip_per_day = 3
failed_login_attempts_per_15m = 10
anonymous_posts_per_ip_per_hour = 10
```

Authenticated post/reply/repost limits are keyed by user id. Registration, failed login, and anonymous post limits are keyed by the direct peer IP address observed by Axum. Forwarded headers are not trusted implicitly.

Profile pictures and banners are managed from `/settings`. Profile media must be an allowed image upload, is processed through the same safe media pipeline as post uploads, and is converted to WebP when `ffmpeg` and WebP support are available. Replacing or deleting profile media removes the previously served media file and database row. If `media.keep_original_uploads = true`, retained conversion originals are intentionally preserved according to the global media retention setting.

Reposts appear as timeline events with the reposter shown above the original post. Duplicate reposts by the same user are idempotent and do not create duplicate notifications. If the original post is deleted, the repost renders as an unavailable-post event instead of breaking the timeline.

## FFmpeg

RustPost boots without `ffmpeg`. Admin health shows whether `ffmpeg` is available and whether WebP / VP9 encoders are detected.

When available:

- JPEG, PNG, GIF, and WebP images are converted to WebP where configured.
- MP4, WebM, and QuickTime videos are converted to WebM VP9 where configured.
- VP9 conversion uses `yuv420p` and explicit BT.709 color metadata to avoid common browser playback and encoder failures.
- Image conversions are bounded by a 120 second timeout; video conversions are bounded by a 300 second timeout.
- Recent conversion successes/fallbacks and stderr summaries are available from admin media/health pages.

If conversion fails or `ffmpeg` is missing, RustPost serves the original upload only when the original content type is allowed.

## Tor / Arti

RustPost includes embedded Arti onion-service startup in the single binary. `tor.enabled = false` remains the default and starts no Arti tasks. With `tor.enabled = true` and `tor_only = false`, RustPost starts normal clearnet service and attempts an Arti onion service; if Tor startup fails, clearnet remains available and admin health reports the Tor error. With `tor.enabled = true` and `tor_only = true`, RustPost binds only a loopback listener for internal Arti forwarding and fails startup if Arti/onion startup fails.

Arti version note: the Tor Project released Arti 2.3.0 on 2026-05-07. The corresponding library crate family used here is 0.42.0. Direct Arti/Tor dependencies are:

- `arti-client = 0.42.0`: bootstraps the embedded Tor client and launches the onion service.
- `tor-hsservice = 0.42.0`: onion-service config, running service handle, and rendezvous stream handling.
- `tor-proto = 0.42.0` and `tor-cell = 0.42.0`: inspect and accept incoming onion stream requests.
- `tor-rtcompat = 0.42.0`: names the Tokio-compatible Arti runtime kept alive by RustPost.

RustPost uses `rusqlite` for SQLite access. The app is SQLite-only, and rusqlite keeps the database dependency stable while avoiding the `sqlx 0.9` alpha that was previously needed for `libsqlite3-sys` compatibility. This also aligns the app with the Arti/rusqlite/libsqlite3-sys dependency family. `cargo tree -i libsqlite3-sys` should show only one `libsqlite3-sys` version.

Arti state, directory cache, and onion-service keys live under the configured runtime data directory. With the default config this is:

```text
rustpost-data/tor/cache/
rustpost-data/tor/onion-service/state/
```

On Unix, RustPost sets the Tor directories it creates to `0700`. Tor private keys are not stored in SQLite, not logged by RustPost, and not rendered in normal UI/admin health. Admin health shows Tor enabled/running state, bootstrap status when available, the onion hostname only after Arti reports it, and the last clear startup/forwarding error if one exists.

Normal backups exclude Tor onion-service keys. `rustpost-cli backup --include-tor-keys` includes them explicitly. Restore rejects Tor key archive paths unless `--include-tor-keys` is also passed, and restore path validation rejects traversal, absolute paths, links, Windows prefixes, encoded traversal/slash forms, and slash-like Unicode bypasses.

Limitations: normal tests do not require live Tor network access. A real onion hostname requires successful Arti bootstrap and descriptor publication, which depends on network access and can take time. If bootstrap times out, increase `tor.bootstrap_timeout_secs` or check local network/Tor reachability. Onion forwarding accepts conventional HTTP onion streams on virtual port 80 and forwards them to the internal RustPost loopback listener.

## Security Model

- Passwords are never stored in plaintext.
- Session cookies are HttpOnly and SameSite=Lax; `Secure` is controlled by config.
- State-changing authenticated routes require CSRF tokens.
- User content is HTML escaped before rendering.
- Upload filenames are ignored for storage and checked for traversal tricks.
- Uploaded content is sniffed and stored under fixed upload roots.
- SVG is not an allowed default upload type.
- Admin routes require an admin account and CSRF protection.
- Trusted proxy CIDRs are explicit config; forwarded headers are not blindly trusted.

## Backup And Restore

Create a normal backup:

```sh
rustpost-cli backup
```

Include Tor onion-service keys only with explicit opt-in:

```sh
rustpost-cli backup --include-tor-keys
```

Restore validates every archive path, rejects absolute paths and traversal, and rejects Tor key paths unless `--include-tor-keys` is provided.

Restore also rejects symlink/hardlink archive entries, Windows drive/prefix syntax, backslash paths, encoded traversal markers, encoded slash markers, and slash-like Unicode bypass characters.

## Development

```sh
cargo update
cargo tree
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

## Major Dependencies

- `axum`: HTTP routing and multipart handling.
- `tokio`: async runtime, filesystem, process handling, and signals.
- `rusqlite`: SQLite access through a dedicated DB worker, WAL/foreign-key setup, and migrations.
- `argon2`: Argon2id password hashing.
- `rand_core` and `uuid`: secure salts and generated opaque tokens/names.
- `infer`: content-based media detection.
- `tower-http`: static upload serving and HTTP tracing.
- `clap`: CLI parsing.
- `toml` and `serde`: config load/save.
- `tar` and `walkdir`: backup and restore archive handling.
- `arti-client`, `tor-hsservice`, `tor-proto`, `tor-cell`, and `tor-rtcompat`: embedded Arti onion-service startup and local stream forwarding.
- `tracing`: privacy-conscious operational logging.

Cargo selected compatible versions for Rust 1.90. The Arti crate family is pinned to 0.42.0 for the Arti 2.3.0 mapping.

## Screenshots

Screenshots are not checked in yet. The first screens to capture are the local timeline, thread view, profile page, settings page, and admin health dashboard.

## Known Limitations

- Live Tor verification requires network access and may time out in restricted build environments.
- Onion service support maps HTTP virtual port 80 to the RustPost app; custom onion virtual ports are not configurable yet.
- Media conversion is synchronous during upload.
- Reports and admin settings toggles are represented in schema/admin structure but are minimal.
- Search uses SQLite FTS5 and simple user matching with fixed result limits.
