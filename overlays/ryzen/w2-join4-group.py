#!/usr/bin/env python3
"""Cut the group-key half out of join4.seq -> join4g.seq (a group rekey).

JOIN4 installs the pairwise key (security-CAM entry 0, DCTL, ADDR_CAM, two
polled 22-byte commands) and then the group key (entry 1, DCTL and ADDR_CAM
carrying the group key id) and the beacon filter. A group rekey needs only the
second part, minus the beacon filter, which a rekey does not touch. The cut is
at op boundaries found by walking the stream: the first command whose
substitutions include GTK (kind 5) starts it, and the last 0x41 command (the
beacon filter) is left out.

    python3 overlays/ryzen/w2-join4-group.py            # rewrites join4g.seq
"""
import os, struct, sys

HERE = os.path.dirname(os.path.abspath(__file__))
SEQ = os.path.join(HERE, "..", "..", "crates", "akuma-rtw89", "seq")
GTK = 5

def ops(s):
    i = 0
    while True:
        st, op = i, s[i]
        i += 1
        if op == 0:
            return
        w = {1: 1, 2: 2, 3: 4}.get(op & 3, 0)
        h = op & 0xF0
        if h in (0, 0x10):
            i += 4 + w
        elif h == 0x20:
            i += 4 + 2 * w
        elif h == 0x30:
            i += 4
        elif op == 0x40:
            i += 4 + struct.unpack_from("<H", s, i)[0]
        elif op == 0x41:
            n, k = struct.unpack_from("<HB", s, i)
            kinds = [s[i + 3 + 3 * j] for j in range(k)]
            i += 3 + 3 * k + n
            yield st, i, op, kinds
            continue
        else:
            sys.exit(f"bad op {op:#x} at {st}")
        yield st, i, op, []

s = open(os.path.join(SEQ, "join4.seq"), "rb").read()
walk = list(ops(s))
first = next(st for st, _, op, k in walk if op == 0x41 and GTK in k)
last = [(st, en) for st, en, op, _ in walk if op == 0x41][-1]
out = s[first:last[0]] + b"\x00"
open(os.path.join(SEQ, "join4g.seq"), "wb").write(out)
print(f"join4g.seq: {len(out)} bytes (ops {first}..{last[0]} of join4.seq)")
