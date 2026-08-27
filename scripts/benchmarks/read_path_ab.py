#!/usr/bin/env python3
"""Measure what a `read(2)` spends re-resolving its path, in the guest.

The lever: per-fd inode caching (`KernelFile::inode`). `read(2)` used to
re-resolve the fd's path on *every* call — a full `lookup_path_internal` walk
per syscall — so the cost is paid **per syscall and per path component**, not
per byte.

**The measurement is a difference within one boot, not a comparison between
two.** This host's wall clock swings several-fold between sessions and ~10%
between minutes (`crates/akuma-ext2/README.md` § Performance;
`docs/archive/EXT2_WRITEBACK_FOLLOWUP_FIXES.md` §8 — the same probe measured the
same commit ~2x apart hours later), which is the same order as the effect. So
read *the same 8 MB of bytes* through two paths of different depth,
back-to-back, interleaved:

  * `/tmp/readab/a/b/c/d/big.bin` — 5 components
  * `/shallow.bin`                — 1 component

Identical inode work, identical block-cache traffic, identical byte count: the
**only** difference is four extra directory components to walk. Whatever drift
the host has affects both alike, so `deep - shallow` survives it.

  * reading by path  -> deep costs measurably more than shallow
  * reading by inode -> the walk happens once at `open(2)`, so the gap collapses

`bs=1024` (8192 read calls) is the signal; `bs=65536` (128 calls) is the
control — the same bytes with 64x fewer syscalls, where a per-syscall effect
must shrink by roughly that factor or it was never per-syscall.

**Check the host is idle first.** These same arms have measured 20x apart
depending on whether something else was using the CPU
(`docs/archive/EXT2_PER_FD_INODE_READ_PATH.md` § Background). A loaded host does
not just add noise, it changes the answer.

Usage:
    scripts/benchmarks/read_path_ab.py --label with-fd-inode --runs 5
    scripts/benchmarks/read_path_ab.py --sweep          # gap vs syscall count
    scripts/benchmarks/read_path_ab.py --label baseline --runs 5 \
        --out /tmp/baseline.json

Compare two saved runs (optional — each run already carries its own verdict):
    scripts/benchmarks/read_path_ab.py --compare /tmp/baseline.json /tmp/new.json
"""

import argparse
import json
import re
import statistics
import subprocess
import sys

SSH = [
    "ssh",
    "-o", "StrictHostKeyChecking=no",
    "-o", "UserKnownHostsFile=/dev/null",
    "-o", "LogLevel=ERROR",
    "-o", "ConnectTimeout=15",
    "-p", "2222",
    "root@localhost",
]

DEEP_DIR = "/tmp/readab/a/b/c/d"
DEEP = f"{DEEP_DIR}/big.bin"
SHALLOW = "/shallow.bin"
MB = 8


def sh(cmd, timeout=600):
    r = subprocess.run(SSH + [cmd], capture_output=True, text=True, timeout=timeout)
    return r.returncode, r.stdout, r.stderr


def setup():
    """Build both fixtures: the same 8 MB at two path depths."""
    rc, out, err = sh(
        f"mkdir -p {DEEP_DIR} && dd if=/dev/zero of={DEEP} bs=65536 "
        f"count={MB * 16} 2>/dev/null && cp {DEEP} {SHALLOW} && "
        f"ls -l {DEEP} {SHALLOW}"
    )
    if rc != 0 or out.count("8388608") != 2:
        sys.exit(f"fixture setup failed rc={rc} out={out!r} err={err!r}")
    # Read both once so the block cache is warm and equally warm: this measures
    # syscall overhead, not disk.
    sh(f"cat {DEEP} > /dev/null")
    sh(f"cat {SHALLOW} > /dev/null")
    return out.strip().replace("\n", " | ")


def timed_read(path, bs, count):
    """One `dd` pass over `path`; returns milliseconds.

    `dd` reports its own elapsed time to microseconds, so the number is measured
    inside the guest: SSH round-trip, host scheduling and process spawn stay out
    of it. (`date +%s%N` is not an option — busybox `date` has no `%N` and
    silently emits whole seconds.)
    """
    cmd = f"dd if={path} of=/dev/null bs={bs} count={count} 2>&1 | tail -1"
    for _ in range(3):
        rc, out, err = sh(cmd)
        if rc == 0:
            break
    else:
        # A dropped SSH connection is not a measurement; retrying beats
        # discarding the whole arm (rc=255 killed a five-run arm once).
        sys.exit(f"read pass failed rc={rc} err={err!r}")
    m = re.search(r"copied,\s*([0-9.]+)\s*seconds", out)
    if not m:
        sys.exit(f"unparseable dd output: {out!r}")
    return round(float(m.group(1)) * 1000)


def summarize(name, samples):
    return {
        "name": name,
        "samples": samples,
        "median": statistics.median(samples),
        "min": min(samples),
        "max": max(samples),
    }


def walk_cost(result, arm, reads):
    """`deep - shallow` per read, in microseconds — the whole point of the run."""
    d, s = result[arm]["deep"], result[arm]["shallow"]
    gap_ms = d["median"] - s["median"]
    return gap_ms, round(gap_ms * 1000 / reads)


def report(result):
    print(f"\n=== {result['label']} ===")
    for arm, reads in (("small", MB * 1024), ("large", MB * 16)):
        print(f"  {result[arm]['name']}:")
        for depth in ("deep", "shallow"):
            s = result[arm][depth]
            print(
                f"    {depth:<8} median={s['median']:>7} ms  "
                f"range={s['min']}-{s['max']} ms  n={len(s['samples'])}"
            )
        gap_ms, per_read_us = walk_cost(result, arm, reads)
        d, sh_ = result[arm]["deep"], result[arm]["shallow"]
        disjoint = d["min"] > sh_["max"]
        print(
            f"    -> 4 extra components cost {gap_ms} ms over {reads} reads "
            f"= {per_read_us} us/read  "
            f"({'DISJOINT' if disjoint else 'ranges overlap'})"
        )
    print(
        "\n  Reading by path: the small arm's gap is real and the large arm's is\n"
        "  ~64x smaller. Reading by inode: both gaps collapse toward zero, because\n"
        "  the walk happens once at open(2) instead of once per read(2)."
    )


def compare(a, b):
    """Optional cross-boot view. The within-arm gap is the primary result."""
    print(f"\n=== {a['label']}  ->  {b['label']} ===")
    for arm, reads in (("small", MB * 1024), ("large", MB * 16)):
        ga, ua = walk_cost(a, arm, reads)
        gb, ub = walk_cost(b, arm, reads)
        print(f"  {a[arm]['name']}:")
        print(f"    path-walk cost per read: {ua} us -> {ub} us  (gap {ga} -> {gb} ms)")
        for depth in ("deep", "shallow"):
            x, y = a[arm][depth], b[arm][depth]
            print(
                f"    {depth:<8} {x['median']:>7} -> {y['median']:>7} ms   "
                f"ranges {x['min']}-{x['max']} vs {y['min']}-{y['max']}"
            )


def sweep(runs):
    """The depth gap at four block sizes: same 8 MB, 64x range of syscall counts.

    A per-syscall cost tracks the call count; a per-byte one does not move at
    all. Reading by path, the gap falls with the call count; reading by inode it
    is gone at every size.
    """
    total = MB * 1024 * 1024
    print(f"fixture: {setup()}")
    print(
        f"\n{'block size':>10} {'read() calls':>13} {'deep ms':>9} "
        f"{'shallow ms':>11} {'gap ms':>8} {'us/read':>9}"
    )
    for bs in (1024, 4096, 16384, 65536):
        count = total // bs
        d = statistics.median(timed_read(DEEP, bs, count) for _ in range(runs))
        sh_ = statistics.median(timed_read(SHALLOW, bs, count) for _ in range(runs))
        print(
            f"{bs:>10} {count:>13} {d:>9.1f} {sh_:>11.1f} "
            f"{d - sh_:>8.1f} {(d - sh_) * 1000 / count:>9.1f}"
        )


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--label", default="arm")
    ap.add_argument("--runs", type=int, default=5)
    ap.add_argument("--out")
    ap.add_argument("--compare", nargs=2, metavar=("BEFORE", "AFTER"))
    ap.add_argument(
        "--sweep",
        action="store_true",
        help="report the depth gap at four block sizes instead of two",
    )
    args = ap.parse_args()

    if args.compare:
        with open(args.compare[0]) as f:
            before = json.load(f)
        with open(args.compare[1]) as f:
            after = json.load(f)
        compare(before, after)
        return

    if args.sweep:
        sweep(max(args.runs, 1))
        return

    print(f"fixture: {setup()}")
    samples = {("small", "deep"): [], ("small", "shallow"): [],
               ("large", "deep"): [], ("large", "shallow"): []}
    for i in range(args.runs):
        # Interleaved, not blocked: host speed drifting mid-run then hits deep
        # and shallow alike instead of landing entirely on one of them, which is
        # exactly what the difference is here to survive.
        for arm, bs, count in (("small", 1024, MB * 1024), ("large", 65536, MB * 16)):
            for depth, path in (("deep", DEEP), ("shallow", SHALLOW)):
                samples[(arm, depth)].append(timed_read(path, bs, count))
        print(
            f"  run {i}: "
            + "  ".join(
                f"{a}/{d}={samples[(a, d)][-1]}ms"
                for a in ("small", "large")
                for d in ("deep", "shallow")
            )
        )

    result = {"label": args.label}
    for arm, reads, unit in (("small", MB * 1024, "1 KB"), ("large", MB * 16, "64 KB")):
        result[arm] = {
            "name": f"8 MB in {reads} x {unit} reads",
            "deep": summarize("deep", samples[(arm, "deep")]),
            "shallow": summarize("shallow", samples[(arm, "shallow")]),
        }
    report(result)
    if args.out:
        with open(args.out, "w") as f:
            json.dump(result, f, indent=2)
        print(f"\nwrote {args.out}")


if __name__ == "__main__":
    main()
