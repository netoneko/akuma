#!/usr/bin/env python3
"""Run the `execpost` static probe (exec from a non-leader thread into a threaded image that joins its threads) on guest 1's host root.

    ECO_KERNEL=<kernel> execpost.py PATH/TO/execpost [--n 3]
Expected on Linux: POST_JOIN_OK and rc=7 each time. A hang is reported as `timeout`.
"""
import argparse, subprocess, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent))
import launch

def main():
    ap = argparse.ArgumentParser(); ap.add_argument("binary", type=Path); ap.add_argument("--n", type=int, default=3); ap.add_argument("--variants", default="libc")
    a = ap.parse_args()
    run_dir = Path(tempfile.mkdtemp(prefix="eco-execpost-", dir=launch.WORK))
    d = launch.Demo(launch.WORK / "out", run_dir, 3072, 2)
    try:
        d.up((1,)); g = d.guests[1]; time.sleep(3)
        with a.binary.open("rb") as f:
            subprocess.run(g.ssh_argv("cat > /tmp/execpost && chmod 755 /tmp/execpost"), stdin=f, check=True)
        print(f"[X] kernel={launch.KERNEL.name}", flush=True)
        for i, variant in enumerate(v for v in a.variants.split(",") for _ in range(a.n)):
            try:
                rc, o = g.run("rm -f /tmp/o.txt; timeout 20 /tmp/execpost pre " + variant + " >/tmp/o.txt 2>&1; echo rc=$?; echo --out--; cat /tmp/o.txt", timeout=90)
            except subprocess.TimeoutExpired:
                rc, o = -1, "ssh timeout"
            print(f"[X] run {i} {variant}: " + " | ".join(o.split("\n")), flush=True)
            if "rc=" not in o or "rc=124" in o:
                _, p = g.run("ps -eo pid,ppid,time,stat,args | grep execpost | grep -v grep; for t in $(ps | grep 'execpost post' | grep -v grep | awk '{print $1}'); do echo pid $t tasks: $(ls /proc/$t/task 2>&1 | tr '\\n' ' '); grep -E 'State|Threads' /proc/$t/status; done; echo --out-now--; cat /tmp/o.txt")
                print(p, flush=True)
    finally:
        d.down()
main()
