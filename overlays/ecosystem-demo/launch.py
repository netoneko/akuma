#!/usr/bin/env python3
"""Boot the ecosystem demo's two guests on one network segment and (optionally) verify them.

    overlays/ecosystem-demo/launch.py verify     boot both guests, run every stage-1 check, tear down
    overlays/ecosystem-demo/launch.py up         boot both guests and stay up (Ctrl-C tears down)

Needs the images from `build-images.py all` ($ECO_WORK/out by default) and the main amd64 kernel
(`cargo build -p akuma --target x86_64-unknown-none --release --no-default-features --features headless,no-tests`).
Runs itself inside an unprivileged user+net namespace (see ecolib.py); it kills only its own QEMUs.

Secrets are random per run, exist only in the metadata server's memory and the guests' root-only
files, and are never printed or written to a log; `verify` checks that.
"""
import argparse
import json
import os
import secrets
import shutil
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import ecolib as E

WORK = Path(os.environ.get("ECO_WORK", "/var/tmp/akuma-eco"))
KERNEL = Path(os.environ.get("ECO_KERNEL", E.ROOT / "target/x86_64-unknown-none/release/akuma"))
BOOTSTRAP = E.HERE / "bootstrap/bootstrap.sh"

AGENTS = {1: ["rust", "scala"], 2: ["python"]}


class Demo:
    def __init__(self, out: Path, run_dir: Path, mem=3072, smp=2, tamper=False):
        self.out, self.run_dir, self.mem, self.smp = out, run_dir, mem, smp
        self.key = run_dir / "launch-key"
        self.pub = E.gen_key(self.key)
        self.net = E.Network(run_dir, WORK / "cache/mirror", 2)
        self.md = E.MetadataServer()
        self.guests: dict[int, E.Guest] = {}
        self.secret_values: dict[str, str] = {}
        script = BOOTSTRAP.read_bytes()
        self.script_sha = E.sha256_hex(script)
        for n in (1, 2):
            agents = {}
            for a in AGENTS[n]:
                tok = secrets.token_hex(24)
                self.secret_values[a] = tok
                agents[a] = E.AgentData(
                    prompts={"task.md": f"# Task for the {a} agent\nThis is a stage-1 placeholder prompt (no agent runs yet).\n"},
                    secrets={"AGENT_TOKEN": tok})
            sha = ("0" * 64) if (tamper and n == 2) else self.script_sha
            self.md.add_guest(E.GuestMeta(f"guest{n}", E.ip_for(n), self.pub, bootstrap=script,
                                          bootstrap_sha256=sha, agents=agents))

    def up(self, which=(1, 2)):
        self.net.up()
        self.md.start()
        for n in which:
            g = E.Guest(n, self.out / f"guest{n}.img", KERNEL, self.run_dir, self.key, mem=self.mem, smp=self.smp,
                        cmdline_extra="strace_err" if os.environ.get("ECO_STRACE") else "")
            self.guests[n] = g
            g.start()
        for n in which:
            self.guests[n].wait_ready(300)

    def down(self):
        for g in self.guests.values():
            g.stop()
        try:
            self.md.stop()
        except Exception:
            pass
        self.net.down()


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("cmd", choices=["up", "verify"])
    ap.add_argument("--out", type=Path, default=WORK / "out")
    ap.add_argument("--mem", type=int, default=3072)
    ap.add_argument("--smp", type=int, default=2)
    ap.add_argument("--report", type=Path)
    a = ap.parse_args()
    E.enter_namespace()
    run_dir = Path(tempfile.mkdtemp(prefix="eco-run-", dir=WORK))
    if a.cmd == "up":
        d = Demo(a.out, run_dir, a.mem, a.smp)
        try:
            d.up()
            print(f"guests up: guest1={E.ip_for(1)} guest2={E.ip_for(2)} key={d.key} run_dir={run_dir}")
            print(f"ssh from another terminal: nsenter --preserve-credentials -U -n -t {os.getpid()} ssh -i {d.key} root@{E.ip_for(1)}")
            while True:
                time.sleep(3600)
        except KeyboardInterrupt:
            pass
        finally:
            d.down()
        return
    import verify
    sys.exit(verify.main(a, run_dir, Demo))


if __name__ == "__main__":
    main()
