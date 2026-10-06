#!/usr/bin/env python3
"""Find Akuma on ryzen over wifi and run a command on it through ssh.

    python3 overlays/ryzen/wifi-ssh.py --key KEY [--net 192.168.1] [--port 2222] [--wait 900] [CMD...]

The station's MAC is fixed (`amd64/src/rtw89_sta.rs`: `02:41:4b:55:4d:41`), its
address is whatever the network's DHCP gave it. This sweeps the /24 with one
ping each (so the Mac's ARP table learns everyone on it), looks the MAC up in
`arp -an`, and then runs CMD (default: a short health check) as root through
ssh with KEY, the image's `amd64-ssh-test-key` — the one `p3`'s
`/etc/sshd/authorized_keys` accepts. Repeats until it gets in or `--wait`
seconds pass. Akuma's sshd listens on **2222** (port 22 answers with a reset).
Python, not a shell `ssh` line: see CLAUDE.md, "VM Access".

Prints only the address and the command's output: nothing about the network.
"""
import argparse
import re
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor

MAC = "2:41:4b:55:4d:41"  # macOS `arp -an` drops leading zeros


def sweep(net):
    def one(i):
        subprocess.run(["ping", "-c", "1", "-t", "1", f"{net}.{i}"], capture_output=True)
    with ThreadPoolExecutor(64) as ex:
        list(ex.map(one, range(1, 255)))


def find(net):
    sweep(net)
    out = subprocess.run(["arp", "-an"], capture_output=True, text=True).stdout
    for line in out.splitlines():
        if re.search(rf"\bat {re.escape(MAC)}\b", line, re.I):
            m = re.search(r"\((\d+\.\d+\.\d+\.\d+)\)", line)
            if m:
                return m.group(1)
    return None


def ssh(ip, port, key, cmd, timeout=60):
    return subprocess.run(
        ["ssh", "-i", key, "-p", str(port), "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
         "-o", "LogLevel=ERROR", "-o", "ConnectTimeout=15", "-o", "BatchMode=yes", f"root@{ip}", cmd],
        capture_output=True, text=True, timeout=timeout)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--key", required=True)
    ap.add_argument("--net", default="192.168.1")
    ap.add_argument("--port", type=int, default=2222)
    ap.add_argument("--wait", type=int, default=900)
    ap.add_argument("cmd", nargs="*")
    a = ap.parse_args()
    cmd = " ".join(a.cmd) or "uname -a; date; uptime; ifconfig | grep -E 'inet addr'; cat /dev/wifi0 | grep -E '^(state|chan|signal|security)='"
    t0 = time.time()
    ip = None
    while time.time() - t0 < a.wait:
        ip = ip or find(a.net)
        if not ip:
            print(f"[{int(time.time() - t0)} s] not on {a.net}.0/24 yet", flush=True)
            time.sleep(15)
            continue
        print(f"[{int(time.time() - t0)} s] station at {ip}", flush=True)
        try:
            r = ssh(ip, a.port, a.key, cmd)
        except subprocess.TimeoutExpired:
            print("ssh timed out", flush=True)
            time.sleep(10)
            continue
        if r.returncode == 0:
            print(r.stdout, end="")
            return 0
        print(f"ssh rc={r.returncode}: {r.stderr.strip()[:200]}", flush=True)
        time.sleep(10)
    print("gave up", flush=True)
    return 1


if __name__ == "__main__":
    sys.exit(main())
