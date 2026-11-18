#!/usr/bin/env python3
"""Private GTK manager smoke: tags, deletion, settings, and persistence."""
import importlib.util
import os
from pathlib import Path
import stat
import struct
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


def window(environment):
    return s.wait(
        lambda: s.windows(environment, r"^Snippy .*Vault .*Unlocked$"),
        "manager did not open unlocked",
    )[0]


def text_bytes(value):
    data = value.encode()
    return struct.pack(">H", len(data)) + data


def manager_info(environment):
    session = s.status(environment)["session"]
    tag, info = s.request(environment, 25, struct.pack(">Q", session))
    s.require(tag == 14, "manager info failed")
    return session, info


def page(environment, text="", source_id=0, full=True, offset=0):
    session = s.status(environment)["session"]
    payload = (
        struct.pack(">Q", session)
        + text_bytes(text)
        + text_bytes("")
        + struct.pack(">Q", source_id)
        + text_bytes("")
        + bytes([0, 1 if full else 0])
        + struct.pack(">Q", offset)
    )
    tag, value = s.request(environment, 13, payload)
    s.require(tag == 7, "manager page failed")
    return value


def config(environment):
    session = s.status(environment)["session"]
    tag, value = s.request(environment, 16, struct.pack(">Q", session))
    s.require(tag == 10, "config read failed")
    return value


def sensitive(environment):
    session = s.status(environment)["session"]
    tag, value = s.request(environment, 18, struct.pack(">Q", session))
    s.require(tag == 11, "sensitive list read failed")
    return value


def items(environment):
    return manager_info(environment)[1]["items"]


def dialog_windows(environment):
    # Confirmation dialogs are untitled; every other GUI window here has a title.
    result = subprocess.run(
        ["xdotool", "search", "--onlyvisible", "--name", "^$"],
        env=environment, capture_output=True, text=True, timeout=3,
    )
    return result.stdout.split() if result.returncode == 0 else []


def confirm(environment, window, shortcut, answer):
    # The shortcut is ignored while the page reloads (no row selected yet), so
    # retry it until its modal dialog appears, then answer the dialog. This also
    # keeps the answer from reaching the main window.
    before = set(dialog_windows(environment))
    deadline = time.monotonic() + 8
    while time.monotonic() < deadline:
        s.keys(environment, window, shortcut)
        try:
            s.wait(
                lambda: set(dialog_windows(environment)) - before,
                "confirmation dialog did not appear",
                seconds=1,
            )
        except RuntimeError:
            continue
        active_key(environment, answer)
        return
    raise RuntimeError(f"{shortcut} did not open a confirmation dialog")


def active_key(environment, *sequence):
    # Confirmation dialogs are modal and take focus; send through the active window.
    subprocess.run(
        ["xdotool", "key", "--clearmodifiers", *sequence],
        env=environment,
        check=True,
        timeout=3,
    )


def type_text(environment, value):
    subprocess.run(
        ["xdotool", "type", "--clearmodifiers", "--delay", "1", "--file", "-"],
        input=value.encode(),
        env=environment,
        check=True,
        timeout=5,
    )


def readback(environment, *expected):
    subprocess.run(
        [str(s.ROOT / "target/debug/examples/picker-read"), *expected],
        env=dict(environment, SNIPPY_TEST_X11="1"),
        check=True,
        timeout=10,
    )


def open_manager(environment, locked=True):
    s.require(s.cli(environment, "manager").returncode == 0, "manager command failed")
    if locked:
        found = s.wait(
            lambda: s.windows(environment, "^Snippy .*Vault .*Locked$"),
            "locked manager did not open",
        )[0]
        s.keys(environment, found, "Home")
        s.type_secret(environment)
        s.keys(environment, found, "Return")
        return window(environment)
    return window(environment)


def main():
    subprocess.run(
        ["xdotool", "getdisplaygeometry"], capture_output=True, check=True, timeout=3
    )
    with tempfile.TemporaryDirectory(prefix="snippy-manager-") as directory:
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
        process = s.start(environment, timeout=60)
        failed = True
        try:
            manager = open_manager(environment)
            _, info = manager_info(environment)
            s.require(info["items"] == 25, "fixture item count mismatch")
            s.require(info["physical_bytes"] > 0, "physical size missing")
            first = page(environment)["rows"][0]
            s.require(first["source_id"] > 0, "source category id missing")
            s.require(first["tags"], "fixture tags missing from manager page")
            print(
                "PASS: manager opens unlocked with 25 items, categories, tags, usage, and database size",
                flush=True,
            )

            time.sleep(1.2)
            s.keys(environment, manager, "ctrl+Return")
            time.sleep(0.5)
            s.require(
                manager in s.windows(environment, r"^Snippy .*Vault .*Unlocked$"),
                "window closed after Copy (Ctrl+Enter)",
            )
            readback(environment, "--image")
            print("PASS: Copy (Ctrl+Enter) serves the selected item and keeps the window open", flush=True)

            s.keys(environment, manager, "ctrl+f")
            type_text(environment, "Synthetic note 07")
            time.sleep(1.2)
            # Copy and delete asks for confirmation like Delete: cancel once, then accept.
            confirm(environment, manager, "ctrl+x", "Escape")
            time.sleep(0.6)
            s.require(items(environment) == 25, "canceled Copy and delete changed history")
            s.require(
                manager in s.windows(environment, r"^Snippy .*Vault .*Unlocked$"),
                "canceling the confirmation closed the window",
            )
            confirm(environment, manager, "ctrl+x", "Return")
            s.wait(lambda: items(environment) == 24, "Copy and delete did not remove the item")
            readback(environment, "--note", "7")
            s.require(
                manager in s.windows(environment, r"^Snippy .*Vault .*Unlocked$"),
                "manager closed after Copy and delete",
            )
            s.keys(environment, manager, "ctrl+f")
            time.sleep(0.2)
            s.keys(environment, manager, "ctrl+a", "BackSpace")
            time.sleep(1.2)
            print(
                "PASS: manager search finds the exact note; Copy and delete is confirmed (cancel keeps it) and serves it once",
                flush=True,
            )

            s.keys(environment, manager, "ctrl+t")
            time.sleep(0.2)
            type_text(environment, "reviewtag")
            s.keys(environment, manager, "Return")
            s.wait(
                lambda: any(
                    "reviewtag" in row["tags"] for row in page(environment)["rows"]
                ),
                "tag was not added from the manager",
            )
            print("PASS: tag creation from the manager persists", flush=True)

            confirm(environment, manager, "ctrl+d", "Return")
            s.wait(lambda: items(environment) == 23, "single deletion did not apply")
            print("PASS: single deletion requires confirmation and updates usage", flush=True)

            s.keys(environment, manager, "ctrl+k")
            s.wait(lambda: items(environment) == 23, "checking rows changed history")
            confirm(environment, manager, "ctrl+shift+d", "Return")
            s.wait(lambda: items(environment) == 13, "checked bulk deletion did not apply")
            print("PASS: checked-page bulk deletion requires confirmation", flush=True)

            s.keys(environment, manager, "ctrl+comma")
            time.sleep(0.4)
            s.keys(environment, manager, "alt+l", "ctrl+a")
            type_text(environment, "90")
            s.keys(environment, manager, "alt+a")
            s.wait(
                lambda: config(environment)["timeout"] == 90,
                "settings apply did not update the inactivity timeout",
            )
            settings_file = root / "config/snippy/settings.conf"
            text = settings_file.read_text()
            s.require("lock_timeout_seconds=90" in text, "timeout was not persisted")
            s.require(
                stat.S_IMODE(settings_file.stat().st_mode) == 0o600,
                "settings file is not private",
            )
            print("PASS: settings edits apply at runtime and persist privately", flush=True)

            s.keys(environment, manager, "alt+i")
            type_text(environment, "x11:wm-class:ReviewApp")
            s.keys(environment, manager, "Return")
            s.wait(
                lambda: "x11:wm-class:ReviewApp" in sensitive(environment),
                "sensitive rule was not added",
            )
            print("PASS: encrypted sensitive-app rule can be added from settings", flush=True)

            s.keys(environment, manager, "alt+h")
            s.require(s.cli(environment, "lock").returncode == 0, "lock failed")
            s.wait(
                lambda: s.windows(environment, "^Snippy .*Vault .*Locked$"),
                "lock did not clear the manager view",
            )
            s.require(s.request(environment, 4, s.secret(s.KEY))[0] == 1, "unlock failed")
            manager = open_manager(environment, locked=False)
            s.require(items(environment) == 13, "unlock changed saved history")
            print("PASS: lock clears the manager and unlock restores persisted history", flush=True)

            s.stop(process)
            process = subprocess.Popen(
                [str(s.CLI), "daemon", "--no-shortcuts"],
                env=environment,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
            )
            s.wait(
                lambda: process.poll() is None and s.status(environment),
                "restarted daemon did not become ready",
            )
            s.require(s.request(environment, 4, s.secret(s.KEY))[0] == 1, "reopen unlock failed")
            reopened = config(environment)
            s.require(reopened["timeout"] == 90, "persisted timeout was not loaded")
            s.require(items(environment) == 13, "restart changed saved history")
            s.require(
                "x11:wm-class:ReviewApp" in sensitive(environment),
                "encrypted sensitive rule did not survive restart",
            )
            print(
                "PASS: restart loads private settings and preserves encrypted rules/history",
                flush=True,
            )
            failed = False
        finally:
            s.stop(process)
            for window_id in s.windows(environment, r"^Snippy .*"):
                try:
                    s.keys(environment, window_id, "alt+c")
                except subprocess.SubprocessError:
                    pass
            if failed:
                if process.stderr is not None:
                    os.set_blocking(process.stderr.fileno(), False)
                    diagnostics = process.stderr.read(64 * 1024)
                    if diagnostics:
                        sys.stderr.write("Isolated service/GUI diagnostics:\n")
                        sys.stderr.buffer.write(diagnostics)
                        sys.stderr.buffer.flush()


if __name__ == "__main__":
    main()
