#!/usr/bin/env python3
"""Feedback round: hand agent C its own previous work plus the tester's (D's) report, then re-judge it.

    overlays/ecosystem-demo/rework.py --run /var/tmp/akuma-eco/agent-runs/<stamp>

Boots both guests, restores C's committed workspace from the earlier run into a fresh scala box, appends D's
failure list to C's brief (delivered through the metadata service like the first one), runs C again, starts
the board from its run.sh, and runs D's committed board tests against it from guest 2.
"""
import argparse
import re
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import agents as A
import launch
from verify import bx


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--run", type=Path, required=True, help="the earlier agents.py run directory")
    ap.add_argument("--ref", type=Path, default=Path("../akuma-demo-reference-impl"))
    ap.add_argument("--out", type=Path, default=launch.WORK / "out")
    ap.add_argument("--agent-timeout", type=int, default=2400)
    a = ap.parse_args()
    prev = a.run.resolve()
    spec = (a.ref.resolve() / "spec/SPEC.md").read_text()
    report = (prev / "ws-python/work/REPORT.md").read_text()
    failed = re.search(r"### Failed.*?(?=\n## |\Z)", report, re.S)
    feedback = (A.brief_for("scala", spec) +
                "\n\n## Update: defects found by an independent tester\n\n"
                "Your earlier work is already in `/work` (committed). An independent tester wrote a suite from the contract above "
                "and ran it against your running service. It reported these failures; the contract is authoritative, so read "
                "the contract again, reproduce each one yourself, fix the service, re-test everything, keep `/work/run.sh` working, "
                "update `NOTES.md` and commit.\n\n" + (failed.group(0) if failed else report) + "\n")
    stamp = time.strftime("%Y%m%d-%H%M%S")
    run_dir = launch.WORK / "agent-runs" / f"{stamp}-rework"
    run_dir.mkdir(parents=True)
    (run_dir / "brief-scala.md").write_text(feedback)

    d = launch.Demo(a.out, run_dir, 4096, 2)
    ad = d.mds[1].guest.agents["scala"]
    ad.prompts = {"task.md": feedback}
    ad.secrets = {"ZAI_API_KEY": (Path.home() / ".akuma/z.ai/token").read_text().strip()}
    try:
        d.up()
        g1, g2 = d.guests[1], d.guests[2]
        time.sleep(3)
        A.install_agent(g1, "scala", A.AGENTS["scala"])
        up = "/var/lib/box/containers/scala/upper"
        with (prev / "scala-workspace.tgz").open("rb") as f:
            r = subprocess.run(g1.ssh_argv(f"tar xzf - -C {up}"), stdin=f, capture_output=True, timeout=300)
        assert r.returncode == 0, r.stderr
        print("[A] guests up, scala workspace restored, rework brief delivered", flush=True)

        cmd = A.agent_cmd("scala", A.AGENTS["scala"])
        q = "'" + cmd.replace("'", "'\\''") + "'"
        t = time.monotonic()
        try:
            rc, out = g1.run(f"/sbin/eco-box exec scala scala {q}", timeout=a.agent_timeout)
        except subprocess.TimeoutExpired:
            rc = 124
        print(f"[A] agent scala (rework) finished rc={rc} after {time.monotonic() - t:.0f}s", flush=True)

        g1.run("/sbin/eco-box daemon scala scala 'cd /work && PORT=8082 ./run.sh > /tmp/service.log 2>&1'")
        board = d.service_url(8082)
        up_ok = False
        for _ in range(120):
            _, o = g2.run(f"curl -s -m 5 -o /dev/null -w '%{{http_code}}' {board}/health")
            if o.strip() == "200":
                up_ok = True
                break
            time.sleep(5)
        print(f"[A] board from guest 2: {'UP' if up_ok else 'NOT answering'} ({board})", flush=True)

        if up_ok:
            # D's committed board tests, from guest 2's python box
            g2.run("/sbin/eco-box exec python python true")
            tar = subprocess.Popen(["tar", "-C", str(prev / "ws-python/work"), "-czf", "-", "tests"], stdout=subprocess.PIPE)
            subprocess.run(g2.ssh_argv("tar xzf - -C /var/lib/box/containers/python/upper/work"), stdin=tar.stdout)
            tar.wait()
            r, o = bx(g2, "python", "python",
                      f"cd /work && BOARD_URL={board} GIT_URL=http://127.0.0.1:1 python3 -m pytest -q tests/test_board.py -p no:cacheprovider >/tmp/p.out 2>&1; rc=$?; tail -25 /tmp/p.out; exit $rc",
                      timeout=900)
            print(f"[A] D's board tests against the reworked board: rc={r}\n{o}", flush=True)
        for f, dest in (("tmp/transcript.jsonl", "transcript.jsonl"), ("tmp/stderr.log", "stderr.log")):
            _, o = g1.run(f"cat {up}/{f} 2>/dev/null", timeout=120)
            (run_dir / f"scala-{dest}").write_text(o)
        with (run_dir / "scala-workspace.tgz").open("wb") as fh:
            subprocess.run(g1.ssh_argv(f"tar -C {up} -czf - --exclude=target --exclude=.scala-build --exclude=.bsp work"), stdout=fh, timeout=300)
    finally:
        d.down()
    print(f"[A] saved to {run_dir}", flush=True)


if __name__ == "__main__":
    main()
