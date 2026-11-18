#!/bin/sh
# Complete phase 8 automated suite. Synthetic data only; no live clipboard.
set -eu
cd "$(dirname "$0")/.."
if [ -z "${CARGO_HOME:-}" ] && [ -d .cache/cargo ]; then
    CARGO_HOME="$PWD/.cache/cargo"
    export CARGO_HOME
fi
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked --offline -- -D warnings
cargo test --workspace --locked --offline
cargo build --workspace --all-targets --locked --offline
sh scripts/check-manager.sh
sh scripts/check-picker.sh
sh scripts/check-capture.sh
sh scripts/check-x11.sh
sh scripts/smoke-gui.sh
sh scripts/check-storage.sh
