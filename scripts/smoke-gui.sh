#!/bin/sh
# Build first, then exercise only a private display with synthetic state.
set -eu

task_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
binary=${1:-"$task_root/target/debug/snippy-gui"}
title_pattern=${2:-'^Snippy .*Vault.*$'}
# Keys that close the window: the vault window closes on Escape; the X11 probe
# passes "Tab space" to activate its Close button.
close_keys=${3:-Escape}

for tool in xvfb-run Xvfb xauth xdotool python3; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        printf 'Missing GUI smoke-test tool: %s\n' "$tool" >&2
        exit 1
    fi
done

if [ ! -x "$binary" ]; then
    printf 'Build the GUI first: cargo build --workspace --locked\n' >&2
    exit 1
fi

sh "$task_root/scripts/with-xvfb.sh" -a -s '-screen 0 1024x768x24 -nolisten tcp' \
    python3 - "$binary" "$title_pattern" "$close_keys" <<'PY'
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time


def run_check():
    # xvfb-run can reach its command even after exhausting server retries.
    # Probe the display explicitly instead of mistaking that for a passing test.
    probe = subprocess.run(
        ["xdotool", "getdisplaygeometry"], capture_output=True, text=True, timeout=5
    )
    if probe.returncode:
        raise RuntimeError(
            "Private Xvfb display is unavailable; inspect the Xvfb diagnostics "
            "(display sockets may be restricted)."
        )

    binary = str(Path(sys.argv[1]).resolve())
    with tempfile.TemporaryDirectory(prefix="snippy-gui-smoke-") as directory:
        root = Path(directory)
        environment = os.environ.copy()
        environment.pop("WAYLAND_DISPLAY", None)
        environment.update({
            "GDK_BACKEND": "x11",
            "GSK_RENDERER": "cairo",
            "GTK_A11Y": "none",
            "DBUS_SESSION_BUS_ADDRESS": f"unix:path={root / 'no-session-bus'}",
            "XDG_DATA_HOME": str(root / "data"),
            "XDG_CONFIG_HOME": str(root / "config"),
            "XDG_CACHE_HOME": str(root / "cache"),
            "XDG_RUNTIME_DIR": str(root / "runtime"),
        })
        (root / "runtime").mkdir(mode=0o700)

        headless_environment = environment.copy()
        headless_environment.pop("DISPLAY", None)
        headless = subprocess.run(
            [binary], env=headless_environment, capture_output=True, timeout=10
        )
        if headless.returncode != 1 or b"could not initialize GTK" not in headless.stderr:
            raise RuntimeError("GUI did not report the expected no-display error")
        print("PASS: GUI fails cleanly without a display", flush=True)

        with (root / "gui.log").open("w+") as log:
            process = subprocess.Popen(
                [binary], env=environment, stdout=log, stderr=log
            )
            try:
                deadline = time.monotonic() + 10
                window = None
                while time.monotonic() < deadline:
                    if process.poll() is not None:
                        raise RuntimeError("GUI exited before presenting its window")
                    search = subprocess.run(
                        ["xdotool", "search", "--onlyvisible", "--all",
                         "--pid", str(process.pid), "--name",
                         sys.argv[2]],
                        env=environment, capture_output=True, text=True, timeout=3,
                    )
                    if search.returncode == 0 and search.stdout.strip():
                        window = search.stdout.splitlines()[0]
                        break
                    time.sleep(0.1)
                if window is None:
                    raise RuntimeError("Development window was not visible within 10 seconds")
                print("PASS: development window is visible on private Xvfb display", flush=True)

                # With no WM required, focus directly, then send the window's own
                # close keys (Escape, or Tab+Space onto the probe's Close button).
                subprocess.run(
                    ["xdotool", "windowfocus", "--sync", window],
                    env=environment, check=True, timeout=5,
                )
                subprocess.run(
                    ["xdotool", "key", "--clearmodifiers", *sys.argv[3].split()],
                    env=environment, check=True, timeout=5,
                )
                if process.wait(timeout=5) != 0:
                    raise RuntimeError("GUI did not exit successfully after closing")
                print("PASS: closing the window exits the GUI successfully without a WM or session bus", flush=True)
            except Exception:
                log.seek(0)
                sys.stderr.write(log.read())
                raise
            finally:
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=3)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=3)

        for name in ("data", "config", "cache", "runtime"):
            if (root / name / "snippy").exists():
                raise RuntimeError(f"Unexpected snippy state created in {name}")
        print("PASS: no snippy vault, config, cache, or runtime state created", flush=True)


try:
    run_check()
except Exception as error:
    print(f"FAIL: {error}", file=sys.stderr)
    sys.exit(1)
PY
