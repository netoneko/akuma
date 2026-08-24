#!/usr/bin/env python3
"""Run a command in an Akuma guest over ssh.

Exists because the `ssh` CLI is blocked by security policy in this
environment (CLAUDE.md § "VM Access") — driving it from Python is the
documented way in. Thin on purpose: one command, one connection, prints
stdout/stderr and exits with the remote status.

    scripts/benchmarks/gssh.py 'box ls'
    scripts/benchmarks/gssh.py --port 2224 'nproc'
"""
import argparse
import subprocess
import sys

ap = argparse.ArgumentParser()
ap.add_argument("--port", type=int, default=2222)
ap.add_argument("--timeout", type=float, default=30.0)
ap.add_argument("cmd", nargs="+")
a = ap.parse_args()

p = subprocess.run(
    ["ssh", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
     "-o", "LogLevel=ERROR", "-o", f"ConnectTimeout={int(a.timeout)}",
     "-p", str(a.port), "root@localhost", " ".join(a.cmd)],
    capture_output=True, text=True, timeout=a.timeout,
)
sys.stdout.write(p.stdout)
sys.stderr.write(p.stderr)
sys.exit(p.returncode)
