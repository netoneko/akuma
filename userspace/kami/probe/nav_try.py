#!/usr/bin/env python3
"""Scroll and click through two long pages that link to each other.

    python3 nav_try.py HOST [--hops 6] [--scroll 3]

Needs /tmp/pageA.html and /tmp/pageB.html on the box (`testdata/`; each is 40
rows tall with one link at the end to the other). From a cold Chromium, headless
(`kami --fb none` over an ssh pty): load pageA, then HOPS times: `j` x SCROLL,
`G` (bottom), `f` (hints), `a` (the only link on screen), then wait for the
page to change. Scores, from /tmp/kami-input.log in *line order* (the file is
written by one thread; its timestamps can skew by ~1 s across cores):
  - each hop: did `top frame now` name the other page within the wait?
  - frames presented after every key and after every navigation.
Exit status 0 only if every hop landed and kami exited cleanly.
"""
import os, pty, re, select, subprocess, sys, time
host = sys.argv[1]
hops, scroll, wait_nav = 6, 3, 25
a = sys.argv[2:]
while a:
    x = a.pop(0)
    if x == "--hops": hops = int(a.pop(0))
    elif x == "--scroll": scroll = int(a.pop(0))
O = ["ssh", "-p", "2222", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
     "-o", "LogLevel=ERROR", "-o", "BatchMode=yes", "-o", "ConnectTimeout=15", f"root@{host}"]
def remote(c, t=60):
    return subprocess.run(O + [c], capture_output=True, text=True, timeout=t, stdin=subprocess.DEVNULL).stdout
remote("timeout 15 kami --kill >/dev/null 2>&1; killall chromium chrome_crashpad_handler 2>/dev/null; sleep 1; "
       "rm -rf /tmp/kami-profile/Singleton*; rm -f /tmp/kami-input.log", 90)
m, s = pty.openpty()
p = subprocess.Popen(O[:1] + ["-tt"] + O[1:] + ["kami --fb none file:///tmp/pageA.html"], stdin=s, stdout=s, stderr=s, close_fds=True)
os.close(s)
def drain(sec):
    end = time.time() + sec
    while time.time() < end:
        r, _, _ = select.select([m], [], [], 0.05)
        if r:
            try: os.read(m, 65536)
            except OSError: return
def tops():
    return re.findall(r"top frame now (\S+)", remote("cat /tmp/kami-input.log"))
t0 = time.time(); ready = False
while time.time() - t0 < 90 and p.poll() is None:
    drain(3)
    if "ready for frames" in remote("grep -a 'ready for frames' /tmp/kami-input.log 2>/dev/null"):
        ready = True; break
print(f"ready: {ready} after {time.time()-t0:.0f} s")
if not ready:
    p.kill(); sys.exit(1)
drain(4)
want = ["pageB", "pageA"]
landed = 0
for h in range(hops):
    for _ in range(scroll): os.write(m, b"j"); drain(0.6)
    os.write(m, b"G"); drain(2.5)
    os.write(m, b"f"); drain(2.5)
    os.write(m, b"a")
    target = want[h % 2]; ok = False; t1 = time.time()
    while time.time() - t1 < wait_nav:
        drain(1.5)
        t = tops()
        if t and target in t[-1]: ok = True; break
    landed += ok
    print(f"hop {h+1}: -> {target}: {'OK' if ok else 'FAILED'} after {time.time()-t1:.1f} s")
    if not ok:
        # Freeze-frame of the hung renderer(s): per-thread state and CPU, twice.
        snap = ('for f in $(grep -l -a "type=renderer" /proc/[0-9]*/cmdline 2>/dev/null); do p=${f%/cmdline}; '
                'echo "renderer $(basename $p)"; for t in $p/task/*; do echo "  $(basename $t) $(cut -d" " -f2,3,14,15 $t/stat 2>/dev/null)"; done; done')
        print("---- renderer threads at the hang ----"); print(remote(snap, 120))
        drain(3)
        print("---- 3 s later ----"); print(remote(snap, 120))
        break
    drain(3)
os.write(m, b"\x11"); drain(2)
log = remote("cat /tmp/kami-input.log")
p.kill()
# line-order check: a frame after every input line
lines = log.splitlines()
keys = [i for i, l in enumerate(lines) if "input Text(" in l or "input Key(" in l]
frames = [i for i, l in enumerate(lines) if "] presented" in l]
no = [k for k in keys if not any(f > k for f in frames)]
print(f"inputs {len(keys)}, frames {len(frames)}, inputs with no later frame: {len(no)}")
print(f"hops landed {landed}/{hops}")
sys.exit(0 if landed == hops else 1)
