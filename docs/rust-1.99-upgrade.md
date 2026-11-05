# Compiler upgrade history

For a current build, follow [Development](development.md): both the compiler pin and minimum supported version are now **Rust 1.99.1**. You do not need this historical report to run RustPost.

[All guides](README.md) · [Current checks](development.md#checks) · [Changelog](../CHANGELOG.md)

## Historical report: October 1, 2026

The record below describes the earlier 1.99.0 migration and its validation at that time. It is preserved as project history. The old 1.91 minimum, floating-stable CI checks, updater binary, release-tool tests, and application test counts do not describe the current tree. Current dev removed the installer machinery and uses a manual release-link check; later work added terminal/CLI regressions.

The new rust-toolchain.toml selects exact Rust 1.99.0 with rustfmt and Clippy.
CI, releases and the container now build with 1.99.0 instead of 1.91.0.
Cargo's Rust 1.91 MSRV is retained as a compatibility floor, independently of
the build compiler. Edition 2024, application version 1.0.0, dependencies and
Cargo.lock are unchanged.

CI explicitly selects latest stable for strict Clippy and full tests, and
1.91.0 for the MSRV check. Release compilers remain reproducibly pinned and
advance after the full validation gates pass. There is no automatic background
updater or floating compiler in release builds. All build/test resolution is locked.

The official Rust 1.99 Bookworm Docker tags were unavailable on release day.
The Dockerfile bootstraps from official rust:1.98.1-bookworm, then installs and
selects exact Rust 1.99.0 using official rustup. The final build uses 1.99.0;
Debian 12, ffmpeg and the unprivileged runtime account remain unchanged.

Strict Clippy findings were fixed by sharing the identical HEAD response
construction after its match and using diagnostic equality/length assertions
in the existing tests. No test or lint gate was removed or suppressed.

### Validation recorded at the time

All checks passed locally on macOS Apple Silicon:

- cargo +1.99.0 fmt --all --check
- cargo +1.99.0 check --locked --workspace --all-features
- cargo +1.99.0 clippy --locked --workspace --all-targets --all-features -- -D warnings -D clippy::all -D clippy::pedantic -D clippy::nursery -D clippy::cargo
- cargo +1.99.0 test --locked --workspace --all-features: 530 passed.
- python3 tools/test-release-tools.py: four signed-update packaging/rejection tests passed.
- cargo +1.99.0 build --locked --release --workspace --all-features --bins: CLI and updater built.
- cargo +1.91.0 check --locked --workspace --all-targets --all-features
- Updated multi-stage Docker build on Linux ARM64, version probe, unprivileged startup and HTTP home-page smoke check.
- Workflow YAML parsing and git diff --check.

Other existing native CI/release targets are retained; no new remote CI,
Windows host or physical hardware run is claimed. The work is local only:
no push, tag, published release, signing credentials or production deployment.
