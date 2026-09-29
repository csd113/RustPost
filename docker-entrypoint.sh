#!/bin/sh
set -eu

if [ ! -e /data/settings.toml ]; then
    rustpost-cli --data-dir /data init >/dev/null
    sed -i 's/^host = "127\.0\.0\.1"$/host = "0.0.0.0"/' /data/settings.toml
fi

exec rustpost-cli --data-dir /data "$@"
