#!/usr/bin/env python3
"""Scroll responsiveness on a live Akuma, headless: how long from a scroll key
to the next frame?

    python3 scroll_try.py HOST URL [--slow 10] [--burst 20] [--gap 0.1] [--wait 120] [--settle 6]

Runs `kami --fb none URL` over an ssh pty (nothing is painted; the console is
left alone), waits for `ready for frames`, then sends `j` (one 120 px wheel
tick) `--slow` times one second apart, then `--burst` times `--gap` seconds
apart, then Ctrl-Q. From /tmp/kami-input.log it reports, per phase, the time
from each `input Text("j")` line to the first `presented` line after it, and
how many frames arrived within 1 s of it.

The two clocks in that log can differ by up to ~1 s across cores (README, "Logs
and telemetry"), so single values near 0.99 s are skew; read the median.
"""
import os, pty, re, select, statistics, subprocess, sys, time

host, url = sys.argv[1], sys.argv[2]
slow, burst, gap, wait, settle = 10, 20, 0.1, 120, 6
a = sys.argv[3:]
while a:
    x = a.pop(0)
    if x == "--slow": slow = int(a.pop(0))
    elif x == "--burst": burst = int(a.pop(0))
    elif x == "--gap": gap = float(a.pop(0))
    elif x == "--wait": wait = int(a.pop(0))
    elif x == "--settle": settle = int(a.pop(0))
O = ["ssh", "-p", "2222", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
     "-o", "LogLevel=ERROR", "-o", "BatchMode=yes", "-o", "ConnectTimeout=15", f"root@{host}"]

def remote(cmd, t=60):
    return subprocess.run(O + [cmd], capture_output=True, text=True, timeout=t, stdin=subprocess.DEVNULL).stdout

remote("timeout 15 kami --kill >/dev/null 2>&1; killall chromium chrome_crashpad_handler 2>/dev/null; sleep 1; "
       "rm -rf /tmp/kami-profile/Singleton*; rm -f /tmp/kami-input.log", 90)  # a cold Chromium, as page_try does
m, s = pty.openpty()
p = subprocess.Popen(O[:1] + ["-tt"] + O[1:] + [f"kami --fb none {url}"], stdin=s, stdout=s, stderr=s, close_fds=True)
os.close(s)

def drain(sec):
    end = time.time() + sec
    while time.time() < end:
        r, _, _ = select.select([m], [], [], 0.05)
        if r:
            try: os.read(m, 65536)
            except OSError: return

t0 = time.time()
ready = False
while time.time() - t0 < wait and p.poll() is None:
    drain(3)
    if "ready for frames" in remote("grep -a 'ready for frames' /tmp/kami-input.log 2>/dev/null"):
        ready = True
        break
print(f"ready: {ready} after {time.time() - t0:.0f} s")
if not ready:
    p.kill(); sys.exit(1)
drain(settle)  # let the page's own frames settle
marks = {}
for phase, n, g in (("slow", slow, 1.0), ("burst", burst, gap)):
    marks[phase] = n
    for _ in range(n):
        os.write(m, b"j"); drain(g)
    drain(3)
os.write(m, b"\x11"); drain(2)
log = remote("cat /tmp/kami-input.log")
p.kill()

# "A frame after this key" is decided in LINE order, not by timestamp: the log
# is written by one thread, but its clock skews by up to ~1 s across cores, so
# a frame can carry a timestamp earlier than the key that caused it (seen
# 2026-10-09: key at 17.417 s, its frame at 16.529 s) and a timestamp-only
# match scored a delivered frame as missing. The latencies still come from the
# timestamps (the only clock there is); a skewed pair shows up as a negative
# or ~1 s value, which the median absorbs.
ev = []  # (line index, t, kind)
for i, l in enumerate(log.splitlines()):
    mm = re.match(r"\[\s*([\d.]+)\] (.*)", l)
    if not mm: continue
    t, rest = float(mm.group(1)), mm.group(2)
    if rest.startswith('input Text("j")'): ev.append((i, t, "key"))
    elif rest.startswith("presented"):
        ev.append((i, t, "frame"))
keys = [(i, t) for i, t, k in ev if k == "key"]
frames = [(i, t) for i, t, k in ev if k == "frame"]
print(f"keys seen {len(keys)}, frames {len(frames)} in total")
i = 0
for phase in ("slow", "burst"):
    n = marks[phase]
    ks = keys[i:i + n]; i += n
    lat, cnt = [], []
    for ki, k in ks:
        nxt = [ft for fi, ft in frames if fi > ki]
        lat.append((nxt[0] - k) if nxt else float("nan"))
        cnt.append(len([ft for _, ft in frames if k <= ft < k + 1.0]))
    ok = [x for x in lat if x == x]
    if ok:
        print(f"{phase}: n={len(ks)} key->frame median {statistics.median(ok)*1000:.0f} ms, "
              f"max {max(ok)*1000:.0f} ms, no-frame {len(lat)-len(ok)}; frames/1s median {statistics.median(cnt):.0f}")
    else:
        print(f"{phase}: n={len(ks)} NO FRAMES AFTER ANY KEY")
print("last lines:"); print("\n".join(log.splitlines()[-3:]))
