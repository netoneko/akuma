#!/usr/bin/env python3
"""A join recording's timeline, by kind only: which firmware command, which
802.11 frame, how many register accesses in between. Never prints addresses,
names or payload bytes, so its output is safe to paste anywhere.

    python3 overlays/ryzen/w2-merge.py <run> --phase join --collapse --no-fwdl --ts > join.txt
    python3 overlays/ryzen/w3-timeline.py join.txt [--regs]

`--regs` also lists, per gap, the registers written (offsets only).
"""
import sys

CL = {  # (cat, class) -> name, func -> name
    (1, 0): ("FW_INFO", {0: "LOG_CFG", 1: "GENERAL_PKT"}),
    (1, 2): ("PS", {}),
    (1, 5): ("FR_EXCHG", {2: "CCTL", 5: "BCN_UPD", 9: "DCTL_V1", 10: "CCTL_V1"}),
    (1, 6): ("ADDR_CAM", {0: "UPD"}),
    (1, 8): ("MEDIA_RPT", {0: "JOININFO", 4: "ROLE_MAINTAIN", 5: "NOTIFY_DBCC"}),
    (1, 9): ("FW_OFLD", {1: "PKT_OFLD", 8: "MACID_PAUSE", 0xf: "USR_EDCA", 0x10: "TSF32_TOGL",
                         0x14: "OFLD_CFG", 0x16: "ADD_SCAN_CH", 0x17: "SCANOFLD", 0x18: "TX_DUTY",
                         0x1b: "PKT_DROP", 0x1e: "BCNFLTR", 0x1f: "OFLD_RSSI", 0x20: "OFLD_TP",
                         0x28: "MACID_PAUSE_SLEEP"}),
    (1, 0xa): ("SEC_CAM", {1: "SEC_UPD"}),
    (1, 0xc): ("BA_CAM", {0: "BA_CAM", 1: "BA_CAM_V1", 2: "BA_CAM_INIT"}),
    (1, 0xe): ("MCC", {}),
    (2, 1): ("RA", {0: "MACIDCFG"}),
    (2, 2): ("DM", {}),
    (2, 8): ("RF_REG_A", {}),
    (2, 9): ("RF_REG_B", {}),
    (2, 0xa): ("RF_FW_NOTIFY", {}),
    (2, 0xb): ("RF_FW_RFK", {}),
    (2, 0xc): ("BTC", {}),
}

MGMT = {0: "assoc-req", 1: "assoc-resp", 2: "reassoc-req", 3: "reassoc-resp", 4: "probe-req",
        5: "probe-resp", 8: "beacon", 10: "disassoc", 11: "auth", 12: "deauth", 13: "action"}
DATA = {0: "data", 4: "null", 8: "qos-data", 12: "qos-null"}


def h2c_name(b):
    h0 = int.from_bytes(b[0:4], "little")
    cat, cl, fn = h0 & 3, (h0 >> 2) & 0x3f, (h0 >> 8) & 0xff
    c = CL.get((cat, cl))
    if c:
        return f"{c[0]}.{c[1].get(fn, hex(fn))}"
    return f"cat{cat}.cl{cl:#x}.f{fn:#x}"


def frame_name(b):
    fc = b[0]
    ty, st = (fc >> 2) & 3, fc >> 4
    if ty == 0:
        return MGMT.get(st, f"mgmt{st}")
    if ty == 2:
        n = DATA.get(st, f"data{st}")
        # EAPOL / ARP / IPv4 by LLC/SNAP ethertype, when the header was captured whole
        hl = 26 if st & 8 else 24
        if len(b) >= hl + 8 and b[hl:hl + 6] == bytes.fromhex("aaaa03000000"):
            et = int.from_bytes(b[hl + 6:hl + 8], "big")
            n += {0x888e: "/eapol", 0x0806: "/arp", 0x0800: "/ip", 0x86dd: "/ip6"}.get(et, f"/{et:#x}")
        return n
    return f"ctl{st}"


def main():
    lines = open(sys.argv[1]).read().splitlines()
    regs = "--regs" in sys.argv
    t0 = None
    gap_r = gap_w = 0
    wset = []

    def flush():
        nonlocal gap_r, gap_w, wset
        if gap_r or gap_w:
            s = f"{'':>10}  .. {gap_r} reads, {gap_w} writes"
            if regs and wset:
                s += "  W:" + ",".join(f"{o:x}" for o in sorted(set(wset)))
            print(s)
        gap_r = gap_w = 0
        wset = []

    for l in lines:
        ts = None
        if " @" in l:
            l, t = l.rsplit(" @", 1)
            ts = float(t)
        f = l.split()
        if not f:
            continue
        if t0 is None and ts is not None:
            t0 = ts
        if f[0][0] in "RW" and f[0][1:].isdigit():
            if f[0][0] == "R":
                gap_r += 1
            else:
                gap_w += 1
                wset.append(int(f[1], 16))
            continue
        flush()
        kind, n = f[0], int(f[1])
        b = bytes.fromhex(f[2]) if len(f) > 2 and f[2] != "..." else b""
        t = f"{(ts - t0) * 1000:10.3f}" if ts is not None else " " * 10
        if kind == "H2C":
            print(f"{t}  H2C {h2c_name(b):24} {n}")
        elif kind == "C2H":
            print(f"{t}  C2H {h2c_name(b):24} {n}")
        elif kind in ("TXH", "TXM", "RXM"):
            print(f"{t}  {kind} {frame_name(b):24} {n}")
        elif kind == "TXD":
            print(f"{t}  TXD {'':24} {n}")
    flush()


if __name__ == "__main__":
    main()
