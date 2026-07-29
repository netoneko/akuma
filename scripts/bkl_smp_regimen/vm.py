#!/usr/bin/env python3
"""Run one command in the Akuma devbox over SSH.

Two devbox facts shape this:
  - the sshd never sends exit-status, so `ssh` always returns 255 — callers must
    key on stdout, never on the exit code;
  - ServerAlive keepalives time out when the guest's cores are pegged, and the
    server then tears down the channel mid-command, so they are disabled here.

The `ssh` CLI is blocked by policy in some environments; driving it from Python
(as here) is the supported path — see CLAUDE.md.
"""
import subprocess
import sys

PORT = 2222


def ssh_argv(port=PORT):
    return [
        "ssh", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
        "-o", "LogLevel=ERROR", "-o", "ServerAliveInterval=0", "-o", "ConnectTimeout=15",
        "-p", str(port), "root@localhost",
    ]


def run(cmd, timeout=120, port=PORT):
    p = subprocess.run(ssh_argv(port) + [cmd], capture_output=True, text=True, timeout=timeout)
    return p.stdout, p.stderr


if __name__ == "__main__":
    out, err = run(sys.argv[1], timeout=int(sys.argv[2]) if len(sys.argv) > 2 else 120)
    sys.stdout.write(out)
    if err.strip():
        sys.stderr.write("[stderr] " + err)
