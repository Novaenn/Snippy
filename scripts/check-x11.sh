#!/bin/sh
# Every protocol test runs on a private display, never the live clipboard.
set -eu
task_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$task_root"
if [ -z "${CARGO_HOME:-}" ] && [ -d "$task_root/.cache/cargo" ]; then
    CARGO_HOME="$task_root/.cache/cargo"
    export CARGO_HOME
fi
for tool in cargo xvfb-run Xvfb xauth xdotool; do
    command -v "$tool" >/dev/null 2>&1 || { printf 'Missing tool: %s\n' "$tool" >&2; exit 1; }
done
cargo test -p snippy --test x11 --no-run --locked --offline
cargo build -p snippy-gui --example x11-probe --locked --offline
sh scripts/with-xvfb.sh -a -s '-screen 0 1024x768x24 -nolisten tcp' \
    sh -eu -c '
        xdotool getdisplaygeometry >/dev/null
        export SNIPPY_TEST_X11=1
        cargo test -p snippy --test x11 --locked --offline -- --ignored --test-threads=1 --skip missing_xfixes --skip missing_optional_xres
    '
sh scripts/with-xvfb.sh -a -s '-screen 0 1024x768x24 -nolisten tcp -extension X-Resource' \
    sh -eu -c '
        xdotool getdisplaygeometry >/dev/null
        export SNIPPY_TEST_X11=1
        cargo test -p snippy --test x11 --locked --offline -- --ignored --test-threads=1 missing_optional_xres
    '
sh scripts/with-xvfb.sh -a -s '-screen 0 1024x768x24 -nolisten tcp -extension XFIXES' \
    sh -eu -c '
        xdotool getdisplaygeometry >/dev/null
        export SNIPPY_TEST_X11=1
        cargo test -p snippy --test x11 --locked --offline -- --ignored --test-threads=1 missing_xfixes
    '
sh scripts/smoke-gui.sh "$task_root/target/debug/examples/x11-probe" '^Snippy .*X11 metadata probe$' 'Tab space'
