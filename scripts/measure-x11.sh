#!/bin/sh
# Aggregate resource results from synthetic sources on a private display only.
set -eu
task_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$task_root"
if [ -z "${CARGO_HOME:-}" ] && [ -d "$task_root/.cache/cargo" ]; then
    CARGO_HOME="$task_root/.cache/cargo"
    export CARGO_HOME
fi
cargo build -p snippy --examples --release --locked --offline
sh scripts/with-xvfb.sh -a -s '-screen 0 1024x768x24 -nolisten tcp' \
    python3 - "$task_root" <<'PY'
import json
import os
from pathlib import Path
import platform
import select
import subprocess
import sys

root = Path(sys.argv[1])
subprocess.run(["xdotool", "getdisplaygeometry"], check=True, capture_output=True, timeout=5)
environment = os.environ.copy()
environment["SNIPPY_TEST_X11"] = "1"
results = {}
for mode in ("idle", "churn"):
    meter = subprocess.Popen([root / "target/release/examples/x11-meter", "--seconds", "3"],
                             env=environment, stdout=subprocess.PIPE, bufsize=0)
    try:
        if not select.select([meter.stdout], [], [], 10)[0]:
            raise RuntimeError("Resource observer did not become ready within 10 seconds")
        if meter.stdout.readline().strip() != b"READY":
            raise RuntimeError("Resource observer did not become ready")
        if mode == "churn":
            subprocess.run([root / "target/release/examples/x11-churn", "--changes", "1000"],
                           env=environment, check=True, timeout=15)
        output, _ = meter.communicate(timeout=10)
        if meter.returncode:
            raise RuntimeError("Resource observer failed")
        results[mode] = dict(item.split("=", 1) for item in output.decode("ascii").strip().split())
        results[mode] = {key: int(value) for key, value in results[mode].items()}
        if mode == "churn" and results[mode]["ownership_updates"] <= 1:
            raise RuntimeError("Synthetic changes were not observed")
    finally:
        if meter.poll() is None:
            meter.kill()
            meter.wait(timeout=5)
report = {
    "environment": platform.platform(),
    "compiler": subprocess.check_output(["rustc", "--version"], text=True).strip(),
    "clock_ticks_per_second": os.sysconf("SC_CLK_TCK"),
    "profile": "release",
    "scope": "X11 observer process only; fixture generator is a separate process, no GUI or vault",
    "synthetic_changes": 1000,
    "results": results,
}
print(json.dumps(report, indent=2))
PY
