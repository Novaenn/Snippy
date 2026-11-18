"""Private Xvfb picker smoke: fixture vault, keyboard actions, and synthetic readback."""
import importlib.util
import os
from pathlib import Path
import struct
import subprocess
import sys
import tempfile
import time
sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("service_check", Path(__file__).with_name("check-service.py"))
s = importlib.util.module_from_spec(spec)
spec.loader.exec_module(s)


def type_text(environment, value):
    subprocess.run(["xdotool", "type", "--clearmodifiers", "--delay", "1", "--file", "-"], input=value.encode(), env=environment, check=True, timeout=5)


def gui_windows(environment):
    # Picker and manager share one window titled "Vault"; also accept the
    # older "Picker" title so a mismatched binary cannot look closed.
    return s.windows(environment, r"^Snippy .*(Vault|Picker)")


def choose(environment, window):
    # A key before an async page arrives is intentionally ignored. Retrying here
    # verifies readiness without exposing content in window titles/test hooks.
    deadline = time.monotonic() + 8
    while time.monotonic() < deadline:
        if window not in gui_windows(environment):
            return
        s.keys(environment, window, "alt+s", "Return")
        time.sleep(0.1)
    raise RuntimeError("picker did not close after choosing a loaded result")


def dialog_windows(environment):
    # Confirmation dialogs are untitled; the GUI window itself has a title.
    result = subprocess.run(["xdotool", "search", "--onlyvisible", "--name", "^$"], env=environment, capture_output=True, text=True, timeout=3)
    return set(result.stdout.split()) if result.returncode == 0 else set()


def confirm(environment, window, shortcut, answer):
    before = dialog_windows(environment)
    deadline = time.monotonic() + 8
    while time.monotonic() < deadline:
        s.keys(environment, window, shortcut)
        try:
            s.wait(lambda: dialog_windows(environment) - before, "confirmation dialog did not appear", seconds=1)
        except RuntimeError:
            continue
        subprocess.run(["xdotool", "key", "--clearmodifiers", answer], env=environment, check=True, timeout=3)
        return
    raise RuntimeError(f"{shortcut} did not open a confirmation dialog")


def readback(environment, *expected):
    subprocess.run([str(s.ROOT / "target/debug/examples/picker-read"), *expected], env=dict(environment, SNIPPY_TEST_X11="1"), check=True, timeout=10)


def open_picker(environment):
    s.require(s.cli(environment, "picker").returncode == 0, "picker command failed")
    return s.wait(lambda: s.windows(environment, "^Snippy .*(Vault|Picker) .*Unlocked$"), "picker did not open unlocked")[0]


def count(environment):
    state = s.status(environment)
    tag, value = s.request(environment, 11, struct.pack(">Q", state["session"]))
    s.require(tag == 6, "verification failed")
    return value["items"]


def report_errors(process):
    # This process and its GUI use only the isolated synthetic vault. Read a
    # bounded amount without blocking on a GUI that still owns the pipe.
    if process.stderr is not None:
        os.set_blocking(process.stderr.fileno(), False)
        diagnostics = process.stderr.read(64 * 1024)
        if diagnostics:
            sys.stderr.write("Isolated service/GUI diagnostics:\n")
            sys.stderr.flush()
            sys.stderr.buffer.write(diagnostics)
            sys.stderr.buffer.flush()


def main():
    subprocess.run(["xdotool", "getdisplaygeometry"], capture_output=True, check=True, timeout=3)
    with tempfile.TemporaryDirectory(prefix="snippy-picker-") as directory:
        root = Path(directory)
        runtime = root / "runtime"
        runtime.mkdir(mode=0o700)
        env = dict(os.environ, XDG_RUNTIME_DIR=str(runtime), XDG_DATA_HOME=str(root / "data"), XDG_CONFIG_HOME=str(root / "config"), XDG_CACHE_HOME=str(root / "cache"), XDG_SESSION_TYPE="x11", GDK_BACKEND="x11", GSK_RENDERER="cairo", GTK_A11Y="none", GSETTINGS_BACKEND="memory", DBUS_SESSION_BUS_ADDRESS="unix:path=/nonexistent-snippy-fixture-bus")
        subprocess.run([str(s.ROOT / "target/debug/examples/picker-fixture"), "--data-dir", str(root / "data"), "--stdin-passphrase"], input=s.KEY + b"\n" + s.KEY + b"\n", check=True, timeout=10)
        process = s.start(env, timeout=60)
        failed = True
        try:
            s.require(s.cli(env, "picker").returncode == 0, "locked picker command failed")
            window = s.wait(lambda: s.windows(env, "^Snippy .*(Vault|Picker) .*Locked$"), "locked picker did not open")[0]
            s.keys(env, window, "Home")
            s.type_secret(env)
            s.keys(env, window, "Return")
            s.wait(lambda: s.windows(env, "^Snippy .*(Vault|Picker) .*Unlocked$"), "picker did not unlock")
            choose(env, window)
            readback(env, "--image")
            s.require(count(env) == 25, "opening/reusing image changed history")
            s.require(s.cli(env, "lock").returncode == 0, "lock failed")
            readback(env, "--image")
            result = s.cli(env, "save")
            s.require(result.returncode == 0 and b"Already saved" in result.stdout, "Save of served item created a capture")
            s.require(s.status(env)["phase"] == 0, "Save of served clip unexpectedly unlocked")
            print("PASS: locked opening, initial selection, PNG reuse after GUI exit/lock, Save while serving", flush=True)

            s.require(s.request(env, 4, s.secret(s.KEY))[0] == 1, "unlock failed")
            window = open_picker(env)
            s.keys(env, window, "alt+s")
            type_text(env, "Synthetic note 07")
            choose(env, window)
            readback(env, "--note", "7")
            s.require(count(env) == 25, "text reuse changed history")
            print("PASS: focused search and literal text lookup copy exact Unicode/whitespace", flush=True)

            window = open_picker(env)
            s.keys(env, window, "alt+a")
            type_text(env, "fixture:editor")
            s.keys(env, window, "alt+t")
            type_text(env, "even")
            s.keys(env, window, "alt+y", "Down", "Return")
            s.keys(env, window, "alt+s")
            type_text(env, "nonexistent fixture phrase")
            time.sleep(0.3)
            s.keys(env, window, "Return")
            time.sleep(0.1)
            s.require(window in gui_windows(env), "empty results copied a stale item")
            s.keys(env, window, "ctrl+a", "BackSpace")
            choose(env, window)
            readback(env, "--note", "22")
            print("PASS: combined source/tag/type filters and no-results Enter behavior", flush=True)

            window = open_picker(env)
            s.keys(env, window, "alt+s")
            type_text(env, "Synthetic note")
            time.sleep(0.3)
            s.keys(env, window, "alt+n")
            time.sleep(0.3)
            choose(env, window)
            readback(env, "--note", "13")
            window = open_picker(env)
            time.sleep(0.3)
            s.keys(env, window, "alt+s", "Down", "Up", "Down")
            choose(env, window)
            readback(env, "--note", "23")
            window = open_picker(env)
            s.keys(env, window, "Escape")
            s.wait(lambda: window not in gui_windows(env), "Escape did not close picker")
            readback(env, "--note", "23")
            s.require(count(env) == 25, "pagination or Escape changed history")
            print("PASS: bounded next page, Up/Down selection, clean reopen, Escape leaves clipboard/history unchanged", flush=True)

            window = open_picker(env)
            s.keys(env, window, "alt+s")
            type_text(env, "Synthetic note 07")
            time.sleep(1.2)
            # Copy and delete asks for confirmation like Delete; wait for the
            # dialog (retrying while the page is still loading), then accept it.
            confirm(env, window, "ctrl+x", "Return")
            s.wait(lambda: count(env) == 24, "Copy and delete did not remove exactly one item")
            readback(env, "--note", "7")
            s.require(window in gui_windows(env), "Copy and delete closed the window")
            s.keys(env, window, "Escape")
            s.wait(lambda: window not in gui_windows(env), "Escape did not close the window")
            print("PASS: Copy and delete serves the item once, then removes it from history", flush=True)
            failed = False
        finally:
            s.stop(process)
            for window in gui_windows(env):
                try:
                    s.keys(env, window, "alt+c")
                except subprocess.SubprocessError:
                    pass
            if failed:
                report_errors(process)

if __name__ == "__main__":
    main()
