#!/usr/bin/env python3
"""kami tui scroll cost on this machine (no Akuma): real Chrome, a pty of a given size.

    python3 tui_scroll_local.py KAMI_BIN CHROME_BIN URL [--cols 200 --rows 60 --keys 150 --gap 0.03 --wait 25]

Starts `kami tui` in a pty, waits, sends `j` --keys times --gap apart, then Ctrl-Q,
and prints the log lines that cost time (layout parse/build, draw, snapshot).
Output volume the terminal had to take is counted too.
"""
import os, pty, sys, time, struct, fcntl, termios, select, subprocess, tempfile, re, statistics
kami, chrome, url = sys.argv[1:4]
o = dict(zip(sys.argv[4::2], sys.argv[5::2]))
cols, rows = int(o.get("--cols", 200)), int(o.get("--rows", 60))
keys, gap, wait = int(o.get("--keys", 150)), float(o.get("--gap", 0.03)), float(o.get("--wait", 25))
d = tempfile.mkdtemp(prefix="kami-local-")
os.symlink(chrome, f"{d}/chromium")
log = f"{d}/input.log"
env = dict(os.environ, PATH=f"{d}:{os.environ['PATH']}", KAMI_INPUT_LOG=log, KAMI_IMAGES="blocks", TERM="xterm-256color")
m, s = pty.openpty()
fcntl.ioctl(s, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, cols * 10, rows * 20))
p = subprocess.Popen([kami, "tui", "--sock", f"{d}/k.sock", "--chrome-arg", f"--user-data-dir={d}/prof", "--chrome-arg", "--headless=new", url],
                     stdin=s, stdout=s, stderr=s, env=env, close_fds=True)
os.close(s)
total = 0
def pump(t):
    global total
    end = time.time() + t
    while time.time() < end:
        if select.select([m], [], [], 0.01)[0]:
            try: total += len(os.read(m, 1 << 20))
            except OSError: return
pump(wait)
before = total
t0 = time.time()
for _ in range(keys):
    os.write(m, b"j"); pump(gap)
pump(2)
print(f"scroll phase: {time.time()-t0:.1f}s, {(total-before)/1024:.0f} KB of terminal output for {keys} keys")
os.write(m, b"\x11"); pump(1)
p.terminate()
txt = open(log, errors="replace").read()
open(f"{d}/input.log.txt", "w").write(txt)
def ms(pat):
    v = [int(x) for x in re.findall(pat, txt)]
    return f"n={len(v)} median={statistics.median(v) if v else '-'} max={max(v) if v else '-'}"
print("draw ms     ", ms(r"draw: (\d+) ms"))
print("layout build", ms(r"rows at [\d.]+ px/col in (\d+) ms"))
print("parse ms    ", ms(r"parsed \d+ KB in (\d+) ms"))
print("snapshot ms ", ms(r"layout: snapshot (\d+) ms"))
print("log:", f"{d}/input.log.txt")
