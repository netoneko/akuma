#!/usr/bin/env python3
"""A/B the one cell the delayed-ACK change might regress: 64 KB SET.

Everything else about `set_ack_delay(Some(10ms))` is a win — the round-trip
ceiling goes 14,085 -> 20,202 rps because the bare ACK stops costing a second
`add_notify_wait_pop` spin. The open question is the case the original comment
named: receive-heavy bulk, where a delayed ACK could in principle stall the
sender waiting for a window update.

A single run showed 64 KB SET down 19%, which is inside the run-to-run spread
these cells have. This runs the one contested cell with repeats on both
kernels, in one session, alternating arms so session drift (§11.7: the same
build gave 1,108 and 1,040 req/s hours apart) cannot be mistaken for the
effect.

The baseline arm is the `main` worktree, which carries `ack_delay=None`
unchanged — so no rebuild of the working tree is needed to get a clean control.

Usage:  scripts/benchmarks/redis_bulk_ab.py --repeats 3
"""

import argparse
import re
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import redis_smp_sweep as s  # noqa: E402

MAIN_WT = "/private/tmp/akuma_main_wt"


def one_cell(size, n, clients, test):
    p = subprocess.run(
        ["redis-benchmark", "-h", "127.0.0.1", "-p", str(s.REDIS_PORT),
         "-n", str(n), "-c", str(clients), "-d", str(size), "-P", "1",
         "-t", test, "--csv"],
        capture_output=True, text=True, timeout=600,
    )
    m = re.search(r'"[^"]+","([0-9.]+)"', p.stdout)
    return float(m.group(1)) if m else None


def arm(label, tree, repeats, size, n, clients, tests):
    logp = Path("logs/redis_why") / f"boot_bulkab_{label}.log"
    s.kill_existing_qemu()
    s.boot(4, logp, tree=tree)
    if not s.wait_for_sshd(logp):
        print(f"  {label}: never came up")
        return {}
    if not s.start_redis():
        print(f"  {label}: redis never came up")
        return {}
    out = {}
    for test in tests:
        vals = []
        for _ in range(repeats):
            v = one_cell(size, n, clients, test)
            if v:
                vals.append(v)
            time.sleep(10)
        if vals:
            vals.sort()
            med = vals[len(vals) // 2]
            out[test] = (med, vals)
            spread = (vals[-1] - vals[0]) / med
            print(f"  {label:22} {test.upper():4} d={size} "
                  f"median {med:>9,.1f} ops/s  {med*size/1e6:>6.1f} MB/s  "
                  f"spread {spread:>5.1%}  {['%.0f' % v for v in vals]}")
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--repeats", type=int, default=3)
    ap.add_argument("--size", type=int, default=65536)
    ap.add_argument("--requests", type=int, default=20000)
    ap.add_argument("--clients", type=int, default=8)
    ap.add_argument("--tests", default="set,get")
    a = ap.parse_args()
    tests = a.tests.split(",")

    print(f"64 KB bulk A/B — {a.repeats} repeats, -c {a.clients}, -n {a.requests}\n")
    fixed = arm("ackdelay-10ms", None, a.repeats, a.size, a.requests, a.clients, tests)
    base = arm("baseline-None", MAIN_WT, a.repeats, a.size, a.requests, a.clients, tests)

    print("\n--- verdict ---")
    for t in tests:
        if t in fixed and t in base:
            f, b = fixed[t][0], base[t][0]
            fs = (fixed[t][1][-1] - fixed[t][1][0]) / f
            bs = (base[t][1][-1] - base[t][1][0]) / b
            noise = max(fs, bs)
            delta = (f - b) / b
            verdict = "inside spread" if abs(delta) <= noise else \
                      ("delayed ACK WINS" if delta > 0 else "delayed ACK LOSES")
            print(f"  {t.upper():4} d={a.size}: {b:,.0f} -> {f:,.0f} "
                  f"({delta:+.1%}, worst spread {noise:.1%}) -> {verdict}")


if __name__ == "__main__":
    main()
