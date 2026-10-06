#!/usr/bin/env python3
"""Summarize a W0 mmiotrace (w0-trace.sh output), on the laptop.

    python3 overlays/ryzen/w0-summary.py <dir>/mmiotrace.txt.gz [--dump PHASE]

Splits the trace at the script's `MARK w0: <phase>` lines (unbind, bind, up,
scan, down, end) and, per phase, counts reads and writes and lists the busiest
registers. Addresses are printed as offsets into the card's BAR (the MAP whose
physical base the access falls in). `--dump bind` prints that phase's accesses
in order, one per line: the sequence W1 has to reproduce.

mmiotrace line formats (Documentation/trace/mmiotrace.rst):
    R|W width time map-id phys value pc pid
    MAP time map-id phys virt len pc pid
    MARK time text
"""
import collections
import gzip
import sys


def open_trace(path):
    return gzip.open(path, "rt", errors="replace") if path.endswith(".gz") else open(path, errors="replace")


def main():
    args = sys.argv[1:]
    if not args:
        sys.exit(__doc__)
    dump = None
    if "--dump" in args:
        i = args.index("--dump")
        dump = args[i + 1]
        del args[i:i + 2]
    maps = {}  # map-id -> (phys base, len)
    phase = "pre"
    order = ["pre"]
    rw = collections.defaultdict(collections.Counter)  # phase -> {R,W}
    regs = collections.defaultdict(collections.Counter)  # phase -> {(op,off)}
    for line in open_trace(args[0]):
        f = line.split()
        if not f:
            continue
        if f[0] == "MAP" and len(f) >= 6:
            maps[f[2]] = (int(f[3], 16), int(f[5], 16))
        elif f[0] == "MARK":
            text = line.split(None, 2)[2].strip() if len(f) > 2 else ""
            if text.startswith("w0: "):
                phase = text[4:]
                order.append(phase)
        elif f[0] in ("R", "W") and len(f) >= 6:
            width, mid, phys, val = int(f[1]), f[3], int(f[4], 16), int(f[5], 16)
            base = maps.get(mid, (0, 0))[0]
            off = phys - base
            rw[phase][f[0]] += 1
            regs[phase][(f[0], off)] += 1
            if dump == phase:
                print(f"{f[0]}{width * 8:<3} 0x{off:05x} = 0x{val:0{width * 2}x}")
    if dump:
        return
    for mid, (base, ln) in maps.items():
        print(f"map {mid}: phys 0x{base:x} len 0x{ln:x}")
    for p in order:
        c = rw.get(p, {})
        print(f"\n[{p}] reads {c.get('R', 0)}  writes {c.get('W', 0)}")
        for (op, off), n in regs[p].most_common(8):
            print(f"    {op} 0x{off:05x} x{n}")


if __name__ == "__main__":
    main()
