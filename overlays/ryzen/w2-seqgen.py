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

## `--join N`: the recorded join, as four segments

    python3 overlays/ryzen/w2-merge.py <run> --phase join --no-fwdl --ts > join.txt
    python3 overlays/ryzen/w2-seqgen.py join.txt crates/akuma-rtw89/seq/joinN.seq \
        --join N --mac <card MAC> --bssid <AP MAC>

Segments of the recorded join (`docs/archive/NEXT_AGENT_RYZEN_WIFI_JOIN.md`),
each replayed by the join task at the point its name says:

| N | starts | ends before | replayed after |
|---|---|---|---|
| 1 | the last `R8 0x001e0 = 0xe2` | the first ADDR_CAM H2C carrying the AP's address | `up.seq` |
| 2 | that ADDR_CAM | the authentication frame TX | J1 |
| 3 | the association response RX | the first periodic OFLD_RSSI H2C | the association response RX |
| 4 | the first security-CAM H2C | (ends after) the BCNFLTR H2C | EAPOL message 4 TX |

Every private value leaves the file: the card's MAC and the AP's BSSID are
replaced wherever they appear (the positions recorded as `0x41` substitutions
[`script::sub`]), the association id is patched from the ADDR_CAM's AID12
field and the PS-Poll template's duration field, the group key's id from its
bits 7:6 fields in the ADDR_CAM and DCTL commands, and the temporal keys —
whose bodies the recording redacted to zeros anyway — are synthesized into the
security-CAM command bodies from `cam.c rtw89_cam_get_sec_key_cmd`
(CCMP-128: `[idx, 0, 20, 0, 6, 0, 0, 0, key[16]]`, key at byte 8). Generation
fails if a MAC or BSSID byte survives anywhere in the output.
"""
import argparse
import re
import struct

IRQ = {0x30b0, 0x30b4, 0x10b0, 0x10b4, 0x01a0, 0x01a8, 0x1218, 0x121c, 0x1080}
FIXED_POLLS = {0x1174c: (0x03000000, 0), 0x00270: (0x80000000, 0)}
OP_END, OP_W, OP_R, OP_POLL, OP_DELAY, OP_H2C = 0x00, 0x00, 0x10, 0x20, 0x30, 0x40
WCODE = {8: 1, 16: 2, 32: 3}
# `akuma_rtw89::script::sub` kinds.
SUB_MAC, SUB_BSSID, SUB_AID12, SUB_AID_PSPOLL, SUB_TK, SUB_GTK, SUB_GTK_IDX_HI2 = range(7)
# The two H2Cs the join's key installs use: class 10 func 1 (security CAM)
# with a redacted body, synthesized as `cam.c` fills it for CCMP-128; the
# temporal key lands at byte 8. First of the join is the pairwise key
# (security entry 0), second the group key (entry 1) — the order Linux
# installs them after EAPOL message 3.
SEC_CAM_CLS, SEC_CAM_FN = 0x0a, 0x01
SEC_CAM_BODY = bytes([0, 0, 20, 0, 6, 0, 0, 0])  # idx 0, offset 0, len 20, CCMP-128


def h2c_class_func(data):
    h0 = struct.unpack("<I", data[:4])[0]
    return (h0 >> 2) & 0x3f, (h0 >> 8) & 0xff


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
        elif f[0] in ("TXD", "TXH", "TXM", "RXM"):
            out.append((f[0], None, None, bytes.fromhex(f[2]), ts))
        else:
            op, width = f[0][0], int(f[0][1:])
            out.append((op, width, int(f[1], 16), int(f[3], 16), ts))
    return out


def fc_of(data):
    return struct.unpack("<H", data[:2])[0]


def join_bounds(lines, bssid):
    """Line indexes where the four join segments start and end, as
    `(start, end)` pairs with `end` exclusive."""
    fw_ready = max(i for i, l in enumerate(lines) if l.startswith("R8   0x001e0 = 0xe2"))
    addr_cam = next(i for i, l in enumerate(lines) if l.startswith("H2C")
                    and bssid in bytes.fromhex(l.split()[2])
                    and h2c_class_func(bytes.fromhex(l.split()[2])) == (6, 0))
    auth_tx = next(i for i, l in enumerate(lines) if l.startswith("TXH")
                   and (fc_of(bytes.fromhex(l.split()[2])) >> 2 & 3) == 0
                   and (fc_of(bytes.fromhex(l.split()[2])) >> 4 & 0xf) == 0xB)
    assoc_rx = next(i for i, l in enumerate(lines) if l.startswith("RXM")
                    and (fc_of(bytes.fromhex(l.split()[2])) >> 2 & 3) == 0
                    and (fc_of(bytes.fromhex(l.split()[2])) >> 4 & 0xf) == 0x1)
    ofld_rssi = next(i for i, l in enumerate(lines[assoc_rx:], assoc_rx)
                     if l.startswith("H2C")
                     and h2c_class_func(bytes.fromhex(l.split()[2])) == (9, 0x1f))
    sec_cam = next(i for i, l in enumerate(lines[ofld_rssi:], ofld_rssi)
                   if l.startswith("H2C")
                   and h2c_class_func(bytes.fromhex(l.split()[2])) == (SEC_CAM_CLS, SEC_CAM_FN))
    bcnfltr = next(i for i, l in enumerate(lines[sec_cam:], sec_cam)
                   if l.startswith("H2C")
                   and h2c_class_func(bytes.fromhex(l.split()[2])) == (9, 0x1e))
    return [(fw_ready + 1, addr_cam), (addr_cam, auth_tx),
            (assoc_rx + 1, ofld_rssi), (sec_cam, bcnfltr + 1)]


def h2c_op(data, mac, bssid, stats):
    """One H2C line as an `0x41` op: private bytes blanked, positions
    recorded. Security-CAM bodies (redacted in the recording) are synthesized
    from `cam.c` with the temporal key as the one substitution."""
    cls, fn = h2c_class_func(data)
    body = bytearray(data)
    subs = []
    if cls == SEC_CAM_CLS and fn == SEC_CAM_FN and not any(body[8:]):
        is_group = stats["sec_cam"] > 0
        stats["sec_cam"] += 1
        body = bytearray(8 + 24)
        body[:8] = data[:8]
        body[8 + 0] = 1 if is_group else 0  # security entry: group 1, pairwise 0
        body[8 + 2] = 20  # RTW89_SEC_CAM_LEN
        body[8 + 4] = 6  # RTW89_SEC_KEY_TYPE_CCMP128
        subs.append((SUB_GTK if is_group else SUB_TK, 16))
        stats["sec_cam_synth"] += 1
    else:
        for needle, kind in ((mac, SUB_MAC), (bssid, SUB_BSSID)):
            at = 0
            while True:
                at = body.find(needle, at)
                if at < 0:
                    break
                subs.append((kind, at))
                at += 6
        if cls == 6 and fn == 0 and body[16] == 5:  # ADDR_CAM, net_type infra
            subs.append((SUB_AID12, 44))  # AID12: body dword 9, bits 11:0
            if body[46] & 0xC0:  # the group key's id, bits 7:6 of SEC_ENT key id
                subs.append((SUB_GTK_IDX_HI2, 46))
        if cls == 5 and fn == 9 and body[30] & 0xC0:  # DCTL v1, group key's id
            subs.append((SUB_GTK_IDX_HI2, 30))
        if cls == 9 and fn == 1 and body[12:14] == b"\xa4\x00":  # PS-Poll template
            subs.append((SUB_AID_PSPOLL, 14))
    stats["h2c"] += 1
    stats["subs"] += len(subs)
    # Blank what the recording must not keep: the addresses and the keys.
    # The AID and key-id substitutions replace whole bit fields at runtime and
    # need the byte's other bits as Linux recorded them, so those stay.
    for kind, at in subs:
        n = {SUB_MAC: 6, SUB_BSSID: 6, SUB_TK: 16, SUB_GTK: 16}.get(kind)
        if n:
            body[at:at + n] = bytes(n)
    out = struct.pack("<BHB", OP_H2C | 1, len(body), len(subs))
    for kind, at in subs:
        for (k, a) in subs:
            if a == at and k != kind:
                raise SystemExit(f"two substitutions at byte {at} of a {cls:#x}/{fn:#x} H2C")
        out += struct.pack("<BH", kind, at)
    return out + bytes(body), body


def check_privacy(out, mac, bssid):
    for name, needle in (("card MAC", mac), ("AP BSSID", bssid)):
        if needle in out:
            raise SystemExit(f"{name} bytes survived in the sequence — refusing to write it")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("inp")
    ap.add_argument("out")
    ap.add_argument("--from-fw-ready", action="store_true")
    ap.add_argument("--until-stop", action="store_true")
    ap.add_argument("--mac", default="", help="the card's MAC as 12 hex digits, to be zeroed in H2Cs")
    ap.add_argument("--join", type=int, metavar="N", help="emit join segment N (1..4) instead of the up stream")
    ap.add_argument("--bssid", default="", help="the AP's MAC as 12 hex digits (join mode)")
    a = ap.parse_args()
    lines = open(a.inp).read().splitlines()
    join_mode = a.join is not None
    if join_mode:
        if len(a.mac) != 12 or len(a.bssid) != 12:
            raise SystemExit("--join needs --mac and --bssid (12 hex digits each)")
        mac, bssid = bytes.fromhex(a.mac), bytes.fromhex(a.bssid)
        start, end = join_bounds(lines, bssid)[a.join - 1]
        lines = lines[start:end]
    if a.from_fw_ready:
        lines = lines[next(i for i, l in enumerate(lines) if l.startswith("R8   0x001e0 = 0xe2")) + 1:]
    if a.until_stop:
        lines = lines[:next(i for i, l in enumerate(lines) if l.startswith("R32  0x01058"))]
    acc = [e for e in parse(lines) if e[0] != "C2H" and e[0] not in ("TXD", "TXH", "TXM", "RXM")
           and not (e[0] in "RW" and e[2] in IRQ)]
    if join_mode:
        # The periodic RSSI exchange is the runtime's own business; dropping
        # it lets a delay bridge the gap (J4's middle has one). The TX ring
        # index registers (J4 reads one) belong to the runtime's own TX
        # rings, not to the replay.
        acc = [e for e in acc if not (e[0] == "H2C" and h2c_class_func(e[3]) == (9, 0x1f))
               and not (e[0] in "RW" and 0x1058 <= e[2] <= 0x107c)]
    mac = bytes.fromhex(a.mac) if a.mac else None

    out = bytearray()
    stats = dict(w=0, r=0, poll=0, delay=0, delay_us=0, h2c=0, mac=0, subs=0, sec_cam=0, sec_cam_synth=0)
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
            if join_mode:
                blob, _ = h2c_op(val, mac, bssid, stats)
                out += blob
                prev_ts = ts
                i += 1
                continue
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
    if join_mode:
        check_privacy(out, mac, bssid)
    open(a.out, "wb").write(out)
    print(f"{a.out}: {len(out)} bytes;", ", ".join(f"{k} {v}" for k, v in stats.items()))


if __name__ == "__main__":
    main()
