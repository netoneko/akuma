#!/usr/bin/env python3
"""kami tui scroll cost on a live Akuma, over an ssh pty of a given size.

    python3 tui_scroll_ssh.py HOST URL [--cols 200 --rows 60 --keys 100 --gap 0.05 --wait 60 --key KEY]

Like scroll_try.py but for `kami tui`: sends `j` --keys times, then Ctrl-Q, and
summarises /tmp/kami-input.log (draw / parse / layout-build / snapshot ms) and
the bytes the terminal was sent. Needs the draw timing lines (kami >= 2026-10-10).
"""
import os, pty, sys, time, struct, fcntl, termios, select, subprocess, re, statistics
host, url = sys.argv[1:3]
o = dict(zip(sys.argv[3::2], sys.argv[4::2]))
cols, rows = int(o.get("--cols", 200)), int(o.get("--rows", 60))
keys, gap, wait = int(o.get("--keys", 100)), float(o.get("--gap", 0.05)), float(o.get("--wait", 60))
key = o.get("--key", os.path.expanduser("~/.ssh/id_ed25519"))
O = ["ssh", "-i", key, "-p", "2222", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
     "-o", "LogLevel=ERROR", "-o", "BatchMode=yes", f"root@{host}"]
def remote(cmd, t=90):
    return subprocess.run(O + [cmd], capture_output=True, text=True, timeout=t, stdin=subprocess.DEVNULL).stdout
remote("timeout 15 kami --kill >/dev/null 2>&1; rm -f /tmp/kami-input.log")
m, s = pty.openpty()
fcntl.ioctl(s, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, cols * 10, rows * 20))
p = subprocess.Popen(O[:1] + ["-tt"] + O[1:] + [f"kami tui --images blocks {url}"], stdin=s, stdout=s, stderr=s, close_fds=True)
os.close(s)
out = open(o["--out"], "wb") if "--out" in o else None
total = 0
def pump(t):
    global total
    end = time.time() + t
    while time.time() < end:
        if select.select([m], [], [], 0.01)[0]:
            try:
                b = os.read(m, 1 << 20)
            except OSError: return
            total += len(b)
            if out: out.write(b)
pump(wait)
before = total
if out: out.write(b"\n=====SCROLL=====\n")
t0 = time.time()
for _ in range(keys):
    os.write(m, b"j"); pump(gap)
pump(3)
print(f"scroll phase: {time.time()-t0:.1f}s, {(total-before)/1024:.0f} KB terminal output for {keys} keys")
try: os.write(m, b"\x11"); pump(1)
except OSError: pass
p.terminate()
txt = remote("cat /tmp/kami-input.log")
def ms(pat):
    v = [int(x) for x in re.findall(pat, txt)]
    return f"n={len(v)} median={statistics.median(v) if v else '-'} max={max(v) if v else '-'}"
print("draw ms     ", ms(r"draw: (\d+) ms"))
print("layout build", ms(r"rows at [\d.]+ px/col in (\d+) ms"))
print("parse ms    ", ms(r"parsed \d+ KB in (\d+) ms"))
print("snapshot ms ", ms(r"layout: snapshot (\d+) ms"))
print("snapshots during scroll:", len(re.findall(r"layout: snapshot", txt)))

# Key -> frame: a `tty chunk` line to the next `draw:` line (same clock, one thread).
lat, last = [], None
for line in txt.splitlines():
    m_ = re.match(r"\[\s*([\d.]+)\] (tty chunk|draw:)", line)
    if not m_: continue
    t_ = float(m_.group(1))
    if m_.group(2) == "tty chunk": last = t_
    elif last is not None: lat.append((t_ - last) * 1000); last = None
if lat: print(f"key->frame ms: median={statistics.median(lat):.0f} max={max(lat):.0f} n={len(lat)}")
# Snapshots that landed between the first and last scroll key.
ts = [float(x) for x in re.findall(r"\[\s*([\d.]+)\] tty chunk \[6a", txt)]
if ts:
    sn = [float(x) for x in re.findall(r"\[\s*([\d.]+)\] layout: snapshot", txt)]
    print("snapshots while scrolling:", sum(ts[0] <= x <= ts[-1] for x in sn))
