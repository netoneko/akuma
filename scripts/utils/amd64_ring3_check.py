#!/usr/bin/env python3
"""The ring-3 workload check for amd64, on local QEMU.

# Why the boot suite is not enough

`amd64_trials.py` runs the boot self-tests, and those execute under
`BypassValidationGuard` on a task that is init's — so they cannot prove a
*user-facing* path. Every process-model change on this target has therefore been
signed off with a second, manual step: log in over ssh many times, run a
workload that forks and execs, and read `free` either side. That step has been
retyped from a hand-off document each time (slices 1, 2 and 3 all describe it in
prose); this is it as a program.

What it checks, and why each part is there:

* **N ssh sessions of `( ls /bin >/dev/null; ls /bin >/dev/null ); echo r$$`** —
  the subshell with **two** execs. One exec is a child; two is a *grandchild*,
  and that shape is what wedged this kernel until `wait4` grew a ppid filter
  (`docs/archive/AKUMA_AMD64_WAIT4_OWNERSHIP.md`). Each session is roughly four
  process lifetimes: sshd's fork, the shell, and the two `ls`.
* **`free` before and after** — the leak check. A process-model change that
  loses an address space shows up here and nowhere else; `used` should come back
  to roughly where it started and `free` should not move at all.
* **`Cached:` from `/proc/meminfo`, before and after** — the **kernel-heap**
  reading. `free` is blind to the entire kernel-heap bug class: across a
  135 MB excursion into `fd.rs`'s whole-file cache, `free` reported the same
  number before and after (`proposals/AMD64_FD_WHOLE_FILE_HEAP.md` § "And a
  method correction"). `meminfo`'s `Cached:` column is
  `akuma_alloc::stats().allocated` on this target, i.e. live kernel-heap bytes,
  so it is the witness the fd-cache class needs: every ssh session's file
  descriptors open and close, and the heap should come back. Drift beyond
  `--heap-tolerance` KiB fails the run; the workload allocates nothing of that
  scale deliberately.
* **`ps | wc -l`** — the reap check. A row per unreaped zombie, so a steady
  count across the churn is what says the parent link and the reap still work.
* **`/probes/grandfork`** — all five rungs, the probe that pins the `wait4`
  ownership fix. It prints before each step because the failure mode is a hang
  with no exit status, so read its last line, not its verdict.

# Usage

    scripts/utils/amd64_ring3_check.py                 # 40 sessions, SMP=1
    scripts/utils/amd64_ring3_check.py --smp 4 -n 40
    scripts/utils/amd64_ring3_check.py --keep           # leave the VM running

Build the kernel first. This does not chain a `cargo build` into the boot, for
the reason `amd64_trials.py` records: racing a partially-linked kernel reports a
run that never happened.

`SMP=1` by default because this target's CoW `fork` demotes the parent's live
PTEs with no TLB shootdown (`docs/archive/AKUMA_AMD64_COW.md`); `--smp 4` is the
interesting-and-known-racy arm, not the baseline.
"""

import argparse
import os
import pathlib
import subprocess
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, HERE)
sys.path.insert(0, os.path.join(REPO, "scripts"))

from amd64_mem_trials import inject_local  # noqa: E402

IMG = os.path.join(REPO, "target/x86_64-unknown-none/release/amd64-root.img")
KEY = os.path.join(REPO, "target/x86_64-unknown-none/release/amd64-ssh-test-key")
PROBE_SRC = pathlib.Path(REPO) / "userspace/forktest/c_stress"
OUTDIR = PROBE_SRC / "x86_64"
CC = "x86_64-linux-musl-gcc"
WORKLOAD = "( ls /bin >/dev/null; ls /bin >/dev/null ); echo r$$"


def ssh(port, cmd, timeout=60):
    """One command in the guest. Returns `(rc, stdout+stderr)`."""
    r = subprocess.run(
        ["ssh", "-o", "StrictHostKeyChecking=no",
         "-o", "UserKnownHostsFile=/dev/null", "-o", "LogLevel=ERROR",
         "-o", "BatchMode=yes", "-o", "ConnectTimeout=10",
         "-i", KEY, "-o", "IdentitiesOnly=yes",
         "-p", str(port), "root@localhost", cmd],
        capture_output=True, text=True, timeout=timeout)
    return r.returncode, (r.stdout or "") + (r.stderr or "")


def wait_ready(port, timeout, proc):
    """Block until the guest answers ssh, or QEMU dies.

    `scripts/vm_ready.py` is the tree's one readiness check and this is the same
    check — an ssh round-trip, never a console marker grep, for every reason its
    header gives. It is spelled out here only because that module's probe
    presents the host's default identities, and this image authorises exactly
    one key: against the amd64 guest it would poll until the timeout while sshd
    logged `Publickey auth failed` on the console, which reads as "the guest
    never came up" and is not.
    """
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            rc, out = ssh(port, "echo __VM_READY__", timeout=8)
            if rc == 0 and "__VM_READY__" in out:
                return True
        except subprocess.TimeoutExpired:
            pass
        if proc.poll() is not None:
            return False
        time.sleep(3)
    return False


def free_columns(text):
    """`used` and `free` off `free`'s Mem: row, or `(None, None)`."""
    for line in text.splitlines():
        if line.strip().startswith("Mem:"):
            parts = line.split()
            if len(parts) >= 4:
                return parts[2], parts[3]
    return None, None


def heap_used_kib(text):
    """Live kernel-heap bytes off `/proc/meminfo`'s `Slab:` row, or `None`.

    `Slab:` is `akuma_alloc::stats().allocated` — the kernel heap. This is the
    reading `free` cannot give: busybox `free`'s numbers come from the PMM and
    never moved across the 135 MB whole-file-cache excursion.

    **It was `Cached:` until 2026-09-10, and had been reading a hard 0 since 4b
    batch 2c.** amd64 rendered the heap number into `Cached:` from its own
    synthetic `/proc`; batch 2c deleted that view for the mounted
    `ProcFilesystem`, where `Cached:` is the *file page* cache — which amd64
    does not have. Nothing failed: the column reported `0 -> 0 kB` and a drift
    of `+0`, which reads as a perfect result and is a dead check. The shared
    render carries the heap under its own Linux name now, so this row means the
    same thing on both kernels. A kernel that predates that row makes this
    return `None`, which the caller reports rather than scoring.
    """
    for line in text.splitlines():
        if line.strip().startswith("Slab:"):
            parts = line.split()
            if len(parts) >= 2:
                return int(parts[1])
    return None


def boot(smp, ssh_port, http_port):
    env = dict(os.environ, SMP=str(smp), SSH_PORT=str(ssh_port),
               HTTP_PORT=str(http_port), DISK=IMG, INIT="/bin/sshd", INITARGS="")
    proc = subprocess.Popen(["sh", os.path.join(REPO, "amd64", "run.sh")],
                            env=env, cwd=REPO, stdin=subprocess.DEVNULL,
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                            text=True, bufsize=1)
    lines = []

    def reader():
        for line in proc.stdout:
            lines.append(line)

    threading.Thread(target=reader, daemon=True).start()
    return proc, lines


def main(argv=None):
    ap = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--smp", type=int, default=1)
    ap.add_argument("-n", "--sessions", type=int, default=40)
    ap.add_argument("--ssh-port", type=int, default=2247)
    ap.add_argument("--http-port", type=int, default=8047)
    ap.add_argument("--boot-timeout", type=int, default=300)
    ap.add_argument("--keep", action="store_true", help="leave the VM running")
    ap.add_argument("--heap-tolerance", type=int, default=8192,
                    help="max kernel-heap drift (KiB) across the sessions "
                         "before the run fails")
    a = ap.parse_args(argv)

    OUTDIR.mkdir(parents=True, exist_ok=True)
    subprocess.run([CC, "-static", "-O2", "-o", str(OUTDIR / "grandfork"),
                    str(PROBE_SRC / "grandfork.c")], check=True)
    if not os.path.exists(IMG):
        subprocess.run(["sh", os.path.join(REPO, "amd64", "mkdisk.sh"), IMG, "128"],
                       cwd=REPO, check=True, capture_output=True)
    inject_local(IMG, OUTDIR, ["grandfork"])

    proc, lines = boot(a.smp, a.ssh_port, a.http_port)
    ok = True
    try:
        if not wait_ready(a.ssh_port, a.boot_timeout, proc):
            print("NOT READY — the guest never answered ssh")
            print("".join(lines[-25:]))
            return 1
        print(f"guest up on port {a.ssh_port} (smp={a.smp})", flush=True)

        _rc, before = ssh(a.ssh_port, "free; cat /proc/meminfo")
        u0, f0 = free_columns(before)
        h0 = heap_used_kib(before)
        _rc, ps0 = ssh(a.ssh_port, "ps | wc -l")

        good = 0
        for i in range(a.sessions):
            rc, out = ssh(a.ssh_port, WORKLOAD)
            if rc == 0 and "r" in out:
                good += 1
            else:
                print(f"  session {i + 1}: rc={rc} {out.strip()[:120]}")
        print(f"sessions: {good}/{a.sessions} returned")
        ok = ok and good == a.sessions

        _rc, after = ssh(a.ssh_port, "free; cat /proc/meminfo")
        u1, f1 = free_columns(after)
        h1 = heap_used_kib(after)
        _rc, ps1 = ssh(a.ssh_port, "ps | wc -l")
        print(f"free:  used {u0} -> {u1}   free {f0} -> {f1}")
        print(f"ps rows: {ps0.strip()} -> {ps1.strip()}")
        if f0 is not None and f0 != f1:
            print("  NOTE: the `free` column moved; read it against the "
                  "workload before calling it a leak")
        if h0 is not None and h1 is not None:
            drift = h1 - h0
            print(f"heap:  {h0} -> {h1} kB (drift {drift:+d} kB, "
                  f"tolerance {a.heap_tolerance})")
            if abs(drift) > a.heap_tolerance:
                print("  HEAP DRIFT — the kernel heap did not come back; this "
                      "is the class `free` cannot see "
                      "(AMD64_FD_WHOLE_FILE_HEAP.md)")
                ok = False
        else:
            print("heap:  NO READING — /proc/meminfo had no `Slab:` row. A "
                  "kernel older than 2026-09-10 does not render one; this run "
                  "proves nothing about the kernel heap.")
            ok = False

        rc, gf = ssh(a.ssh_port, "/probes/grandfork", timeout=120)
        print(f"grandfork: rc={rc}")
        print("\n".join("  | " + l for l in gf.strip().splitlines()[-12:]))
        ok = ok and rc == 0
    finally:
        if not a.keep:
            if proc.poll() is None:
                proc.kill()
            subprocess.run(["pkill", "-f", f"hostfwd=tcp::{a.ssh_port}-"],
                           capture_output=True)
        else:
            print(f"VM left running; kill with "
                  f"pkill -f 'hostfwd=tcp::{a.ssh_port}-'")
    print("RING-3 CHECK:", "OK" if ok else "FAILED")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
