#!/usr/bin/env python3
"""Workload-restricted BKL attribution — the §17.2 view.

`analyze.py`'s default is whole-boot cumulative, which dilutes a regimen's numbers
with service bringup and idle/teardown windows (docs/archive/BKL_VFS_CARVE_OUT.md
§17.2). This tool sums `[BKLPROF]` per-tag spins over ONLY the 10 s windows that
fall inside the workload, and reports each tag's share of that total.

Window selection, in order of preference:
  1. `--auto` (recommended) — derive the interval from the regimen's own footprint
     in the serial log: the first `execve` of `job.sh`/`curl` to the last `execve`
     of a regimen command, rounded outward to 10 s window boundaries. Reproducible
     and identical across the two sides of an A/B, which markers are not (whether
     the sshd echoes a command to the console varies by boot);
  2. explicit `--from/--to` guest uptimes in seconds;
  3. positional — between `BKLMARK_START` and `BKLMARK_DONE` in the serial log.

Read with errors="replace" and matched by substring/regex per line, because cores
byte-splice each other's serial writes — a spliced line is simply dropped rather
than corrupting the totals.
"""
import argparse
import collections
import re
import sys

WINDOW_RE = re.compile(r"\[BKLPROF\] w(\d+) t=(\d+)s spins=(\d+) attributed=(\d+)")
TAG_RE = re.compile(r"\[BKLPROF\]\s+(\S+) tag=(\d+) [\d.]+% spins=(\d+)")
# `[T<sec>.<frac>] [syscall] execve(path="…", args=[…])` — the regimen's footprint.
EXECVE_RE = re.compile(r"\[T(\d+)\.\d+\] \[syscall\] execve\(path=\"([^\"]*)\", args=\[([^\]]*)\]")
# Commands only the regimen runs (job.sh and the four phases). `curl` also appears
# during apk bootstrap on some boots, which is why `job.sh` anchors the start.
REGIMEN_CMDS = ("job.sh", "curl", "sha256sum", "cp", "rm")
DUMP_INTERVAL_S = 10


def auto_window(text):
    """Guest-uptime interval the regimen occupied, rounded out to window boundaries."""
    times = []
    start = None
    for m in EXECVE_RE.finditer(text):
        t, path, args = int(m.group(1)), m.group(2), m.group(3)
        blob = path + " " + args
        if "job.sh" in blob and start is None:
            start = t
        if start is not None and any(c in blob for c in REGIMEN_CMDS):
            times.append(t)
    if start is None or not times:
        return None
    # A window printed at t=T covers (T-10, T], so include the window that contains
    # the first execve and the one that contains the last.
    lo = (start // DUMP_INTERVAL_S) * DUMP_INTERVAL_S + DUMP_INTERVAL_S
    hi = (max(times) // DUMP_INTERVAL_S) * DUMP_INTERVAL_S + DUMP_INTERVAL_S
    return lo, hi, start, max(times)


def slice_by_markers(text):
    """Byte range between the first START marker and the last DONE marker."""
    start = text.find("BKLMARK_START")
    end = text.rfind("BKLMARK_DONE")
    if start < 0:
        return None
    # Everything after the marker if the run never wrote a DONE (crash / still live).
    return text[start : end if end > start else len(text)]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("log")
    ap.add_argument("--from", dest="t_from", type=int, default=None,
                    help="guest uptime (s) of the first workload window")
    ap.add_argument("--to", dest="t_to", type=int, default=None,
                    help="guest uptime (s) of the last workload window")
    ap.add_argument("--auto", action="store_true",
                    help="derive the interval from the regimen's execve footprint")
    ap.add_argument("--top", type=int, default=12)
    args = ap.parse_args()

    text = open(args.log, errors="replace").read()

    if args.auto:
        w = auto_window(text)
        if w is None:
            sys.exit("--auto found no regimen execve trace in this log")
        lo, hi, first, last = w
        region = text
        how = (f"auto: regimen execve T={first}..{last}s "
               f"-> windows t={lo}..{hi}s")
    elif args.t_from is not None or args.t_to is not None:
        lo = args.t_from if args.t_from is not None else 0
        hi = args.t_to if args.t_to is not None else 10**9
        region, how = text, f"uptime window t={lo}..{hi}s"
    else:
        region = slice_by_markers(text)
        if region is None:
            sys.exit("no BKLMARK_START in log; pass --from/--to instead")
        lo, hi = 0, 10**9
        how = "between BKLMARK_START and BKLMARK_DONE"

    per_tag = collections.Counter()
    windows = []
    total_spins = 0          # every contended spin in the region
    attributed_spins = 0     # the part the profiler could attribute to a tag
    keep = True
    for line in region.splitlines():
        m = WINDOW_RE.search(line)
        if m:
            t = int(m.group(2))
            keep = lo <= t <= hi
            if keep:
                windows.append((int(m.group(1)), t, int(m.group(3)), int(m.group(4))))
                total_spins += int(m.group(3))
                attributed_spins += int(m.group(4))
            continue
        m = TAG_RE.search(line)
        if m and keep:
            per_tag[(m.group(1), int(m.group(2)))] += int(m.group(3))

    print(f"== {args.log}")
    print(f"   selection: {how}")
    if not windows:
        sys.exit("   no [BKLPROF] windows in the selected region")
    print(f"   windows: {len(windows)}  (w{windows[0][0]} t={windows[0][1]}s "
          f".. w{windows[-1][0]} t={windows[-1][1]}s)")
    print(f"   total contended spins: {total_spins}   attributed: {attributed_spins}")

    # Shares are of the ATTRIBUTED total: that is what the per-tag lines sum to, and
    # it is the number the campaign's tables have always quoted.
    denom = sum(per_tag.values())
    print(f"   per-tag sum: {denom}")
    print(f"\n   {'share':>7}  {'holder':16s} {'tag':>4}  spins")
    for (label, tag), spins in per_tag.most_common(args.top):
        pct = 100.0 * spins / denom if denom else 0.0
        print(f"   {pct:6.1f}%  {label:16s} {tag:>4}  {spins}")

    # Stability inside the workload only — whole-boot counts include teardown noise.
    print()
    for needle in ("[BKL] stuck", "PANIC", "WILD", "SPURIOUS", "stale dropped-window",
                   "RECOVERED", "No available user threads",
                   "Preemption disabled for"):
        print(f"   {region.count(needle):6d}  {needle}")


if __name__ == "__main__":
    main()
