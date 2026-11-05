# Looking after your RustPost site

Start RustPost, sign in as your administrator, and open **Admin**. You can manage accounts, change settings, check media processing, and create backups there. For your first installation, use [Getting started](getting-started.md).

[All guides](README.md) · [Settings](#configuration) · [Backups](#backup-and-restore) · [Commands](#cli-reference) · [Public deployment](deployment.md) · [Updates](software-updates.md)

This guide describes **v1.1.0**. Older v1.0.0 differs in some operational behavior; in particular, v1.1.0 requires restoring with the server stopped.

## Everyday administration

| Task | Where to go |
| --- | --- |
| Rename the site or close registration | **Admin → Deep server settings**; review, save, then restart when indicated |
| Manage accounts and sessions | **Admin → Users** |
| Publish an announcement or pause publishing | **Admin** overview |
| Check database, Tor, or media readiness | **Admin → Health** |
| Review conversion results | **Admin → Media jobs** |
| Create, download, or schedule backups | **Admin → Backups** |
| Check for a newer release | **Admin → Software updates** |

Maintenance mode pauses publishing for ordinary users while keeping the site readable. It does not stop the process or permit a live restore. See [community controls](account-and-instance-features.md#community-announcements-and-maintenance).

## Configuration

RustPost creates `settings.toml` on first run. It is a text file with sections such as `[site]` and `[accounts]`. You can edit most settings from **Deep server settings**: change values, review the before/after summary, then confirm or discard. The page shows saved values and differences from the running configuration.

Most changes take effect **after restarting RustPost**. The NSFW blur switch applies immediately when saved through the admin editor. Backup policy is read for operations and by the scheduler on its next check (about once a minute). Announcement and maintenance changes are live database settings.

For a file edit, change existing entries rather than adding a second copy of a section:

```toml
[site]
name = "Our community"

[accounts]
registration_enabled = false
```

This is an excerpt, not a complete replacement file. Keep the other generated sections. Validate the file before restarting:

```sh
./rustpost-cli --data-dir ./rustpost-data check
```

If validation fails, fix the reported value before restarting. `check` reads settings and inspects schema compatibility; it does not initialize or repair the database. Use `init` first for a new directory.

For Docker, see [Editing settings](docker.md#editing-settings). For every key, default, and special case, see the [configuration reference](admin-configuration-audit.md).

### Useful defaults

| Setting | Default |
| --- | --- |
| New registrations | Enabled |
| Anonymous posting | Disabled |
| Registration CAPTCHA | Disabled |
| Maximum post length | 280 characters |
| Text-edit window | 15 seconds |
| Attachments per post | Up to 4 images or 1 video; 4 total |
| Image / video upload limit | 50 MiB / 150 MiB per file |
| Account deletion grace period | 30 days |
| Automatic backups | Disabled |
| Tor | Disabled |

Limits can be changed in the editor within the ranges the app accepts. The `posts.allow_hashtags` field is reserved; current hashtag extraction does not depend on it. `tor.display_onion_address` must stay blank: the app displays its actual running onion address.

## Runtime storage

The **data directory** holds your site. Always point administrator commands at the same directory used by `serve`.

- With the README commands, `./rustpost-data` is relative to the terminal's current folder. Changing folders changes what that relative path means.
- With no `--data-dir`, RustPost uses `rustpost-data` beside the executable.
- For a service, use an absolute path such as `/srv/rustpost` to avoid ambiguity.
- In the documented Docker setup, the data directory is `/data` in a persistent volume.

The directory contains `settings.toml`, `db/rustpost.sqlite3`, `uploads/`, `assets/`, `backups/`, `logs/`, `tmp/`, and Tor state. Keep it private and out of Git; Unix installations restrict its directory permissions to `0700`.

Legacy `app.sqlite3` files at the data-directory root are moved to `db/rustpost.sqlite3` on startup. If both exist, startup reports a conflict and preserves both. Back up and investigate the two copies instead of deleting one blindly.

## Administrator accounts

Use hidden prompts to create or reset an administrator. Run these from the executable's folder, with your actual data path:

```sh
./rustpost-cli --data-dir ./rustpost-data create-admin-interactive
./rustpost-cli --data-dir ./rustpost-data reset-admin-password-interactive USERNAME
```

Replace `USERNAME` with the administrator's username. On Windows, use `.\rustpost-cli.exe`. In Docker, use `docker exec -it rustpost rustpost-cli --data-dir /data` followed by the command.

For someone else's ordinary account, the **Users** page can require a password change or revoke their active sessions. See [account controls](account-reference.md#admin-forced-password-reset-31) for what those actions allow and restrict.

### Password input for automation

Version 1.1.0 supports `create-admin USERNAME --password-stdin` and `reset-admin-password USERNAME --password-stdin`. They read one UTF-8 line, up to 4096 bytes, from redirected input. Supply it from a protected file or secret-management tool; avoid literal passwords in commands or shell history. Legacy positional passwords still work but can appear in process listings.

Hidden prompts require terminal stdin and stderr. Ctrl+C, Ctrl+D, or Esc cancels; validation or confirmation failures allow retry. On legacy Windows consoles, paste one line at a time. Forced termination or closing the terminal can prevent normal terminal cleanup.

## Backup and restore

A full-site backup saves the database, settings, media, and assets. In **Admin → Backups**, create a backup and download it to another computer or storage location. A backup left only on the server cannot protect against losing that server.

To create one from the terminal:

```sh
./rustpost-cli --data-dir ./rustpost-data backup
```

The command reports the archive path under `backups/`. Backup creation can run while the site is serving; it uses an online SQLite snapshot and coordinates application writes. Tor private keys are excluded unless you explicitly include them.

### Restore with the server stopped

Restoring replaces site data with the archive's contents. First preserve a separate backup of the current site and keep the restore archive outside the folders being replaced.

1. Stop RustPost and wait for it to exit. Stop any supervisor that would restart it.
2. From the executable's folder, restore to the original data directory:

   ```sh
   ./rustpost-cli --data-dir ./rustpost-data restore /path/to/ARCHIVE_NAME.tar
   ./rustpost-cli --data-dir ./rustpost-data check
   ```

3. If restore and validation succeed, start it again with the usual `serve` command. Verify accounts, posts, and media in the browser.

Replace the archive path with your file. On Windows, use `.\rustpost-cli.exe` and a quoted Windows path if needed. Use the [Docker restore procedure](docker.md#backing-up-and-restoring) for a container.

**Version 1.1.0 disables browser/live restore** and takes a server lock during terminal restore. Older v1.0.0 guides described uploading a restore archive in Admin; that is not the current flow. If restore reports a recovery directory or rollback failure, preserve all recovery files and investigate the error before restarting. Startup deliberately refuses unresolved restore recovery state.

### Tor keys and backup contents

Add `--include-tor-keys` to `backup` to include private onion-service identity keys. Restoring an archive with those keys also requires `--include-tor-keys`. Protect such archives with access controls and encrypted storage. Without the old identity keys, a replacement onion service may have a different address.

The backup manifest records application/schema versions, timestamps, components, sizes, SHA-256 hashes, and Tor-key inclusion. Durable content includes `db/rustpost.sqlite3`, `settings.toml`, upload variants, and assets. Temporary files, logs, old backups, caches, and symlinks are excluded.

Restore validates entry paths and types, manifest hashes, settings, database integrity, foreign keys, and schema compatibility before replacing runtime files. It stages under the data directory, creates a pre-restore safety backup, and keeps rollback evidence if recovery cannot complete. Archive validation rejects traversal, absolute paths, links, duplicate entries, and other unsafe names. Allow free disk space for staging, the safety backup, and recovery copies.

### Automatic backups

Enable scheduled backups in **Admin → Backups**. Defaults are a 1440-minute interval, at least the newest 10 automatic archives retained, and age cleanup after 30 days. Automatic backups start disabled.

Retention applies to `rustpost-auto-*.tar` archives only. It protects the configured newest count and never prunes manual or pre-restore backups. Review disk usage and copy backups off the host yourself. Automatic Tor-key inclusion is a separate explicit policy.

## Media and FFmpeg

RustPost runs without `ffmpeg`. When available and enabled, it converts accepted images to WebP, videos to WebM/VP9, and can generate video thumbnails. The default accepted types are JPEG, PNG, GIF, WebP, MP4, WebM, and QuickTime. Profile pictures and banners use the media pipeline too.

Conversion happens during upload, with bounded concurrent work. Image conversions have a 120-second timeout; videos have a 300-second timeout. Converted videos use `yuv420p` and BT.709 color metadata. An allowed original upload is used when conversion is unavailable or fails; that does not ensure every browser can play its codec.

Check **Health** for executable/encoder availability and **Media jobs** for outcomes. `media.keep_original_uploads` controls retention alongside successful conversions and is false by default. Upload limits and encoding quality are listed in the [configuration reference](admin-configuration-audit.md).

## Tor / Arti

Tor is optional. It gives the same site an onion address using embedded [Arti](https://gitlab.torproject.org/tpo/core/arti); you do not need a separate Tor daemon for the server.

To add onion access, edit the existing `[tor]` section or use the admin editor, then restart:

```toml
[tor]
enabled = true
tor_only = false
```

Leave the other generated Tor settings in place. In this dual mode, ordinary HTTP starts immediately and Tor starts in the background. If Tor fails, HTTP keeps serving and status reports the failure.

For onion-only access, set both `enabled = true` and `tor_only = true`. Only the local Arti forwarder listens; startup fails if Tor cannot start. Verify onion access before depending on this mode. Onion HTTP uses virtual port 80; custom onion virtual ports are not configurable.

The running server supplies the onion address shown in status, the public header, and admin health. Keep `display_onion_address` empty. State lives under `tor/cache/` and `tor/onion-service/state/`; changing the service name or moving keys without a migration can change the site's identity.

### Verify access

Tor status, address assignment, and descriptor publication are useful diagnostics, but **external reachability needs a separate Tor client**. Open the displayed address in Tor Browser. If using its SOCKS proxy at `127.0.0.1:9150` (or a system Tor proxy at `127.0.0.1:9050`), replace the placeholder below with the actual address:

```sh
curl --socks5-hostname 127.0.0.1:9150 -fsS 'http://YOUR_ACTUAL_ONION_ADDRESS.onion/'
```

The v3 hostname has 56 characters before `.onion`. Without an available SOCKS proxy or Tor network access, only local HTTP and server-side status can be checked. Tor transport alone does not guarantee anonymity.

## CLI reference

Prefix each command with the executable and your data directory, for example `./rustpost-cli --data-dir ./rustpost-data`. Use `--help` or `COMMAND --help` for the installed build's exact options.

| Command | Purpose |
| --- | --- |
| `init` | Create runtime directories and default settings |
| `check` | Read settings and inspect optional services/schema; does not migrate or initialize |
| `serve` | Start the website; also the default when no subcommand is supplied |
| `create-admin-interactive` | Create an admin with hidden prompts |
| `reset-admin-password-interactive USERNAME` | Reset an admin password with hidden prompts |
| `create-admin USERNAME --password-stdin` | Create an admin from protected redirected input |
| `reset-admin-password USERNAME --password-stdin` | Reset an admin from protected redirected input |
| `backup [--include-tor-keys]` | Create an archive |
| `restore ARCHIVE.tar [--include-tor-keys]` | Restore with the server stopped |
| `seed-demo` | Seed an explicit disposable demo directory; see [demo guide](demo-preview.md) |

Global console options are `--console auto|tui|plain|off` and `--ascii`. `--config PATH` selects a settings file; `--data-dir PATH` selects site storage. Put both path options before the subcommand. They do not override individual settings. See the [terminal guide](terminal-console.md) for controls and measurement limits.

`check` reports missing fresh databases, adoptable legacy schemas, and optional FFmpeg absence without treating them as corruption. It exits nonzero for incompatible/unreadable databases or layout conflicts. A legacy database is inspected in place.

## Further reference

- [Deployment and security](deployment.md), including HTTPS and trusted proxies.
- [Software updates](software-updates.md), including version comparisons and manual replacement.
- [Development checks](development.md#checks) and [dependency review](deployment.md#security-and-dependency-review).
- [Account controls](account-and-instance-features.md), including blocking, muting, and transfers.
