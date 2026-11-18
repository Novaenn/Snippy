#!/bin/sh
# Synthetic sources, private authenticated display, isolated vault/runtime paths.
set -eu
cd "$(dirname "$0")/.."
if [ -z "${CARGO_HOME:-}" ] && [ -d .cache/cargo ]; then
    CARGO_HOME="$PWD/.cache/cargo"
    export CARGO_HOME
fi
cargo build --workspace --all-targets --locked --offline
sh scripts/with-xvfb.sh -a -s '-screen 0 1024x768x24 -nolisten tcp' \
    sh -eu -c 'xdotool getdisplaygeometry >/dev/null; export SNIPPY_TEST_X11=1; cargo test -p snippy --test capture --locked --offline -- --ignored --test-threads=1'
sh scripts/check-service.sh
sh scripts/with-xvfb.sh -a -s '-screen 0 1024x768x24 -nolisten tcp' python3 scripts/check-capture-gui.py
