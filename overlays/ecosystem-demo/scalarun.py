#!/usr/bin/env python3
"""Does `scala-cli run` finish? Plain (no tty) or under Codex's own exec session (a pty), on whatever kernel ECO_KERNEL names.

    scalarun.py --mode plain --project DIR --cmd 'scala-cli run . --server=false --main-class TaskBoard'
    scalarun.py --mode codex --project DIR --cmd '...'      # a GLM agent runs the command through Codex's tool

Boots guest 1 only. Prints [R] RESULT: DONE (with the output tail) or HUNG (process states). The Codex mode costs a few GLM calls.
"""
import argparse
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import agents as A
import launch


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--mode", choices=["plain", "codex"], required=True)
    ap.add_argument("--project", type=Path, required=True)
    ap.add_argument("--cmd", required=True)
    ap.add_argument("--wait", type=int, default=420)
    ap.add_argument("--out", type=Path, default=launch.WORK / "out")
    a = ap.parse_args()
    run_dir = Path(tempfile.mkdtemp(prefix="eco-scalarun-", dir=launch.WORK))
    d = launch.Demo(a.out, run_dir, 4096, 2)
    brief = ("# Task\nRun exactly this one command in the foreground in /work, wait for it to finish (it can take a few minutes; "
             f"poll it, do not start a second copy), then report its complete output and exit code and stop:\n\n    {a.cmd}\n")
    ad = d.mds[1].guest.agents["scala"]
    ad.prompts = {"task.md": brief}
    ad.secrets = {"ZAI_API_KEY": (Path.home() / ".akuma/z.ai/token").read_text().strip()}
    try:
        d.up((1,))
        g = d.guests[1]
        time.sleep(3)
        A.install_agent(g, "scala", A.AGENTS["scala"])
        up = "/var/lib/box/containers/scala/upper"
        tar = subprocess.Popen(["tar", "-C", str(a.project), "-czf", "-", "."], stdout=subprocess.PIPE)
        subprocess.run(g.ssh_argv(f"tar xzf - -C {up}/work"), stdin=tar.stdout, check=True)
        tar.wait()
        t0 = time.monotonic()
        status = "HUNG"
        out = ""
        if a.mode == "plain":
            q = "'" + f"cd /work && timeout {a.wait} {a.cmd}; echo EXIT=$?".replace("'", "'\\''") + "'"
            try:
                rc, out = g.run(f"/sbin/eco-box exec scala scala {q}", timeout=a.wait + 60)
                status = "DONE" if "EXIT=" in out and "EXIT=124" not in out else "HUNG"
            except subprocess.TimeoutExpired:
                pass
        else:
            cmd = A.agent_cmd("scala", A.AGENTS["scala"])
            q = "'" + cmd.replace("'", "'\\''") + "'"
            try:
                rc, out = g.run(f"/sbin/eco-box exec scala scala {q}", timeout=a.wait)
                status = "DONE"
            except subprocess.TimeoutExpired:
                pass
            _, out = g.run(f"tail -n 6 {up}/tmp/transcript.jsonl | cut -c1-900")
        print(f"[R] RESULT: {status} after {time.monotonic() - t0:.0f}s (mode={a.mode}, kernel={launch.KERNEL.name})", flush=True)
        print(out[-2500:], flush=True)
        if status == "HUNG":
            _, o = g.run("ps -eo pid,ppid,time,stat,args | grep -E 'java|dotty|scala|timeout' | grep -v grep | cut -c1-130 | head -8; cat /proc/loadavg")
            print(o, flush=True)
            _, o = g.run(f"echo '--- command output so far:'; cat {up}/tmp/.eco.out 2>/dev/null | tail -c 1500; echo; echo '--- app jvm threads:'; ls /proc/112/task 2>&1 | head -3")
            print(o, flush=True)
    finally:
        d.down()
    # exit 0 only when the command finished and printed the ujson marker (used by `git bisect run`)
    return 0 if (status == "DONE" and ("UJSON_OK" in out or a.mode == "codex")) else 1


if __name__ == "__main__":
    sys.exit(main())
