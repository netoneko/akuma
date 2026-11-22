#!/usr/bin/env python3
"""Does wait4 from a thread other than the forking one see the child exit? (the JVM's `process reaper` pattern)

    ECO_KERNEL=<kernel> overlays/ecosystem-demo/reaperprobe.py

Boots guest 2 only and runs four spawn/wait thread layouts in its python box; prints REAPED/HUNG per layout.
"""
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import launch
from verify import bx

PROBE = r'''
import os, subprocess, threading, time, sys
def case(name, spawn_thread, wait_thread, cmd):
    res = {}
    def spawn():
        res["p"] = subprocess.Popen(cmd)
    if spawn_thread == "main":
        spawn()
    else:
        t = threading.Thread(target=spawn); t.start(); t.join()
    pid = res["p"].pid
    out = []
    def wait():
        out.append(os.waitpid(pid, 0))
    if wait_thread == "main":
        # emulate a bounded wait in the main thread with WNOHANG polling is NOT the test; do a blocking wait in an alarm-guarded way
        import signal
        signal.signal(signal.SIGALRM, lambda *a: (_ for _ in ()).throw(TimeoutError()))
        signal.alarm(15)
        try:
            out.append(os.waitpid(pid, 0))
        except TimeoutError:
            pass
        signal.alarm(0)
    else:
        t = threading.Thread(target=wait, daemon=True); t.start(); t.join(15)
    print("%-34s %s" % (name, "REAPED" if out else "HUNG (child exited, wait4 never returned)"), flush=True)
case("spawn main, wait thread, sleep 1", "main", "thread", ["sleep", "1"])
case("spawn main, wait thread, exit now", "main", "thread", ["true"])
case("spawn thread, wait main, sleep 1", "thread", "main", ["sleep", "1"])
case("spawn thread, wait thread, sleep 1", "thread", "thread", ["sleep", "1"])
case("spawn main, wait main (control)", "main", "main", ["sleep", "1"])
os._exit(0)
'''


def main():
    run_dir = Path(tempfile.mkdtemp(prefix="eco-reaper-", dir=launch.WORK))
    d = launch.Demo(launch.WORK / "out", run_dir, 3072, 2)
    try:
        d.up((2,))
        g = d.guests[2]
        time.sleep(3)
        g.run("/sbin/eco-box exec python python true", check=True)
        subprocess.run(g.ssh_argv("cat > /var/lib/box/containers/python/upper/work/probe.py"), input=PROBE.encode(), check=True)
        rc, o = bx(g, "python", "python", "cd /work && python3 probe.py", timeout=300)
        print(f"[P] kernel={launch.KERNEL.name}\n{o}", flush=True)
    finally:
        d.down()


if __name__ == "__main__":
    main()
