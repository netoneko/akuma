#!/usr/bin/env python3
"""Read the amd64 kernel's `[SLOT]` / `[FUTEX]` dumps and say what moved.

`sched::dump_slot_table` and `futex::dump_waiters` print one block every 30 s
from the idle loop. A single block is a photograph; this reads two and prints
the *difference*, which is the only thing that answers the question the
`-j4` wedge actually poses — not "is this thread waiting?" but "is it making
progress while it waits?".

    scripts/benchmarks/amd64_slot_report.py                 # pull from the box
    scripts/benchmarks/amd64_slot_report.py --file cons.log # a saved log
    scripts/benchmarks/amd64_slot_report.py --blocks 4      # last four blocks

# Why a diff and not a dump

The wedge was mis-read for two sessions as "created and never scheduled"
because `ps` reports CPU time in whole seconds, so a thread that has been
scheduled 300 times and parked 300 times reads `0:00` exactly like one that
has never run. The two columns that settle it are `ins` (switch-ins, which
climbs for a parked-and-woken thread) and `scn` (syscall entries, which does
not). A diff shows both at once:

    slot 13 pid=66  ins +30   scn +0    st=WAITING  sc=futex
             ^ scheduled 30 times in 30 s, and made no syscall while doing it
             = woken by the untimed-park backstop, re-tested, re-parked.

# What the columns mean

See `sched::dump_slot_table`'s own doc comment; it is the authority. The short
version: `st` is the thread state, `gate` the on-CPU gate, `pick` is
hits/skipped-for-gate/skipped-for-pinning from the picker itself, `sc` the last
syscall *entered* (so a WAITING thread is parked inside it).
"""

import argparse
import re
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "utils"))
import hpbox  # noqa: E402

FC_LOG = "/root/akuma-fc.log"

STATES = {0: "FREE", 1: "READY", 2: "RUNNING", 3: "TERM", 4: "INIT", 5: "WAITING"}

# x86_64 syscall numbers seen in this kernel's wedges. Not a full table — the
# point is to name the handful that matter without a lookup, and anything else
# prints as its number rather than as a wrong guess.
SYSCALLS = {0: "read", 1: "write", 7: "poll", 23: "select", 35: "nanosleep",
            45: "recvfrom", 61: "wait4", 202: "futex", 231: "exit_group",
            232: "epoll_wait", 270: "pselect6", 271: "ppoll", 281: "epoll_pwait"}

SLOT_RE = re.compile(
    r"\[SLOT\] (\d+) st=(\d+) gate=(\d+) ins=(\d+) core=(\S+) "
    r"pick=(\d+)/(\d+)/(\d+) wake=(\d+) woken=(\S+) sc=(-?\d+) scn=(\d+) "
    r"gen=(\d+) root=0x([0-9a-f]+) daemon=(\d) idle=(\d) pin=(\d+) "
    r"(?:park=(\S+) )?pid=(\S+)")
FUTEX_KEY_RE = re.compile(
    r"\[FUTEX\] key tgid=(\d+) uaddr=0x([0-9a-f]+) waiters=(\d+) tids=(.*)")
FUTEX_TALLY_RE = re.compile(
    r"\[FUTEX\] wakes=(\d+) empty=(\d+) woken=(\d+) enqueues=(\d+)")


def fetch(path=None):
    if path:
        return Path(path).read_text(errors="replace")
    rc, out, err = hpbox.ubuntu(
        f"grep -aE '\\[SLOT\\]|\\[FUTEX\\]' {FC_LOG} | tail -n 2000", timeout=90)
    if rc not in (0, None):
        print("could not read the box's console log:", (out + err)[:400])
        sys.exit(2)
    return out


def parse(text):
    """Split into blocks. A block starts at a census line and ends at `end`."""
    blocks, cur = [], None
    for ln in text.splitlines():
        if "[SLOT] census" in ln:
            # A block runs census-to-census, **not** to `[SLOT] --- end ---`:
            # `futex::dump_waiters` prints after the slot table, and closing at
            # `end` silently dropped every futex line — which read as "the futex
            # table is empty" when it was simply never parsed.
            if cur:
                blocks.append(cur)
            cur = {"census": ln.strip(), "slots": {}, "futex_keys": [],
                   "futex_tally": None}
        elif cur is None:
            continue
        elif m := SLOT_RE.search(ln):
            g = m.groups()
            cur["slots"][int(g[0])] = {
                "st": int(g[1]), "gate": int(g[2]), "ins": int(g[3]),
                "core": g[4], "hit": int(g[5]), "skip_gate": int(g[6]),
                "skip_pin": int(g[7]), "wake": int(g[8]), "sc": int(g[10]),
                "scn": int(g[11]), "gen": int(g[12]), "root": g[13],
                "daemon": g[14] == "1", "idle": g[15] == "1",
                "park": g[17] or "-", "pid": g[18]}
        elif m := FUTEX_TALLY_RE.search(ln):
            cur["futex_tally"] = tuple(int(x) for x in m.groups())
        elif m := FUTEX_KEY_RE.search(ln):
            cur["futex_keys"].append(
                (int(m.group(1)), m.group(2), int(m.group(3)), m.group(4).strip()))
    if cur:
        blocks.append(cur)
    return blocks


def sc_name(n):
    return "-" if n < 0 else SYSCALLS.get(n, str(n))


def report(blocks, n):
    if not blocks:
        print("no [SLOT] blocks found — is the kernel the one with the dump in it?")
        return 1
    print(f"{len(blocks)} block(s); showing the last {min(n, len(blocks))}\n")
    for b in blocks[-n:]:
        print(b["census"])
    prev, last = (blocks[-2] if len(blocks) > 1 else None), blocks[-1]
    # Which slots the futex table actually holds. A thread reported `WAITING`
    # with `sc=futex` that is **not** in this set is parked somewhere other than
    # the futex wait loop, and the `park` column says where — the combination
    # that §13's first futex dump turned up and could not explain.
    queued = set()
    for _, _, _, tids in last["futex_keys"]:
        for t in tids.split():
            queued.add(int(t.split("/")[0]))
    print(f"\n{'slot':>5} {'pid':>10} {'state':>8} {'sc':>10}Q {'gate':>4} "
          f"{'ins':>9} {'+ins':>6} {'scn':>9} {'+scn':>7} {'park site':>34}")
    for slot, s in sorted(last["slots"].items()):
        p = (prev or {}).get("slots", {}).get(slot) if prev else None
        d_ins = f"+{s['ins'] - p['ins']}" if p else "-"
        d_scn = f"+{s['scn'] - p['scn']}" if p else "-"
        q = "Q" if slot in queued else " "
        print(f"{slot:>5} {s['pid']:>10} {STATES.get(s['st'], s['st']):>8} "
              f"{sc_name(s['sc']):>10}{q} {s['gate']:>4} {s['ins']:>9} {d_ins:>6} "
              f"{s['scn']:>9} {d_scn:>7} {s['park']:>34}")

    # The reading the diff exists to make, stated rather than left to the eye.
    stuck = [(slot, s) for slot, s in sorted(last["slots"].items())
             if prev and slot in prev["slots"] and s["st"] == 5
             and s["scn"] == prev["slots"][slot]["scn"]
             and s["ins"] > prev["slots"][slot]["ins"]]
    if stuck:
        print(f"\n{len(stuck)} thread(s) scheduled but making no syscall — "
              "woken by the park backstop, re-tested, re-parked:")
        for slot, s in stuck:
            print(f"    slot {slot} pid={s['pid']} parked in {sc_name(s['sc'])} "
                  f"at {s['park']} (root=0x{s['root']})")
    leaked = [(slot, s) for slot, s in last["slots"].items()
              if s["gate"] and s["st"] in (0, 3)]
    print("\nleaked on-CPU gates:",
          ", ".join(f"slot {i}" for i, _ in leaked) if leaked else "none")

    if last["futex_tally"]:
        calls, empty, woken, enq = last["futex_tally"]
        line = (f"\nfutex: wakes={calls} empty={empty} woken={woken} "
                f"enqueues={enq}")
        if prev and prev["futex_tally"]:
            d = [a - b for a, b in zip(last["futex_tally"], prev["futex_tally"])]
            line += f"   (delta +{d[0]}/+{d[1]}/+{d[2]}/+{d[3]})"
        print(line)
    for tgid, uaddr, n_w, tids in last["futex_keys"]:
        print(f"    key tgid={tgid} uaddr=0x{uaddr} waiters={n_w} tids={tids}")
    return 0


def main():
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--file", help="a saved console log instead of the box's")
    ap.add_argument("--blocks", type=int, default=2,
                    help="how many census lines to echo (default 2)")
    a = ap.parse_args()
    return report(parse(fetch(a.file)), a.blocks)


if __name__ == "__main__":
    sys.exit(main())
