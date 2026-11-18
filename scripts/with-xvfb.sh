#!/bin/sh
# Keep server diagnostics quiet on success; omit known XF86 keymap noise on failure.
# The test command's own output stays visible. Arguments go to xvfb-run.
set -eu

log_dir=$(mktemp -d "${TMPDIR:-/tmp}/snippy-xvfb.XXXXXX")
trap 'rm -rf -- "$log_dir"' 0
trap 'exit 130' 2
trap 'exit 143' 15

if xvfb-run -e "$log_dir/server.log" "$@"; then
    if [ "${SNIPPY_XVFB_VERBOSE:-0}" = 1 ] && [ -s "$log_dir/server.log" ]; then
        cat "$log_dir/server.log" >&2
    fi
else
    status=$?
    diagnostics="$log_dir/server.log"
    if [ "${SNIPPY_XVFB_VERBOSE:-0}" != 1 ]; then
        # Remove only unresolved XF86 keysyms and empty xkbcomp wrappers. Keep
        # other keymap diagnostics and all X server errors, including on failure.
        if awk '
            function flush_block(    i) {
                if (details) {
                    print heading
                    for (i = 1; i <= count; i++) print block[i]
                }
                delete block
                heading = ""
                count = details = 0
            }
            /^[[:space:]]*The XKEYBOARD keymap compiler \(xkbcomp\) reports:[[:space:]]*$/ {
                flush_block()
                heading = $0
                next
            }
            /^[[:space:]]*>[[:space:]]*Warning:[[:space:]]+Could not resolve keysym[[:space:]]+XF86[A-Za-z0-9_]+[[:space:]]*$/ { next }
            /^[[:space:]]*Errors from xkbcomp are not fatal to the X server[[:space:]]*$/ {
                if (heading != "" && details) block[++count] = $0
                flush_block()
                next
            }
            {
                if (heading == "") print
                else {
                    block[++count] = $0
                    if ($0 !~ /^[[:space:]]*$/) details = 1
                }
            }
            END { flush_block() }
        ' "$log_dir/server.log" > "$log_dir/diagnostics.log"; then
            diagnostics="$log_dir/diagnostics.log"
        fi
    fi
    if [ -s "$diagnostics" ]; then
        printf '\nXvfb diagnostics (command exited %s):\n' "$status" >&2
        cat "$diagnostics" >&2
    fi
    exit "$status"
fi
