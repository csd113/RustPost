# Administrator software updates

Open **Admin → Software updates** (also linked as **Updates** in the admin navigation). The installed version always comes from the running executable. **Check for updates** queries the public API of `csd113/RustPost` on GitHub: published stable releases only, ordered by semantic version. No token is required. Network/API failures are shown without affecting ordinary operation. Availability checks are manual; installation is never unattended.

A managed native Linux instance displays the latest compatible version (falling back past newer incompatible releases), date, bounded escaped plain-text release notes, download size, signature/compatibility result, install protections, backup history and last transaction result. Install requires the administrator's current password, valid CSRF token and a one-use updater approval. Another check replaces the old approval; concurrent or replayed installations are refused. RustPost has one administrator role, and suspended, forced-password-reset and deletion-pending accounts retain their existing restrictions.

Click **Install update** after reviewing the release. User mutations pause, the updater checks space/paths/schema, downloads and verifies the release, stages its executable, and verifies a live database/configuration backup before stopping the service. It then refreshes the backup after shutdown to capture all acknowledged writes, atomically activates the staged release, and starts it. The new executable performs its own transactional startup migrations. Health checks must confirm the expected version, schema, DB access and writable persistent directories before success is recorded and writes resume.

Expect a brief outage, plus any startup/migration time. JavaScript progressively polls the authenticated status endpoint and reconnects after restart. Without JavaScript, reload **Admin → Software updates** after the service returns; the final result survives both application and updater restarts. A rollback result means the previous executable **and exact pre-upgrade database/configuration** were restored before the previous service restarted. User media, assets, Tor keys and instance identity are preserved in place. Do not allow external jobs to write to the database during an update.

## Backups and retention

Managed update snapshots live in `/var/lib/rustpost-updater/backups/<job UUID>/` by default, independently of ordinary backup settings. Every snapshot contains `database.sqlite3`, `settings.toml` and `metadata.json`: timestamp, old/new version and schema, sizes, SHA-256 hashes, verification state and `pre_upgrade` reason. SQLite `VACUUM INTO` captures WAL contents consistently; integrity and foreign-key checks verify the exact schema snapshot without migrating it. Configuration backup is mandatory even when ordinary backups are disabled. The final backup is synced and published before activation.

Both Updates and Backups show pre-upgrade backup history. The updater configuration defaults to five backups (allowed range 2–20). Pruning runs after verified success, safe preparation failure or a healthy rollback, reconciles snapshots published before a journal interruption, keeps the newest configured count, and always protects the current transaction's rollback snapshot; unresolved rollback failures prohibit new updates and retain their snapshots. The previous executable is retained; older program versions are removed only after a successful later upgrade. Ordinary manual/full-site archives retain their existing policies. The panel does not add an arbitrary snapshot restore endpoint.

## Troubleshooting

- **Updater unavailable:** check `systemctl status rustpost-updater` and `journalctl -u rustpost-updater`. Verify socket permissions and the configured web UID. Managed application startup/mutations fail closed while updater state cannot be verified.
- **Unverified/incompatible release:** provision the trusted signing public key and confirm that the official release contains the signed manifest for your target. Older releases without update artifacts are check-only. Do not bypass signature checks.
- **Update stopped safely:** old program and DB were not activated/migrated. Resolve the reported preflight, network, space, checksum or backup issue, then check again.
- **Rolled back:** inspect updater/service logs to identify the new version's startup, migration or health failure before another attempt. The retained snapshot is the pre-upgrade state.
- **Operator intervention required:** keep RustPost stopped. Preserve the journal and protected backup. Repair service/permissions/storage or the verified backup, then have an operator restore the old DB/config and pointer together. Do not simply launch the old executable against an upgraded database. There is deliberately no web-accessible force-reset/downgrade command.
- **Crash/reboot during upgrade:** the updater's durable journal and OS locks determine recovery. Application startup checks updater readiness before migrations; a daemon boot barrier permits an interrupted transaction to start only through its controlled recovery step. HTTP mutations, automatic backups and destructive background maintenance remain paused until the terminal journal is durable and the worker releases admission. Release checks do not pause ordinary writes. Preparation interruptions resume the unchanged old DB; interruptions after activation begins restore the old snapshot first. Never delete active update state to clear a lock.

Containers are check-only. Do not mount a Docker socket or grant the web process host permissions. Create and verify an ordinary backup, stop the container, replace the image through your deployment system, reuse the existing persistent volume and verify application/database health. If it fails, stop the new container and restore the pre-upgrade database/config backup before restarting the old image. See [Docker deployment](docker.md). macOS, Windows and unmanaged direct CLI deployments display availability and explain deployment-managed installation.

# Native managed Linux deployment

The supplied integration targets Linux GNU x86_64 and aarch64 with systemd and polkit. Releases built on Ubuntu 24.04 require a compatible GNU libc/system library baseline; do not assume older distributions are supported. Use a clearnet loopback listener for machine health (`127.0.0.1:8080` by default), including dual Tor mode. Tor-only deployments lack that stable listener and cannot use this integration.

The HTTP account has **no sudo, systemd authority, shell installer, program-file write permission or Docker socket**. A separate unprivileged `rustpost-updater` account owns `/opt/rustpost` and `/var/lib/rustpost-updater`. It shares only the persistent data group with the web account. The systemd service sandbox restricts writes to the managed program, data, update-state and socket directories, with no capabilities and no new privileges. The polkit rule allows only `start` and `stop` of `rustpost.service` for that account. Calls use a fixed absolute systemctl executable, fixed unit and bounded wait; no shell or HTTP-supplied command arguments are executed.

The Unix socket `/run/rustpost-updater/control.sock` is mode `0660`, owned by the updater with group `rustpost`; the parent is `0750`. Peer credentials must match the configured web UID. IPC accepts only status, stable check, install using a persisted one-use approval/positive admin ID, and startup-readiness operations. Paths, URLs, service names and command arguments cannot be submitted. Treat compromise of the web account as authorization to request official signed updates only. Protect the updater account and root-controlled trust/configuration separately.

## One-time setup or adoption of an existing install

These are operator provisioning steps, performed once on the intended Linux host. No normal admin-panel update requires SSH. First take a verified full-site backup and stop the old instance. Preserve its explicit data directory and settings path; never rely on executable-relative defaults in a versioned install. Review the supplied files under `deploy/systemd/` before installing them.

1. Create system accounts `rustpost` and `rustpost-updater`; use a shared `rustpost` group. The web UID must be nonzero and different from the updater UID. Existing web accounts can be kept if all paths/unit names and group ownership match. Add the updater to the persistent-data group, never the web account to a privileged group.
2. Create `/opt/rustpost/versions/<installed version>/`, install the current compatible `rustpost-cli` there, and make `/opt/rustpost/current` a **relative** symlink to `versions/<installed version>`. The managed installation root, `versions/` and version directories must be owned by the updater, with directories `0755` and executable `0755`, no group/other write access. Executables may be root-owned and read-only to the updater; writable staging/retention parents must belong to the updater. Use the newly built implementation as the initial version: older RustPost binaries lack the startup gate/health endpoint and must be adopted offline first.
3. Install the matching `rustpost-updater` binary as `/usr/local/libexec/rustpost-updater`, root-owned `0755`. The helper is maintained by the operator separately; application updates never replace the helper or trust key.
4. Keep persistent state at `/var/lib/rustpost`, owned by web user and group `rustpost`. Set directories `0770`, database/config/sidecars and an existing `tmp/update-coordination.lock` `0660` where group access is needed, and use unit `UMask=0007`. Do not weaken private Tor key file permissions. Managed runtime setup keeps sensitive directory group access for the trusted updater, while unmanaged deployments retain their original `0700` modes. If adopting another directory, adjust both unit paths and updater configuration; all privileged paths must be absolute and have no symlink ancestors. Program/state/trust/config parents must be owned by root or their trusted leaf owner, with no web ownership or group/other write permission. Root-owned sticky shared temporary parents cannot permit another UID to replace protected children.
5. Create `/var/lib/rustpost-updater` and its `backups/` subdirectory, owned by the updater and mode `0700`. Do not put update state under the web-owned data directory. Create `/etc/rustpost-updater`, root-owned `0755`, and copy `deploy/systemd/updater.toml` there as root-owned `0644`. Set `web_uid` to the output of `id -u rustpost`, verify the stable data/settings paths and set the health port to the actual loopback port. Configuration may use an external settings path, but it must match the web service's `--config` and sandbox writable paths.
6. Provision `/etc/rustpost-updater/public-key.hex` from the maintainer's authenticated Ed25519 public key (32 raw bytes, 64 hex digits, newline allowed), root-owned `0644`. Never obtain a replacement trust key from the release being installed. The daemon refuses missing, invalid, web-writable or non-root-owned keys/configuration.
7. Copy both `.service` files into `/etc/systemd/system/` and the supplied rule into `/etc/polkit-1/rules.d/49-rustpost-updater.rules`, all root-owned `0644`. Reload systemd, enable/start the updater, then enable/start RustPost. Do not add an `After=rustpost.service` dependency to the updater: it must recover while RustPost is stopped. Ensure `/usr/bin/pkcheck` and polkit are installed; preflight verifies both fixed service permissions before shutdown. Validate the polkit rule on your distribution and the installed units with `systemd-analyze verify`.
8. Sign in locally, open Updates, check releases, and verify that only an authenticated admin can reach actions. Perform successful and deliberately failed upgrades in a disposable Linux staging instance before production adoption. No test should stop an existing host service.

Sample ownership checks:

```sh
id rustpost
id rustpost-updater
namei -l /opt/rustpost/current/rustpost-cli
namei -l /var/lib/rustpost-updater/backups
systemctl status rustpost-updater rustpost
journalctl -u rustpost-updater -u rustpost
```

Keep the fixed `rustpost.service` name and socket path. The web process startup gate and mutation pause depend on `RUSTPOST_MANAGED=1` from the supplied unit. Protect unit environment, CLI arguments, updater config and installation files from web writes. Block `/internal/update-health` in an external reverse proxy; the updater calls the loopback listener directly. Its JSON contains no paths or secrets.

# Maintainer release contract

The existing macOS/Windows/Linux manual-download archives and checksums are preserved. Stable native Linux releases additionally publish one `rustpost-update-<target>.tar.gz` containing **only** a regular `rustpost-cli` file, a `rustpost-update-<target>.json` manifest, and a raw 64-byte `.json.sig` Ed25519 signature. Target names are exactly `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu`.

The signed manifest binds format, stable version, GitHub release ID, target, artifact filename/size/SHA-256, executable size/SHA-256, target schema, minimum source schema and minimum updater version. The updater checks HTTPS source/redirect allowlists, signature, all identities/limits, archive checksum and layout, executable checksum and ELF architecture. Unknown paths, links, entries, targets and mismatches are rejected. New binaries execute only under the unprivileged web service, never as the updater.

`rustpost-cli --update-info` prints compatibility/version metadata without opening runtime storage. `tools/package-update.py` checks binary metadata against the release tag/target before building the dedicated archive. `tools/sign-updates.py` validates both Linux packages, hashes, executable architecture and schema metadata, then signs exact JSON bytes and verifies its signatures. The release workflow collects **all four** manual-download target packages first, verifies checksums, stages a GitHub draft, signs using its release ID and publishes only after all required assets pass. A published release is immutable to retries; failed signing leaves an unpublished draft. Prerelease tags retain the manual-download archives and publish as GitHub prereleases without auto-update artifacts or stable/latest promotion; signing secrets are mandatory for stable publication. Containers use tag-derived versions and retain non-root/data-volume deployment.

## Signing provisioning

Generate an Ed25519 private key offline using OpenSSL 3:

```sh
openssl genpkey -algorithm ED25519 -out rustpost-update-signing.pem
openssl pkey -in rustpost-update-signing.pem -pubout -outform DER -out rustpost-update-public.der
python3 -c 'from pathlib import Path; d=Path("rustpost-update-public.der").read_bytes(); assert d[:12].hex()=="302a300506032b6570032100" and len(d)==44; print(d[12:].hex())'
```

Set repository Actions secret **`RUSTPOST_UPDATE_SIGNING_KEY`** to the entire PEM private key. Publish the printed public-key hex through an authenticated maintainer channel and provision it into each host's root-owned trust file. Store the private key offline and in CI secrets only; never commit it. CI writes a temporary `0600` key and removes it on exit. Missing secrets fail the release job; there is no unsigned installation fallback. Key rotation requires an explicit trusted operator update of the host key. OpenSSL 3 is needed for the release scripts.

Keep the schema metadata and actual migration support aligned. `--update-info` currently declares minimum schema 1 because the inspected migration system supports release schemas 1–4; revise this when dropping an old migration path. Keep migrations transactional, preserve persistent file formats, and do not introduce media/Tor mutations during startup: automatic rollback restores DB/config, not arbitrary side effects. New updater protocol requirements must increase `minimum_updater`, with an operator helper upgrade before installation. Health must report expected schema only after structural validation; never use process existence alone.

Tests use ephemeral signatures, localhost artifact servers, temporary SQLite WAL databases and simulated services. They exercise download/verify/backup/migrate/activate/restart/health success, migration/start/health rollback, config/data restoration, checksum/layout/architecture failures, interrupted journals, OS locks, retention and protected backups. The production source allowlist is never loosened for test URLs. Route/browser tests cover admin authorization, CSRF, reauthentication, GET refusal, safe external notes, no-JS behavior and persisted result rendering. Local browser outage fixtures exercise an install response disappearing before redirect, progress/backup display, automatic reconnect to a GET result page, and both success and rollback; no-JS clients refresh the persisted result. Manual local review used the same isolated UI fixtures; actual database/binary recovery is asserted by transaction tests.

Validation:

```sh
cargo fmt --all --check
cargo check --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings -D clippy::all -D clippy::pedantic -D clippy::nursery -D clippy::cargo
cargo test --workspace --all-features
cargo build --release --bins
node node_modules/@playwright/test/cli.js test -c tests/playwright/software-updates.config.mjs
node node_modules/@playwright/test/cli.js test -c tests/playwright/configuration-admin.config.mjs
```

Portable tests simulate service management. A disposable Linux/systemd/polkit deployment must additionally validate actual UID/group permissions, service stop/start, reboot during activation, and successful/failed signed releases before production rollout. macOS development cannot exercise Linux systemd itself. Do not point tests at a real production installation.

For the macOS 27 Playwright Firefox app-data collision ([upstream issue 42768](https://github.com/microsoft/playwright/issues/42768)), browser configurations accept `RUSTPOST_FIREFOX_EXECUTABLE` pointing to a disposable same-revision launcher with an isolated app identity. Stock browsers remain the default; do not disable browser sandboxing or change an installed personal Firefox profile. The local validation used this override for Firefox and reran the complete suites.

SQLite's [supported snapshot mechanisms](https://www.sqlite.org/backup.html) and GitHub's [release asset API](https://docs.github.com/en/rest/releases/assets) underpin the backup and source/integrity contract.
