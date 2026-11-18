#!/usr/bin/env python3
"""Capture manager, settings, and picker screenshots from a synthetic vault.

Runs on a private display (see screenshot-gui.sh) with isolated XDG paths and the
encrypted picker fixture. Never touches the user's clipboard, vault, or display.
"""
import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location(
    "service_check", Path(__file__).with_name("check-service.py")
)
s = importlib.util.module_from_spec(spec)
spec.loader.exec_module(s)


def shot(environment, window, path):
    time.sleep(0.8)
    subprocess.run(
        ["import", "-window", window, str(path)],
        env=environment,
        check=True,
        timeout=10,
    )
    print(f"Saved {path}", flush=True)


def main():
    output = Path(sys.argv[1] if len(sys.argv) > 1 else "/tmp/snippy-shots")
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="snippy-shots-") as directory:
        root = Path(directory)
        runtime = root / "runtime"
        runtime.mkdir(mode=0o700)
        environment = dict(
            os.environ,
            XDG_RUNTIME_DIR=str(runtime),
            XDG_DATA_HOME=str(root / "data"),
            XDG_CONFIG_HOME=str(root / "config"),
            XDG_CACHE_HOME=str(root / "cache"),
            XDG_SESSION_TYPE="x11",
            GDK_BACKEND="x11",
            GSK_RENDERER="cairo",
            GTK_A11Y="none",
            GSETTINGS_BACKEND="memory",
            DBUS_SESSION_BUS_ADDRESS="unix:path=/nonexistent-snippy-fixture-bus",
        )
        subprocess.run(
            [
                str(s.ROOT / "target/debug/examples/picker-fixture"),
                "--data-dir",
                str(root / "data"),
                "--stdin-passphrase",
            ],
            input=s.KEY + b"\n" + s.KEY + b"\n",
            check=True,
            timeout=10,
        )
        process = s.start(environment, timeout=300)
        try:
            s.require(s.request(environment, 4, s.secret(s.KEY))[0] == 1, "unlock failed")
            s.require(s.cli(environment, "manager").returncode == 0, "manager failed")
            manager = s.wait(
                lambda: s.windows(environment, r"^Snippy .*Unlocked$"),
                "manager did not open",
            )[0]
            time.sleep(1.2)
            shot(environment, manager, output / "manager.png")
            s.keys(environment, manager, "ctrl+comma")
            shot(environment, manager, output / "settings.png")
            s.keys(environment, manager, "Escape")
            s.wait(
                lambda: not s.windows(environment, r"^Snippy .*"),
                "manager did not close",
            )
            s.require(s.cli(environment, "picker").returncode == 0, "picker failed")
            picker = s.wait(
                lambda: s.windows(environment, r"^Snippy .*Unlocked$"),
                "picker did not open",
            )[0]
            time.sleep(1.2)
            shot(environment, picker, output / "picker.png")
            s.keys(environment, picker, "Escape")
        finally:
            s.stop(process)


if __name__ == "__main__":
    main()
