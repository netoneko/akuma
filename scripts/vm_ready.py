#!/usr/bin/env python3
"""Is the guest up? Ask it, don't read the tea leaves in its console log.

Every harness in `scripts/` used to decide "the VM booted" by grepping the boot
log for `Started sshd` / `sshd started`. That check is wrong in both directions
and cost real time in both:

* **False negative — the marker never appears on a healthy VM.** At SMP>1 the
  cores interleave console output, so the line arrives torn: `[herd] Starting
  service: sshd` on one line and `sshd (pid= 2)` on another, with the
  `[herd] Started ` prefix separated from its tail (observed 2026-08-16). Some
  builds never print either spelling at all: measured 2026-08-28, a VM ran for
  570 s of guest uptime with `bind=1 listen=1 accept=371380` in `[PSTATS]` — sshd
  serving perfectly — and **zero** matches for either marker. A 10-minute wait
  timed out against a VM that had been ready in seconds.
* **False positive — the marker appears before the VM can serve.** It is printed
  when sshd *starts*, not when it can accept, and it stays in the log forever, so
  a stale log from a previous run reads as "ready".

An ssh round-trip cannot be torn by another core's printf, cannot go stale, and
tests the thing the harness actually needs: that the guest will answer commands.
Poll that.

`wait_ready` also fails fast: if the QEMU process is handed in and has already
exited, there is nothing to wait for, so it returns immediately instead of
burning the whole timeout.

Note the port: QEMU's `hostfwd` listener is opened on the **host** as soon as
QEMU starts, so a TCP connect to the forwarded port succeeds long before the
guest is up. Connecting is not readiness either — only a completed command is.
"""

import subprocess
import time


def ssh_port(instance=0):
    """`scripts/cargo_runner.sh` maps INSTANCE N to ssh port 2222 + 100*N."""
    return 2222 + 100 * int(instance)


def ssh_probe(port, timeout=6):
    """One round-trip. True only if the guest actually ran our command."""
    try:
        r = subprocess.run(
            ["ssh", "-q", "-o", "StrictHostKeyChecking=no",
             "-o", "UserKnownHostsFile=/dev/null",
             "-o", f"ConnectTimeout={max(2, int(timeout) - 1)}",
             "-o", "BatchMode=yes",
             "-p", str(port), "root@localhost", "echo __VM_READY__"],
            capture_output=True, timeout=timeout)
    except (subprocess.TimeoutExpired, OSError):
        return False
    return b"__VM_READY__" in r.stdout


def wait_ready(port=2222, timeout=480, interval=3, proc=None):
    """Block until the guest answers ssh. Returns True/False, never raises.

    `proc`: optional `subprocess.Popen` for the QEMU process. If it has exited,
    stop waiting — the VM is not coming up and the caller wants to know now.
    """
    deadline = time.time() + timeout
    while time.time() < deadline:
        if ssh_probe(port):
            return True
        if proc is not None and proc.poll() is not None:
            return False
        time.sleep(interval)
    return False


if __name__ == "__main__":
    import sys
    p = int(sys.argv[1]) if len(sys.argv) > 1 else 2222
    t = int(sys.argv[2]) if len(sys.argv) > 2 else 480
    ok = wait_ready(p, t)
    print(f"vm on port {p}: {'READY' if ok else 'NOT READY'}")
    sys.exit(0 if ok else 1)
