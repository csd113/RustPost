# RustPost operator guide

This guide covers detailed configuration, administration, backups, Tor, and the v1.0.0 release checks. For installation and everyday use, start with the [README](../README.md).

[Commands](#cli-reference) · [Configuration](#configuration) · [Security](#security-model) · [Tor](#tor--arti) · [Media](#media-and-ffmpeg) · [Backups](#backup-and-restore) · [Release checks](#release-verification)

## CLI reference

For an installed app, add `--data-dir /path/to/your/site` before the subcommand, using the same directory every time. In Docker, use `docker exec rustpost rustpost-cli --data-dir /data <command>`; add `-it` to `docker exec` for interactive prompts.

```sh
rustpost-cli init                                       # Initialize data directory
rustpost-cli check                                      # Validate config, data directory, and DB schema status
rustpost-cli create-admin <username> <password>         # Create an admin account
rustpost-cli create-admin-interactive                   # Create an admin with hidden password prompts
rustpost-cli reset-admin-password <username> <password> # Reset an admin's password
rustpost-cli --data-dir target/debug/rustpost-demo seed-demo # Create local demo data
rustpost-cli serve                                      # Start the HTTP server (default)
rustpost-cli backup                                     # Create a backup archive
rustpost-cli backup --include-tor-keys                  # Backup including Tor private keys
rustpost-cli restore <archive.tar>                      # Restore from a backup
rustpost-cli restore <archive.tar> --include-tor-keys   # Restore including Tor keys
```

Prefer `create-admin-interactive` for local setup: it hides the password while you type. Passwords passed as arguments to `create-admin` or `reset-admin-password` can be visible to other local processes.

### Runtime storage

An explicit `--data-dir` keeps settings, the database at `db/rustpost.sqlite3`, uploads, assets, backups, logs, temporary upload files, and Tor state together. Without it, the default `rustpost-data` folder is beside the executable. Runtime paths do not depend on the current working directory. On Unix the app restricts the data directory to mode `0700`.

Older data directories with `app.sqlite3` at their root are migrated to `db/rustpost.sqlite3` on startup. If both files exist, the app stops with a conflict error and preserves both.

---

## Configuration

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

### Account lifecycle

```toml
[accounts]
deletion_grace_period_days = 30
max_archive_upload_bytes = 314572800
max_archive_expanded_bytes = 1073741824
max_archive_entries = 10000
```

Deleting an account stores a deletion deadline instead of removing data immediately; the owner can cancel until the deadline, and finalization runs from the maintenance scheduler (including after a restart). The default is 30 days; `0` deletes immediately after password confirmation.

The three archive ceilings are independent and documented in [account-and-instance-features.md](account-and-instance-features.md): the compressed upload/export size, the total decompressed media size accepted during import, and the entry count. The `/settings/import` body limit is derived from the compressed ceiling plus a small multipart allowance, so archive imports never depend on the global media body limit, and an oversized upload is rejected with a clear `413` page before any account state changes.

Announcements and maintenance mode are managed from the admin dashboard and stored in the database, so they survive restarts and are included in backups. See [account-and-instance-features.md](account-and-instance-features.md) for the full behavior and policy details.

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

## Security model

| Control | Implementation |
|---|---|
| Passwords | Argon2id — never stored in plaintext |
| Session cookies | HttpOnly · SameSite=Lax · `Secure` controlled by config |
| CSRF | All state-changing authenticated routes require a CSRF token |
| Output escaping | User content is HTML-escaped before rendering |
| Upload safety | Filenames ignored; content sniffed; stored under fixed upload roots |
| SVG | Not an allowed default upload type |
| Admin routes | Require an admin session **and** CSRF protection |
| Client IPs | Rate limits use the direct peer IP; forwarded headers are not used. `trusted_proxy_cidrs` is present in settings but does not currently change this behavior. |
| Tor key material | Not stored in SQLite, not logged, not rendered in UI or admin health |

---

## Tor / Arti

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
arti-client      = 0.46.0   # bootstraps the embedded Tor client and onion service
tor-hsservice    = 0.46.0   # onion-service config, handle, and rendezvous streams
tor-proto        = 0.46.0   # inspect and accept incoming onion stream requests
tor-cell         = 0.46.0   # cell-level protocol handling
tor-rtcompat     = 0.46.0   # Tokio-compatible Arti runtime
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
- Valid public local smoke endpoints are `/`, `/home`, `/login`, and `/register`. RustPost does not implement `/healthz` or `/readyz`; managed updates use the loopback-only `/internal/update-health` endpoint. For ordinary operation use the startup/status output and authenticated `/admin/health` page for operational status.
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

## Media and FFmpeg

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

## Backup and restore

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

## Blocking, muting, and muted words

RustPost enforces these rules in the database and query layer, not only in the UI.

| Action | Effect |
|---|---|
| Block | Removes follow relationships in both directions. The blocker and the blocked account cannot follow, reply to, quote, like, repost, bookmark, or mention each other's content, and neither account's posts appear in the other's feeds, threads, search results, mention suggestions, or notifications. The blocked account sees the profile without activity; neither account's display name, bio, or counts are removed from the database. Unblocking restores normal visibility and interactions. |
| Mute | Hides the muted account's posts from the muter's feeds, search, mention suggestions, notifications, and profile activity. Mutes do not notify the muted account. |
| Muted words | Per-user list, editable in Settings. Matching is case-insensitive (Unicode-aware), does not use regular expressions, and matches anywhere in the post text, so muting `cat` also hides `concatenate`. Matching applies to home, profile, media, likes, search, and quote previews, and to notification previews for other people's posts. Your own posts are never hidden by your own muted words. |

Block and mute rows are deleted when either account is deleted, and deleting a post or account removes the associated media files once no other row references them.


---

## Release verification

*Live-flow sweep: **August 15, 2026**. Rust release gates and advisory review: **September 29, 2026**.*

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

## Known limitations

- **Tor verification requires network access** — may time out in restricted build or CI environments.
- **Onion virtual port** — HTTP virtual port 80 is mapped to the RustPost listener; custom onion virtual ports are not yet configurable.
- **Synchronous media conversion** — conversion happens inline during upload; no background queue.
- **Reports and admin toggles** — present in schema and admin structure but functionality is minimal in this release.
- **Search** — uses SQLite FTS5 with simple user matching and fixed result limits; no ranking tuning yet.
- **UI polish** — server-rendered HTML/CSS covers the core flows, but final visual design and broader accessibility review are still future work.

---

## Software updates

Use **Admin → Software updates** for stable GitHub release checks. A configured native Linux updater supports signed installation with mandatory exact SQLite/config backups, atomic activation, bounded health checks and automatic binary/database rollback. Containers remain deployment-managed. See [software-updates.md](software-updates.md) for administrator behavior, provisioning, retention, troubleshooting and signing/release requirements.
