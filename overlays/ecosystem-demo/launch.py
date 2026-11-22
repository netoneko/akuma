#!/usr/bin/env python3
"""Boot the ecosystem demo's two guests on one network segment and (optionally) verify them.

    overlays/ecosystem-demo/launch.py verify     boot both guests, run every stage-1 check, tear down
    overlays/ecosystem-demo/launch.py up         boot both guests and stay up (Ctrl-C tears down)

Needs the images from `build-images.py all` ($ECO_WORK/out by default) and the main amd64 kernel
(`cargo build -p akuma --target x86_64-unknown-none --release --no-default-features --features headless,no-tests`).
Unprivileged: QEMU user networking only (see ecolib.py). It kills only its own QEMUs.

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
SERVICE_PORTS = (8081, 8082)


class Demo:
    """Two guests, each with its own metadata server, ssh forward and (guest 1) service forwards."""

    def __init__(self, out: Path, run_dir: Path, mem=3072, smp=2, tamper=False):
        self.out, self.run_dir, self.mem, self.smp = out, run_dir, mem, smp
        self.key = run_dir / "launch-key"
        self.pub = E.gen_key(self.key)
        self.mirror = None
        self.mds: dict[int, E.MetadataServer] = {}
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
            self.mds[n] = E.MetadataServer(E.GuestMeta(f"guest{n}", "169.254.0.15", self.pub, bootstrap=script,
                                                       bootstrap_sha256=sha, agents=agents))

    def md_log(self, n):
        return self.mds[n].guest.log

    def start_guest(self, n):
        g = E.Guest(n, self.out / f"guest{n}.img", KERNEL, self.run_dir, self.key, self.mds[n].port,
                    mem=self.mem, smp=self.smp, forwards=SERVICE_PORTS if n == 1 else (),
                    cmdline_extra="strace_err" if os.environ.get("ECO_STRACE") else "")
        self.guests[n] = g
        g.start()
        return g

    def service_url(self, port, seen_from=2):
        """The URL of guest 1's service as guest `seen_from` must use it: the host (169.254.0.2) and the forwarded port."""
        return f"http://{E.HOST_ALIAS}:{self.guests[1].forwards[port]}"

    def up(self, which=(1, 2)):
        self.mirror = E.MirrorServer(WORK / "cache/mirror", self.run_dir)
        for m in self.mds.values():
            m.start()
        for n in which:
            self.start_guest(n)
        for n in which:
            self.guests[n].wait_ready(300)

    def down(self):
        for g in self.guests.values():
            g.stop()
        for m in self.mds.values():
            try:
                m.stop()
            except Exception:
                pass
        if self.mirror:
            self.mirror.stop()


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("cmd", choices=["up", "verify"])
    ap.add_argument("--out", type=Path, default=WORK / "out")
    ap.add_argument("--mem", type=int, default=3072)
    ap.add_argument("--smp", type=int, default=2)
    ap.add_argument("--report", type=Path)
    a = ap.parse_args()
    run_dir = Path(tempfile.mkdtemp(prefix="eco-run-", dir=WORK))
    if a.cmd == "up":
        d = Demo(a.out, run_dir, a.mem, a.smp)
        try:
            d.up()
            for n, g in d.guests.items():
                print(f"guest{n}: ssh -i {d.key} -p {g.ssh_port} root@127.0.0.1", flush=True)
            print(f"guest 1 services as guest 2 sees them: {d.service_url(8081)} {d.service_url(8082)}; run_dir={run_dir}", flush=True)
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
