#!/usr/bin/env python3
"""Load a URL on a live Akuma N times without touching its screen, and fetch the
final frame of each run.

    python3 page_try.py HOST URL [--runs 3] [--hold 40] [--out DIR] [--keep]
                        [--arg "--chrome-arg=..."]...

Runs `kami --fb none --seconds HOLD URL` (the null display: nothing is painted
and the console and keyboard are left alone), reports per run whether Chromium
survived, the time to first pixels and any `!!` stall, and saves the last frame
as DIR/run-N.png (open it to see what Chromium drew). Each run starts from a
cold Chromium unless --keep. Writes /tmp/kami-input.log and owns the kami daemon
on the box.
"""
import base64, os, re, subprocess, sys

host, url = sys.argv[1], sys.argv[2]
runs, hold, out, keep, extra = 3, 40, "/tmp/kami-page-try", False, []
a = sys.argv[3:]
while a:
    x = a.pop(0)
    if x == "--runs": runs = int(a.pop(0))
    elif x == "--hold": hold = int(a.pop(0))
    elif x == "--out": out = a.pop(0)
    elif x == "--keep": keep = True
    elif x == "--arg": extra.append(a.pop(0))
os.makedirs(out, exist_ok=True)
O = ["ssh", "-p", "2222", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
     "-o", "LogLevel=ERROR", "-o", "BatchMode=yes", "-o", "ConnectTimeout=15", f"root@{host}"]

def rem(c, t=240):
    try:
        r = subprocess.run(O + [c], capture_output=True, text=True, timeout=t, stdin=subprocess.DEVNULL)
        return r.stdout + r.stderr
    except subprocess.TimeoutExpired:
        return "(TIMEOUT)"

for i in range(1, runs + 1):
    clean = "" if keep else ("timeout 15 kami --kill >/dev/null 2>&1; killall chromium chrome_crashpad_handler 2>/dev/null; sleep 1; "
                             "rm -rf /tmp/kami-profile/Singleton*; ")
    r = rem(clean + f"rm -f /tmp/kami-input.log /tmp/pt.png; KAMI_DUMP=/tmp/pt.png kami --fb none --seconds {hold} {' '.join(extra)} {url} >/dev/null 2>&1; "
            "grep -a -E 'ready for frames|FIRST PIXELS|session done|!!|top frame' /tmp/kami-input.log | cut -c1-160; "
            "grep -a 'kami-daemon' /tmp/kami.log | tail -2", hold + 120)
    ready = re.search(r"ready for frames, (\d+) ms", r)
    done = re.search(r"session done: (\S+)", r)
    died = "SIGSEGV" in r or "SIGTRAP" in r or (done and done.group(1) != "None")
    png = rem("base64 /tmp/pt.png 2>/dev/null")
    got = False
    try:
        d = base64.b64decode("".join(png.split()), validate=True)
        if d[:4] == b"\x89PNG":
            open(f"{out}/run-{i}.png", "wb").write(d); got = True
    except Exception:
        pass
    last = [l for l in r.splitlines() if "kami-daemon" in l][-1:] or [""]
    print(f"run {i}: {'DIED' if died else 'survived'}, ready {ready.group(1) if ready else None} ms, frame {'saved' if got else 'none'}; {last[0][:90]}", flush=True)
    for l in r.splitlines():
        if "top frame" in l or "!!" in l:
            print("   ", l[:150])
