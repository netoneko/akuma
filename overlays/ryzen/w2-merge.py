#!/usr/bin/env python3
"""Merge a W2 trace's register accesses and firmware messages into one stream.

    python3 overlays/ryzen/w2-merge.py <run-dir> [--phase up] [--collapse] [--no-fwdl] [--ts]

`<run-dir>` is a `w0-trace.sh` output directory holding `mmiotrace.txt.gz` and
`h2c.txt.gz` (the fprobe dump of every H2C the driver sent and every C2H the
firmware returned). Both carry timestamps from the same trace clock, so they
merge into the order things happened — almost: the instance's timestamps run
about 0.75 s off the mmiotrace ones (measured 2026-10-06), so every H2C is
instead anchored to its own doorbell (the k-th H2C is the k-th write of
`CH12_TXBD_IDX`, 0x1080: 585 of each in run 3) and printed just before it, and
each C2H is placed by its timestamp corrected with the offset of the nearest
preceding H2C. Output, one per line:

    R32  0x001e0 = 0x000000e2        a register access (offset into BAR2)
    H2C  <len> <hex bytes>           a firmware command, H2C header included
    C2H  <len> <hex bytes>           a firmware event, C2H header included

H2C bytes beyond 2048 and C2H bytes beyond 512 were not captured; such lines
end in `...`. `--no-fwdl` drops the firmware-download packets (H2C lines of
2020-byte section data, which are just the firmware file). `--collapse` folds
runs of identical register *reads* (polls). `--ts` appends each line's
trace time (`@seconds`; for an H2C or C2H, the time of the access after it); unlike `w0-summary.py --collapse`,
repeated writes are kept.
"""
import gzip
import re
import sys

EV = re.compile(r"\s(\d+\.\d+): (h2c|c2h): .*? len=(\d+) (.*)$")


def words_to_bytes(fields):
    out = bytearray()
    for arr in re.findall(r"d\d=\{([^}]*)\}", fields):
        for w in arr.split(","):
            out += int(w, 16).to_bytes(8, "little")
    return bytes(out)


def main():
    args = sys.argv[1:]
    if not args:
        sys.exit(__doc__)
    phase_want = "up"
    if "--phase" in args:
        i = args.index("--phase")
        phase_want = args[i + 1]
        del args[i:i + 2]
    collapse = "--collapse" in args
    with_ts = "--ts" in args
    nofwdl = "--no-fwdl" in args
    d = args[0]

    events = []  # (ts, kind, payload)
    for line in gzip.open(f"{d}/h2c.txt.gz", "rt", errors="replace"):
        m = EV.search(line)
        if not m:
            continue
        ts, kind, ln, rest = float(m.group(1)), m.group(2).upper(), int(m.group(3)), m.group(4)
        data = words_to_bytes(rest)
        cap = 2048 if kind == "H2C" else 512
        events.append((ts, kind, ln, data[:min(ln, cap)], ln > cap))
    events.sort(key=lambda e: e[0])

    maps = {}
    # Pass 1: the mmio stream, and the timestamp of every CH12 doorbell.
    mmio = []
    kicks = []
    for line in gzip.open(f"{d}/mmiotrace.txt.gz", "rt", errors="replace"):
        f = line.split()
        if not f:
            continue
        if f[0] == "MAP" and len(f) >= 6:
            maps[f[2]] = int(f[3], 16)
        elif f[0] == "MARK":
            text = line.split(None, 2)[2].strip()
            mmio.append(("MARK", float(f[1]), text))
        elif f[0] in ("R", "W") and len(f) >= 6:
            width, mid, phys, val = int(f[1]) * 8, f[3], int(f[4], 16), int(f[5], 16)
            off = phys - maps.get(mid, 0)
            if f[0] == "W" and off == 0x1080 and width == 16:
                kicks.append(len(mmio))
            mmio.append((f[0], float(f[2]), (width, off, val)))
    h2cs = [e for e in events if e[1] == "H2C"]
    c2hs = [e for e in events if e[1] == "C2H"]
    if len(h2cs) != len(kicks):
        sys.exit(f"{len(h2cs)} H2C events but {len(kicks)} CH12 doorbells: cannot anchor")
    # Where each event goes: before mmio index i.
    before = {}
    for (k, e) in zip(kicks, h2cs):
        before.setdefault(k, []).append(e)
    offs = [(e[0], mmio[k][1] - e[0]) for k, e in zip(kicks, h2cs)]
    oi = 0
    mi = 0
    for e in c2hs:
        while oi + 1 < len(offs) and offs[oi + 1][0] <= e[0]:
            oi += 1
        ts = e[0] + offs[oi][1]
        while mi < len(mmio) and mmio[mi][1] <= ts:
            mi += 1
        before.setdefault(mi, []).append(e)

    phase = "pre"
    prev = None
    out = sys.stdout
    for i in range(len(mmio) + 1):
        for (t, kind, ln, data, trunc) in before.get(i, []):
            if phase != phase_want:
                continue
            if nofwdl and kind == "H2C" and ln == 2020 and not _is_h2c_cmd(data):
                continue
            tail = f" @{mmio[i][1] if i < len(mmio) else 0:.6f}" if with_ts else ""
            out.write(f"{kind}  {ln} {data.hex()}{' ...' if trunc else ''}{tail}\n")
            prev = None
        if i == len(mmio):
            break
        op, ts, x = mmio[i]
        if op == "MARK":
            if x.startswith("w0: "):
                phase = x[4:]
            continue
        if phase != phase_want:
            continue
        width, off, val = x
        line = f"{op}{width:<3} 0x{off:05x} = 0x{val:0{width // 4}x}"
        # Only reads fold: a run of identical reads is a poll. Two identical
        # writes in a row are two writes.
        if collapse and line == prev and op == "R":
            continue
        prev = line
        out.write(line + (f" @{ts:.6f}" if with_ts else "") + "\n")


def _is_h2c_cmd(data):
    # A firmware section packet has no H2C header; a command's hdr1 length
    # field equals the packet length. Section data almost never matches.
    if len(data) < 8:
        return False
    hdr1 = int.from_bytes(data[4:8], "little")
    return (hdr1 & 0x3fff) == len(data)


if __name__ == "__main__":
    main()
