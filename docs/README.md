# RustPost guides

Start with [Getting started](getting-started.md) to download RustPost, run it, and create your first administrator. You can read the administration and development references when you need them.

## Start and use a site

- [Getting started](getting-started.md): download, first run, checksums, and troubleshooting.
- [Using RustPost](account-and-instance-features.md): posts, profiles, follows, account transfers, and deletion.
- [Docker](docker.md): a container with persistent storage and media tools included.
- [Local demo](demo-preview.md): sample accounts and screenshot reproduction.

## Look after a site

- [Operator guide](operator-guide.md): settings, users, storage, backups, Tor, and command reference.
- [Deployment](deployment.md): public access, HTTPS, proxy behavior, and security notes.
- [Software updates](software-updates.md): release checks and manual installation.
- [Terminal console](terminal-console.md): controls, status, logs, and their limits.

## Build and understand the implementation

- [Development](development.md): current toolchain, checks, source map, and release packaging.
- [Configuration reference](admin-configuration-audit.md): the complete settings inventory and web editor contract.
- [Account behavior reference](account-reference.md): archive format, account rules, and implementation details.
- [Compiler upgrade history](rust-1.99-upgrade.md): historical migration notes, separated from current build instructions.
- [Changelog](../CHANGELOG.md): unreleased work and previous releases.
- [License](../LICENSE): RustPost's MIT license.

## Which version do these guides describe?

The repository guides follow **`dev`**. The published **v1.0.0** binaries and container predate some current development changes, including the new terminal console, offline-only restore, and simplified release checker. Dev still reports package version 1.0.0, so record the source commit as well when building it yourself.

The [download instructions](getting-started.md) name the assets actually available in v1.0.0. [Release packaging](development.md#release-packaging) describes what the current workflow will prepare for a future release. Historical changelog entries describe their own versions, not current guarantees.
