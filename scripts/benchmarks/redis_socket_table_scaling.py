#!/usr/bin/env python3
"""Does the throughput ceiling track the SIZE of the socket table?

`REDIS_ROUND_TRIP_CEILING.md` §2 says throughput is
`1 / (us in smoltcp_net::poll() per round trip)`, and that ~50 % of that budget
is `iface.poll()` walking the whole `SocketSet` on every call. If that reading
is right, the ceiling is a function of how many sockets are in the table —
including ones doing nothing.

That is directly testable **without touching the kernel**, because a listener
in Akuma is not one socket: `crates/akuma-net/src/socket.rs:1047` makes it a
pool of `MAX_BACKLOG` (32, with `many-sessions`) pre-`listen()`ed smoltcp
sockets. So every extra listening port adds 32 entries that `iface.poll()` must
walk and that never have anything to say.

Method: measure the ceiling, add N idle listeners, measure again. Idle
listeners generate no traffic, consume no CPU, and answer no requests — under
any hypothesis *except* the table-walk one they should cost nothing at all.

Prediction, from the §2 budget (ackdelay kernel: 43.7 us/rt total, of which
21.6 us is the smoltcp walk over `S` sockets):

    us_per_rt(S') = (43.7 - 21.6) + 21.6 * S'/S
    ceiling(S')   = 1e6 / us_per_rt(S')

The script prints predicted vs measured for each step, so the hypothesis can
fail visibly rather than being confirmed by eye.

Usage:  scripts/benchmarks/redis_socket_table_scaling.py --listeners 0,4,8
"""

import argparse
import json
import re
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import redis_smp_sweep as s  # noqa: E402
import rtt_load  # noqa: E402

# Ports for the idle listeners. Well away from anything forwarded or in use.
BASE_PORT = 39000


def sockets_live(logpath):
    """Last `sockets=N/CAP` the guest printed."""
    txt = Path(logpath).read_text(errors="replace")
    m = re.findall(r"sockets=(\d+)/(\d+)", txt)
    return (int(m[-1][0]), int(m[-1][1])) if m else (None, None)


def measure(clients, requests, repeats):
    """Median rps at one client count, via the blocking-socket generator.

    NOT `redis-benchmark`: at the ceiling its `writeHandler` livelocks on a
    single `EAGAIN` and never returns
    (`REDIS_BENCHMARK_HOST_CONTENTION_LIVELOCK.md`). It killed the 4-listener
    cell of this very experiment on the first attempt. `rtt_load.py` uses
    blocking sockets, which never see `EAGAIN`.

    `procs=32` because the generator is otherwise the bottleneck: against
    Docker it reads 37.6k at 8 processes and 67.1k at 32, and the true value
    is ~64.5k. Under-provisioning it would cap the guest measurement and look
    exactly like a guest ceiling.
    """
    del requests  # this generator is time-bounded, not request-bounded
    vals = []
    for _ in range(repeats):
        rps, _ops, errs = rtt_load.run("127.0.0.1", s.REDIS_PORT, clients,
                                       seconds=6.0, warmup=1.5, procs=32)
        if errs:
            print(f"    generator errors: {errs[:2]}")
        if rps:
            vals.append(rps)
        time.sleep(5)
    vals.sort()
    return (vals[len(vals) // 2], vals) if vals else (None, [])


def spawn_listeners(n):
    """Start n idle listening ports in the guest.

    `nc -l -p P` calls listen(2), which is all that is needed: the pool is
    built by listen, not by any connection arriving. Redirected from /dev/null
    and backgrounded so the ssh channel closes and nothing holds output.
    """
    if n == 0:
        return
    cmd = "; ".join(
        f"(nc -l -p {BASE_PORT + i} </dev/null >/dev/null 2>&1 &)"
        for i in range(n)
    )
    s.gssh(cmd)
    time.sleep(3)


def kill_listeners():
    s.gssh("killall nc 2>/dev/null; true")
    time.sleep(2)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--listeners", default="0,4,8")
    ap.add_argument("--clients", type=int, default=32)
    ap.add_argument("--requests", type=int, default=10000)
    ap.add_argument("--repeats", type=int, default=3)
    ap.add_argument("--out", default="logs/redis_why/socket_table_scaling.json")
    a = ap.parse_args()

    logpath = Path("logs/redis_why/boot_socktable.log")
    s.kill_existing_qemu()
    s.boot(4, logpath)
    if not s.wait_for_sshd(logpath):
        sys.exit("guest never came up")
    if not s.start_redis():
        sys.exit("redis never came up")

    steps = [int(x) for x in a.listeners.split(",")]
    rows = []
    base_us = base_sockets = None

    print(f"\n{'listeners':>10}{'sockets':>9}{'ops/s':>10}{'us/rt':>8}"
          f"{'predicted':>11}{'error':>8}")
    for n in steps:
        kill_listeners()
        spawn_listeners(n)
        time.sleep(3)
        live, cap = sockets_live(logpath)
        ops, vals = measure(a.clients, a.requests, a.repeats)
        if ops is None:
            print(f"{n:>10}{str(live):>9}    FAILED")
            continue
        us = 1e6 / ops
        # First step is the reference the prediction is built from.
        if base_us is None:
            base_us, base_sockets = us, live
            pred, err = us, 0.0
        else:
            # 21.6/43.7 of the ackdelay budget is the table walk; scale only
            # that part with the table size, hold the rest fixed.
            walk_frac = 21.6 / 43.7
            fixed = base_us * (1 - walk_frac)
            walk = base_us * walk_frac * (live / base_sockets)
            pred = fixed + walk
            err = (us - pred) / pred
        print(f"{n:>10}{live:>9}{ops:>10,.0f}{us:>8.1f}{pred:>11.1f}{err:>+8.1%}")
        rows.append({"listeners": n, "sockets": live, "cap": cap,
                     "ops": ops, "us_per_rt": us, "predicted_us": pred,
                     "all": vals})

    kill_listeners()
    Path(a.out).write_text(json.dumps({"clients": a.clients, "rows": rows}, indent=2))
    print(f"\nwrote {a.out}")
    if len(rows) >= 2:
        d_s = rows[-1]["sockets"] - rows[0]["sockets"]
        d_o = (rows[-1]["ops"] - rows[0]["ops"]) / rows[0]["ops"]
        print(f"\n{d_s:+d} idle sockets (pure listener pools, zero traffic) "
              f"moved throughput {d_o:+.1%}")


if __name__ == "__main__":
    main()
