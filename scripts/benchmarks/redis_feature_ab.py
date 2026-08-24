#!/usr/bin/env python3
"""A/B kernel feature sets against the round-trip ceiling.

Built to re-open one verdict in particular. `AKUMA_NET_ISSUES.md` §12 recorded
`net-noalloc` — the static RX/TX rings that make transmit **non-blocking** — as
"NEUTRAL, still off". That was measured with `bench_nic_rtt.py`, which is
**serial**, and a serial client never reaches the throughput ceiling: it is
latency-bound by construction, so a fix that removes a *serialized* cost has
nothing to show.

`REDIS_ROUND_TRIP_CEILING.md` §2 measures that serialized cost directly:
`tx_wait` — the `add_notify_wait_pop` busy-spin inside the `NETWORK` lock — is
17.8 us of the 43.7 us per-round-trip budget, i.e. **41 %** of what sets the
ceiling. Removing the spin should therefore be worth ~40 % of throughput at
saturation while remaining invisible to a serial harness. If that is right,
§12's verdict was a harness artifact, not a property of the rings.

Load comes from `rtt_load.py` (blocking sockets, 32 processes) because
`redis-benchmark` livelocks at the ceiling and cannot be trusted here.

Usage:
    scripts/benchmarks/redis_feature_ab.py \
        --arms "base=devbox-smoltcp,no-tests,net-profile" \
               "rings=devbox-smoltcp,no-tests,net-profile,net-noalloc"
"""

import argparse
import json
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import redis_smp_sweep as s  # noqa: E402
import rtt_load  # noqa: E402


def measure(clients, repeats, seconds):
    vals = []
    for _ in range(repeats):
        rps, _ops, errs = rtt_load.run("127.0.0.1", s.REDIS_PORT, clients,
                                       seconds=seconds, warmup=1.5, procs=32)
        if errs:
            print(f"    generator errors: {errs[:2]}")
        if rps:
            vals.append(rps)
        time.sleep(5)
    vals.sort()
    return (vals[len(vals) // 2], vals) if vals else (None, [])


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--arms", nargs="+", required=True,
                    help="name=feature,list pairs")
    ap.add_argument("--clients", default="1,8,32")
    ap.add_argument("--smp", type=int, default=4)
    ap.add_argument("--repeats", type=int, default=3)
    ap.add_argument("--seconds", type=float, default=6.0)
    ap.add_argument("--out", default="logs/redis_why/feature_ab.json")
    a = ap.parse_args()

    clients = [int(c) for c in a.clients.split(",")]
    results = {}

    for spec in a.arms:
        name, feats = spec.split("=", 1)
        print(f"\n{'='*70}\n== {name}   SMP={a.smp}   features={feats}\n{'='*70}")
        logp = Path("logs/redis_why") / f"boot_feat_{name}.log"
        s.kill_existing_qemu()
        s.boot(a.smp, logp, features=feats)
        if not s.wait_for_sshd(logp):
            print(f"  {name}: never came up")
            continue
        if not s.start_redis():
            print(f"  {name}: redis never came up")
            continue
        row = {}
        for c in clients:
            med, vals = measure(c, a.repeats, a.seconds)
            if med is None:
                print(f"  c={c:<4} FAILED")
                continue
            print(f"  c={c:<4} {med:>10,.0f} rps  {1e6/med:>7.1f} us/rt  "
                  f"{['%.0f' % v for v in vals]}")
            row[c] = med
        results[name] = row

    if len(results) >= 2:
        names = list(results)
        base = names[0]
        print(f"\n--- vs {base} ---")
        for n in names[1:]:
            for c in clients:
                b, v = results[base].get(c), results[n].get(c)
                if b and v:
                    print(f"  {n:>10} c={c:<4} {b:>9,.0f} -> {v:>9,.0f}  "
                          f"{(v-b)/b:+7.1%}")

    Path(a.out).write_text(json.dumps(results, indent=2))
    print(f"\nwrote {a.out}")


if __name__ == "__main__":
    main()
