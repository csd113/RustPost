# Demo Preview

The visual preview screenshots in the README were captured from generated local-only demo data. The demo uses fictional users, generated SVG images, and short local WebM clips. It does not use production data or internet media.

## Build

```sh
cargo build --workspace --all-features
```

## Seed

```sh
rm -rf target/debug/rustpost-demo
./target/debug/rustpost-cli --data-dir target/debug/rustpost-demo seed-demo
```

The `seed-demo` command is guarded and only writes to an explicit `target/debug/rustpost-demo` data directory.

## Run

```sh
./target/debug/rustpost-cli --data-dir target/debug/rustpost-demo serve
```

Open [http://127.0.0.1:8098](http://127.0.0.1:8098).

## Demo Accounts

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

The generated runtime database, uploads, temp upload staging, logs, and backups stay under `target/debug/rustpost-demo/` and should not be committed.
