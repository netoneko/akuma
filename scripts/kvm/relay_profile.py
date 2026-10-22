#!/usr/bin/env python3
"""Separate what limits the KVM relay: ssh setup, steady-state bulk, and the wifi link.

    scripts/kvm/relay_profile.py HOST [--mib 32] [--wifi-kib 512]

Everything is timed on the machine running this script. Each ssh to the host adds a fixed cost
(the outer connection), measured with `true` and subtracted, so the rows are:

  outer          ssh to the host's own sshd running `true` (the cost every other row includes)
  setup          `ssh -p 2223 127.0.0.1 echo ok` run on the host: the guest's key exchange + login
  bulk down/up   --mib MiB through the relay from the host itself, minus `setup` => steady MB/s
  wifi host      --wifi-kib KiB between this machine and the host's own sshd (no guest involved)
  wifi guest     --wifi-kib KiB between this machine and the guest through the relay (port 2223)

If `wifi host` is as slow as `wifi guest`, the link is the limit, not the hypervisor.
Needs the service guest running (the host trusts it: `kvm-serve-stage.py`).
"""
import argparse, subprocess, time

ap = argparse.ArgumentParser()
ap.add_argument("host")
ap.add_argument("--mib", type=int, default=32)
ap.add_argument("--wifi-kib", type=int, default=512)
a = ap.parse_args()

SSH = ["ssh", "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
       "-o", "LogLevel=ERROR", "-o", "ConnectTimeout=30"]


def timed(argv, stdin_bytes=None, timeout=900):
    t0 = time.monotonic()
    r = subprocess.run(argv, capture_output=True, input=stdin_bytes, timeout=timeout)
    return time.monotonic() - t0, r.stdout.decode(errors="replace").strip(), r.returncode


def host(cmd, **kw):
    return timed(SSH + ["-p", "2222", f"root@{a.host}", cmd], **kw)


G = "ssh -p 2223 root@127.0.0.1"
host(f"rm -f /root/.ssh/known_hosts; echo yes | {G} 'echo guest-ok' 2>&1 | tail -1")  # warm-up: host key

outer = min(host("true")[0] for _ in range(3))
print(f"outer          {outer * 1000:7.0f} ms  (ssh to the host's sshd running true)")
setup = min(host(f"{G} 'echo ok' 2>/dev/null")[0] for _ in range(3)) - outer
print(f"setup          {setup * 1000:7.0f} ms  (the guest's ssh handshake, from the host itself)")
n = a.mib << 20
for name, cmd in (("down", f"{G} 'head -c {n} /dev/zero' 2>/dev/null | wc -c"),
                  ("up", f"head -c {n} /dev/zero | {G} 'wc -c' 2>/dev/null")):
    secs, out, _ = host(cmd)
    try:
        got = int(out.split()[-1])
    except (ValueError, IndexError):
        print(f"bulk {name:4}      unparsed {out[-80:]!r}")
        continue
    body = max(secs - outer - setup, 1e-3)
    print(f"bulk {name:4}      {got / body / 1e6:7.2f} MB/s  ({got >> 20} MiB in {secs:.2f}s, {body:.2f}s net of outer+setup)")

wn = a.wifi_kib << 10
for label, port, user in (("wifi host ", 2222, "root"), ("wifi guest", 2223, "root")):
    for name in ("down", "up"):
        if name == "down":
            argv = SSH + ["-p", str(port), f"{user}@{a.host}", f"head -c {wn} /dev/zero"]
            secs, out, rc = timed(argv)
            got = len(out) if rc == 0 else 0
            # stdout was decoded and stripped; the byte count is only approximate: use the exit code.
            got = wn if rc == 0 else 0
        else:
            argv = SSH + ["-p", str(port), f"{user}@{a.host}", "wc -c"]
            secs, out, rc = timed(argv, stdin_bytes=bytes(wn))
            got = int(out.split()[-1]) if out.split() else 0
        base = timed(SSH + ["-p", str(port), f"{user}@{a.host}", "true"])[0]
        body = max(secs - base, 1e-3)
        print(f"{label} {name:4} {got / 1024 / body:7.0f} KiB/s  ({got >> 10} KiB in {secs:.2f}s, setup {base:.2f}s)")
