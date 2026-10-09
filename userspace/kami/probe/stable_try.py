#!/usr/bin/env python3
"""Is kami stable on a local page? N cold starts, each: load URL, scroll, score.

    python3 stable_try.py HOST [URL] [--runs 8] [--keys 4]

A run PASSES when kami got ready for frames, every one of `--keys` scroll keys
(1 s apart) was followed by a frame, and kami exited cleanly. Failures keep the
tail of /tmp/kami-input.log and the daemon/Chromium lines of that run in
OUT/fail-N.txt (OUT = /tmp/kami-stable) so the cause survives the next run.
Drives `scroll_try.py` (a cold Chromium each time, nothing painted on the fb).
"""
import os, re, subprocess, sys
host = sys.argv[1]
url, runs, keys = "file:///tmp/render.html", 8, 4
a = sys.argv[2:]
while a:
    x = a.pop(0)
    if x == "--runs": runs = int(a.pop(0))
    elif x == "--keys": keys = int(a.pop(0))
    else: url = x
out = "/tmp/kami-stable"; os.makedirs(out, exist_ok=True)
here = os.path.dirname(os.path.abspath(__file__))
O = ["ssh", "-p", "2222", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
     "-o", "LogLevel=ERROR", "-o", "BatchMode=yes", "-o", "ConnectTimeout=15", f"root@{host}"]
def rem(c, t=60):
    return subprocess.run(O + [c], capture_output=True, text=True, timeout=t, stdin=subprocess.DEVNULL).stdout
passed = 0
for i in range(1, runs + 1):
    try:
        r = subprocess.run([sys.executable, f"{here}/scroll_try.py", host, url, "--slow", str(keys), "--burst", "0", "--settle", "4", "--wait", "90"],
                           capture_output=True, text=True, timeout=300).stdout
    except subprocess.TimeoutExpired:
        r = "TIMEOUT"
    ready = "ready: True" in r
    m = re.search(r"slow: n=\d+ key->frame median (\d+) ms.*no-frame (\d+)", r)
    ok = ready and m is not None and m.group(2) == "0"
    passed += ok
    print(f"run {i}: {'PASS' if ok else 'FAIL'}  ready={ready}  " + (f"median {m.group(1)} ms" if m else r.strip().splitlines()[-1][:100] if r.strip() else "no output"), flush=True)
    if not ok:
        open(f"{out}/fail-{i}.txt", "w").write(r + "\n---- input log tail ----\n" + rem("tail -40 /tmp/kami-input.log")
            + "\n---- kami.log (daemon + errors) ----\n" + rem("grep -a -E 'kami-daemon|FATAL|crash|SIG|Check failed|terminated' /tmp/kami.log | tail -25"))
print(f"{passed}/{runs} passed")
