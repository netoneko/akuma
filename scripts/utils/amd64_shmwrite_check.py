#!/usr/bin/env python3
"""Writable `MAP_SHARED` coherence on amd64, on local QEMU, over ssh.

The gate for the shared writable page table (`crates/akuma-fpcache-rw`,
`amd64/src/shmpages.rs`, 2026-10-03): the two coherence probes plus the
regression probes the change could plausibly break, each run as its own ssh
command so one hang names itself instead of eating the rest.

* `shmcoh`   — the one question: are two processes' mappings live? (both `YES`)
* `shmwrite` — twelve rungs, one per path a shared page can take (fork,
  `write(2)`, truncate, `MADV_DONTNEED`, `mprotect`, `mremap`, exit, unlink).
* `fcntl_lock`, `unixsock_amd64`, `groupexit_thread`, `shmanon`, `madvshared`,
  `mremapmove` — the record locks, AF_UNIX, group exit from a thread, the other
  identity-shared mapping, and the two paths (`MADV_DONTNEED`, `mremap`) this
  change rewired for every mapping. (`fpcpoison` is AArch64-only: it spins on
  `yield`.)

The ten console-driven mmap probes are `amd64_mem_trials.py --local-only`, and
fork/signal/clock are `amd64_ring3_check.py`; run those too. This does not chain
a `cargo build` (the reason `amd64_trials.py` records).

    scripts/utils/amd64_shmwrite_check.py            # SMP=1
    scripts/utils/amd64_shmwrite_check.py --smp 4    # the interesting arm for `shmwrite` rung 10
    scripts/utils/amd64_shmwrite_check.py --repeat 5 # run the probe list N times in one boot

Kills only the QEMU it started, matched on its own `hostfwd`.
"""

import argparse
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import amd64_ring3_check as r3  # noqa: E402  (ssh, wait_ready, boot, paths)
from amd64_mem_trials import inject_local  # noqa: E402

# (probe, argument string, timeout seconds, a substring a pass must print or
# None, the exit status a pass returns). `groupexit_thread` passes by exiting 3
# from a worker thread, once per way the leader can be parked.
PROBES = [
    ("shmcoh", "", 60, "parent sees child's live write:  YES", 0),
    ("shmwrite", "", 300, "shmwrite: PASS", 0),
    ("fcntl_lock", "", 120, "RESULT: all passed", 0),
    ("unixsock_amd64", "", 120, "RESULT: all passed", 0),
    ("groupexit_thread", "0", 30, None, 3),
    ("groupexit_thread", "1", 30, None, 3),
    ("groupexit_thread", "2", 30, None, 3),
    ("shmanon", "", 120, "SHARED (correct)", 0),
    ("madvshared", "", 120, "ALL PASS", 0),
    ("mremapmove", "", 120, "ALL PASS", 0),
]


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--smp", type=int, default=1)
    ap.add_argument("--ssh-port", type=int, default=2257)
    ap.add_argument("--http-port", type=int, default=8057)
    ap.add_argument("--boot-timeout", type=int, default=300)
    ap.add_argument("--repeat", type=int, default=1)
    ap.add_argument("--only", default="", help="comma-separated probe names")
    ap.add_argument("--keep", action="store_true", help="leave the VM running")
    a = ap.parse_args(argv)

    probes = [p for p in PROBES if not a.only or p[0] in a.only.split(",")]
    r3.OUTDIR.mkdir(parents=True, exist_ok=True)
    for name in dict.fromkeys(p[0] for p in probes):
        subprocess.run([r3.CC, "-static", "-O1", "-pthread", "-o", str(r3.OUTDIR / name),
                        str(r3.PROBE_SRC / f"{name}.c")], check=True)
    if not os.path.exists(r3.IMG):
        subprocess.run(["sh", os.path.join(r3.REPO, "amd64", "mkdisk.sh"), r3.IMG, "128"],
                       cwd=r3.REPO, check=True, capture_output=True)
    inject_local(r3.IMG, r3.OUTDIR, list(dict.fromkeys(p[0] for p in probes)))

    proc, lines = r3.boot(a.smp, a.ssh_port, a.http_port)
    failed = []
    try:
        if not r3.wait_ready(a.ssh_port, a.boot_timeout, proc):
            print("NOT READY — the guest never answered ssh")
            print("".join(lines[-40:]))
            return 1
        print(f"guest up on port {a.ssh_port} (smp={a.smp})", flush=True)
        for rep in range(a.repeat):
            for name, args, timeout, must, want_rc in probes:
                try:
                    rc, out = r3.ssh(a.ssh_port, f"cd /tmp && /probes/{name} {args}", timeout=timeout)
                except subprocess.TimeoutExpired:
                    rc, out = -1, "(timed out)"
                ok = rc == want_rc and (must is None or must in out)
                tag = f"{name} {args}".strip() + (f" #{rep}" if a.repeat > 1 else "")
                print(f"--- {tag}: rc={rc} {'PASS' if ok else 'FAIL'}")
                print(out.rstrip())
                if not ok:
                    failed.append(tag)
        rc, alive = r3.ssh(a.ssh_port, "echo __ALIVE__; dmesg | grep -a -i 'fpcache-rw\\|MM-WB' | tail -5",
                           timeout=30)
        print(alive.rstrip())
        if "__ALIVE__" not in alive:
            failed.append("alive")
    finally:
        if not a.keep:
            subprocess.run(["pkill", "-f", f"hostfwd=tcp::{a.ssh_port}-"], capture_output=True)
        else:
            print(f"VM left running; stop it with pkill -f 'hostfwd=tcp::{a.ssh_port}-'")
    print("FAILED: " + ", ".join(failed) if failed else "ALL PASS")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
