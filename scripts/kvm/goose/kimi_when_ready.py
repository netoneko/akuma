#!/usr/bin/env python3
"""Wait for the Kimi Code 5-hour window to reset, then start a goose-kimi session on the Mac.

    python3 scripts/kvm/goose/kimi_when_ready.py PROMPT_FILE LOG [--cwd DIR] [--name N] [--min-month-left 0.08]

Polls GET https://api.kimi.com/coding/v1/usages (token in ~/.akuma/kimi/token; only numbers are printed) until the
5-hour window has room, refuses to start when the monthly allowance left is below --min-month-left (a fraction;
the monthly figure is the scarce one: 37 tool calls cost about 13% of the month on 2026-10-28), then runs
scripts/goose_acp_prompt.py against the Kimi server on 127.0.0.1:3285 (see the runbook, "Kimi as a planning
second opinion"). Polling, no kill timeout; Ctrl-C to give up. Exit status is the driver's.
"""
import argparse
import json
import os
import pathlib
import subprocess
import sys
import time
import urllib.request

REPO = pathlib.Path(__file__).resolve().parents[2]
HOME = pathlib.Path.home()


def usage():
    tok = (HOME / ".akuma/kimi/token").read_text().strip()
    req = urllib.request.Request("https://api.kimi.com/coding/v1/usages", headers={"Authorization": "Bearer " + tok})
    d = json.load(urllib.request.urlopen(req, timeout=30))
    det = d["limits"][0]["detail"]
    used5 = int(det["used"]) if "used" in det else int(det["limit"]) - int(det["remaining"])
    return used5, int(det["limit"]), det["resetTime"], d["usages"]["limit_month_total"]["used_ratio"]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("prompt")
    ap.add_argument("log")
    ap.add_argument("--cwd", default=str(REPO))
    ap.add_argument("--name", default="kimi")
    ap.add_argument("--min-month-left", type=float, default=0.08)
    a = ap.parse_args()
    while True:
        try:
            used5, lim, reset, month = usage()
        except Exception as e:  # transient network trouble: keep polling
            print(f"{time.strftime('%H:%M:%S')} usage query failed ({type(e).__name__}); retrying", flush=True)
            time.sleep(120)
            continue
        left = 1 - month
        if left < a.min_month_left:
            print(f"month nearly spent ({month:.3f} used, {left:.3f} left < {a.min_month_left}): not starting", flush=True)
            return 4
        if used5 < lim * 0.5:
            break
        print(f"{time.strftime('%H:%M:%S')} 5h window {used5}/{lim} used, resets {reset}; month {month:.3f} used; waiting", flush=True)
        time.sleep(300)
    print(f"{time.strftime('%H:%M:%S')} window has room ({used5}/{lim}); month {month:.3f} used: starting {a.name}", flush=True)
    env = dict(os.environ, GOOSE_ACP="http://127.0.0.1:3285/acp", GOOSE_ACP_KEY_FILE=str(HOME / ".akuma/goose/acp_secret_key"))
    return subprocess.run([sys.executable, "-I", str(REPO / "scripts/goose_acp_prompt.py"), a.prompt, "--out", a.log,
                           "--name", a.name, "--cwd", a.cwd, "--budget", "43200"], env=env).returncode


if __name__ == "__main__":
    sys.exit(main())
