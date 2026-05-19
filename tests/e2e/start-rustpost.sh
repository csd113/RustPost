#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DATA_DIR="${RUSTPOST_E2E_DATA_DIR:-$ROOT_DIR/output/playwright/runtime}"
PORT="${RUSTPOST_E2E_PORT:-18080}"
ADMIN_USER="${RUSTPOST_E2E_ADMIN_USER:-admin_e2e}"
ADMIN_PASSWORD="${RUSTPOST_E2E_ADMIN_PASSWORD:-very secure admin password}"

cd "$ROOT_DIR"

rm -rf "$DATA_DIR"
mkdir -p "$DATA_DIR"

cargo run --quiet --bin rustpost-cli -- --data-dir "$DATA_DIR" init

perl -0pi -e "s/port = 8080/port = $PORT/; s/create_admin_on_first_boot = true/create_admin_on_first_boot = false/; s/posts_per_minute = 5/posts_per_minute = 100/; s/replies_per_minute = 10/replies_per_minute = 100/; s/reposts_per_minute = 10/reposts_per_minute = 100/; s/account_creations_per_ip_per_day = 3/account_creations_per_ip_per_day = 100/" "$DATA_DIR/settings.toml"

cargo run --quiet --bin rustpost-cli -- --data-dir "$DATA_DIR" create-admin "$ADMIN_USER" "$ADMIN_PASSWORD"

exec cargo run --quiet --bin rustpost-cli -- --data-dir "$DATA_DIR" serve
