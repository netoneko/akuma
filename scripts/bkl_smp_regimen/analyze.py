#!/usr/bin/env python3
"""Summarize a devbox kernel log from the BKL SMP regimen.

Two halves, matching the campaign's two questions:
  stability  — how many long BKL holds / recoveries / hard failures occurred
  attribution — which excursion the BKL *holder* was in while peers spun
                (only present in a `bkl-profile` build)

The log is read with errors="replace": concurrent cores interleave their serial
writes, so a fraction of lines are byte-spliced. Counting substrings tolerates
that; regexes anchored to whole lines do not, which is why the signal counts
below are substring counts.
"""
import collections
import re
import sys

SIGNALS = [
    ("[BKL] stuck", "long holds (>10M-spin waits, owner genuinely holding)"),
    ("RECOVERED", "ticket-leak self-heals (benign)"),
    ("stale dropped-window", "ledger leak healed at EL0 entry"),
    ("No available user threads", "thread-slot pool exhausted (see doc §11.4)"),
    ("PANIC", "kernel panic"),
    ("WILD", "wild data abort"),
    ("SPURIOUS", "spurious SVC"),
]


def main(path):
    text = open(path, errors="replace").read()
    print(f"== {path} ({len(text)} bytes)")
    ticks = re.findall(r"\[TMR\] t=(\d+)", text)
    if ticks:
        print(f"   uptime reached: {int(ticks[-1]) / 1000:.0f}s")

    print("\n-- stability")
    for needle, why in SIGNALS:
        print(f"   {text.count(needle):6d}  {needle:28s} {why}")

    # Who was holding while peers waited? Only meaningful with bkl-profile.
    owners = collections.Counter(re.findall(r"stuck: owner=(\d+) waiter=(\d+)", text))
    if owners:
        print("\n-- stuck pairs (owner→waiter), top 8")
        for (o, w), n in owners.most_common(8):
            print(f"   {n:6d}  owner={o} waiter={w}")
    tags = collections.Counter(re.findall(r"stuck: owner=\d+ waiter=\d+ tag=(\d+)", text))
    if tags:
        print("   holder tags on stuck lines:", dict(tags.most_common(8)),
              "(511 = profiler off)")

    windows = re.findall(
        r"\[BKLPROF\] w(\d+) t=(\d+)s spins=(\d+) attributed=(\d+) windows_preserved=(\d+)",
        text)
    if not windows:
        print("\n-- attribution: none (build lacks the `bkl-profile` feature)")
        return
    print(f"\n-- attribution: {len(windows)} windows")
    per_tag = collections.Counter()
    for line in text.splitlines():
        m = re.search(r"\[BKLPROF\]\s+(\S+) tag=(\d+) [\d.]+% spins=(\d+)", line)
        if m:
            per_tag[(m.group(1), m.group(2))] += int(m.group(3))
    total = sum(per_tag.values())
    busiest = sorted(windows, key=lambda w: -int(w[2]))[:5]
    print("   busiest windows (spins):")
    for w, t, spins, attributed, preserved in busiest:
        print(f"     w{w:>3} t={t}s spins={spins} attributed={attributed} preserved={preserved}")
    print(f"   cumulative attributed spins by holder tag (total {total}):")
    for (label, tag), spins in per_tag.most_common(12):
        pct = 100.0 * spins / total if total else 0.0
        print(f"     {pct:5.1f}%  {label:14s} tag={tag:>3}  spins={spins}")


if __name__ == "__main__":
    if len(sys.argv) < 2:
        sys.exit("usage: analyze.py <kernel.log> [more.log ...]")
    for p in sys.argv[1:]:
        main(p)
        print()
