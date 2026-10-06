#!/usr/bin/env python3
"""Compile a recorded Linux bring-up into an `akuma-rtw89` sequence file.

    python3 overlays/ryzen/w2-merge.py <run> --phase up --collapse --no-fwdl --ts > up.txt
    python3 overlays/ryzen/w2-seqgen.py up.txt crates/akuma-rtw89/seq/up.seq [--from-fw-ready] [--until-stop]

Input is `w2-merge.py --ts` output: register accesses, H2C commands and C2H
events in the order Linux made them, with trace timestamps. Output is the op
stream `akuma_rtw89::script` runs (format in `crates/akuma-rtw89/src/script.rs`):

- every **write** becomes a write of the value Linux wrote;
- a run of reads of one register whose value changed is a **poll**: wait until
  the bits that changed read as they did last (`mask`, `val`);
- a single read becomes a **check**: read, and count it if it differs (the
  runtime's report of where the chip departs from Linux's run). Two registers
  Linux always polls with a fixed condition are polls even when the first
  read already satisfied it: `SWSI` busy (0x1174c, bits 25:24 clear) and the
  XTAL serial interface (0x270, bit 31 clear);
- an **H2C** line becomes an H2C command with those bytes. The card's MAC
  address, wherever it appears (`--mac`), is zeroed and its position
  recorded, so the runtime puts in the MAC it read from the efuse;
- a gap of more than 100 µs before an access (outside a poll) becomes a
  **delay** of the gap less 20 µs, at most 20 ms: Linux's `udelay`s,
  `fsleep`s and waits for a firmware acknowledgement, which no trace line shows.

Dropped: C2H lines (the runtime drains the RX ring itself), the CH12 doorbell
and its index reads (0x1080: the runtime's H2C sender makes its own), and
everything Linux's interrupt handler touched (HIMR/HISR 0x10b0/0x10b4/0x30b0/
0x30b4/0x1a0/0x1a8, the RX ring indices 0x1218/0x121c): this driver polls.

`--from-fw-ready` starts after the first `R8 0x001e0 = 0xe2` (the firmware's
init-ready, where `bringup::bring_up` stops). `--until-stop` ends before the
first read of `0x01058`, where Linux's `rtw89_core_stop` begins (the "up" phase
ends with the card entering idle power save).
"""
import argparse
import re
import struct

IRQ = {0x30b0, 0x30b4, 0x10b0, 0x10b4, 0x01a0, 0x01a8, 0x1218, 0x121c, 0x1080}
FIXED_POLLS = {0x1174c: (0x03000000, 0), 0x00270: (0x80000000, 0)}
OP_END, OP_W, OP_R, OP_POLL, OP_DELAY, OP_H2C = 0x00, 0x00, 0x10, 0x20, 0x30, 0x40
WCODE = {8: 1, 16: 2, 32: 3}


def parse(lines):
    out = []
    for l in lines:
        ts = None
        if " @" in l:
            l, t = l.rsplit(" @", 1)
            ts = float(t)
        f = l.split()
        if not f:
            continue
        if f[0] in ("H2C", "C2H"):
            out.append((f[0], None, None, bytes.fromhex(f[2]), ts))
        else:
            op, width = f[0][0], int(f[0][1:])
            out.append((op, width, int(f[1], 16), int(f[3], 16), ts))
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("inp")
    ap.add_argument("out")
    ap.add_argument("--from-fw-ready", action="store_true")
    ap.add_argument("--until-stop", action="store_true")
    ap.add_argument("--mac", default="", help="the card's MAC as 12 hex digits, to be zeroed in H2Cs")
    a = ap.parse_args()
    lines = open(a.inp).read().splitlines()
    if a.from_fw_ready:
        lines = lines[next(i for i, l in enumerate(lines) if l.startswith("R8   0x001e0 = 0xe2")) + 1:]
    if a.until_stop:
        lines = lines[:next(i for i, l in enumerate(lines) if l.startswith("R32  0x01058"))]
    acc = [e for e in parse(lines) if e[0] != "C2H" and not (e[0] in "RW" and e[2] in IRQ)]
    mac = bytes.fromhex(a.mac) if a.mac else None

    out = bytearray()
    stats = dict(w=0, r=0, poll=0, delay=0, delay_us=0, h2c=0, mac=0)
    prev_ts = None
    i = 0
    while i < len(acc):
        kind, width, off, val, ts = acc[i]
        # Delay for a gap Linux spent outside any access.
        if prev_ts is not None and ts is not None:
            gap = ts - prev_ts
            if gap > 100e-6:
                us = min(int((gap - 20e-6) * 1e6), 20000)
                out += struct.pack("<BI", OP_DELAY, us)
                stats["delay"] += 1
                stats["delay_us"] += us
        if kind == "H2C":
            data = bytearray(val)
            at = 0xffff
            if mac and mac in data:
                at = data.index(mac)
                data[at:at + 6] = bytes(6)
                stats["mac"] += 1
            out += struct.pack("<BHH", OP_H2C, len(data), at) + data
            stats["h2c"] += 1
            prev_ts = ts
            i += 1
            continue
        code = WCODE[width]
        fmt = {8: "B", 16: "H", 32: "I"}[width]
        if kind == "W":
            out += struct.pack("<BI" + fmt, OP_W | code, off, val)
            stats["w"] += 1
            prev_ts = ts
            i += 1
            continue
        # A read: gather the run of reads of this register.
        j = i
        vals = []
        while j < len(acc) and acc[j][0] == "R" and acc[j][2] == off and acc[j][1] == width:
            vals.append(acc[j][3])
            j += 1
        last = vals[-1]
        changed = 0
        for v in vals:
            changed |= v ^ last
        if changed:
            mask, want = changed, last & changed
        elif off in FIXED_POLLS:
            mask, want = FIXED_POLLS[off]
        else:
            mask = None
        if mask is not None:
            out += struct.pack("<BI" + fmt + fmt, OP_POLL | code, off, mask, want)
            stats["poll"] += 1
            # The poll's own duration is not a delay.
            prev_ts = acc[j - 1][4]
            i = j
            continue
        out += struct.pack("<BI" + fmt, OP_R | code, off, val)
        stats["r"] += 1
        prev_ts = ts
        i += 1
    out += bytes([OP_END])
    open(a.out, "wb").write(out)
    print(f"{a.out}: {len(out)} bytes;", ", ".join(f"{k} {v}" for k, v in stats.items()))


if __name__ == "__main__":
    main()
