"""Invoked by check-service.sh on a private display, using synthetic data only."""
import itertools
import os
from pathlib import Path
import socket
import stat
import struct
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parent.parent
CLI = ROOT / "target/debug/snippy"
GUI = ROOT / "target/debug/snippy-gui"
IDS = itertools.count(1)
KEY = b"synthetic-service-fixture-key"


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def exact(stream, count):
    data = bytearray()
    while len(data) < count:
        block = stream.recv(count - len(data))
        if not block:
            raise RuntimeError("service disconnected before completing its response")
        data.extend(block)
    return data


def source_value(body, offset):
    kind = body[offset]
    offset += 1
    if kind == 0:
        return ("unknown", None, None), offset
    if kind == 1:
        n = struct.unpack(">H", body[offset:offset+2])[0]
        app = body[offset+2:offset+2+n].decode()
        offset += 2 + n
        n = struct.unpack(">H", body[offset:offset+2])[0]
        label = body[offset+2:offset+2+n].decode()
        offset += 2 + n
        return ("observed", app, label), offset
    if kind == 2:
        n = struct.unpack(">H", body[offset:offset+2])[0]
        label = body[offset+2:offset+2+n].decode()
        offset += 2 + n
        return ("manual", None, label), offset
    raise RuntimeError("invalid source encoding")


def text_value(body, offset):
    n = struct.unpack(">H", body[offset:offset+2])[0]
    return body[offset+2:offset+2+n].decode(), offset + 2 + n


def response(stream):
    size = struct.unpack(">I", exact(stream, 4))[0]
    require(11 <= size <= 300_000, "response size is invalid")
    body = exact(stream, size)
    version, identity, kind = struct.unpack(">HQB", body[:11])
    require(version == 5, "response version mismatch")
    if kind == 1:
        require(len(body) == 50, "invalid status length")
        revision, session, phase, exists, save, picker = struct.unpack(">QQBBBB", body[11:31])
        capture, capture_stage, save_outcome = struct.unpack(">QBB", body[31:41])
        return identity, kind, dict(revision=revision, session=session, phase=phase, exists=exists, save=save, picker=picker, capture=capture, capture_stage=capture_stage, save_outcome=save_outcome)
    if kind == 6:
        session, items, logical_bytes = struct.unpack(">QQQ", body[11:35])
        return identity, kind, dict(session=session, items=items, logical_bytes=logical_bytes)
    if kind == 7:
        session, offset = struct.unpack(">QQ", body[11:27])
        more = body[27] == 1
        count = body[28]
        cursor = 29
        rows = []
        for _ in range(count):
            clip, source_id = struct.unpack(">QQ", body[cursor:cursor+16]); cursor += 16
            source, cursor = source_value(body, cursor)
            clip_kind = body[cursor]; cursor += 1
            byte_count, first_saved, last_saved = struct.unpack(">Qqq", body[cursor:cursor+24]); cursor += 24
            text, cursor = text_value(body, cursor)
            tags_count = body[cursor]; cursor += 1
            tags = []
            for _ in range(tags_count):
                tag, cursor = text_value(body, cursor)
                tags.append(tag)
            rows.append(dict(id=clip, source_id=source_id, source=source, kind=clip_kind, byte_count=byte_count, first_saved=first_saved, last_saved=last_saved, text=text, tags=tags))
        require(cursor == len(body), "page length mismatch")
        return identity, kind, dict(session=session, offset=offset, more=more, rows=rows)
    if kind == 10:
        session = struct.unpack(">Q", body[11:19])[0]
        enabled = body[19] == 1
        autostart = body[20] == 1
        notifications = body[21] == 1
        save, cursor = text_value(body, 22)
        picker, cursor = text_value(body, cursor)
        max_clip, max_vault, timeout = struct.unpack(">QQQ", body[cursor:cursor+24]); cursor += 24
        require(cursor == len(body), "config length mismatch")
        return identity, kind, dict(session=session, enabled=enabled, autostart=autostart, notifications=notifications, save=save, picker=picker, max_clip=max_clip, max_vault=max_vault, timeout=timeout)
    if kind == 11:
        count = body[11]
        cursor = 12
        apps = []
        for _ in range(count):
            app, cursor = text_value(body, cursor)
            apps.append(app)
        require(cursor == len(body), "sensitive list length mismatch")
        return identity, kind, apps
    if kind == 12:
        clip = struct.unpack(">Q", body[11:19])[0]
        count = body[19]
        cursor = 20
        tags = []
        for _ in range(count):
            tag, cursor = text_value(body, cursor)
            tags.append(tag)
        require(cursor == len(body), "tags length mismatch")
        return identity, kind, dict(id=clip, tags=tags)
    if kind == 13:
        return identity, kind, struct.unpack(">Q", body[11:19])[0]
    if kind == 14:
        session, items, logical, physical = struct.unpack(">QQQQ", body[11:43])
        source_count = body[43]
        cursor = 44
        sources = []
        for _ in range(source_count):
            source_id = struct.unpack(">Q", body[cursor:cursor+8])[0]; cursor += 8
            source, cursor = source_value(body, cursor)
            clips = struct.unpack(">Q", body[cursor:cursor+8])[0]; cursor += 8
            sources.append(dict(id=source_id, source=source, clips=clips))
        tag_count = body[cursor]; cursor += 1
        tags = []
        for _ in range(tag_count):
            name, cursor = text_value(body, cursor)
            clips = struct.unpack(">Q", body[cursor:cursor+8])[0]; cursor += 8
            tags.append(dict(name=name, clips=clips))
        require(cursor == len(body), "manager info length mismatch")
        return identity, kind, dict(session=session, items=items, logical_bytes=logical, physical_bytes=physical, sources=sources, tags=tags)
    require(kind in (2, 3), "unknown response")
    return identity, kind, body[11] if kind == 2 else None


def secret(value):
    return struct.pack(">H", len(value)) + value


def connect(environment):
    stream = socket.socket(socket.AF_UNIX)
    stream.settimeout(5)
    stream.connect(str(Path(environment["XDG_RUNTIME_DIR"]) / "snippy/service.sock"))
    return stream


def request(environment, kind, payload=b"", version=5):
    identity = next(IDS)
    body = struct.pack(">HQB", version, identity, kind) + payload
    with connect(environment) as stream:
        stream.sendall(struct.pack(">I", len(body)) + body)
        got, tag, result = response(stream)
    require(got == identity, "request ID mismatch")
    return tag, result


def status(environment):
    tag, value = request(environment, 1)
    require(tag == 1, "status request failed")
    return value


def wait(predicate, message, seconds=8):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        try:
            value = predicate()
            if value:
                return value
        except (OSError, RuntimeError, subprocess.SubprocessError):
            pass
        time.sleep(0.05)
    raise RuntimeError(message)


def start(environment, timeout=2):
    process = subprocess.Popen([str(CLI), "daemon", "--lock-timeout-seconds", str(timeout), "--no-shortcuts"], env=environment, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    try:
        wait(lambda: process.poll() is None and status(environment), "daemon did not become ready")
    except Exception:
        stop(process)
        raise
    return process


def stop(process):
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=8)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)


def cli(environment, *args):
    return subprocess.run([str(CLI), *args], env=environment, capture_output=True, timeout=8)


def windows(environment, pattern="^Snippy .*Vault"):
    result = subprocess.run(["xdotool", "search", "--onlyvisible", "--name", pattern], env=environment, capture_output=True, text=True, timeout=3)
    return result.stdout.splitlines() if result.returncode == 0 else []


def keys(environment, window, *sequence):
    subprocess.run(["xdotool", "windowfocus", "--sync", window], env=environment, check=True, timeout=3)
    subprocess.run(["xdotool", "key", "--clearmodifiers", *sequence], env=environment, check=True, timeout=3)


def type_secret(environment):
    # Input through stdin: even the synthetic key stays out of command arguments.
    subprocess.run(["xdotool", "type", "--clearmodifiers", "--delay", "1", "--file", "-"], input=KEY, env=environment, check=True, timeout=5)


def main():
    subprocess.run(["xdotool", "getdisplaygeometry"], check=True, capture_output=True, timeout=3)
    with tempfile.TemporaryDirectory(prefix="snippy-service-check-") as directory:
        root = Path(directory)
        runtime = root / "runtime"
        runtime.mkdir(mode=0o700)
        environment = dict(os.environ, XDG_RUNTIME_DIR=str(runtime), XDG_DATA_HOME=str(root / "data"), XDG_CONFIG_HOME=str(root / "config"), XDG_CACHE_HOME=str(root / "cache"), XDG_SESSION_TYPE="x11", GDK_BACKEND="x11", GSETTINGS_BACKEND="memory", DBUS_SESSION_BUS_ADDRESS="unix:path=/nonexistent-snippy-fixture-bus")
        process = start(environment)
        extra = None
        try:
            initial = status(environment)
            require(initial["phase"] == 0 and not initial["exists"], "service must start locked and require setup")
            require(not (root / "data/snippy").exists(), "daemon created a vault without confirmation")
            app_runtime = runtime / "snippy"
            require(stat.S_IMODE(app_runtime.stat().st_mode) == 0o700, "runtime directory mode")
            require(stat.S_IMODE((app_runtime / "service.sock").stat().st_mode) == 0o600, "socket mode")
            duplicate = cli(environment, "daemon", "--no-shortcuts")
            require(duplicate.returncode == 1 and b"already running" in duplicate.stderr, "duplicate startup not rejected")
            require(status(environment)["phase"] == 0, "duplicate affected existing service")
            print("PASS: locked startup, private paths, real peer credentials, duplicate daemon isolation", flush=True)

            require(request(environment, 3, secret(KEY) + secret(b"mismatch")) == (2, 11), "mismatched creation must fail")
            require(not (root / "data/snippy").exists(), "mismatched creation wrote a vault")
            require(request(environment, 3, secret(KEY) + secret(KEY))[0] == 1, "vault creation failed")
            require(cli(environment, "lock").returncode == 0, "CLI lock failed")
            require(request(environment, 4, secret(b"incorrect")) == (2, 10), "wrong passphrase was accepted")
            require(status(environment)["phase"] == 0, "wrong password unlocked vault")
            require(request(environment, 4, secret(KEY))[0] == 1, "correct passphrase failed")
            current = status(environment)
            require(request(environment, 6, struct.pack(">Q", current["session"] - 1)) == (2, 8), "stale activity was accepted")
            require(request(environment, 1, version=1) == (2, 5), "incompatible version was not reported")
            with connect(environment) as stream:
                stream.sendall(struct.pack(">I", 4097))
                require(response(stream)[1:] == (2, 4), "oversized frame accepted")
            require(request(environment, 99) == (2, 4), "unknown opcode accepted")
            print("PASS: confirmed creation, wrong/correct unlock, explicit lock, framing/version/session rejection", flush=True)

            # Continuous ordinary ownership changes must not keep the vault unlocked.
            require(cli(environment, "lock").returncode == 0, "lock before timeout check")
            require(request(environment, 4, secret(KEY))[0] == 1, "unlock before timeout check")
            extra = subprocess.Popen([str(ROOT / "target/debug/examples/x11-churn"), "--changes", "5000"], env=dict(environment, SNIPPY_TEST_X11="1"), stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
            time.sleep(2.5)
            require(status(environment)["phase"] == 0, "ordinary clipboard changes extended vault inactivity")
            stop(extra)
            extra = None
            print("PASS: inactivity lock fires during ordinary clipboard ownership churn", flush=True)

            process.kill()
            process.wait(timeout=5)
            require((app_runtime / "service.sock").exists(), "crash fixture did not leave a stale socket")
            process = start(environment)
            require(status(environment)["phase"] == 0 and status(environment)["exists"], "restart did not preserve the locked vault")
            require(request(environment, 4, secret(KEY))[0] == 1, "vault did not survive restart")
            stop(process)
            require(not (app_runtime / "service.sock").exists(), "graceful shutdown left service socket")
            marker = root / "untouched"
            marker.write_bytes(b"synthetic untouched fixture")
            (app_runtime / "service.sock").symlink_to(marker)
            result = cli(environment, "daemon", "--no-shortcuts")
            require(result.returncode == 1 and marker.read_bytes() == b"synthetic untouched fixture" and (app_runtime / "service.sock").is_symlink(), "unsafe socket was modified")
            (app_runtime / "service.sock").unlink()
            print("PASS: crash recovery starts locked, encrypted vault survives, unsafe socket is preserved", flush=True)

            # Fresh data root exercises the real setup UI, without WM or session bus.
            environment["XDG_DATA_HOME"] = str(root / "gui-data")
            process = start(environment, timeout=5)
            require(cli(environment, "manager").returncode == 0, "manager did not accept launch")
            window = wait(lambda: windows(environment, "^Snippy .*Vault .*Locked$"), "setup window did not appear")[0]
            keys(environment, window, "Home")
            type_secret(environment)
            keys(environment, window, "Tab")
            type_secret(environment)
            keys(environment, window, "Return")
            wait(lambda: windows(environment, "^Snippy .*Vault .*Unlocked$"), "GUI setup did not unlock vault")
            require(cli(environment, "manager").returncode == 0, "manager focus request failed")
            require(len(windows(environment)) == 1, "manager opened a duplicate window")
            keys(environment, window, "alt+c")
            wait(lambda: not windows(environment), "unlocked GUI did not exit")
            require(status(environment)["phase"] == 2, "closing GUI stopped or locked service")
            require(cli(environment, "manager").returncode == 0, "reopening unlocked GUI failed")
            window = wait(lambda: windows(environment, "^Snippy .*Vault .*Unlocked$"), "unlocked GUI did not reopen")[0]
            require(cli(environment, "lock").returncode == 0, "lock while GUI open failed")
            wait(lambda: windows(environment, "^Snippy .*Vault .*Locked$"), "GUI did not receive lock event")
            keys(environment, window, "Home")
            type_secret(environment)
            keys(environment, window, "Tab", "Tab", "space")
            require(status(environment)["phase"] == 0, "Cancel unlocked the vault")
            keys(environment, window, "alt+c")
            wait(lambda: not windows(environment), "GUI did not exit")
            require(status(environment)["phase"] == 0, "GUI exit stopped service")
            require(cli(environment, "manager").returncode == 0, "GUI reopen failed")
            window = wait(lambda: windows(environment, "^Snippy .*Vault .*Locked$"), "unlock window did not reopen")[0]
            keys(environment, window, "Home")
            type_secret(environment)
            keys(environment, window, "Return")
            wait(lambda: windows(environment, "^Snippy .*Vault .*Unlocked$"), "GUI unlock failed")
            wait(lambda: windows(environment, "^Snippy .*Vault .*Locked$"), "GUI did not receive idle lock", seconds=8)
            stop(process)
            wait(lambda: windows(environment, "Service unavailable$"), "GUI did not clear on service exit")
            keys(environment, window, "alt+c")
            wait(lambda: not windows(environment), "disconnected GUI did not exit")
            print("PASS: setup/unlock GUI, launch/focus/reopen, lock/timeout/disconnect clearing, clean exit without WM/session bus", flush=True)
        finally:
            if extra:
                stop(extra)
            stop(process)
            for window in windows(environment):
                try:
                    keys(environment, window, "alt+c")
                except subprocess.SubprocessError:
                    pass


if __name__ == "__main__":
    main()
