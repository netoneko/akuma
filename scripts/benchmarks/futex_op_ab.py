#!/usr/bin/env python3
"""Drive `futex_op_cost` in a guest, aggregate rounds, and A/B two kernel builds.

Why a driver at all: the probe reports the cheapest pass of one *run*, and this
host's syscall floor drifts within a single boot (measured: the `getpid` control
moved 130 -> 230 ns across three runs of one boot as the tick governor settled).
One run is therefore not a measurement — the median of several rounds is.

**Compare the RATIO column, not the ns columns.** The probe prints each arm's
distance from the `getpid` control in the same process (`floor+N`), which looks
like it should divide the drift out and does not: measured across two boots of
the same kernel, a boot whose floor read 180 ns instead of 130 also showed every
arm's `floor+N` inflated by roughly the same proportion. The drift is
multiplicative — a slower boot makes the whole syscall path slower, not just its
fixed part — so the invariant statistic is `arm / getpid`, and that is what
`--compare` tests. `floor+N` is still printed because it is the easier number to
hold in your head while watching rounds go by.

Usage:
  # baseline, before a change:
  scripts/benchmarks/futex_op_ab.py --port 2322 --rounds 8 --save before.json
  # after rebuilding + rebooting the other arm:
  scripts/benchmarks/futex_op_ab.py --port 2322 --rounds 8 --save after.json \
      --compare before.json

The probe binary must already be in the guest (userspace/build.sh puts it at
/bin/futex_op_cost; userspace/futexprobe/c/build.sh --push-akuma <port> pushes a
rebuilt one into a running VM). See `userspace/futexprobe/c/futex_op_cost.c` for
what each arm measures and `docs/archive/AKUMA_SYSCALL_PERFORMANCE_AUDIT.md`
for the method the probe follows.
"""
import argparse
import json
import re
import statistics
import subprocess
import sys

LINE = re.compile(
    r"^(\w+)\s+(-?\d+) ns\s+\(floor\s*([+-]\d+)\)\s+mean\s+(\d+)\s+worst\s+(\d+)\s+ret=(-?\d+)(.*)$"
)


def ssh(port, cmd, timeout=1800):
    p = subprocess.run(
        ["ssh", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
         "-o", "LogLevel=ERROR", "-p", str(port), "root@localhost", cmd],
        capture_output=True, timeout=timeout)
    return p.stdout.decode(), p.stderr.decode(), p.returncode


def one_round(port, exe, passes, calls):
    out, err, rc = ssh(port, f"{exe} {passes} {calls}")
    arms = {}
    for line in out.splitlines():
        m = LINE.match(line.strip())
        if m:
            name, best, delta, mean, worst, ret, tail = m.groups()
            # An arm that stopped returning what it documents is not a slower
            # arm, it is a different arm. Refuse the whole round rather than
            # average a changed code path into a baseline.
            if tail.strip():
                raise SystemExit(f"arm {name!r} returned {ret}: {tail.strip()}\n{out}")
            arms[name] = {"abs": int(best), "delta": int(delta),
                          "mean": int(mean), "worst": int(worst)}
    if not arms:
        raise SystemExit(f"no arms parsed (rc={rc}):\n{out}\n{err}")
    return arms


def aggregate(rounds):
    names = list(rounds[0])
    out = {}
    for n in names:
        # The drift-invariant statistic: each round's arm cost over the same
        # round's floor. Taken per round and then medianed — medianing the two
        # columns separately and dividing would pair a fast arm with a slow
        # floor from a different round.
        ratios = [r[n]["abs"] / r["getpid"]["abs"] for r in rounds]
        out[n] = {
            "abs_min": min(r[n]["abs"] for r in rounds),
            "abs_med": statistics.median(r[n]["abs"] for r in rounds),
            "delta_min": min(r[n]["delta"] for r in rounds),
            "delta_med": statistics.median(r[n]["delta"] for r in rounds),
            "ratio_med": statistics.median(ratios),
            "ratio_min": min(ratios),
            "n": len(rounds),
        }
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=2222)
    ap.add_argument("--exe", default="/bin/futex_op_cost")
    ap.add_argument("--rounds", type=int, default=8)
    ap.add_argument("--passes", type=int, default=100)
    ap.add_argument("--calls", type=int, default=500)
    ap.add_argument("--save")
    ap.add_argument("--compare", help="baseline JSON from an earlier --save")
    ap.add_argument("--label", default="")
    a = ap.parse_args()

    rounds = []
    for i in range(a.rounds):
        r = one_round(a.port, a.exe, a.passes, a.calls)
        rounds.append(r)
        print(f"round {i + 1}/{a.rounds}: " +
              " ".join(f"{k}={v['abs']}({v['delta']:+d})" for k, v in r.items()),
              flush=True)

    agg = aggregate(rounds)
    print(f"\n{'arm':<12} {'abs min':>8} {'abs med':>8} {'floor+N med':>12} {'x floor':>8}")
    for n, v in agg.items():
        print(f"{n:<12} {v['abs_min']:>8} {v['abs_med']:>8.0f} "
              f"{v['delta_med']:>12.0f} {v['ratio_med']:>8.2f}")

    if a.save:
        json.dump({"label": a.label, "rounds": rounds, "agg": agg},
                  open(a.save, "w"), indent=1)
        print(f"\nsaved {a.save}")

    if a.compare:
        base = json.load(open(a.compare))
        bf = base["agg"]["getpid"]["abs_med"]
        nf = agg["getpid"]["abs_med"]
        print(f"\nA/B vs {a.compare} ({base.get('label') or 'baseline'} -> {a.label or 'this run'})")
        print(f"floor: {bf:.0f} ns -> {nf:.0f} ns "
              f"({'comparable boots' if abs(nf - bf) <= 0.2 * bf else 'DIFFERENT boot conditions — read the x-floor column, not the ns one'})")
        print(f"\n{'arm':<12} {'base x floor':>13} {'new x floor':>12} {'delta':>8}"
              f" {'base ns':>9} {'new ns':>8}")
        for n, v in agg.items():
            b = base["agg"].get(n)
            if not b:
                continue
            print(f"{n:<12} {b['ratio_med']:>13.2f} {v['ratio_med']:>12.2f} "
                  f"{v['ratio_med'] - b['ratio_med']:>+8.2f}"
                  f" {b['delta_med']:>+9.0f} {v['delta_med']:>+8.0f}")
        print(f"\nResolution: `clock_gettime` truncates to 1 us and each pass "
              f"times {a.calls} calls, so this run resolves "
              f"{1000 / a.calls:.1f} ns per call. A difference smaller than that "
              f"is not a measurement.")


if __name__ == "__main__":
    sys.exit(main())
