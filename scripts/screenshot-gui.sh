#!/bin/sh
# Screenshots of the manager, settings, and picker from a synthetic vault on a
# private display. Optional argument: output directory.
set -eu
cd "$(dirname "$0")/.."
if [ -z "${CARGO_HOME:-}" ] && [ -d .cache/cargo ]; then
    CARGO_HOME="$PWD/.cache/cargo"
    export CARGO_HOME
fi
cargo build --workspace --all-targets --locked --offline
sh scripts/with-xvfb.sh -a -s '-screen 0 1280x900x24 -nolisten tcp' \
    python3 scripts/screenshot-gui.py "$@"
