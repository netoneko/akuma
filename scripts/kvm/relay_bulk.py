#!/usr/bin/env python3
"""Time bulk transfers through the KVM relay *from the ryzen itself*, so the slow wifi link to
the Mac is not in the measurement.

    scripts/kvm/relay_bulk.py HOST [--kib 4096] [--runs 2]

Runs on the host Akuma (ssh to its port 2222) its own `ssh -p 2223 root@127.0.0.1`, which the
service guest trusts through the host's machine key (`kvm-serve-stage.py`; the Gentoo guest does
not). Download = guest to host (`head -c N /dev/zero` in the guest, counted by `wc -c` here), upload
= host to guest. Prints bytes, seconds (timed on the machine running this script)
and KiB/s, then the relay's `[KVM] relay:` and `[KVM] guest:` counters.
"""
import argparse, subprocess

ap = argparse.ArgumentParser()
ap.add_argument("host")
ap.add_argument("--kib", type=int, default=4096)
ap.add_argument("--runs", type=int, default=2)
a = ap.parse_args()
n = a.kib * 1024

def host(cmd, timeout=900):
    r = subprocess.run(["ssh", "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
                        "-o", "LogLevel=ERROR", "-o", "ConnectTimeout=30", "-p", "2222", f"root@{a.host}", cmd],
                       capture_output=True, text=True, timeout=timeout)
    return r.stdout.strip()

G = "ssh -p 2223 root@127.0.0.1"
# Akuma's ssh client asks about an unknown host key on stdin (the guest makes a new one each boot):
# answer once so the measured runs do not.
print("warm-up:", host(f"rm -f /root/.ssh/known_hosts; echo yes | {G} 'echo guest-ok' 2>&1 | tail -1"), flush=True)
import time
for run in range(a.runs):
    for name, cmd in (("download", f"{G} 'head -c {n} /dev/zero' | wc -c"), ("upload", f"head -c {n} /dev/zero | {G} 'wc -c'")):
        t0 = time.monotonic()
        out = host(f"{cmd} 2>/dev/null")
        secs = time.monotonic() - t0   # the Mac's clock: the guest's `date` is not trusted for this
        try:
            got = int(out.split()[-1])
            print(f"run {run} {name}: {got} of {n} bytes in {secs:.1f}s = {got / 1024 / secs:.0f} KiB/s (includes ssh setup)", flush=True)
        except (ValueError, IndexError):
            print(f"run {run} {name}: unparsed {out[-120:]!r} after {secs:.1f}s", flush=True)
print(host("dmesg | grep -a 'KVM\\] relay: slot' | tail -1; dmesg | grep -a 'KVM\\] guest:' | tail -1"))
