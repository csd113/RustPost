<div align="center">

<br/>

```
██████╗ ██╗   ██╗███████╗████████╗██████╗  ██████╗ ███████╗████████╗
██╔══██╗██║   ██║██╔════╝╚══██╔══╝██╔══██╗██╔═══██╗██╔════╝╚══██╔══╝
██████╔╝██║   ██║███████╗   ██║   ██████╔╝██║   ██║███████╗   ██║   
██╔══██╗██║   ██║╚════██║   ██║   ██╔═══╝ ██║   ██║╚════██║   ██║   
██║  ██║╚██████╔╝███████║   ██║   ██║     ╚██████╔╝███████║   ██║   
╚═╝  ╚═╝ ╚═════╝ ╚══════╝   ╚═╝   ╚═╝      ╚═════╝ ╚══════╝   ╚═╝   
```

**A single-binary, self-hosted microblog — yours alone, with no cloud required.**

[![CI](https://img.shields.io/github/actions/workflow/status/csd113/RustPost/ci.yml?branch=main&style=flat-square&label=CI&logo=github)](https://github.com/csd113/RustPost/actions)
[![Rust](https://img.shields.io/badge/rust-1.90%2B-orange?style=flat-square&logo=rust)](https://www.rust-lang.org/)
[![SQLite](https://img.shields.io/badge/database-SQLite-blue?style=flat-square&logo=sqlite)](https://www.sqlite.org/)
[![Embedded Arti](https://img.shields.io/badge/embedded%20Arti-0.43.0-7D4698?style=flat-square&logo=torproject)](https://www.torproject.org/)
[![License](https://img.shields.io/badge/license-see%20LICENSE-green?style=flat-square)](./LICENSE)

[**Getting Started**](#-getting-started) · [**Configuration**](#-configuration) · [**CLI Reference**](#-cli-reference) · [**Security**](#-security-model) · [**Tor / Arti**](#-tor--arti)

</div>

---

## Overview

RustPost is a **single-binary, self-hosted microblogging platform** written in Rust. It is deliberately single-instance and non-federated: one operator runs one SQLite-backed site with full social features, served as plain server-rendered HTML with no frontend build chain.

> **Design philosophy:** Small, auditable modules over framework magic. Security-sensitive behavior centralized. No JavaScript bundler. No cloud dependency. No federation surface.

This repository is intended for operators who want a small, understandable self-hosted service. Operators remain responsible for deployment hardening, monitoring, backups, upgrades, and deciding whether RustPost is appropriate for their environment.

---

## ✨ Features

### Accounts & Identity
| Feature | Details |
|---|---|
| Password hashing | Argon2id — never stored in plaintext |
| Sessions | Server-side, HttpOnly, SameSite=Lax cookies |
| Profile customization | Bio, avatar, and banner with WebP conversion |
| Admin CLI | Create admins and reset passwords from the command line |

### Social Features
| Feature | Details |
|---|---|
| Posts | Up to 280 Unicode characters |
| Replies & Threads | Threaded conversations with full timeline rendering |
| Reposts | First-class timeline events; deleted originals render gracefully |
| Follows, Blocks & Mutes | Standard social graph primitives |
| Likes & Bookmarks | Likes are public; bookmarks are private |
| Notifications | In-app notification feed |
| Search | Full-text search via SQLite FTS5 + user matching |
| Anonymous posting | Supported but **disabled by default** |

### Media Pipeline
| Feature | Details |
|---|---|
| Upload handling | Multipart with content sniffing and size limits |
| Image conversion | JPEG / PNG / GIF / WebP → WebP (requires `ffmpeg`) |
| Video conversion | MP4 / WebM / QuickTime → WebM VP9 (requires `ffmpeg`) |
| Fallback | Serves original format safely when conversion is unavailable |
| Timeouts | 120 s for images · 300 s for video |

### Operations
| Feature | Details |
|---|---|
| Database | SQLite with WAL, foreign keys, a release schema baseline, FTS5, and timeline indexes |
| Rate limiting | SQLite-backed per-user and per-IP limits for all write operations |
| Backups | Deterministic tar archive with manifest, hashes, DB snapshot, settings, media, assets, and optional Tor keys |
| Restore | Staged manifest/hash/SQLite/settings validation before runtime file swaps |
| Admin dashboard | Site health, users, media jobs, conversion state, and backup management |
| HTTP compression | Browser text responses use gzip when requested; media and binary uploads are intentionally left uncompressed |
| Tor / Arti | Embedded onion-service startup — clearnet-only, Tor-only, or dual mode |

---

## Platform Preview

RustPost renders as plain server-side HTML with full social interactions, profiles, threads, and media attachments.

| Home feed | Profile |
|---|---|
| ![RustPost home feed with reposted media and social actions](docs/screenshots/home-feed.png) | ![RustPost profile page with banner, avatar, bio, and posts](docs/screenshots/profile.png) |

| Threaded replies | Media posts |
|---|---|
| ![RustPost post thread with replies from multiple users](docs/screenshots/post-thread.png) | ![RustPost media post thread showing image attachment rendering](docs/screenshots/media-posts.png) |

| Mobile layout |
|---|
| ![RustPost mobile home feed layout](docs/screenshots/mobile.png) |

The screenshots above were captured from a local generated demo instance with fictional accounts and local placeholder media only. To inspect the same style of demo locally, see [Demo Preview](docs/demo-preview.md).

---

## 🚀 Getting Started

### Prerequisites

- **Rust 1.90+** — install via [rustup](https://rustup.rs/)
- **ffmpeg** *(optional)* — enables image and video conversion. RustPost boots and runs without it.

### Build from source

```sh
cargo build --release
```

This produces the release binary in `target/release/`:

| Binary | Purpose |
|---|---|
| `rustpost-cli` | Server and administration CLI |

### Install a release archive

Download the archive and matching `.sha256` file for your platform from the GitHub release, verify the checksum, then extract it. Unix archives contain `rustpost/rustpost-cli`; the Windows archive contains `rustpost/rustpost-cli.exe`.

Example Unix user-local install:

```sh
# Linux checksum verification
sha256sum -c rustpost-linux-x86_64.tar.gz.sha256
# macOS checksum verification uses: shasum -a 256 -c <archive>.sha256
tar -xzf rustpost-linux-x86_64.tar.gz
install -m 0755 rustpost/rustpost-cli "$HOME/.local/bin/rustpost-cli"
rustpost-cli --version
```

Use an explicit `--data-dir` for installed deployments. Without it, RustPost places `rustpost-data` beside the executable.

### First Run

```sh
# Linux / macOS
./target/release/rustpost-cli --data-dir ./rustpost-data serve

# Windows (PowerShell)
.\target\release\rustpost-cli.exe --data-dir .\rustpost-data serve
```

> If no subcommand is provided, `serve` is assumed.

On first boot, RustPost initializes the data directory automatically:

```
rustpost-data/
├── settings.toml          ← generated config with safe defaults
├── db/
│   ├── rustpost.sqlite3   ← main database
│   ├── rustpost.sqlite3-wal
│   └── rustpost.sqlite3-shm
├── uploads/
│   ├── originals/
│   ├── images/
│   ├── videos/
│   └── thumbs/
├── assets/
├── tmp/
│   └── uploads/           ← interrupted upload staging only
├── backups/
├── logs/
└── tor/
    └── onion-service/
```

> **Note:** All runtime paths are derived from `--data-dir` (or the executable location as fallback). RustPost does not rely on the current working directory.
> Runtime data is local operator state. Databases, uploads, logs, backups, Tor key material, and temporary upload files under `rustpost-data/` should not be committed to git. On Unix, RustPost restricts the runtime data directory to mode `0700`.
> Existing data directories that still contain `app.sqlite3` at the data-dir root are migrated to `db/rustpost.sqlite3` on startup. If both old and new database files exist, RustPost stops with a conflict error and does not overwrite either file.

### Create Your First Admin

If `admin.create_admin_on_first_boot` is enabled and no admin exists, interactive `serve` startup enters a `Create admin account` step before the server begins accepting requests. If stdin is not interactive, RustPost prints the bootstrap commands instead of waiting for input. Startup then prints the data directory, settings path, database path, upload/media paths, log path, backup path, bind address, local URL, and whether an admin account exists.

You can also create the first admin explicitly. The preferred local setup path hides the password while you type:

```sh
./target/release/rustpost-cli --data-dir ./rustpost-data create-admin-interactive
```

For scripted deployments, the non-interactive command is still available. After setting `RUSTPOST_ADMIN_PASSWORD` from the deployment's secret source, run the following. Be aware that command-line arguments can be visible to other local processes on some systems:

```sh
./target/release/rustpost-cli --data-dir ./rustpost-data create-admin alice "$RUSTPOST_ADMIN_PASSWORD"
```

Then open [http://127.0.0.1:8080](http://127.0.0.1:8080) and log in.

### Timeline

| Label | Meaning |
|---|---|
| Home Feed | Public top-level posts from your configured site |

Replies remain attached to their parent thread. They render inside the thread view and are not shown as unrelated top-level Home Feed posts.

---

## 🖥 CLI Reference

```sh
rustpost-cli init                                       # Initialize data directory
rustpost-cli check                                      # Validate config, data directory, and DB schema status
rustpost-cli create-admin <username> <password>         # Create an admin account
rustpost-cli create-admin-interactive                   # Create an admin with hidden password prompts
rustpost-cli reset-admin-password <username> <password> # Reset an admin's password
rustpost-cli seed-demo                                  # Seed a guarded local demo instance under target/debug/rustpost-demo
rustpost-cli serve                                      # Start the HTTP server (default)
rustpost-cli backup                                     # Create a backup archive
rustpost-cli backup --include-tor-keys                  # Backup including Tor private keys
rustpost-cli restore <archive.tar>                      # Restore from a backup
rustpost-cli restore <archive.tar> --include-tor-keys   # Restore including Tor keys
```

---

## ⚙ Configuration

`settings.toml` is generated on first run with conservative, safe defaults. Edit it to match your deployment.

### Site name

The visible site name is configured in `settings.toml`:

```toml
[site]
name = "RustPost"
```

Changing `site.name` updates the rendered browser title, header brand, footer, and user-facing site copy. The executable remains `rustpost-cli`; package names, cookie names, and data paths remain `rustpost` for compatibility.

### Account creation

```toml
[accounts]
registration_enabled = true
registration_captcha_enabled = false
```

`registration_captcha_enabled` adds a single-use CAPTCHA challenge to registration only. Login is unchanged.

### Post editing

```toml
[posts]
post_edit_window_seconds = 15
```

Users can edit their own post text only during this short server-enforced window. The default is 15 seconds; set it to `0` to disable post editing.

### Clearnet only (default)

```toml
[server]
host = "127.0.0.1"
port = 8080
cookie_secure = false   # set true when running behind HTTPS

[tor]
enabled = false
tor_only = false
```

### Tor only

```toml
[server]
host = "127.0.0.1"
port = 8080

[tor]
enabled = true
tor_only = true                        # binds only the loopback Arti forwarder
data_dir = "tor"
onion_service_name = "microblog"
bootstrap_timeout_secs = 120
max_concurrent_streams = 512
```

### Dual mode (clearnet + onion simultaneously)

```toml
[server]
host = "127.0.0.1"
port = 8080

[tor]
enabled = true
tor_only = false                       # clearnet starts immediately; onion boots in background
data_dir = "tor"
onion_service_name = "microblog"
bootstrap_timeout_secs = 120
max_concurrent_streams = 512
```

### Rate limiting

All limits are configured under `[moderation]`:

```toml
[moderation]
posts_per_minute                    = 5
replies_per_minute                  = 10
reposts_per_minute                  = 10
account_creations_per_ip_per_day    = 3
failed_login_attempts_per_15m       = 10
anonymous_posts_per_ip_per_hour     = 10
```

> Authenticated limits are keyed by user ID. Registration, failed login, and anonymous limits are keyed by direct peer IP. Forwarded headers are **not trusted by default**.

---

## 🔒 Security Model

| Control | Implementation |
|---|---|
| Passwords | Argon2id — never stored in plaintext |
| Session cookies | HttpOnly · SameSite=Lax · `Secure` controlled by config |
| CSRF | All state-changing authenticated routes require a CSRF token |
| Output escaping | User content is HTML-escaped before rendering |
| Upload safety | Filenames ignored; content sniffed; stored under fixed upload roots |
| SVG | Not an allowed default upload type |
| Admin routes | Require an admin session **and** CSRF protection |
| Trusted proxies | Explicit config; forwarded headers are not blindly trusted |
| Tor key material | Not stored in SQLite, not logged, not rendered in UI or admin health |

---

## 🧅 Tor / Arti

RustPost embeds [Arti](https://gitlab.torproject.org/tpo/core/arti) (the Rust Tor implementation) directly in the binary. No external `tor` daemon required.

Embedded Arti provides an onion-service transport option, not an anonymity or security guarantee. Real onion reachability depends on Tor network access, bootstrap, descriptor publication, and client routing; verify reachability from a separate Tor client before relying on it.
The active onion address is shown by the running server in its startup/status output, public header, and admin health page; it is not derived from configuration alone.

**Behavior by config:**

| `tor.enabled` | `tor_only` | Behavior |
|---|---|---|
| `false` | — | No Arti tasks started. Pure clearnet. |
| `true` | `false` | Clearnet binds immediately. Arti onion service starts in background. If Tor fails, clearnet keeps running and admin health reports the error. |
| `true` | `true` | Only a loopback listener is bound for Arti forwarding. Startup **fails** if Arti/onion startup fails. |

**Current pinned Arti/Tor crates in `Cargo.toml`:**

```
arti-client      = 0.43.0   # bootstraps the embedded Tor client and onion service
tor-hsservice    = 0.43.0   # onion-service config, handle, and rendezvous streams
tor-proto        = 0.43.0   # inspect and accept incoming onion stream requests
tor-cell         = 0.43.0   # cell-level protocol handling
tor-rtcompat     = 0.43.0   # Tokio-compatible Arti runtime
rustls           = 0.23     # ring crypto provider required by Arti's rustls stack
```

> **Dependency note:** `cargo tree -i libsqlite3-sys` should show exactly one version. RustPost uses `rusqlite` specifically to keep the `libsqlite3-sys` dependency unified with the Arti family.

**Tor data layout:**

```
rustpost-data/tor/
├── cache/                     ← Arti directory cache
└── onion-service/
    └── state/                 ← onion-service private keys (mode 0700 on Unix)
```

**Backups and Tor keys:**

```sh
rustpost-cli backup                          # excludes Tor keys (safe default)
rustpost-cli backup --include-tor-keys       # opt-in to include keys
rustpost-cli restore archive.tar             # rejects Tor key paths unless flag given
rustpost-cli restore archive.tar --include-tor-keys
```

On Unix, the backup directory is mode `0700` and created backup archives are mode `0600`.
Restore path validation rejects: absolute paths, traversal sequences, symlinks/hardlinks, duplicate entries, duplicate separators, Windows drive prefixes, backslash paths, encoded traversal or slash markers, and slash-like Unicode bypass characters.

**Live/local Tor smoke validation:**

- Start dual mode with a fresh explicit `--data-dir`, then confirm Arti bootstrap and onion descriptor publication in the logs.
- Valid public local smoke endpoints are `/`, `/home`, `/login`, and `/register`. RustPost does not implement `/healthz` or `/readyz`; use the startup/status output and authenticated `/admin/health` page for operational status.
- The active v3 onion hostname must contain 56 characters followed by `.onion`, and the same address must appear in startup/status output and the public Tor pill.
- Confirm the printed loopback Arti forwarder target serves the same local page as the clearnet listener.
- Onion-routed validation requires a reachable SOCKS proxy. Prefer Tor Browser at `127.0.0.1:9150`, then system Tor at `127.0.0.1:9050`:

```sh
if nc -z 127.0.0.1 9150; then
  socks_proxy=127.0.0.1:9150
elif nc -z 127.0.0.1 9050; then
  socks_proxy=127.0.0.1:9050
else
  echo "SOCKS unavailable"
fi

test -n "${socks_proxy:-}" &&
  curl --socks5-hostname "$socks_proxy" -fsS "http://<56-character-v3-address>.onion/"
```

No available SOCKS proxy is an environment limitation, not a RustPost product failure. The smoke can still validate Arti bootstrap, descriptor publication, UI onion-address consistency, local HTTP, and the loopback Arti forwarder.

---

## 🎬 Media & FFmpeg

RustPost **boots and runs without `ffmpeg`**. Conversion is optional and detected at runtime. Admin health reports whether `ffmpeg` is present and whether WebP/VP9 encoders are available.

**When `ffmpeg` is detected:**

- Images (JPEG, PNG, GIF, WebP) → converted to **WebP**
- Videos (MP4, WebM, QuickTime) → converted to **WebM VP9** with `yuv420p` and explicit BT.709 color metadata to avoid browser playback issues
- Image conversions: **120 s timeout**
- Video conversions: **300 s timeout**
- Conversion status (successes, fallbacks, stderr summaries) visible in admin media/health pages

**When `ffmpeg` is absent or conversion fails:** RustPost serves the original upload, provided it is an allowed content type.

Profile pictures and banners follow the same media pipeline as post uploads.

---

## 💾 Backup & Restore

```sh
# Create a backup (SQLite DB snapshot + settings + media/assets)
rustpost-cli backup

# Include Tor onion-service keys
rustpost-cli backup --include-tor-keys

# Restore into a fresh data directory
rustpost-cli restore rustpost-20260526T....tar

# Restore including Tor keys
rustpost-cli restore rustpost-20260526T....tar --include-tor-keys
```

Backups are also available from **Admin → Backups**. The page supports manual backup creation, admin-only downloads, restore from uploaded `.tar` archives, automatic backup settings, safe retention controls, recent archive history, and no-JS form flows.

Archive format:

- Tar entries are written in deterministic order with normalized header metadata.
- `manifest.toml` records the RustPost version, DB schema version, created timestamp, included components, runtime-relative paths, file sizes, SHA-256 hashes, and whether Tor keys are included.
- The durable runtime state covered by the format is `db/rustpost.sqlite3`, `settings.toml`, `uploads/originals`, `uploads/images`, `uploads/videos`, `uploads/thumbs`, `assets`, and required empty runtime directories.
- Runtime `tmp`, `logs`, `backups`, cache junk, Playwright artifacts, symlinks, and non-durable files are not included.
- Tor onion-service private keys are excluded by default. `--include-tor-keys` or the admin checkbox is required to include or restore them. Restored Tor key files are permission-restricted on Unix.

Restore safety:

- Backups are treated as hostile input. RustPost validates the manifest, hashes, entry types, paths, settings file, SQLite integrity, foreign keys, and schema compatibility before touching live runtime files.
- Restore stages into `tmp/`, creates a pre-restore safety backup, then swaps approved runtime roots. On failure it rolls back moved live paths and leaves the old runtime in place.
- Concurrent backup/restore attempts are rejected with a lock under `tmp/`.
- Admin-upload restore writes the restored files for the runtime, but the already-running process keeps its existing SQLite connection. Restart RustPost after a successful admin restore so it reopens the restored database and settings.

Automatic backups:

```toml
[backup]
enabled = true
backup_dir = "backups"
automatic_enabled = false
automatic_interval_minutes = 1440
retention_keep_last = 10
retention_max_age_days = 30
automatic_include_tor_keys = false
```

Automatic backups are disabled by default. Retention deletes only automatic archives (`rustpost-auto-*.tar`), always keeps the newest `retention_keep_last`, and never prunes manual or pre-restore safety backups.

---

## 🧪 Development

```sh
cargo update                                                           # update dependencies
cargo fmt --all --check                                                # check formatting
cargo clippy --workspace --all-targets --all-features -- -D warnings  # lint
cargo test --workspace --all-features                                  # run tests
```

### Local UI Regression Pass

Run the app locally with a disposable data directory:

```sh
cargo build --workspace --all-features
./target/debug/rustpost-cli --data-dir /tmp/rustpost-ui serve
```

Then open [http://127.0.0.1:8080](http://127.0.0.1:8080). The server initializes the data directory on first boot.

**CI matrix** (GitHub Actions, Rust 1.90):

| Platform | Arch |
|---|---|
| Linux | x86\_64 |
| Linux | ARM64 |
| macOS | Apple Silicon |
| Windows | x86\_64 |

CI runs: format check → Clippy → tests → release build. A separate strict Clippy job runs `clippy::all`, `clippy::pedantic`, `clippy::nursery`, and `clippy::cargo`.

### Release Artifacts

Tagged releases matching `v*` produce:

```
rustpost-linux-x86_64.tar.gz
rustpost-linux-aarch64.tar.gz
rustpost-macos-aarch64.tar.gz
rustpost-windows-x86_64.zip
```

Each archive contains `rustpost-cli` (`rustpost-cli.exe` on Windows), `README.md`, `LICENSE`, and optional notice files. A `.sha256` checksum is generated for each archive. Runtime data (databases, uploads, backups, logs, Tor keys) is never included.

---

## 📦 Dependencies

| Crate | Role |
|---|---|
| `axum` | HTTP routing and multipart handling |
| `tokio` | Async runtime, filesystem, process, and signals |
| `rusqlite` | SQLite access via a dedicated DB worker; WAL, FK, baseline schema plus forward migrations |
| `argon2` | Argon2id password hashing |
| `rand_core` + `uuid` | Secure salts and opaque generated tokens/filenames |
| `infer` | Content-based media type detection |
| `flate2` | Gzip response compression for browser text responses |
| `tower-http` | Static upload serving and HTTP tracing |
| `clap` | CLI argument parsing |
| `toml` + `serde` | Config load/save |
| `tar` + `walkdir` | Backup and restore archive handling |
| `arti-client` | Bootstraps the embedded Tor client and onion service |
| `tor-hsservice` | Onion-service config, running handle, and rendezvous streams |
| `tor-proto` + `tor-cell` | Inspect and accept incoming onion stream requests |
| `tor-rtcompat` | Tokio-compatible Arti runtime |
| `rustls` | Ring crypto provider for Arti's rustls stack |
| `tracing` | Privacy-conscious operational logging |

---

## ✅ Release Verification

*Last sweep: **June 6, 2026***

Release validation distinguishes the required Rust gates from the dependency-advisory review:

```sh
cargo fmt --all --check
cargo build --release --bins
cargo clippy --workspace --all-targets --all-features -- -D warnings -D clippy::all -D clippy::pedantic -D clippy::nursery -D clippy::cargo
cargo test --workspace --all-features
cargo audit
```

The format, release build, strict Clippy, and test commands must pass. `cargo audit` is reviewed separately and is **not fully clean** for v1.0.0 because of this documented upstream exception:

> **Accepted v1.0.0 upstream audit exception:** `RUSTSEC-2023-0071` affects transitive `rsa 0.9.10` through the current pinned Arti/Tor dependency family. The advisory reports a potential key-recovery timing side channel, and no fixed upgrade is currently available. This is an upstream dependency risk, not a verified RustPost application vulnerability. Re-evaluate it when updating Arti or before the next release.

`cargo audit` also reports unmaintained transitive `bincode 2.0.1` (`RUSTSEC-2025-0141`) and `paste 1.0.15` (`RUSTSEC-2024-0436`) dependencies inherited through the Arti dependency family. These are tracked as upstream maintenance caveats, not RustPost application vulnerabilities.

<details>
<summary>Verified locally</summary>

- Fresh `--data-dir` boot creates `settings.toml`, `db/rustpost.sqlite3`, upload roots, temp upload staging, backup/log dirs, and Tor state dirs.
- `rustpost-cli check` passes on a fresh data directory with `tor.enabled = false`.
- Clearnet serving on `127.0.0.1:8080` loads `/home`.
- Registration with CAPTCHA, login, post creation, replies, quote reposts, repost rendering, likes, bookmarks, followers/following pages, notifications, admin health, and CSRF-protected logout all work through live HTTP/browser flows.
- Anonymous posting is disabled by default — anonymous users cannot see the composer and anonymous post attempts are rejected.
- Non-admin users cannot access admin health; anonymous users cannot access authenticated pages.
- `ffmpeg` 8.1.1 detected with WebP and VP9 support. Image, profile picture/banner, and small video uploads live-tested; WebP and WebM outputs produced; admin media/health pages reported conversion state correctly.
- Normal backups include DB, settings, and media; exclude Tor keys. `--include-tor-keys` includes them only when explicitly requested. Restores into fresh data directories completed and `check` passed with and without controlled test Tor key material.
- Backup archive names include subsecond precision — no same-second overwrite collisions.
- Tor health/status fields render in admin health with Tor disabled in the current local sweep.
- `rustpost-cli --version`, `rustpost-cli --help`, and `rustpost-cli check` report the final release CLI and pass on a fresh data directory.

</details>

<details>
<summary>Partially verified / environment-dependent</summary>

- Live Tor reachability depends on Tor network access, descriptor publication time, and a separate Tor client. The June 2026 live/local smoke verified embedded Arti bootstrap and descriptor publication; onion-over-SOCKS reachability remained untested because no local SOCKS proxy was available.
- Tor private key material was not rendered in admin health or normal logs during the sweep. Operational text may reference key paths or the `--include-tor-keys` flag but does not print key contents.

</details>

---

## ⚠ Known Limitations

- **Tor verification requires network access** — may time out in restricted build or CI environments.
- **Onion virtual port** — HTTP virtual port 80 is mapped to the RustPost listener; custom onion virtual ports are not yet configurable.
- **Synchronous media conversion** — conversion happens inline during upload; no background queue.
- **Reports and admin toggles** — present in schema and admin structure but functionality is minimal in this release.
- **Search** — uses SQLite FTS5 with simple user matching and fixed result limits; no ranking tuning yet.
- **UI polish** — server-rendered HTML/CSS covers the core flows, but final visual design and broader accessibility review are still future work.

---

## 📄 License

See [LICENSE](./LICENSE) for terms.

---

<div align="center">

Built with Rust 🦀 · Powered by SQLite · Optional Tor via Arti

[github.com/csd113/RustPost](https://github.com/csd113/RustPost)

</div>
