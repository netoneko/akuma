# The merge gate's first run (2026-11-19)

`scripts/gate.py` (`docs/runbooks/merge-gate.md`) was written after `HOW_MAIN_BROKE_2026_11.md`. Its first runs, on the
branch `box0-followup` (x86_64 dev box, KVM), found the following. Nothing here is fixed by the gate; each is a line in
`scripts/gate/xfail.txt` until it is.

> **Corrected 2026-11-19 (later):** the `tcp_loopback` hang was a blocking `accept` starving its sibling thread, fixed in
> `docs/archive/TOKIO_KERNELS_LOOPBACK_STARVATION.md`; `many_threads` was the 48-slot spawn ring (a capacity limit), not a
> thread-slot reuse bug. The two bullets below describe what the gate saw first.

## Platform kernels, amd64, `gateprobe` as `init=` (Taoxichuan, Shanghai, Denver)
* **`tcp_loopback`** (accept/connect over 127.0.0.1, 64 KiB echo, half-close): the connect never completes and the guest
  wedges. `bind`, `getsockname` and a non-blocking `accept` work (`tcp_bind` passes), and UDP over loopback passes.
  The main amd64 kernel passes it alone and when run first; run after the spawn checks it timed out once and passed in
  later runs, so it is listed `flaky:`.
* **`many_threads`** (48 threads alive at once): passes alone, and the guest stops making progress at about 23 live
  threads when earlier checks have already created and reaped threads and children. Flaky on Denver. Suspect: thread-slot
  reuse after termination (the `[threads] new high-water` lines show `terminated=N` falling as it stalls).
* Everything else passes on all four kernels: threads (futex), time, memory, files, links/chmod/truncate/pread, 8 MiB file,
  pipes (4 MiB), SIGKILL, argv/env/cwd, an 8 MiB thread stack, spawn/wait, 16 children, children from threads, UDP.

## Repo
* `check_budgets.py` fails on main: the copy and `mut` budgets are stale (ext2 +10, vfs +4, tokio-kernel-core +5, ...).
  Needs a review and `--update`.
* The pre-commit hook had never been installed in this clone, so clippy rotted in about ten crates; fixed in the same
  change. The hook and the host tests assumed an AArch64 host (the author's Mac); `akuma-psci` and `akuma-gic` now compile
  off AArch64 (stubs, not assembly) and the hook skips the two AArch64-only crates elsewhere.

## Cost
Warm: about 1.5 minutes; every target directory cold: 4-5 minutes. Well under the 10-15 minute budget, so the probe
can grow. aarch64 kernels are built, not booted.
