#!/usr/bin/env python3
"""Run the static PTY dup/fork ownership probe on Denver guest 1.

rustc --edition 2021 -O --target x86_64-unknown-linux-musl \
    userspace/forktest/ptyrefs/ptyrefs.rs -o /tmp/ptyrefs
ECO_KERNEL=target/x86_64-unknown-none/release/denver \
    python3 overlays/ecosystem-demo/ptyrefs.py /tmp/ptyrefs
"""
import argparse
import subprocess
import tempfile
import time
from pathlib import Path

import launch


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("binary", type=Path)
    ap.add_argument("--n", type=int, default=1)
    args = ap.parse_args()
    run_dir = Path(tempfile.mkdtemp(prefix="eco-ptyrefs-", dir=launch.WORK))
    demo = launch.Demo(launch.WORK / "out", run_dir, 3072, 2)
    try:
        demo.up((1,))
        guest = demo.guests[1]
        time.sleep(3)
        with args.binary.open("rb") as source:
            subprocess.run(guest.ssh_argv("cat > /tmp/ptyrefs && chmod 755 /tmp/ptyrefs"),
                           stdin=source, check=True)
        ok = True
        for i in range(args.n):
            try:
                rc, output = guest.run("timeout 20 /tmp/ptyrefs", timeout=40)
            except subprocess.TimeoutExpired:
                rc, output = -1, "SSH timed out"
            passed = rc == 0 and "PTYREFS PASS" in output
            ok &= passed
            print(f"[PTY] run {i + 1}: {'PASS' if passed else 'FAIL'} rc={rc}\n{output}", flush=True)
        return 0 if ok else 1
    finally:
        demo.down()


if __name__ == "__main__":
    raise SystemExit(main())
