#!/usr/bin/env python3
"""Round-trip concurrency sweep for a Redis endpoint.

Answers one question that `bench_redis.py` cannot: **is the endpoint's
throughput limited by per-round-trip latency, or by a serially-drained
resource?**

A stack that is merely *slow* scales with concurrency until it saturates
something: 1 client at 200 us/round-trip gives 5,000 rps, 20 clients give
close to 100,000. A stack that services round trips *in series* is flat —
20 clients get the same total as 1, they just each wait 20x longer.

`BENCHMARK_PERFORMANCE_ATTEMPT_0.md` §4 established flatness for the
in-guest arm (10/16/20/32 clients all ~3,000 ops/s) but never swept the
forwarded arm, which is the arm §5 calls "the honest measure of the
server". This sweeps both, plus a Docker control on the same host, so the
three shapes can be compared directly.

Reports ops/s and the derived **per-client round-trip latency**
(clients / ops_per_sec), which is the number that stays constant when a
path is latency-bound and grows linearly when it is serialized.

Usage:
    scripts/benchmarks/redis_conc_sweep.py --port 4444 --label akuma-smp4
    scripts/benchmarks/redis_conc_sweep.py --port 6379 --label docker
    scripts/benchmarks/redis_conc_sweep.py --port 4444 --clients 1,2,4,8,16,32
"""

import argparse
import json
import re
import shutil
import subprocess
import sys
import time

# Kept small on purpose. The point of this sweep is the *shape* across client
# counts, not a headline number, and REDIS_BENCHMARK_HOST_CONTENTION_LIVELOCK.md
# documents a hang whose trigger correlates with long runs under host load.
# Short runs per cell keep the whole sweep inside a couple of minutes.
DEFAULT_REQUESTS = 20000
DEFAULT_CLIENTS = "1,2,4,8,16,32"
# PING_INLINE is the smallest possible round trip: no key, no value, no server
# work. Anything above the floor it measures is transport, not Redis.
DEFAULT_TEST = "ping"


def host_load():
    """Top CPU consumers, printed before every sweep.

    ATTEMPT_0 §10: both near-misses in that session were orphaned load the
    operator did not know about. Print it, do not just check it.
    """
    out = subprocess.run(
        ["ps", "-Ao", "pid,pcpu,comm", "-r"], capture_output=True, text=True
    ).stdout.splitlines()
    return [ln.strip() for ln in out[1:6]]


def run_cell(port, clients, requests, test, size, timeout):
    """One redis-benchmark invocation. Returns ops/s, or None on failure."""
    cmd = [
        "redis-benchmark",
        "-h", "127.0.0.1",
        "-p", str(port),
        "-n", str(requests),
        "-c", str(clients),
        "-d", str(size),
        "-P", "1",
        "-t", test,
        "--csv",
    ]
    t0 = time.time()
    try:
        p = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        return None, timeout, "timeout"
    wall = time.time() - t0

    # redis-benchmark exits 0 after printing this and simply omits the row
    # (ATTEMPT_0 §7). Trusting the exit status records a clean run with
    # missing cells.
    if "No file descriptors available" in (p.stdout + p.stderr):
        return None, wall, "fd-exhausted"

    m = re.search(r'"[^"]+","([0-9.]+)"', p.stdout)
    if not m:
        return None, wall, f"unparsed rc={p.returncode}"
    return float(m.group(1)), wall, "ok"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, required=True)
    ap.add_argument("--label", required=True)
    ap.add_argument("--clients", default=DEFAULT_CLIENTS)
    ap.add_argument("--requests", type=int, default=DEFAULT_REQUESTS)
    ap.add_argument("--test", default=DEFAULT_TEST)
    ap.add_argument("--size", type=int, default=64)
    ap.add_argument("--repeats", type=int, default=3)
    # ATTEMPT_0 §7: Akuma's socket budget is not a client count but how
    # recently the last run ended. -c 20 fails without a gap, passes with one.
    ap.add_argument("--cooldown", type=float, default=8.0)
    ap.add_argument("--timeout", type=float, default=180.0)
    ap.add_argument("--out")
    args = ap.parse_args()

    if not shutil.which("redis-benchmark"):
        sys.exit("redis-benchmark not on PATH")

    print(f"### {args.label} :{args.port}  test={args.test} "
          f"n={args.requests} d={args.size} P=1 repeats={args.repeats}")
    print("host load before sweep:")
    for ln in host_load():
        print(f"  {ln}")
    print()
    print(f"{'clients':>7} {'ops/s':>10} {'us/rt/client':>13} {'spread':>7}  note")

    rows = []
    for clients in [int(c) for c in args.clients.split(",")]:
        vals, notes = [], []
        for _ in range(args.repeats):
            ops, wall, note = run_cell(
                args.port, clients, args.requests, args.test, args.size,
                args.timeout,
            )
            if ops is not None:
                vals.append(ops)
            notes.append(note)
            time.sleep(args.cooldown)

        if not vals:
            print(f"{clients:>7} {'-':>10} {'-':>13} {'-':>7}  {notes}")
            rows.append({"clients": clients, "ops": None, "notes": notes})
            continue

        vals.sort()
        med = vals[len(vals) // 2]
        spread = (vals[-1] - vals[0]) / med if med else 0.0
        # Per-client round-trip latency. Flat across the sweep => latency-bound.
        # Rising linearly => the path is servicing round trips in series.
        us_rt = clients / med * 1e6
        print(f"{clients:>7} {med:>10,.0f} {us_rt:>13,.1f} {spread:>6.1%}  "
              f"{'' if all(n == 'ok' for n in notes) else notes}")
        rows.append({
            "clients": clients, "ops": med, "us_per_rt_per_client": us_rt,
            "spread": spread, "all": vals, "notes": notes,
        })

    if args.out:
        with open(args.out, "w") as f:
            json.dump({"label": args.label, "port": args.port,
                       "test": args.test, "requests": args.requests,
                       "rows": rows}, f, indent=2)
        print(f"\nwrote {args.out}")


if __name__ == "__main__":
    main()
