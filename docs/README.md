# RustPost guides

Start with [Getting started](getting-started.md) to download RustPost, run it, and create your first administrator. You can read the administration and development references when you need them.

## Start and use a site

- [Getting started](getting-started.md): download, first run, checksums, and troubleshooting.
- [Community tools in 1.2.0](community-tools.md): drafts, scheduling, polls, history/search, media processing, undo, and upgrade limits.
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
- [Changelog](../CHANGELOG.md): release changes and previous versions.
- [License](../LICENSE): RustPost's MIT license.

## Which version do these guides describe?

The repository guides describe **v1.2.0**. Older **v1.0.0** binaries and containers predate the live terminal console, offline-only restore, simplified release checker, and final CSRF fixes. Record the source commit when building from `dev`; the package version alone does not identify a development build.

The [download instructions](getting-started.md) describe v1.2.0 ZIPs and checksums. Use the published release page to confirm availability; preparation on `dev` does not publish a release. [Release packaging](development.md#release-packaging) also records the older v1.0.0 archive names. Historical changelog entries describe their own versions, not current guarantees.
