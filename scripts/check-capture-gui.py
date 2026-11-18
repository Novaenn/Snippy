"""Private-display Save GUI smoke tests; synthetic offers and isolated paths only."""
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


def source(environment, kind="unknown", fmt="text"):
    process = subprocess.Popen([str(s.ROOT / "target/debug/examples/capture-source"), "--source", kind, "--format", fmt], env=environment, stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    # Wait for the explicit synthetic owner to finish setup.
    time.sleep(0.2)
    s.require(process.poll() is None, "synthetic source failed to start")
    return process


def items(environment):
    state = s.status(environment)
    tag, value = s.request(environment, 11, struct.pack(">Q", state["session"]))
    s.require(tag == 6, "verification requires a current unlocked session")
    return value["items"]


def pending(environment):
    return subprocess.Popen([str(s.CLI), "save"], env=environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE)


def outcome(process, success):
    output, error = process.communicate(timeout=8)
    s.require((process.returncode == 0) == success, "Save command outcome disagrees with the GUI action")
    s.require(b"synthetic" not in output + error, "Save output exposed synthetic content/source")


def main():
    subprocess.run(["xdotool", "getdisplaygeometry"], check=True, capture_output=True, timeout=3)
    with tempfile.TemporaryDirectory(prefix="snippy-capture-gui-") as directory:
        root = Path(directory)
        runtime = root / "runtime"
        runtime.mkdir(mode=0o700)
        env = dict(os.environ, XDG_RUNTIME_DIR=str(runtime), XDG_DATA_HOME=str(root / "data"), XDG_CONFIG_HOME=str(root / "config"), XDG_CACHE_HOME=str(root / "cache"), XDG_SESSION_TYPE="x11", GDK_BACKEND="x11", GSK_RENDERER="cairo", GTK_A11Y="none", GSETTINGS_BACKEND="memory", DBUS_SESSION_BUS_ADDRESS="unix:path=/nonexistent-snippy-fixture-bus")
        daemon = s.start(env, timeout=60)
        owner = attempt = None
        try:
            owner = source(env)
            attempt = pending(env)
            window = s.wait(lambda: s.windows(env, "^Snippy .*Vault .*Locked$"), "Save did not open setup")[0]
            s.require(s.status(env)["capture_stage"] == 1, "Save must await setup before transfer")
            s.keys(env, window, "Home")
            s.type_secret(env)
            s.keys(env, window, "Tab")
            s.type_secret(env)
            s.keys(env, window, "Return")
            s.wait(lambda: s.windows(env, "Confirm Save$"), "Unknown confirmation did not appear after unlock")
            s.keys(env, window, "alt+v")
            outcome(attempt, False)
            s.require(items(env) == 0, "Cancel saved an item")
            print("PASS: Save launches setup and Unknown confirmation; Cancel leaves history empty", flush=True)

            attempt = pending(env)
            s.wait(lambda: s.windows(env, "Confirm Save$"), "Unknown confirmation did not reopen")
            # The optional manual-label field receives focus when the prompt arrives.
            subprocess.run(["xdotool", "type", "--clearmodifiers", "Manual fixture"], env=env, check=True, timeout=3)
            s.keys(env, window, "alt+s")
            outcome(attempt, True)
            s.require(items(env) == 1, "approved Unknown save did not persist")
            s.keys(env, window, "alt+r")
            print("PASS: manual-source confirmation commits once; verification is available", flush=True)

            s.stop(owner)
            owner = source(env, "sensitive")
            attempt = pending(env)
            s.wait(lambda: s.windows(env, "Confirm Save$"), "sensitive-source warning did not appear")
            s.keys(env, window, "alt+v")
            outcome(attempt, False)
            s.require(items(env) == 1, "sensitive cancellation persisted content")
            attempt = pending(env)
            s.wait(lambda: s.windows(env, "Confirm Save$"), "sensitive warning was permanently bypassed")
            s.keys(env, window, "alt+s")
            outcome(attempt, True)
            s.require(items(env) == 2, "approved sensitive save did not persist")
            print("PASS: sensitive warning applies per Save; cancellation and approval have distinct outcomes", flush=True)

            s.stop(owner)
            owner = source(env)
            attempt = pending(env)
            s.wait(lambda: s.windows(env, "Confirm Save$"), "race confirmation did not appear")
            owner.stdin.write(b"\n")
            owner.stdin.flush()
            outcome(attempt, False)
            s.require(items(env) == 2, "same-owner recopy during confirmation persisted content")
            s.require(s.cli(env, "lock").returncode == 0, "lock failed")
            s.wait(lambda: s.windows(env, "Locked$"), "lock did not clear confirmation/verification")
            s.keys(env, window, "alt+c")
            s.wait(lambda: not s.windows(env), "GUI did not exit")
            print("PASS: selection changes cancel pending Save; lock clears private UI; GUI exits", flush=True)
        finally:
            if attempt and attempt.poll() is None:
                s.stop(attempt)
            if owner:
                s.stop(owner)
            s.stop(daemon)
            for window in s.windows(env):
                try:
                    s.keys(env, window, "alt+c")
                except subprocess.SubprocessError:
                    pass

if __name__ == "__main__":
    main()
