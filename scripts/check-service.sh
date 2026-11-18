#!/bin/sh
# Private synthetic service/GUI fixtures. Never connects to the live display/vault.
set -eu
cd "$(dirname "$0")/.."
cargo build --workspace --all-targets --locked --offline
cargo test -p snippy --lib --locked --offline real_socketpair -- --ignored
sh scripts/with-xvfb.sh -a -s '-screen 0 1024x768x24 -nolisten tcp' python3 scripts/check-service.py
