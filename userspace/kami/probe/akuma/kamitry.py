#!/usr/bin/env python3
"""In-guest: run kami itself (daemon + screencast client, --fb none) on a pty,
the way scroll_try.py drives it over ssh on the metal, and report what it saw.

    kamitry.py URL [seconds] [keys]      keys: comma list, e.g. j,j,j,j

Prints "== kami: T<monotonic>" lines (kernel-time stamped, so they line up
with the serial log's [T..] lines): ready-for-frames, each key sent, and how
kami ended. Then the input log minus the tty noise and the daemon/crash lines
of /tmp/kami.log.
"""
import os, pty, select, subprocess, sys, time

url = sys.argv[1] if len(sys.argv) > 1 else "https://www.tumblr.com/"
dur = float(sys.argv[2]) if len(sys.argv) > 2 else 60
keys = (sys.argv[3] if len(sys.argv) > 3 else "j,j,j,j").split(",")
def T(): return time.clock_gettime(time.CLOCK_MONOTONIC)
def say(*a): print("== kami: T%.2f" % T(), *a, flush=True)

for f in ("/tmp/kami-input.log", "/tmp/kami.log"):
    try: os.unlink(f)
    except OSError: pass
m, s = pty.openpty()
env = dict(os.environ, PATH="/usr/bin:/bin:/usr/sbin:/sbin", HOME="/tmp", TERM="xterm")
p = subprocess.Popen(["/kami", "--fb", "none", "--chromium", "/usr/lib/chromium/chromium", url],
                     stdin=s, stdout=s, stderr=s, close_fds=True, env=env)
os.close(s)
tty = open("/tmp/kami-tty.log", "ab")

def drain(sec):
    end = T()
    end += sec
    while T() < end:
        r, _, _ = select.select([m], [], [], 0.05)
        if r:
            try: tty.write(os.read(m, 65536))
            except OSError: return

def inlog():
    try: return open("/tmp/kami-input.log", "rb").read().decode("utf-8", "replace")
    except OSError: return ""

t0 = T(); ready = False
while T() - t0 < 120 and p.poll() is None:
    drain(2)
    if "ready for frames" in inlog(): ready = True; break
say("ready:", ready, "after %.0f s" % (T() - t0), "kami rc" if p.poll() is not None else "", p.returncode if p.poll() is not None else "")
if ready:
    drain(6)
    for k in keys:
        b = {"j": b"j", "k": b"k", "G": b"G", "gg": b"gg", "d": b"d", "u": b"u"}.get(k, k.encode())
        os.write(m, b); say("key", k); drain(1.0)
    remaining = dur - (T() - t0)
    if remaining > 0: drain(remaining)
if p.poll() is None:
    os.write(m, b"\x11"); say("quit key sent"); drain(4)
if p.poll() is None:
    say("kami did not exit; killing"); p.kill()
p.wait()
say("kami rc", p.returncode)
log = inlog().splitlines()
keep = [l for l in log if "tty bytes" not in l and "tty chunk" not in l]
print("== kami input log: %d lines, %d presented" % (len(log), sum("presented" in l for l in log)), flush=True)
for l in keep[:40]: print("   ", l[:150], flush=True)
if len(keep) > 60: print("    ...")
for l in keep[-20:]: print("   ", l[:150], flush=True)
print("== kami.log daemon / crash lines", flush=True)
try:
    for l in open("/tmp/kami.log", "rb").read().decode("utf-8", "replace").splitlines():
        if any(k in l for k in ("kami-daemon", "FATAL", "crash", "Check failed", "terminated", "Received signal")):
            print("   ", l[:180], flush=True)
except OSError: print("    (no /tmp/kami.log)")
