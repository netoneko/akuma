#!/usr/bin/env python3
"""Hammer the host relay (`kvmfwd=`, port 2223) of a running service guest and report the
first thing that stops answering. Works against the QEMU rehearsal (127.0.0.1) and the ryzen.

    scripts/kvm/relay_stress.py HOST [--port 2223] [--key KEY] [--rounds N]

Phases, each followed by a real ssh that must still get a banner and run a command:
overlap (3 sessions on 2 slots), connect-and-drop, silent holds beyond the slots, garbage
then close. The 2026-10-22 ryzen wedge (slot 1 stuck `connecting`, then the port refused)
left no trace of how it began; this is the shape that should reproduce it.
"""
import argparse, os, socket, subprocess, sys, time

ap = argparse.ArgumentParser()
ap.add_argument("host")
ap.add_argument("--port", type=int, default=2223)
ap.add_argument("--key")
ap.add_argument("--rounds", type=int, default=2)
ap.add_argument("--timeout", type=int, default=60)
a = ap.parse_args()

base = ["ssh", "-p", str(a.port), "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
        "-o", "BatchMode=yes", "-o", "LogLevel=ERROR", "-o", f"ConnectTimeout={a.timeout}"]
if a.key:
    base += ["-i", a.key, "-o", "IdentitiesOnly=yes"]
base.append(f"root@{a.host}")

def ssh(cmd, timeout=None):
    try:
        r = subprocess.run(base + [cmd], capture_output=True, text=True, timeout=timeout or a.timeout + 20)
        return r.stdout.strip() or f"ERR rc={r.returncode} " + r.stderr.strip()[-80:]
    except subprocess.TimeoutExpired:
        return "TIMEOUT"

def health(tag):
    t = time.time()
    out = ssh("echo healthy")
    print(f"  health after {tag}: {out} ({time.time() - t:.1f}s)", flush=True)
    return out == "healthy"

def conn():
    return socket.create_connection((a.host, a.port), timeout=a.timeout)

if not health("start"):
    sys.exit("guest not answering before the test started")
for r in range(a.rounds):
    print(f"round {r}", flush=True)
    ps = [subprocess.Popen(base + [f"sleep {3 + i}; echo s{i}"], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True) for i in range(3)]
    outs = []
    for p in ps:
        try:
            o, e = p.communicate(timeout=a.timeout + 30)
            outs.append(o.strip() or "ERR " + e.strip()[-60:])
        except subprocess.TimeoutExpired:
            p.kill()
            outs.append("TIMEOUT")
    print("  overlap x3:", outs, flush=True)
    if not health("overlap"): sys.exit(1)
    for _ in range(8):
        try: conn().close()
        except OSError as e: print("  connect-drop:", e)
    if not health("connect-and-drop"): sys.exit(1)
    held = []
    for _ in range(4):
        try: held.append(conn())
        except OSError as e: print("  hold:", e)
    time.sleep(5)
    for c in held:
        try: c.sendall(b"garbage\r\n")
        except OSError: pass
        c.close()
    if not health("silent holds"): sys.exit(1)
    for _ in range(3):
        try:
            c = conn(); c.sendall(os.urandom(300)); c.close()
        except OSError as e: print("  garbage:", e)
    time.sleep(2)
    if not health("garbage"): sys.exit(1)
print("relay_stress: PASS")
