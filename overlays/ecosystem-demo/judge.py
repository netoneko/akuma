#!/usr/bin/env python3
"""Re-judge saved agent work with the reference acceptance suite.

    overlays/ecosystem-demo/judge.py --rust RUNDIR --scala RUNDIR [--ref ../akuma-demo-reference-impl]

Boots both guests, restores B's and C's committed workspaces (`<rundir>/{rust,scala}-workspace.tgz`) into fresh
boxes, starts both services from their run.sh, and runs the reference `tests/test_remote.py` from guest 2.
No agent runs and no model credentials are involved.
"""
import argparse
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import launch
from verify import bx


def restore(g, box, tgz: Path):
    g.run(f"/sbin/eco-box exec {box} {box} true", check=True)
    with tgz.open("rb") as f:
        r = subprocess.run(g.ssh_argv(f"tar xzf - -C /var/lib/box/containers/{box}/upper"), stdin=f, capture_output=True, timeout=300)
    assert r.returncode == 0, r.stderr


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--rust", type=Path, required=True)
    ap.add_argument("--scala", type=Path, required=True)
    ap.add_argument("--ref", type=Path, default=Path("../akuma-demo-reference-impl"))
    ap.add_argument("--out", type=Path, default=launch.WORK / "out")
    a = ap.parse_args()
    run_dir = Path(tempfile.mkdtemp(prefix="eco-judge-", dir=launch.WORK))
    d = launch.Demo(a.out, run_dir, 4096, 2)
    rc = 1
    try:
        d.up()
        g1, g2 = d.guests[1], d.guests[2]
        time.sleep(3)
        restore(g1, "rust", a.rust / "rust-workspace.tgz")
        restore(g1, "scala", a.scala / "scala-workspace.tgz")
        bx(g1, "rust", "rust", "mkdir -p /work/repos && for n in rust scala python; do git init -q --bare --initial-branch=main /work/repos/$n.git; done")
        g1.run("/sbin/eco-box daemon rust rust 'cd /work && PORT=8081 GIT_ROOT=/work/repos ./run.sh > /tmp/service.log 2>&1'")
        g1.run("/sbin/eco-box daemon scala scala 'cd /work && PORT=8082 ./run.sh > /tmp/service.log 2>&1'")
        board, git = d.service_url(8082), d.service_url(8081)
        for url, path in ((git, "/healthz"), (board, "/health")):
            for _ in range(120):
                _, o = g2.run(f"curl -s -m 5 -o /dev/null -w '%{{http_code}}' {url}{path}")
                if o.strip() == "200":
                    break
                time.sleep(5)
            else:
                print(f"[J] {url}{path} never answered", flush=True)
        g2.run("/sbin/eco-box exec python python true")
        tar = subprocess.Popen(["tar", "-C", str(a.ref.resolve()), "-czf", "-", "tests"], stdout=subprocess.PIPE)
        subprocess.run(g2.ssh_argv("tar xzf - -C /var/lib/box/containers/python/upper/work"), stdin=tar.stdout)
        tar.wait()
        rc, o = bx(g2, "python", "python", f"cd /work/tests && python3 test_remote.py --board-url {board} --git-url {git} >/tmp/o 2>&1; r=$?; tail -30 /tmp/o; (exit $r)", timeout=900)
        print(f"[J] reference suite: rc={rc}\n{o}", flush=True)
    finally:
        d.down()
    return 0


if __name__ == "__main__":
    sys.exit(main())
