#!/usr/bin/env python3
"""Push and run the futex correctness probes against a running guest.

The three probes are the futex family's regression gate, and each answers a
question the other two cannot:

  futexops   op-by-op against Linux semantics (WAKE_OP, REQUEUE, CMP_REQUEUE,
             WAIT_BITSET, the PI family's ENOSYS). Prints PASS/FAIL per probe.
  futexkey   does a futex key leak between address spaces? Deterministic — two
             processes at the same VA, no stress loop, no timing luck. This is
             the one that catches the musl `__tl_lock` collapse, which no
             single-process probe can see.
  futextest  seven phases of real pthread/futex traffic (spawn+join, mutex,
             condvar, barrier, raw park/unpark). Each phase prints "[N] start"
             then "[N] ok" — a missing "ok" names the phase that hung.

They live in `userspace/forktest/c_stress/`; this builds them from source and
pushes them, so what runs in the guest is what is in the tree right now.

Usage:
  scripts/futex_suite.py --port 2322              # build, push, run all three
  scripts/futex_suite.py --port 2322 --no-build   # reuse what is already there

Exit status is 0 only if every probe passed, so it can gate an A/B arm.
"""
import argparse
import base64
import pathlib
import re
import subprocess
import sys

PROBES = ["futexops", "futexkey", "futextest"]
SRC = pathlib.Path(__file__).resolve().parent.parent / "userspace/forktest/c_stress"


def ssh(port, cmd, timeout=1800):
    p = subprocess.run(
        ["ssh", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
         "-o", "LogLevel=ERROR", "-p", str(port), "root@localhost", cmd],
        capture_output=True, timeout=timeout)
    return p.stdout.decode(errors="replace") + p.stderr.decode(errors="replace")


def push(port, path, dest):
    data = base64.b64encode(path.read_bytes())
    subprocess.run(
        ["ssh", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
         "-o", "LogLevel=ERROR", "-p", str(port), "root@localhost",
         f"base64 -d > {dest} && chmod +x {dest}"],
        input=data, capture_output=True, timeout=600)


def verdict(name, out):
    """A probe passed only if it said so. Absence of FAIL is not success — a
    probe that died before printing anything would otherwise read as clean."""
    fails = len(re.findall(r"\bFAIL\b", out))
    passes = len(re.findall(r"\bPASS\b", out))
    if name == "futextest":
        # Phase convention: "[N] <description>: start" must be followed by
        # "[N] ok". The description between the two is not optional in the
        # probe's output, and a regex that assumed it away reported a clean
        # 7-phase run as a failure with "7/0 phases ok".
        started = set(re.findall(r"\[(\d+)\][^\n]*\bstart\b", out))
        ok = set(re.findall(r"\[(\d+)\] ok", out))
        missing = sorted(started - ok, key=int)
        return (not missing and bool(started),
                f"{len(ok)}/{len(started)} phases ok" +
                (f", HUNG/FAILED at phase {missing}" if missing else ""))
    return fails == 0 and passes > 0, f"{passes} PASS, {fails} FAIL"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=2222)
    ap.add_argument("--no-build", action="store_true")
    ap.add_argument("--timeout", type=int, default=900)
    a = ap.parse_args()

    if not a.no_build:
        for n in PROBES:
            subprocess.run(
                ["aarch64-linux-musl-gcc", "-O2", "-static", "-o", str(SRC / n),
                 str(SRC / f"{n}.c")], check=True)
        print(f"built {', '.join(PROBES)}")

    bad = []
    for n in PROBES:
        push(a.port, SRC / n, f"/tmp/{n}")
        out = ssh(a.port, f"/tmp/{n} 2>&1", timeout=a.timeout)
        ok, summary = verdict(n, out)
        print(f"\n===== {n}: {'PASS' if ok else 'FAIL'} ({summary}) =====")
        print(out.strip()[-4000:])
        if not ok:
            bad.append(n)

    print("\n" + ("ALL PROBES PASSED" if not bad else f"FAILED: {', '.join(bad)}"))
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
