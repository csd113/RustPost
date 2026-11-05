# Build and develop RustPost

You only need the Rust toolchain if you want to build or change the app. To run a ready-made copy, use [Getting started](getting-started.md) or [Docker](docker.md).

[All guides](README.md) · [Source](https://github.com/csd113/RustPost/tree/dev) · [Changelog](../CHANGELOG.md)

## Build and run

Install Git and [rustup](https://rustup.rs/), plus the native compiler/linker tools for your operating system. The repository pins **Rust 1.99.1** in `rust-toolchain.toml`; `Cargo.toml` also declares 1.99.1 as the minimum supported version. Rustup selects the pin inside the repository without changing your global default.

```sh
git clone --branch dev https://github.com/csd113/RustPost.git
cd RustPost
cargo build --locked --release --bin rustpost-cli
./target/release/rustpost-cli --data-dir ./rustpost-data serve
```

On Windows, run `.\target\release\rustpost-cli.exe --data-dir .\rustpost-data serve`. Follow the terminal's administrator prompts and visit **[http://127.0.0.1:8080](http://127.0.0.1:8080)**. The [first-run guide](getting-started.md) applies after the build.

`dev` is the current development branch. It can contain changes that are not in a published release, even while `--version` still prints 1.0.0. Record `git rev-parse HEAD` with a build. To reproduce a release, check out that release's tag instead of assuming dev has the same content.

RustPost uses SQLite and server-rendered HTML, CSS, and JavaScript. There is no separate database server or frontend build step. `ffmpeg` is optional for ordinary operation and required for the [demo's sample-video generation](demo-preview.md).

## Find the code

| Area | Main files |
| --- | --- |
| CLI and runtime paths | `src/cli.rs`, `src/runtime.rs` |
| HTTP routes | `src/server.rs`, `src/server/` |
| Settings and editor fields | `src/config.rs`, `src/config/admin_fields.rs` |
| Database and migrations | `src/db/` |
| Posts, follows, and notifications | `src/social/` |
| Accounts and portability | `src/account.rs`, `src/portability/` |
| Media | `src/media/`, `src/ffmpeg.rs` |
| Backups | `src/backup/` |
| Tor and release discovery | `src/tor.rs`, `src/updates.rs` |
| Terminal and operational logs | `src/console/`, `src/terminal/`, `src/logging.rs` |
| Web rendering and assets | `src/render/`, `assets/` |

Use a disposable data directory for development. Never point a demo or smoke test at a real community's data. For a populated local site, follow [Local demo](demo-preview.md).

## Checks

Run these from the repository root using the pinned toolchain:

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings -D clippy::all -D clippy::pedantic -D clippy::nursery -D clippy::cargo
cargo test --locked --workspace --all-features
cargo build --locked --release --bin rustpost-cli
```

Build API documentation with warnings treated as errors on Linux/macOS:

```sh
RUSTDOCFLAGS='-D warnings' cargo doc --locked --workspace --all-features --no-deps
```

PowerShell equivalent:

```powershell
$env:RUSTDOCFLAGS = '-D warnings'
cargo doc --locked --workspace --all-features --no-deps
Remove-Item Env:RUSTDOCFLAGS
```

Review `cargo audit --file Cargo.lock` separately (requires `cargo-audit`). Advisory review is not evidence of functional correctness, and passing CI is not a clean dependency-audit claim. See [dependency notes](deployment.md#security-and-dependency-review).

Current CI runs formatting, strict Clippy, and rustdoc, then native CLI builds/tests on Linux x86_64, Linux ARM64, Intel Mac, Apple silicon Mac, and Windows x86_64. Current tests include terminal/CLI regressions; older compiler-upgrade records refer to application and Playwright suites that are no longer present. Use the current workflow and `Cargo.toml`, not historical test counts, to identify the active checks.

### Documentation checks

For documentation changes, run `git diff --check`, verify relative file links and heading anchors, and compare commands/settings with the installed CLI or source. Keep fenced TOML excerpts clearly marked when they are not complete configuration files. Preserve existing image paths and license/provenance information.

For setup changes, use a fresh disposable data directory to check `init`, `check`, administrator creation, local HTTP, shutdown, and restart persistence. Test restore only against disposable data with the server stopped. Do not claim other operating systems or external Tor access were tested from a local-only check.

## Release packaging

**Published v1.0.0** has the four older archives listed in the [README](../README.md#1-download-the-app), each with a matching `.sha256` file. Its macOS archive targets Apple silicon. Those existing artifacts are not renamed by changing the dev workflow.

The **current release workflow** prepares these five ZIP patterns for a future `vVERSION` tag:

| Platform | Archive pattern |
| --- | --- |
| Linux x86_64 | `rustpost-cli-vVERSION-linux-x86_64.zip` |
| Linux ARM64 | `rustpost-cli-vVERSION-linux-arm64.zip` |
| macOS Intel | `rustpost-cli-vVERSION-macos-intel.zip` |
| macOS Apple silicon | `rustpost-cli-vVERSION-macos-apple-silicon.zip` |
| Windows x86_64 | `rustpost-cli-vVERSION-windows-x86_64.zip` |

Each ZIP contains a matching version/platform folder with the executable, README, LICENSE, and CHANGELOG. It does not bundle the full `docs/` directory or screenshots; use the repository's [online guides](https://github.com/csd113/RustPost/tree/dev/docs) when reading an extracted package. The workflow writes a shared `SHA256SUMS.txt` and verifies every expected archive.

The release workflow checks that the tag matches the package version, validates code quality/rustdoc, verifies the native executable version and Mac architecture, and prepares a **draft** GitHub release. It refuses to modify an already-published release. Publishing a draft is a separate maintainer action.

The container workflow builds Linux amd64/arm64 images from a source tag and publishes version tags, plus `latest` for stable versions. The image uses an unprivileged UID/GID 10001, persistent `/data`, and bundled `ffmpeg`. Its Dockerfile starts with `rust:1.98.1-bookworm` and installs the exact 1.99.1 compiler; that base-image tag is not the final build compiler version.

## Change the compiler or dependencies

Update `rust-toolchain.toml`, `Cargo.toml`'s supported-version requirement when appropriate, workflow pins, and Dockerfile compiler selection together. Resolve dependencies deliberately, retain `Cargo.lock`, and pass the checks on the intended platforms before using a new build for release. Do not rewrite published migration history.

Arti/Tor dependencies are currently pinned to 0.48.0, with `rusqlite` 0.40.2 to keep SQLite linkage compatible. `cargo tree --locked -i libsqlite3-sys` should show one resolved SQLite sys crate. Re-evaluate upstream advisories when updating the Tor stack.

For provenance of the earlier compiler migration, read [Compiler upgrade history](rust-1.99-upgrade.md). That historical report is not the current build checklist.
