#!/usr/bin/env python3
"""cow_leak_gate.py - the copy-on-write fork leak gate for the Taoxichuan kernel.

    python3 -I scripts/kvm/goose/cow_leak_gate.py --console tx-console.log --key keys/k [--port 2224] [--cycles 20]

Run against a guest booted from a `--features trace-err` build (the build whose reap lines carry
`free A -> B` and `cowrefs=N`). Every ssh command is one spawn whose end prints
`[spawn-reap] ... free A -> B ... cowrefs=N cow=(shared, copied, taken, retried)` on the console, and at that point the
guest is idle (the command's whole process tree is reaped), so B is the free-page count of an idle machine.

Protocol: one `echo` command is the BASELINE (B0). Then `--cycles` rounds each of
  1. fork+exec+exit        sh -c 'true; true'
  2. fork+write+exit       sh -c 'x=parent; (x=child; echo in-sub=$x); echo after=$x'   (forces a Copy break; checked)
  3. fork+exit untouched   sh -c '(exit 0); (exit 0)'
then one final `echo`. PASS needs, exactly: final B == B0 (no tolerance: an idle machine returns every frame),
cowrefs == 0 at the end (no shared-frame entry outlives its holders), the isolation output correct in every round,
`cow=` shared > 0 and copied > 0 (the gate exercised CoW, it did not just pass vacuously), and no `[Fault]`/`PANIC`.
A silent probe (no spawn-reap line) is a FAIL, never a pass.
"""
import argparse, re, subprocess, sys, time

ap = argparse.ArgumentParser()
ap.add_argument("--console", required=True)
ap.add_argument("--key", required=True)
ap.add_argument("--port", default="2224")
ap.add_argument("--cycles", type=int, default=20)
a = ap.parse_args()

REAP = re.compile(r"\[spawn-reap\] .*free (\d+) -> (\d+) .*cowrefs=(\d+) cow=\((\d+), (\d+), (\d+), (\d+)\)")

def run(cmd, timeout=120):
    r = subprocess.run(["ssh", "-i", a.key, "-p", a.port, "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
                        "-o", "BatchMode=yes", "-o", "ConnectTimeout=20", "root@127.0.0.1", cmd],
                       capture_output=True, text=True, timeout=timeout)
    return r.returncode, r.stdout

def last_reap():
    # Wait for the spawn-reap line of the command just run: the count of such lines must have grown.
    return REAP.findall(open(a.console, "rb").read().decode("latin1"))

def one(cmd):
    n0 = len(last_reap())
    rc, out = run(cmd)
    for _ in range(100):
        got = last_reap()
        if len(got) > n0:
            return rc, out, got[-1]
        time.sleep(0.3)
    print("FAIL: no [spawn-reap] line after:", cmd); sys.exit(1)

rc, out, base = one("echo baseline")
b0 = int(base[1]); print("baseline free=%d cowrefs=%s" % (b0, base[2]))
iso = "sh -c 'x=parent; (x=child; echo in-sub=$x); echo after=$x'"
bad = 0
for i in range(a.cycles):
    one("sh -c 'true; true'")
    rc, out, _ = one(iso)
    if "in-sub=child" not in out or "after=parent" not in out or rc != 0:
        bad += 1; print("round %d: isolation WRONG rc=%d out=%r" % (i, rc, out))
    one("sh -c '(exit 0); (exit 0)'")
    if i % 5 == 4:
        print("round %d free=%s" % (i + 1, last_reap()[-1][1]))
rc, out, fin = one("echo final")
bf, refs = int(fin[1]), int(fin[2]); sh, cp = int(fin[3]), int(fin[4])
con = open(a.console, "rb").read().decode("latin1")
fault = "[Fault]" in con or "PANIC" in con
print("final    free=%d cowrefs=%d shared=%d copied=%d taken=%s retried=%s fault=%s" % (bf, refs, sh, cp, fin[5], fin[6], fault))
ok = bf == b0 and refs == 0 and sh > 0 and cp > 0 and bad == 0 and not fault
print("[cow-leak-gate] base=%d end=%d delta=%d %s" % (b0, bf, bf - b0, "PASS" if ok else "FAIL"))
sys.exit(0 if ok else 1)
