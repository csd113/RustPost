# Arti directory manager SQLite compatibility backport

Source is the published tor-dirmgr 0.48.0 crate archive from crates.io.
Its SHA-256 is 86f7ac56b491b26a6f2b73f0f16fa7a0c66b507163e0f0ed850334b86169b326.
The standalone lockfile and original unnormalized manifest are omitted.
The MIT and Apache-2.0 license texts are from the upstream Arti 2.8.0 tag.

The only source change is the rusqlite upper bound in Cargo.toml.
It matches the tor-dirmgr change in [upstream commit 00a62585]. This lets RustPost
and Arti link one bundled SQLite that includes the fix for RUSTSEC-2026-0459.
Rust code and features are unchanged. Remove the backport once the published
Arti family accepts the selected patched rusqlite release.

[upstream commit 00a62585]: https://gitlab.torproject.org/tpo/core/arti/-/commit/00a62585bcfab121797b6f42393149b97b7c231e
