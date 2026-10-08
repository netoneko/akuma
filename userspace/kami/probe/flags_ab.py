#!/usr/bin/env python3
"""A/B Chromium flag sets for kami on a live Akuma: N cold starts per arm.

    python3 flags_ab.py HOST [--runs 4] [--hold 25] [--arm name ...]

Each run: kill any Chromium, start `kami --seconds HOLD URL` with the arm's
extra `--chrome-arg`s, and read /tmp/kami-input.log. A run SURVIVES if the
session ended on its own clock (`session done: None`), and DIED if the daemon
went away first. Prints survivors and the time to first pixels per arm. Paints
on /dev/fb0 and owns the kami daemon on the box, so do not run it while
someone is using kami there.
"""
import subprocess, sys, re

ARMS = {
    "base": [],
    "slim": [
        "--disable-background-networking", "--disable-sync", "--disable-extensions",
        "--disable-component-update", "--disable-default-apps", "--no-default-browser-check",
        "--disable-client-side-phishing-detection", "--disable-domain-reliability",
        "--disable-features=Translate,MediaRouter,OptimizationHints,BackForwardCache,AcceptCHFrame,InterestFeedContentSuggestions",
    ],
    "slim+nocrash": [
        "--disable-background-networking", "--disable-sync", "--disable-extensions",
        "--disable-component-update", "--disable-default-apps", "--no-default-browser-check",
        "--disable-client-side-phishing-detection", "--disable-domain-reliability",
        "--disable-features=Translate,MediaRouter,OptimizationHints,BackForwardCache,AcceptCHFrame,InterestFeedContentSuggestions",
        "--disable-breakpad", "--disable-crash-reporter",
    ],
}

host = sys.argv[1]
runs, hold, only = 4, 25, []
a = sys.argv[2:]
while a:
    x = a.pop(0)
    if x == "--runs": runs = int(a.pop(0))
    elif x == "--hold": hold = int(a.pop(0))
    elif x == "--arm": only.append(a.pop(0))
O = ["ssh", "-p", "2222", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
     "-o", "LogLevel=ERROR", "-o", "BatchMode=yes", "-o", "ConnectTimeout=15", f"root@{host}"]

def rem(c, t=240):
    try:
        r = subprocess.run(O + [c], capture_output=True, text=True, timeout=t, stdin=subprocess.DEVNULL)
        return r.stdout + r.stderr
    except subprocess.TimeoutExpired:
        return "(TIMEOUT)"

for name, flags in ARMS.items():
    if only and name not in only: continue
    args = " ".join(f"--chrome-arg {f}" for f in flags)
    res = []
    for i in range(runs):
        out = rem(
            "timeout 15 kami --kill >/dev/null 2>&1; killall chromium chrome_crashpad_handler 2>/dev/null; "
            "sleep 1; rm -rf /tmp/kami-profile/Singleton* /tmp/kami-input.log /tmp/kami.log; "
            f"kami --seconds {hold} {args} http://example.com >/dev/null 2>&1; "
            "grep -a -E 'ready for frames|FIRST PIXELS|session done' /tmp/kami-input.log; "
            "echo gpu-errors: $(grep -a -c 'GPU process' /tmp/kami.log); "
            "echo left-over: $(ps | grep -E 'chromium|crashpad' | grep -v grep | grep -c .)", t=hold + 120)
        ready = re.search(r"ready for frames, (\d+) ms", out)
        done = re.search(r"session done: (\w+)", out)
        gpu = re.search(r"gpu-errors: (\d+)", out)
        survived = bool(done and done.group(1) == "None")
        res.append((survived, int(ready.group(1)) if ready else None, int(gpu.group(1)) if gpu else -1))
        print(f"  [{name}] run {i + 1}: {'survived' if survived else 'DIED'}, ready {res[-1][1]} ms, gpu-errors {res[-1][2]}", flush=True)
    ok = [r for r in res if r[0]]
    ts = [r[1] for r in res if r[1] is not None]
    print(f"{name}: {len(ok)}/{len(res)} survived; ready ms {sorted(ts)}", flush=True)
