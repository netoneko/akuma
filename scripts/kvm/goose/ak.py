#!/usr/bin/env python3
"""Run a command on the ryzen box (Akuma, sshd on :2222) - the Python successor of ak.sh.

    python3 scripts/kvm/goose/ak.py [--ip IP] CMD...                    # CMD runs as root on the box
    python3 scripts/kvm/goose/ak.py --stdin 'cat > /tmp/x.py' < x.py    # send stdin (never read implicitly)

From Python: `from ak import run` -> run(cmd, stdin_text=None) -> CompletedProcess (text mode).

There is no kill timeout: a command that legitimately runs for minutes (a build, a boot battery) must not be cut
off. A dead link is found by ssh itself (ServerAliveInterval x ServerAliveCountMax, about a minute) and a dead
host by ConnectTimeout, both surfacing as ssh rc 255.

The address is AKUMA_IP (default 192.168.1.159). When ssh cannot connect (rc 255) the box is looked up by the
station's fixed MAC with `overlays/ryzen/wifi-ssh.py`'s sweep, so a DHCP move does not strand the supervisor.
The box has no tty and its shell wants TERM, so every command is prefixed with `export TERM=dumb GIT_PAGER=cat;`.
"""
import argparse
import importlib.util
import os
import pathlib
import subprocess
import sys

REPO = pathlib.Path(__file__).resolve().parents[3]
PORT = int(os.environ.get("AKUMA_PORT", "2222"))
SSH_OPTS = ["-p", str(PORT), "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "-o", "ServerAliveInterval=15",
            "-o", "ServerAliveCountMax=4", "-o", "StrictHostKeyChecking=no",
            "-o", "UserKnownHostsFile=/dev/null", "-o", "LogLevel=ERROR"]
_ip = None


def _discover():
    """The box's current address by its fixed MAC (overlays/ryzen/wifi-ssh.py), or None."""
    spec = importlib.util.spec_from_file_location("wifi_ssh", REPO / "overlays/ryzen/wifi-ssh.py")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod.find(os.environ.get("AKUMA_NET", "192.168.1"))


def _ssh(ip, cmd, stdin_text):
    argv = ["ssh", *SSH_OPTS, f"root@{ip}", "export TERM=dumb GIT_PAGER=cat; " + cmd]
    if stdin_text is None:
        return subprocess.run(argv, capture_output=True, text=True, stdin=subprocess.DEVNULL)
    return subprocess.run(argv, capture_output=True, text=True, input=stdin_text)


def run(cmd, stdin_text=None):
    """Run cmd on the box; on a connection failure look the box up by MAC once and retry."""
    global _ip
    _ip = _ip or os.environ.get("AKUMA_IP", "192.168.1.159")
    r = _ssh(_ip, cmd, stdin_text)
    if r.returncode == 255:
        found = _discover()
        if found and found != _ip:
            print(f"ak.py: box not at {_ip}; found at {found}", file=sys.stderr)
            _ip = found
            r = _ssh(_ip, cmd, stdin_text)
    return r


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--ip")
    ap.add_argument("--stdin", action="store_true", help="read stdin and send it to the command")
    ap.add_argument("cmd", nargs="+")
    a = ap.parse_args()
    if a.ip:
        os.environ["AKUMA_IP"] = a.ip
    stdin_text = sys.stdin.read() if a.stdin else None
    r = run(" ".join(a.cmd), stdin_text)
    sys.stdout.write(r.stdout)
    sys.stderr.write(r.stderr)
    return r.returncode


if __name__ == "__main__":
    sys.exit(main())
