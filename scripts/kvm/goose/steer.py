#!/usr/bin/env python3
"""Send a steer to a running goose session and CONFIRM it arrived.

    python3 scripts/kvm/goose/steer.py SESSION_ID TEXT_FILE [--confirm-secs 45]

1. runs scripts/goose_acp_steer.py with a fresh marker (no short timeout: it legitimately waits up to ~150 s);
   a failure prints "NOT DELIVERED ..." and exits nonzero - silence never means success;
2. copies steer_seen.py to the box and requires the marked message to be in goose's session database
   (retried for --confirm-secs);
3. prints whether the agent has had a turn since ("consumed=yes|no"). `no` is normal while the agent is inside
   a long tool or model call: it reads steers at its next turn. Ask again later with --check-only TAG.

Exit 0 only when the steer is in the session database.
"""
import argparse
import pathlib
import subprocess
import sys
import time

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from ak import run  # noqa: E402

HERE = pathlib.Path(__file__).resolve().parent
REPO = HERE.parents[1]


def seen(sid, tag):
    r = run(f"python3 /tmp/steer_seen.py {sid} {tag} < /dev/null")
    return r.returncode == 0, (r.stdout + r.stderr).strip()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("session")
    ap.add_argument("text", nargs="?")
    ap.add_argument("--confirm-secs", type=int, default=90, help="how long to poll for the message to show up")
    ap.add_argument("--check-only", metavar="TAG", help="only look the marker up (and whether it was consumed)")
    a = ap.parse_args()

    r = run("cat > /tmp/steer_seen.py", (HERE / "steer_seen.py").read_text())
    if r.returncode != 0:
        print("steer.py: cannot reach the box:", (r.stderr or r.stdout).strip()[:200])
        return 3
    if a.check_only:
        ok, out = seen(a.session, a.check_only)
        print(out)
        return 0 if ok else 1

    if not a.text:
        ap.error("TEXT_FILE is required unless --check-only is used")
    tag = time.strftime("%H%M%S")
    p = subprocess.run([sys.executable, "-I", str(REPO / "scripts/goose_acp_steer.py"), a.session, a.text, "--marker", tag],
                       capture_output=True, text=True)
    sys.stdout.write(p.stdout)
    if p.returncode != 0:
        sys.stderr.write(p.stderr[-400:])
        print("steer.py: NOT DELIVERED (see above)")
        return 1
    end = time.time() + a.confirm_secs
    out = ""
    while time.time() < end:
        ok, out = seen(a.session, tag)
        if ok:
            print(f"steer.py: {out} (marker {tag}; check later with --check-only {tag})")
            return 0
        time.sleep(5)
    print(f"steer.py: ACK BUT NOT IN THE SESSION DATABASE after {a.confirm_secs} s: {out}")
    return 1


if __name__ == "__main__":
    sys.exit(main())
