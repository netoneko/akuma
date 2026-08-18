#!/usr/bin/env python3
"""Measure a booted kernel's IDLE host-CPU cost — the cold-start CPU gate.

Why this exists: a change can leave every functional gate in
`docs/runbooks/verify-trim-fat-change.md` green and still make an idle VM burn
whole host cores. The failure is invisible to boot-suite counts and to Tier 3
probes, because nothing it measures is *wrong* — there is just 100x more of it.
Scheduler-tick, wake/preempt and poll-interval changes all land here.

What it measures: QEMU's total CPU-seconds (user+sys, ALL vCPU threads) divided
by wall-clock over a sampling window that starts AFTER the boot marker, so boot
work is excluded. 100.0 = one host core saturated; on an SMP=4 devbox an idle
kernel should sit near single digits and a spinning one pegs ~400.

`ps -o %cpu` is NOT usable for this: on macOS it reports an average over the
whole process lifetime, so a VM that spun during boot and then went quiet reads
identically to one that is still spinning. This samples `ps -o time=` (cumulative
CPU) twice and differences it.

Usage:
    scripts/measure_idle_cpu.py                     # devbox-smoltcp, SMP=4
    scripts/measure_idle_cpu.py --settle 20 --window 30
    scripts/measure_idle_cpu.py --run-cmd 'cargo run --release' --smp 1

Exit status is not a verdict — compare the printed `idle_cpu_pct` against a run
on a worktree at your parent commit, per that runbook's A/B rule.
"""

import argparse
import os
import re
import signal
import subprocess
import sys
import time

REPO = subprocess.run(
    ["git", "rev-parse", "--show-toplevel"], capture_output=True, text=True, check=True
).stdout.strip()

# Both startup paths, plus the torn-console tail: at SMP>1 cores interleave and
# `[herd] Started sshd (pid= 2)` can land split across two lines, so neither
# full marker appears on a perfectly healthy boot. See the trim-fat runbook,
# "Before calling anything a regression" §4.
BOOT_MARKERS = ("Started sshd", "sshd started", "sshd (pid=")


def cpu_seconds(pid):
    """Cumulative CPU time (user+sys, all threads) of `pid`, in seconds."""
    out = subprocess.run(
        ["ps", "-o", "time=", "-p", str(pid)], capture_output=True, text=True
    ).stdout.strip()
    if not out:
        return None
    # macOS ps prints [DD-]HH:MM:SS.ss or MM:SS.ss
    days = 0
    if "-" in out:
        d, out = out.split("-", 1)
        days = int(d)
    parts = [float(p) for p in out.split(":")]
    while len(parts) < 3:
        parts.insert(0, 0.0)
    return days * 86400 + parts[0] * 3600 + parts[1] * 60 + parts[2]


def find_qemu(exclude):
    out = subprocess.run(
        ["pgrep", "-f", "qemu-system-aarch64"], capture_output=True, text=True
    ).stdout.split()
    pids = [int(p) for p in out if int(p) not in exclude]
    return pids[0] if pids else None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--run-cmd", default="overlays/devbox/run-smoltcp.sh",
                    help="command that boots the VM (default: devbox-smoltcp)")
    ap.add_argument("--smp", default=None, help="SMP= override passed to the run command")
    ap.add_argument("--boot-timeout", type=float, default=300.0)
    ap.add_argument("--settle", type=float, default=15.0,
                    help="seconds to wait after the boot marker before sampling")
    ap.add_argument("--window", type=float, default=30.0, help="sampling window, seconds")
    ap.add_argument("--log", default=None, help="boot log path (default: a temp file)")
    ap.add_argument("--keep", action="store_true", help="leave the VM running afterwards")
    args = ap.parse_args()

    log_path = args.log or os.path.join(
        os.environ.get("TMPDIR", "/tmp"), f"idle_cpu_{int(time.time())}.log"
    )

    pre_existing = set()
    out = subprocess.run(
        ["pgrep", "-f", "qemu-system-aarch64"], capture_output=True, text=True
    ).stdout.split()
    if out:
        pre_existing = {int(p) for p in out}
        print(f"WARNING: {len(pre_existing)} qemu already running (pids {sorted(pre_existing)});"
              " they hold the forwarded ports and will be ignored for sampling.",
              file=sys.stderr)

    env = dict(os.environ)
    if args.smp:
        env["SMP"] = str(args.smp)

    logf = open(log_path, "wb")
    proc = subprocess.Popen(
        args.run_cmd, shell=True, cwd=REPO, stdout=logf, stderr=subprocess.STDOUT,
        env=env, start_new_session=True,
    )
    print(f"booting: {args.run_cmd}  (log: {log_path})")

    booted = False
    t0 = time.time()
    while time.time() - t0 < args.boot_timeout:
        if proc.poll() is not None:
            print("ERROR: run command exited before the boot marker", file=sys.stderr)
            break
        try:
            with open(log_path, "rb") as f:
                blob = f.read().decode("utf-8", "replace")
        except FileNotFoundError:
            blob = ""
        if any(m in blob for m in BOOT_MARKERS):
            booted = True
            break
        time.sleep(2)
    boot_wall = time.time() - t0

    qpid = find_qemu(pre_existing)
    result = {"booted": booted, "boot_secs": round(boot_wall, 1), "qemu_pid": qpid}

    if qpid is None:
        print("ERROR: no qemu process to sample", file=sys.stderr)
    else:
        time.sleep(args.settle)
        c0, w0 = cpu_seconds(qpid), time.time()
        time.sleep(args.window)
        c1, w1 = cpu_seconds(qpid), time.time()
        if c0 is None or c1 is None:
            print("ERROR: qemu exited during sampling", file=sys.stderr)
        else:
            result["idle_cpu_pct"] = round(100.0 * (c1 - c0) / (w1 - w0), 1)
            result["sample_secs"] = round(w1 - w0, 1)

    # Host-starvation and spin tripwires, same spirit as the runbook's
    # known-benign table: a high time-jump count invalidates the measurement.
    try:
        with open(log_path, "rb") as f:
            blob = f.read().decode("utf-8", "replace")
        result["time_jumps"] = len(re.findall(r"Time jump detected", blob))
        result["bkl_stuck"] = len(re.findall(r"\[BKL\] stuck", blob))
    except OSError:
        pass

    if not args.keep and qpid:
        try:
            os.killpg(os.getpgid(proc.pid), signal.SIGTERM)
        except (ProcessLookupError, PermissionError):
            pass
        subprocess.run(["kill", str(qpid)], capture_output=True)
        time.sleep(2)
        subprocess.run(["kill", "-9", str(qpid)], capture_output=True)
    logf.close()

    print("=== IDLE CPU ===")
    for k, v in result.items():
        print(f"{k}: {v}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
