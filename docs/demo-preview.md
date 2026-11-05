# Local demo and screenshots

[All guides](README.md) · [Build from source](development.md#build-and-run) · [Screenshots](../README.md#see-the-app)

The README screenshots show a local demo with fictional accounts, generated illustrations, and short sample videos. No real account data or internet media is used.

The screenshot refresh for v1.0.0 was captured on September 29, 2026. Desktop images use a 1440 × 1200 browser viewport; the mobile image uses a 390 × 844 viewport.

## What you need

- Git and rustup, using the repository’s pinned Rust 1.99.1 compiler. Start from a current `dev` checkout; see [Build from source](development.md#build-and-run).
- `ffmpeg` with the `drawtext` filter and the `libvpx-vp9` encoder to generate the sample videos. These are needed for this demo even though `ffmpeg` is optional for normal RustPost use. Check your build with `ffmpeg -filters` and `ffmpeg -encoders`.

## 1. Build

```sh
cargo build --locked --workspace --all-features --bin rustpost-cli
```

## 2. Add sample content

```sh
./target/debug/rustpost-cli --data-dir target/debug/rustpost-demo-screenshots seed-demo
```

The `seed-demo` command only accepts an explicit demo directory under `target/debug` whose name contains `rustpost-demo`. It refuses to seed a database that already has users. Use a new demo folder for another run instead of clearing existing data.

## 3. Run the demo

```sh
./target/debug/rustpost-cli --data-dir target/debug/rustpost-demo-screenshots serve
```

Open [http://127.0.0.1:8098](http://127.0.0.1:8098). The demo uses port **8098**, not the usual 8080. Keep the terminal open and press Ctrl+C when finished.

On Windows, use `.\target\debug\rustpost-cli.exe` and the same explicit demo-directory argument. The seed command currently checks for the literal `target/debug/` path segment, so keep forward slashes in that argument.

## Sign in to a demo account

All demo accounts use the same local-only password:

```text
rustpost demo password
```

| Name | Username | Role |
|---|---|---|
| Ada Byte | `ada` | Systems programmer |
| Nova Fields | `nova` | Photographer |
| Milo Reed | `milo` | Indie maker |
| Jun Park | `jun` | UI designer |
| Tess Vale | `tess` | Video creator |
| Omar Stone | `omar` | Infrastructure/admin focused |

The generated database, uploads, temporary files, logs, and backups stay under the demo directory and should not be committed. Keep the demo on your computer: the shared password is for sample accounts only.

## Reproduce the screenshots

Log in as `ada`, then open these pages. Capture the visible browser viewport, rather than the full scrolling page, so text stays readable in the README.

| Screenshot | Page | Viewport |
| --- | --- | --- |
| Home feed | `/home` | 1440 × 1200 |
| Profile | `/users/nova` | 1440 × 1200 |
| Conversation | `/posts/13` | 1440 × 1200 |
| Image post | `/posts/2` | 1440 × 1200 |
| Mobile | `/home` | 390 × 844 |

These post IDs come from a freshly seeded database. Screenshots are saved in `docs/screenshots/`; browser logs and other temporary capture files should stay in the ignored `output/playwright/` directory.
