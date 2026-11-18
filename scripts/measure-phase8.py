#!/usr/bin/env python3
"""Release-build measurements on a private Xvfb display. Synthetic data only."""
import itertools
import os
import platform
import re
import socket
import statistics
import struct
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BIN = ROOT / "target/release"
CLI = BIN / "snippy"
GUI = BIN / "snippy-gui"
KEY = b"synthetic-phase8-measurement-key"
IDS = itertools.count(1)


def request(environment, kind, payload=b"", timeout=15):
    identity = next(IDS)
    body = struct.pack(">HQB", 5, identity, kind) + payload
    with socket.socket(socket.AF_UNIX) as stream:
        stream.settimeout(timeout)
        stream.connect(str(Path(environment["XDG_RUNTIME_DIR"]) / "snippy/service.sock"))
        stream.sendall(struct.pack(">I", len(body)) + body)
        size = struct.unpack(">I", recv_exact(stream, 4))[0]
        reply = recv_exact(stream, size)
    version, got, tag = struct.unpack(">HQB", reply[:11])
    if version != 5 or got != identity:
        raise RuntimeError("IPC response mismatch")
    return tag, reply[11:]


def recv_exact(stream, count):
    data = b""
    while len(data) < count:
        chunk = stream.recv(count - len(data))
        if not chunk:
            raise RuntimeError("IPC stream ended early")
        data += chunk
    return data


def wait(predicate, message, seconds=10):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        try:
            value = predicate()
            if value:
                return value
        except (OSError, RuntimeError, subprocess.SubprocessError):
            pass
        time.sleep(0.02)
    raise RuntimeError(message)


def pid_stat(pid):
    fields = Path(f"/proc/{pid}/stat").read_text().split()
    return int(fields[13]) + int(fields[14])


def pid_memory(pid):
    status = Path(f"/proc/{pid}/status").read_text()
    rss = int(re.search(r"^VmRSS:\s+(\d+)", status, re.MULTILINE).group(1))
    pss = rss
    try:
        for line in Path(f"/proc/{pid}/smaps_rollup").read_text().splitlines():
            if line.startswith("Pss:"):
                pss = int(line.split()[1])
                break
    except OSError:
        pass
    return rss, pss


def stats(environment):
    state_tag, state = request(environment, 1)
    if state_tag != 1:
        raise RuntimeError("status failed")
    session = struct.unpack(">Q", state[8:16])[0]
    tag, info = request(environment, 25, struct.pack(">Q", session))
    if tag != 14:
        raise RuntimeError("manager info failed")
    items = struct.unpack(">Q", info[8:16])[0]
    logical = struct.unpack(">Q", info[16:24])[0]
    physical = struct.unpack(">Q", info[24:32])[0]
    return session, items, logical, physical


def browse(environment, session):
    text = b""
    payload = (
        struct.pack(">Q", session)
        + struct.pack(">H", 0)
        + struct.pack(">H", 0)
        + struct.pack(">Q", 0)
        + struct.pack(">H", 0)
        + bytes([0, 1])
        + struct.pack(">Q", 0)
    )
    return request(environment, 13, payload)


def gui_windows(environment, pattern=r"^Snippy .*Vault .*Unlocked$"):
    result = subprocess.run(
        ["xdotool", "search", "--onlyvisible", "--name", pattern],
        env=environment,
        capture_output=True,
        text=True,
        timeout=3,
    )
    return result.stdout.splitlines() if result.returncode == 0 else []


def open_gui(environment):
    started = time.monotonic()
    process = subprocess.Popen(
        [str(GUI)],
        env=environment,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    window = wait(
        lambda: gui_windows(environment),
        "GUI did not become visible",
        seconds=10,
    )[0]
    elapsed = (time.monotonic() - started) * 1000
    return process, window, elapsed


def close_gui(environment, process, window):
    try:
        subprocess.run(
            ["xdotool", "windowfocus", "--sync", window],
            env=environment,
            check=True,
            timeout=3,
        )
        subprocess.run(
            ["xdotool", "key", "--clearmodifiers", "alt+c"],
            env=environment,
            check=True,
            timeout=3,
        )
    except subprocess.SubprocessError:
        pass
    try:
        process.wait(timeout=3)
    except subprocess.TimeoutExpired:
        process.terminate()
        process.wait(timeout=3)


def main():
    subprocess.run(["xdotool", "getdisplaygeometry"], check=True, capture_output=True, timeout=3)
    with tempfile.TemporaryDirectory(prefix="snippy-phase8-") as directory:
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
            DBUS_SESSION_BUS_ADDRESS="unix:path=/nonexistent-snippy-measure-bus",
        )
        subprocess.run(
            [
                str(BIN / "examples/phase8-fixture"),
                "--data-dir",
                str(root / "data"),
                "--items",
                "800",
                "--bytes",
                "60000",
                "--quota-bytes",
                "67108864",
                "--stdin-passphrase",
            ],
            input=KEY + b"\n",
            check=True,
            timeout=180,
        )
        daemon = subprocess.Popen(
            [
                str(CLI),
                "daemon",
                "--no-shortcuts",
                "--max-vault-bytes",
                "67108864",
            ],
            env=environment,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
        )
        gui = None
        try:
            wait(lambda: request(environment, 1), "daemon did not start")
            unlock_started = time.perf_counter()
            tag, reply = request(
                environment,
                4,
                struct.pack(">H", len(KEY)) + KEY,
                timeout=30,
            )
            unlock_ms = (time.perf_counter() - unlock_started) * 1000
            if tag != 1:
                raise RuntimeError(f"unlock failed: tag={tag} body={reply[:32]!r}")
            session, items, logical, physical = stats(environment)

            clock = os.sysconf("SC_CLK_TCK")
            before = pid_stat(daemon.pid)
            time.sleep(3)
            after = pid_stat(daemon.pid)
            idle_ticks = after - before
            idle_cpu_ms = idle_ticks * 1000 / clock
            daemon_rss, daemon_pss = pid_memory(daemon.pid)
            print(f"environment: {platform.platform()}")
            print(f"cpu: {cpu_model()}")
            print(f"memory_kib: {total_memory()}")
            print(f"rustc: {subprocess.run(['rustc', '--version'], capture_output=True, text=True, check=True).stdout.strip()}")
            print(f"gtk: {subprocess.run(['pkg-config', '--modversion', 'gtk4'], capture_output=True, text=True, check=True).stdout.strip()}")
            print(f"session: private Xvfb, no window manager, GSK_RENDERER=cairo")
            print(f"vault: items={items} logical_bytes={logical} physical_bytes={physical}")
            print(f"unlock: elapsed_ms={unlock_ms:.1f} samples=1")
            print(
                f"daemon_idle: samples=1 window_ms=3000 cpu_ticks={idle_ticks} "
                f"cpu_percent={100 * idle_cpu_ms / 3000:.3f} rss_kib={daemon_rss} pss_kib={daemon_pss}"
            )

            query_times = []
            for _ in range(20):
                started = time.perf_counter()
                tag, _ = browse(environment, session)
                query_times.append((time.perf_counter() - started) * 1000)
                if tag != 7:
                    raise RuntimeError("query failed")
            print(
                f"query_browse: samples=20 median_ms={statistics.median(query_times):.3f} "
                f"max_ms={max(query_times):.3f}"
            )

            save_times = []
            for size in [512, 768, 1024, 1536, 2048]:
                owner = subprocess.Popen(
                    [
                        str(BIN / "examples/capture-source"),
                        "--source",
                        "editor",
                        "--bytes",
                        str(size),
                        "--incr",
                    ],
                    env=environment,
                    stdin=subprocess.PIPE,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                )
                try:
                    time.sleep(0.25)
                    started = time.perf_counter()
                    tag, _ = request(environment, 8)
                    save_times.append((time.perf_counter() - started) * 1000)
                    if tag != 4:
                        raise RuntimeError("save failed")
                finally:
                    owner.terminate()
                    owner.wait(timeout=3)
            print(
                f"save_capture: samples=5 text_bytes=512..2048 median_ms="
                f"{statistics.median(save_times):.3f} max_ms={max(save_times):.3f}"
            )

            churn = subprocess.Popen(
                [str(BIN / "examples/x11-churn"), "--changes", "3000"],
                env=dict(environment, SNIPPY_TEST_X11="1"),
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            before = pid_stat(daemon.pid)
            time.sleep(3)
            after = pid_stat(daemon.pid)
            churn_ticks = after - before
            churn.wait(timeout=10)
            print(
                f"daemon_owner_events: changes=3000 window_ms=3000 cpu_ticks={churn_ticks} "
                f"cpu_percent={100 * churn_ticks / clock / 3:.3f}"
            )

            gui, window, cold_ms = open_gui(environment)
            time.sleep(0.4)
            gui_rss, gui_pss = pid_memory(gui.pid)
            with_gui_rss, with_gui_pss = pid_memory(daemon.pid)
            print(
                f"gui_cold_open: elapsed_ms={cold_ms:.1f} gui_rss_kib={gui_rss} "
                f"gui_pss_kib={gui_pss} daemon_rss_kib={with_gui_rss} daemon_pss_kib={with_gui_pss}"
            )
            close_gui(environment, gui, window)
            gui, window, warm_ms = open_gui(environment)
            close_gui(environment, gui, window)
            print(f"gui_warm_open: elapsed_ms={warm_ms:.1f}")

            gui, window, _ = open_gui(environment)
            picker_started = time.perf_counter()
            tag, _ = request(environment, 12)
            if tag != 1:
                raise RuntimeError("picker mode request failed")
            wait(
                lambda: gui_windows(environment, r"^Snippy .*(Vault|Picker) .*Unlocked$"),
                "picker mode did not appear",
            )
            picker_ms = (time.perf_counter() - picker_started) * 1000
            picker_rss, picker_pss = pid_memory(gui.pid)
            print(
                f"picker_open: elapsed_ms={picker_ms:.1f} gui_rss_kib={picker_rss} "
                f"gui_pss_kib={picker_pss}"
            )
            close_gui(environment, gui, window)
            print(
                "report: synthetic near-quota vault (800 items, 64 MiB quota); "
                "no live clipboard or real source data used"
            )
        finally:
            if gui is not None and gui.poll() is None:
                gui.terminate()
                gui.wait(timeout=3)
            daemon.terminate()
            try:
                daemon.wait(timeout=8)
            except subprocess.TimeoutExpired:
                daemon.kill()
                daemon.wait(timeout=3)
            if daemon.stderr:
                diagnostics = daemon.stderr.read(4096)
                if diagnostics:
                    sys.stderr.buffer.write(diagnostics)


def cpu_model():
    try:
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.startswith("model name"):
                return line.split(":", 1)[1].strip()
    except OSError:
        pass
    return "unknown"


def total_memory():
    try:
        for line in Path("/proc/meminfo").read_text().splitlines():
            if line.startswith("MemTotal:"):
                return line.split()[1]
    except OSError:
        pass
    return "unknown"


if __name__ == "__main__":
    main()
