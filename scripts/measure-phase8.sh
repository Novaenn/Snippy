#!/bin/sh
# Release measurements with synthetic data on a private Xvfb display.
set -eu
cd "$(dirname "$0")/.."
if [ -z "${CARGO_HOME:-}" ] && [ -d .cache/cargo ]; then
    CARGO_HOME="$PWD/.cache/cargo"
    export CARGO_HOME
fi
cargo build --release --workspace --all-targets --locked --offline
sh scripts/with-xvfb.sh -a -s '-screen 0 1280x900x24 -nolisten tcp' \
    python3 scripts/measure-phase8.py
