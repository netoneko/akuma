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
| Filesystem / ext2 / procfs errors | [`reference/subsystems/vfs.md`](reference/subsystems/vfs.md) + `archive/EXT2_FIRST_DATA_BLOCK_FIX.md` |
| Porting a new binary (missing syscalls) | [`reference/subsystems/syscalls.md`](reference/subsystems/syscalls.md) -> Porting + `archive/*_MISSING_SYSCALLS.md` |
| A/B run shows no difference / boot doesn't match the features you built | [`reference/subsystems/locking.md`](reference/subsystems/locking.md) -> playbook rule 5 + `archive/BKL_VFS_CARVE_OUT.md` §17 |
| `[BKL] RECOVERED` lines under SMP load / "can I just delete the BKL?" | [`reference/subsystems/locking.md`](reference/subsystems/locking.md) -> "What the BKL is still the only lock for" + [`archive/BKL_PHASE7_AUDIT.md`](archive/BKL_PHASE7_AUDIT.md) |
| `[BKL] stale dropped-window ... healed` / converting a syscall off the BKL | [`reference/subsystems/locking.md`](reference/subsystems/locking.md) -> "The per-syscall BKL opt-out list" + [`archive/BKL_PHASE7F_OPTOUT_LIST.md`](archive/BKL_PHASE7F_OPTOUT_LIST.md) |
| `test_epoll_multi_poller_pipe FAILED: woken=1` / a boot-suite test that fails ~1 boot in 3 | [`archive/EPOLL_MULTI_POLLER_PIPE_FLAKE.md`](archive/EPOLL_MULTI_POLLER_PIPE_FLAKE.md) — test defect, not a kernel bug; do not accept/reject a change on one boot |
| Benchmarking in-VM rustc / SMP scaling vs Linux | [`archive/BKL_RUSTC_SCALING_BASELINE.md`](archive/BKL_RUSTC_SCALING_BASELINE.md) + `scripts/bkl_rustc_bench/` |
| `rustc -O big.rs` produces no artifact and prints nothing / `(opt cgu.N) SIGSEGV` | §5.1's SIGSEGV mode is **root-caused + FIXED** (cross-core stack-sharing in the scheduler): [`archive/SMP_SHARED_ONCPU_GATE.md`](archive/SMP_SHARED_ONCPU_GATE.md). A residual *hang* mode (futex-parked rustc, VM healthy) remains — same doc §6 |
| SMP=4 `[BKL] stuck owner=N tag=511` storm (boot or load), EL1 crash with `ELR=0x8` / SP outside thread stack | **FIXED** — the ON_CPU scheduler gate: [`archive/SMP_SHARED_ONCPU_GATE.md`](archive/SMP_SHARED_ONCPU_GATE.md) + [`runbooks/debug-smp.md`](runbooks/debug-smp.md) first row |
| AB-BA deadlock hunting / "can this `Spinlock` just be an atomic?" | [`reference/subsystems/locking.md`](reference/subsystems/locking.md) -> gate 2 + [`archive/BKL_FINE_GRAINED_LOCKING_PLAN.md`](archive/BKL_FINE_GRAINED_LOCKING_PLAN.md) §7.3a (phase 7g classification table) |

## Task list - "I want to do X"

| Task | Start here |
|---|---|
| Boot a VM and connect via SSH | [`runbooks/boot-and-connect.md`](runbooks/boot-and-connect.md) |
| Build the devbox and SSH in | [`runbooks/build-devbox.md`](runbooks/build-devbox.md) |
| Compile the kernel inside Akuma (self-host) | [`runbooks/selfhost-kernel-build.md`](runbooks/selfhost-kernel-build.md) |
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
