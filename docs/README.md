# Akuma OS Documentation

This is the front door for all Akuma documentation. Use the tables below to
find what you need.

## How these docs are organized

```
runbooks/     Action-first. "Do X, expect to see Y." For debugging and building.
reference/    Current-state architecture and invariants. No history.
userspace/    Thin pointers to per-binary docs (kept co-located with source).
archive/      Every historical doc, moved verbatim. Linked from new docs, never rewritten.
```

Authoring conventions for each kind live in that section's own README:
[`runbooks/README.md`](runbooks/README.md) and
[`reference/README.md`](reference/README.md).

## Stability grades

Each reference doc carries a grade based on git churn history (608 commits
across 179 archived docs, Dec 2025 – Jul 2026):

- **A — Stable:** dormant or stabilized 2+ months. Safe to trust.
- **B — Watch:** low/med churn, recent but not chronic. Verify behaviour.
- **C — Active risk:** high churn, touched in Jun/Jul. Expect surprises.

The fire windows were late Feb–Mar 2026 (syscall-gap crisis) and Jun 2026
(memory + signal crisis). April/May were the cool-down tails.

## Symptom matrix - "I see X, what do I read?"

| Symptom | Start here |
|---|---|
| VM won't boot / hangs early | [`runbooks/debug-boot-hang.md`](runbooks/debug-boot-hang.md) |
| SSH unreachable or slow to connect | [`runbooks/debug-devbox.md`](runbooks/debug-devbox.md) (devbox) / [`runbooks/debug-ssh-latency.md`](runbooks/debug-ssh-latency.md) |
| VM wedged, 100% CPU, unresponsive | [`runbooks/recover-wedged-vm.md`](runbooks/recover-wedged-vm.md) |
| `[OOM] allocation of N bytes failed (heap …MB used) — killing process` repeating, sshd can't spawn `/bin/sh` — OR serial log frozen + 100% CPU with **no** `[OOM]` under exec-heavy load | Kernel-heap wall from the execve stack leak (FIXED) and its lock-abandonment hang class: [`reference/subsystems/thread-lifecycle.md`](reference/subsystems/thread-lifecycle.md) §4-§5 + [`archive/EXECVE_STACK_LEAK_OOM_HANG.md`](archive/EXECVE_STACK_LEAK_OOM_HANG.md) |
| Serial freezes under `-jN` concurrent build (first time at N>1; `-j1` clean), tail shows an `mmap`/`mprotect` burst (lazy-region count climbing) + 100% CPU + no `[OOM]` | Was a global-`LAZY_REGION_TABLE` alloc-under-lock site (rule-2, sibling of the above), **FIXED** by moving the map onto `Process::lazy_regions`: [`archive/LAZY_REGION_TABLE_ALLOC_UNDER_LOCK_HANG.md`](archive/LAZY_REGION_TABLE_ALLOC_UNDER_LOCK_HANG.md) §10. If this shape recurs, it is a *different* lock — grep the **full** log for `[OOM]` to separate an alloc-under-lock route from the §5.1 drain route |
| Kernel panic / OOM / allocation failure | [`runbooks/debug-memory-oom.md`](runbooks/debug-memory-oom.md) |
| PMM free pinned near the reserve while `[PSTATS]` shows `retired=N/Mp` non-zero (exec failures, "No current user process" cascades) | Reclaimable memory is parked, not leaked: [`reference/subsystems/memory.md`](reference/subsystems/memory.md) -> "Reclaim under memory pressure" + [`archive/OOM_KILL_DEFERRED_RECLAIM_GAP.md`](archive/OOM_KILL_DEFERRED_RECLAIM_GAP.md) |
| Kernel panic `Process table full (256 slots)` | **Known OPEN item**, not a capacity limit — the slots are reclaimable zombies and `register_process` panics instead of failing the spawn: [`reference/subsystems/memory.md`](reference/subsystems/memory.md) -> "OPEN: a full process table still panics the kernel" |
| Kernel panic with ESR_EL1/FAR_EL1 / "unhandled exception" / EL1 crash | [`runbooks/debug-exceptions.md`](runbooks/debug-exceptions.md) |
| Network doesn't work / can't connect out | [`runbooks/debug-network.md`](runbooks/debug-network.md) (smoltcp) / [`runbooks/debug-devbox.md`](runbooks/debug-devbox.md) (rump) |
| `cargo` or `rustc` crashes in the devbox | [`runbooks/debug-devbox.md`](runbooks/debug-devbox.md) -> Toolchain crashes |
| `git clone` hangs or wedges | [`runbooks/debug-devbox.md`](runbooks/debug-devbox.md) / [`runbooks/debug-network.md`](runbooks/debug-network.md) |
| SSH echo lag / staggering / terminal sizing | [`runbooks/debug-ssh-latency.md`](runbooks/debug-ssh-latency.md) |
| epoll crash / DNS hang under bun | [`runbooks/debug-network.md`](runbooks/debug-network.md) -> epoll |
| HTTPS download measured in minutes | [`runbooks/debug-network.md`](runbooks/debug-network.md) -> TLS |
| Fork / exec / signal misbehaviour | [`reference/subsystems/syscalls/proc.md`](reference/subsystems/syscalls/proc.md) + [`syscalls/signal.md`](reference/subsystems/syscalls/signal.md) + [`scheduler.md`](reference/subsystems/scheduler.md) |
| A threaded program dies by **SIGSEGV with `FAR=0`** where you expected SIGABRT (cargo reporting `signal: 11` and `signal: 6` for the same crate on different runs) / `abort()` returns / `kill -TERM` does nothing | Not memory corruption — an undelivered signal, and musl's `a_crash()` is the `FAR=0` store. Two causes, both **FIXED**: `tkill` was handed a PID instead of a thread slot, and fatal `SIG_DFL` signals pended while blocked were dropped at syscall return. [`archive/SELFHOST_DEVBOX_SMOLTCP.md`](archive/SELFHOST_DEVBOX_SMOLTCP.md) -> "SIGABRT delivery" + [`syscalls/signal.md`](reference/subsystems/syscalls/signal.md) -> "Default action for pended signals". Regress with `userspace/forktest/c_stress/abortsig.c` |
| Filesystem / ext2 / procfs errors | [`reference/subsystems/vfs.md`](reference/subsystems/vfs.md) + `archive/EXT2_FIRST_DATA_BLOCK_FIX.md` |
| Porting a new binary (missing syscalls) | [`reference/subsystems/syscalls.md`](reference/subsystems/syscalls.md) -> Porting + `archive/*_MISSING_SYSCALLS.md` |
| A/B run shows no difference / boot doesn't match the features you built | [`reference/subsystems/locking.md`](reference/subsystems/locking.md) -> playbook rule 5 + `archive/BKL_VFS_CARVE_OUT.md` §17 |
| `[BKL] RECOVERED` lines under SMP load / "can I just delete the BKL?" | [`reference/subsystems/locking.md`](reference/subsystems/locking.md) -> "What the BKL is still the only lock for" + [`archive/BKL_PHASE7_AUDIT.md`](archive/BKL_PHASE7_AUDIT.md) |
| `[BKL] stale dropped-window ... healed` / converting a syscall off the BKL | [`reference/subsystems/locking.md`](reference/subsystems/locking.md) -> "The per-syscall BKL opt-out list" + [`archive/BKL_PHASE7F_OPTOUT_LIST.md`](archive/BKL_PHASE7F_OPTOUT_LIST.md) |
| `test_epoll_multi_poller_pipe FAILED: woken=1` / a boot-suite test that fails ~1 boot in 3 | [`archive/EPOLL_MULTI_POLLER_PIPE_FLAKE.md`](archive/EPOLL_MULTI_POLLER_PIPE_FLAKE.md) — test defect, not a kernel bug; do not accept/reject a change on one boot |
| Benchmarking in-VM rustc / SMP scaling vs Linux | [`archive/BKL_RUSTC_SCALING_BASELINE.md`](archive/BKL_RUSTC_SCALING_BASELINE.md) + `scripts/bkl_rustc_bench/` |
| `rustc -O big.rs` produces no artifact and prints nothing / `(opt cgu.N) SIGSEGV` | **Both modes root-caused + FIXED.** SIGSEGV = cross-core stack-sharing in the scheduler: [`archive/SMP_SHARED_ONCPU_GATE.md`](archive/SMP_SHARED_ONCPU_GATE.md). The residual *hang* = a stale thread-slot index letting one rustc's teardown kill another rustc's linker: [`archive/STALE_THREAD_SLOT_KILL.md`](archive/STALE_THREAD_SLOT_KILL.md) |
| A process sits in `ps`/`PSTATS` with a frozen syscall count forever / its parent's `wait4` never returns | A process left with **no thread**: [`archive/STALE_THREAD_SLOT_KILL.md`](archive/STALE_THREAD_SLOT_KILL.md). Check the log for `stale tid=` guard fires |
| SMP=4 `[BKL] stuck owner=N tag=511` storm (boot or load), EL1 crash with `ELR=0x8` / SP outside thread stack | **FIXED** — the ON_CPU scheduler gate: [`archive/SMP_SHARED_ONCPU_GATE.md`](archive/SMP_SHARED_ONCPU_GATE.md) + [`runbooks/debug-smp.md`](runbooks/debug-smp.md) first row |
| A thread parked forever in an unreturned `futex` (`sc=-1 tsc=98` in `[THR-DUMP]`), low CPU, frozen syscall count | [`runbooks/debug-futex-lost-wakeup.md`](runbooks/debug-futex-lost-wakeup.md) — it separates the four causes that share this one symptom, starting from whether `[FUTEX-DUMP]` still has the waiter queued. Five earlier divergences (chiefly **requeued waiters never removed from the requeue target**) are **FIXED**: [`archive/FUTEX_REQUEUE_LOST_WAKEUP.md`](archive/FUTEX_REQUEUE_LOST_WAKEUP.md). Current behaviour: [`reference/subsystems/syscalls/sync.md`](reference/subsystems/syscalls/sync.md) |
| Several copies of one musl binary run at once and one wedges in `futex` at an address they all share (e.g. `0x300c2340`) | Non-private futexes were keyed by **virtual address alone**, so every process's musl `__thread_list_lock` shared one queue and wakes were stolen across processes — **FIXED** (2026-08-04): [`runbooks/debug-futex-lost-wakeup.md`](runbooks/debug-futex-lost-wakeup.md) §5. Regress with `userspace/forktest/c_stress/futexkey.c` (deterministic); the 8×`futextest_rs` stress run passes on **both** arms and cannot detect it |
| `mprotect` appears to do nothing: a `PROT_NONE` guard page stays writable, RELRO stays writable, a stack overflow scribbles instead of faulting | **FIXED** (2026-08-05) — `flush_tlb_range` invalidated with `tlbi vale1is`, whose ASID field is zero for every user VA while user processes run under non-zero ASIDs, so it matched nothing. Now `vaae1is` (all-ASID), which is required because `new_shared` aliases one L0 table under several ASIDs: [`runbooks/debug-thread-spawn-segv.md`](runbooks/debug-thread-spawn-segv.md) §2b. Regress with `userspace/forktest/c_stress/mprotectlb.c` |
| A `pthread_create`d thread dies `SIGSEGV after 0.00s` at a **fixed** `ELR` with near-null `FAR`, preceded by `sig 11 needs sigaltstack but slot N has none — re-pending`, and the group is killed by `SIGSEGV in clone_thread` | **OPEN** — the `-j4` self-host blocker. The child reads the first 8 bytes of its own clone argument and gets stale/zero content: [`runbooks/debug-thread-spawn-segv.md`](runbooks/debug-thread-spawn-segv.md). The `pthread_join` waiters it leaves behind look exactly like a futex lost wakeup and are not one |
| AB-BA deadlock hunting / "can this `Spinlock` just be an atomic?" | [`reference/subsystems/locking.md`](reference/subsystems/locking.md) -> gate 2 + [`archive/BKL_FINE_GRAINED_LOCKING_PLAN.md`](archive/BKL_FINE_GRAINED_LOCKING_PLAN.md) §7.3a (phase 7g classification table) |

## Task list - "I want to do X"

| Task | Start here |
|---|---|
| Boot a VM and connect via SSH | [`runbooks/boot-and-connect.md`](runbooks/boot-and-connect.md) |
| Build the devbox and SSH in | [`runbooks/build-devbox.md`](runbooks/build-devbox.md) |
| Compile the kernel inside Akuma (self-host) | [`runbooks/selfhost-kernel-build.md`](runbooks/selfhost-kernel-build.md) — read its **Status** section first: the `-j1` and `-j4` builds fail for different reasons |
| Add an apk package to the devbox | [`runbooks/add-apk-package.md`](runbooks/add-apk-package.md) |
| Add a kernel `sc-*` feature | [`runbooks/add-syscall-feature.md`](runbooks/add-syscall-feature.md) |
| Recover a wedged/hung VM | [`runbooks/recover-wedged-vm.md`](runbooks/recover-wedged-vm.md) |
| Toggle debug knobs | [`reference/subsystems/config-flags.md`](reference/subsystems/config-flags.md) |
| Run the acceptance playbooks | [`acceptance/`](../acceptance/) |

## Subsystem reference index

| Subsystem | Doc | Grade |
|---|---|---|
| Boot / MMU / DTB | [`reference/subsystems/boot.md`](reference/subsystems/boot.md) | B |
| Memory (PMM, heap, COW) | [`reference/subsystems/memory.md`](reference/subsystems/memory.md) | **C** |
| Scheduler / threads | [`reference/subsystems/scheduler.md`](reference/subsystems/scheduler.md) | A |
| Thread & thread-group lifecycle (states, teardown lock tree, eret leaves) | [`reference/subsystems/thread-lifecycle.md`](reference/subsystems/thread-lifecycle.md) | **C** |
| SMP / multikernel | [`reference/subsystems/smp.md`](reference/subsystems/smp.md) | **C** |
| Kernel locking (BKL + carve-outs) | [`reference/subsystems/locking.md`](reference/subsystems/locking.md) | B |
| Networking (box model, two stacks) | [`reference/subsystems/networking.md`](reference/subsystems/networking.md) | **C** |
| Rump stack (sysproxy, fiber) | [`reference/subsystems/rump-stack.md`](reference/subsystems/rump-stack.md) | **C** |
| SSH (built-in + userspace) | [`reference/subsystems/ssh.md`](reference/subsystems/ssh.md) | A |
| VFS (ext2, procfs, pipes) | [`reference/subsystems/vfs.md`](reference/subsystems/vfs.md) | A |
| Syscalls / Linux ABI | [`reference/subsystems/syscalls.md`](reference/subsystems/syscalls.md) | A |
| Containers / boxes / herd | [`reference/subsystems/containers.md`](reference/subsystems/containers.md) | B |
| Cargo features + env knobs | [`reference/subsystems/config-flags.md`](reference/subsystems/config-flags.md) | — |
| Build profiles / distributions (release, size, extreme-size, devbox, release-smp) | [`reference/build-profiles.md`](reference/build-profiles.md) | — |
| IRQ / console / RNG / async-fs | [`reference/subsystems/irq.md`](reference/subsystems/irq.md) etc. | B / B / B / A |
| In-kernel shell / editor | [`reference/subsystems/shell.md`](reference/subsystems/shell.md) / [`editor.md`](reference/subsystems/editor.md) | **C** / A |
| Drivers (GIC, timers, block, fw_cfg) | [`reference/subsystems/drivers/`](reference/subsystems/drivers/) | B / B / A / A |
| Exceptions (vector table, trap frame, ESR_EL1) | [`reference/subsystems/exceptions.md`](reference/subsystems/exceptions.md) | **C** |

Syscalls / Linux ABI now has 17 per-family docs under
[`reference/subsystems/syscalls/`](reference/subsystems/syscalls/) — grades
vary per family; `mem`, `net`, `signal`, `sync` are **C** (active risk,
touched in the Jun 2026 crisis).

**Still undocumented (deferred gap list):** audio (`src/audio.rs`) and the
framebuffer device itself (`src/ramfb.rs` — distinct from the `fb.rs` syscall
wrapper, which is documented), both Tier C / niche. The full gap list is in
[`reference/README.md`](reference/README.md) -> Not yet written.

## When in doubt

If a runbook or reference doc doesn't cover your problem, search `archive/` —
the historical investigations are thorough even if not action-oriented. Every
new doc has a "Background" footer linking back to the relevant archive originals.
