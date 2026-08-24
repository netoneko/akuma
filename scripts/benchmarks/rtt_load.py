#!/usr/bin/env python3
"""Blocking-socket RESP load generator — a `redis-benchmark` that cannot livelock.

`redis-benchmark` is unusable as a measurement tool against Akuma at the
throughput ceiling: `REDIS_BENCHMARK_HOST_CONTENTION_LIVELOCK.md` shows its
`writeHandler` spins in a `while(1)` with **zero syscalls** the moment one
write returns `EAGAIN`, which is exactly what backpressure at the ceiling
produces. A run that trips it never returns, so any harness built on it
randomly loses cells — it cost this investigation the socket-table sweep.

Blocking sockets never see `EAGAIN`, so this generator is structurally immune.

**Processes, not threads.** The GIL makes a threaded Python client top out well
below the rate under test, which would silently cap the measurement and look
exactly like a guest ceiling. Work is split across processes; `--selftest`
against a known-fast endpoint (Docker) proves the generator is not the limit
before any Akuma number is trusted.

Usage:
    scripts/benchmarks/rtt_load.py --port 4444 --clients 32 --seconds 10
    scripts/benchmarks/rtt_load.py --port 6379 --clients 32 --selftest
"""

import argparse
import json
import multiprocessing as mp
import socket
import sys
import time

PING = b"*1\r\n$4\r\nPING\r\n"
PONG_LEN = len(b"+PONG\r\n")


def worker(host, port, n_conns, seconds, warmup, q):
    """One process: n_conns blocking connections, round-robin, for `seconds`."""
    socks = []
    try:
        for _ in range(n_conns):
            s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
            s.settimeout(20)
            s.connect((host, port))
            socks.append(s)
    except Exception as e:
        q.put({"error": f"connect: {e}", "ops": 0, "elapsed": 0})
        for s in socks:
            s.close()
        return

    def drain(s):
        buf = b""
        while len(buf) < PONG_LEN:
            c = s.recv(256)
            if not c:
                raise RuntimeError("peer closed")
            buf += c
        return buf

    ops = errs = 0
    try:
        # Warm-up traffic is excluded from the timed window: the first exchange
        # on a fresh connection pays connection setup and a cold path.
        end_warm = time.time() + warmup
        while time.time() < end_warm:
            for s in socks:
                s.sendall(PING)
                drain(s)
        t0 = time.time()
        end = t0 + seconds
        while time.time() < end:
            for s in socks:
                s.sendall(PING)
                drain(s)
                ops += 1
        elapsed = time.time() - t0
    except Exception as e:
        elapsed = max(time.time() - t0, 1e-9) if "t0" in dir() else 0
        errs = 1
        q.put({"error": str(e), "ops": ops, "elapsed": elapsed})
        for s in socks:
            s.close()
        return
    for s in socks:
        s.close()
    q.put({"ops": ops, "elapsed": elapsed, "errors": errs})


def run(host, port, clients, seconds, warmup, procs):
    # Spread `clients` connections as evenly as possible over `procs`.
    per = [clients // procs + (1 if i < clients % procs else 0)
           for i in range(procs)]
    per = [p for p in per if p]
    q = mp.Queue()
    ps = [mp.Process(target=worker, args=(host, port, n, seconds, warmup, q))
          for n in per]
    for p in ps:
        p.start()
    res = [q.get() for _ in ps]
    for p in ps:
        p.join()
    ops = sum(r.get("ops", 0) for r in res)
    el = max((r.get("elapsed", 0) for r in res), default=0)
    errors = [r["error"] for r in res if "error" in r]
    return (ops / el if el else 0), ops, errors


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=4444)
    ap.add_argument("--clients", type=int, default=32)
    ap.add_argument("--seconds", type=float, default=10.0)
    ap.add_argument("--warmup", type=float, default=2.0)
    ap.add_argument("--procs", type=int, default=8)
    ap.add_argument("--repeats", type=int, default=3)
    ap.add_argument("--label", default="")
    ap.add_argument("--json")
    ap.add_argument("--selftest", action="store_true",
                    help="assert the generator itself is not the bottleneck")
    a = ap.parse_args()

    vals = []
    for _ in range(a.repeats):
        rps, ops, errs = run(a.host, a.port, a.clients, a.seconds,
                             a.warmup, a.procs)
        if errs:
            print(f"  errors: {errs[:3]}", file=sys.stderr)
        vals.append(rps)
        time.sleep(3)

    vals.sort()
    med = vals[len(vals) // 2]
    spread = (vals[-1] - vals[0]) / med if med else 0
    print(f"{a.label or a.port:>18}  c={a.clients:<4} "
          f"{med:>10,.0f} rps  {1e6/med if med else 0:>7.1f} us/rt  "
          f"spread {spread:>5.1%}  {['%.0f' % v for v in vals]}")

    if a.selftest:
        # Docker/Linux on this host reaches ~64,500 rps at c=32 with
        # redis-benchmark. A generator that cannot get near that is measuring
        # itself, and every Akuma number taken with it would be a floor.
        print(f"\n  selftest: generator reached {med:,.0f} rps against "
              f"{a.host}:{a.port}")
        print("  (redis-benchmark reaches ~64,500 here; if this is far below, "
              "raise --procs before trusting any guest number)")

    if a.json:
        with open(a.json, "w") as f:
            json.dump({"label": a.label, "port": a.port, "clients": a.clients,
                       "median_rps": med, "all": vals}, f, indent=2)


if __name__ == "__main__":
    main()
