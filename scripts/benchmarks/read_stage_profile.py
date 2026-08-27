#!/usr/bin/env python3
"""Drive the `[READPROF]` per-stage `read(2)` profiler and summarise its windows.

Companion to `src/syscall/utils/read_profile.rs` (kernel feature `read-profile`) and to
`read_path_ab.py`, which answers a different question: that script measures how
much a `read(2)` spends *re-resolving its path* by differencing two path depths
in userspace, and cannot see inside a single syscall. This one reads the
kernel's own per-stage accounting.

    scripts/build_readprof.sh          # or: cargo build --release --features read-profile
    INSTANCE=1 MEMORY=2048 SMP=1 DISK=<clone> scripts/cargo_runner.sh <elf> > log &
    scripts/benchmarks/read_stage_profile.py --log log --port 2322 --label baseline

# Why the summary drops windows

Two things make a raw `[READPROF]` window unusable, and both are silent:

* **Mixed request sizes.** Every file read in the system lands in the window,
  so a `cat` running alongside `dd` puts 64 KB reads next to 4 KB ones and every
  per-stage number becomes an average over two workloads. The kernel prints
  `bytes=<mean>/<min>..<max>`; only `min == max` windows are kept.
* **Preemption.** Two or three reads per thousand get descheduled mid-syscall
  for hundreds of microseconds, which is enough to move a stage's *mean* by
  more than the stage costs. The kernel prints a log2-microsecond histogram of
  the whole excursion for exactly this; windows with any sample past
  `--outlier-bucket` are dropped rather than averaged in.

What is left is the cost of an uninterrupted `read(2)`, which is the thing a
change to the read path can move. `--keep-dirty` shows the rejects too.

# The `dd` wall time this prints is not a throughput measurement

The kernel's window dump costs ~55 ms of serial console per 256 reads, inside a
`read(2)`. The `bs=...:` lines below are printed so you can see the workload ran,
not so you can compare them to anything. Per-read wall cost comes from a plain
`--release` build and `userspace/ext2probe/c/read_syscall_cost.c`.

# Reading the output

`exc` is the whole EL0 excursion; `wrap` is what `rust_sync_el0_handler` adds
around `handle_syscall`; `pro_epi` is `handle_syscall`'s own prologue and
epilogue; the named stages are inside `sys_read`. `resid` is `sr` minus the
named stages — it should be near zero, and a large one means the stage list has
a hole in it, not that the time disappeared.
"""

import argparse
import json
import re
import statistics
import subprocess
import sys

STAGES = ["validate", "fd", "bkl", "alloc", "fs", "copy", "pos"]
SPANS = ["exc", "hs", "sr", "wrap", "pro_epi", "resid"]
BUCKETS = ["<1", "1-2", "2-4", "4-8", "8-16", "16-32", "32-64", "64-128", "128-256", "256+"]


def ssh_cmd(port):
    return [
        "ssh", "-o", "StrictHostKeyChecking=no",
        "-o", "UserKnownHostsFile=/dev/null",
        "-o", "LogLevel=ERROR", "-o", "ConnectTimeout=15",
        "-p", str(port), "root@localhost",
    ]


def sh(port, cmd, timeout=1800):
    r = subprocess.run(ssh_cmd(port) + [cmd], capture_output=True, text=True, timeout=timeout)
    # `print_dec` in the guest pads with a NUL, so guest output is not clean text.
    return r.returncode, r.stdout.replace("\x00", "").strip(), r.stderr.strip()


def read_log(path):
    # `-a`/binary: QEMU emits control bytes into the serial log.
    with open(path, "rb") as fh:
        return fh.read().decode("utf8", "replace")


def parse_windows(text):
    """Group `[READPROF]` lines by their `w=` index into one dict per window."""
    windows = {}
    for line in text.splitlines():
        if "[READPROF]" not in line:
            continue
        m = re.search(r"w=(\d+)", line)
        if not m:
            continue
        w = windows.setdefault(int(m.group(1)), {"w": int(m.group(1)), "hist": {}})
        if "n_exc=" in line:
            for k, v in re.findall(r"(n|n_hs|n_exc|freq)=(\d+)", line):
                w[k] = int(v)
            b = re.search(r"bytes=(\d+)/(\d+)\.\.(\d+)", line)
            if b:
                w["bytes_mean"], w["bytes_min"], w["bytes_max"] = (int(x) for x in b.groups())
            c = re.search(r"cal=(\d+)ns", line)
            if c:
                w["cal"] = int(c.group(1))
        elif " mean exc=" in line or " min  exc=" in line:
            kind = "mean" if " mean " in line else "min"
            for k, v in re.findall(r"(exc|hs|sr|wrap|pro_epi|resid)=(\d+)ns", line):
                w[f"{kind}_{k}"] = int(v)
        elif "commit mean=" in line:
            m = re.search(r"commit mean=(\d+)ns min=(\d+)ns", line)
            if m:
                w["mean_commit"], w["min_commit"] = int(m.group(1)), int(m.group(2))
        elif "exc_us" in line:
            for k, v in re.findall(r"([<\d][^:\s]*):(\d+)", line.split("exc_us", 1)[1]):
                w["hist"][k] = int(v)
        else:
            m = re.search(r"w=\d+ (\w+): min=(\d+)ns mean=(\d+)ns", line)
            if m:
                w[f"min_{m.group(1)}"] = int(m.group(2))
                w[f"mean_{m.group(1)}"] = int(m.group(3))
    return [windows[k] for k in sorted(windows)]


def clean(w, outlier_bucket):
    """Is this window one homogeneous, uninterrupted workload? Returns a reason."""
    if w.get("bytes_min") != w.get("bytes_max"):
        return f"mixed sizes {w.get('bytes_min')}..{w.get('bytes_max')}"
    if w.get("n") != w.get("n_exc") or w.get("n") != w.get("n_hs"):
        return f"count mismatch n={w.get('n')} hs={w.get('n_hs')} exc={w.get('n_exc')}"
    idx = BUCKETS.index(outlier_bucket)
    stray = sum(v for k, v in w["hist"].items() if k in BUCKETS and BUCKETS.index(k) >= idx)
    if stray:
        return f"{stray} sample(s) >= {outlier_bucket}us"
    return None


def summarize(windows, outlier_bucket, keep_dirty):
    kept, dropped = [], []
    for w in windows:
        why = clean(w, outlier_bucket)
        (dropped if why else kept).append((w, why))
    if keep_dirty:
        for w, why in dropped:
            print(f"  drop w={w['w']}: {why}")
    by_size = {}
    for w, _ in kept:
        by_size.setdefault(w["bytes_min"], []).append(w)
    return by_size, dropped


def report(by_size, label):
    if not by_size:
        print("no clean windows — run a longer workload or raise --outlier-bucket")
        return
    for size in sorted(by_size):
        ws = by_size[size]
        med = lambda k: int(statistics.median([w[k] for w in ws if k in w]))  # noqa: E731
        print(f"\n== {label}  bs={size}  ({len(ws)} clean window(s), {sum(w['n'] for w in ws)} reads)")
        print(f"   cal={med('cal')}ns/lap   exc: min={med('min_exc')}ns  mean={med('mean_exc')}ns")
        # `min` is the number to read, and `mean` is here only to show how much
        # interference the window carried. Even after the dirty-window filter,
        # sub-threshold stalls inflate every mean — enough that the mean stage
        # table once put `pro_epi` at 572 ns inside a syscall whose whole
        # undisturbed round trip is 440 ns. A stage cannot cost more than the
        # syscall containing it; that contradiction is what the minima resolve.
        total = med("min_exc") or 1
        rows = [("wrap (EL0 handler)", "wrap"), ("pro_epi (handle_syscall)", "pro_epi")]
        rows += [(f"  {s}", s) for s in STAGES] + [("  resid", "resid")]
        print(f"   {'stage':<26} {'min':>7}  {'%of min exc':>11}   {'mean':>8}")
        for name, key in rows:
            mn, mean = med(f"min_{key}"), med(f"mean_{key}")
            print(f"   {name:<26} {mn:>5}ns  {100.0 * mn / total:10.1f}%   {mean:>6}ns")
        named = sum(med(f"min_{s}") for s in STAGES)
        print(f"   {'-- named stages (min sum)':<26} {named:>5}ns  "
              f"{100.0 * named / total:10.1f}%")
        if any("min_commit" in w for w in ws):
            c = med("min_commit")
            print(f"   {'-- of which instrument:':<26} {c:>5}ns of pro_epi is "
                  f"commit()'s own atomics -> real pro_epi ~"
                  f"{med('min_pro_epi') - c}ns")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--log", required=True, help="QEMU serial log of the read-profile kernel")
    ap.add_argument("--port", type=int, default=2322, help="guest SSH port")
    ap.add_argument("--file", default="/shallow.bin", help="warm fixture to read")
    ap.add_argument("--bs", type=int, nargs="+", default=[4096, 8192], help="block sizes to sweep")
    ap.add_argument("--reads", type=int, default=2048, help="reads per block size")
    ap.add_argument("--label", default="run")
    ap.add_argument("--outlier-bucket", default="8-16", choices=BUCKETS,
                    help="drop a window if any read landed in this bucket or above")
    ap.add_argument("--keep-dirty", action="store_true", help="list the dropped windows")
    ap.add_argument("--no-run", action="store_true", help="only parse the log")
    ap.add_argument("--out", help="write the parsed windows here as JSON")
    args = ap.parse_args()

    before = len(parse_windows(read_log(args.log)))
    if not args.no_run:
        sh(args.port, f"cat {args.file} > /dev/null")
        for bs in args.bs:
            rc, out, err = sh(args.port,
                              f"dd if={args.file} of=/dev/null bs={bs} count={args.reads} 2>&1 | tail -1")
            if rc != 0:
                sys.exit(f"dd bs={bs} failed rc={rc} err={err!r}")
            print(f"bs={bs}: {out}")

    windows = parse_windows(read_log(args.log))[before:]
    by_size, dropped = summarize(windows, args.outlier_bucket, args.keep_dirty)
    print(f"\n{len(windows)} window(s), {len(dropped)} dropped as dirty")
    report(by_size, args.label)
    if args.out:
        with open(args.out, "w") as fh:
            json.dump({"label": args.label, "windows": windows}, fh, indent=1)
        print(f"\nwrote {args.out}")


if __name__ == "__main__":
    main()
