#!/usr/bin/env python3
"""Repro for the Denver `scala-cli run` hang: boot guest 1 only, start a Scala HTTP service in the scala box
with `scala-cli run --server=false`, and see whether /health answers.

    ECO_KERNEL=<kernel> overlays/ecosystem-demo/scalahang.py --project path/to/project.scala [--wait 300]

Exit 0 if the service answered, 1 if it never did (the hang). No agents, no credentials.
"""
import argparse
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import launch


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--project", type=Path, required=True, help="a project.scala file, or a directory copied to /work")
    ap.add_argument("--cmd", default="scala-cli run project.scala --server=false", help="command run in /work with PORT=8082")
    ap.add_argument("--wait", type=int, default=300)
    ap.add_argument("--out", type=Path, default=launch.WORK / "out")
    a = ap.parse_args()
    run_dir = Path(tempfile.mkdtemp(prefix="eco-scalahang-", dir=launch.WORK))
    d = launch.Demo(a.out, run_dir, 4096, 2)
    ok = False
    try:
        d.up((1,))
        g = d.guests[1]
        time.sleep(3)
        g.run("/sbin/eco-box exec scala scala true", check=True)
        up = "/var/lib/box/containers/scala/upper/work"
        if a.project.is_dir():
            tar = subprocess.Popen(["tar", "-C", str(a.project), "-czf", "-", "."], stdout=subprocess.PIPE)
            subprocess.run(g.ssh_argv(f"tar xzf - -C {up}"), stdin=tar.stdout, check=True)
            tar.wait()
        else:
            with a.project.open("rb") as f:
                subprocess.run(g.ssh_argv(f"cat > {up}/project.scala"), stdin=f, check=True)
        g.run(f"/sbin/eco-box daemon scala scala 'cd /work && PORT=8082 {a.cmd} > /tmp/out.log 2>&1'")
        t0 = time.monotonic()
        while time.monotonic() - t0 < a.wait:
            _, o = g.run("curl -s -m 3 http://127.0.0.1:8082/health")
            if '"ok"' in o:
                ok = True
                break
            time.sleep(5)
        print(f"[S] kernel={launch.KERNEL.name} health={'ANSWERED' if ok else 'NEVER'} after {time.monotonic() - t0:.0f}s", flush=True)
        if not ok:
            _, o = g.run("ps -eo pid,time,stat,args | grep -E 'java|dotty' | grep -v grep | cut -c1-120 | head -6; tail -c 600 /var/lib/box/containers/scala/upper/tmp/out.log")
            print(o, flush=True)
    finally:
        d.down()
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
