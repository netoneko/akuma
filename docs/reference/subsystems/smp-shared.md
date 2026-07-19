# Real (shared-kernel) SMP

Classic symmetric multiprocessing: ONE shared kernel across all cores — one
page-table set, one PMM/heap, one global run queue, real cross-core locking. Source:
`src/smp_shared.rs`. Behind `cfg(kernel_smp_shared)` (the `smp-shared` feature, paired
with the `release-smp-shared` profile); the default build compiles none of it.

> **Stability: C (active development).** M0–M3 done as of 2026-07-19 (userspace runs
> across cores). Progress log: [`../../archive/SMP_SHARED.md`](../../archive/SMP_SHARED.md).

This is the **inverse** of the multikernel ([`smp.md`](smp.md), `cfg(kernel_smp)`),
which is share-nothing (one kernel per core, replicated `.data`/`.bss`, disjoint RAM
partitions, syscall forwarding). Here nothing is replicated: a kernel `static` is
genuinely shared cross-core. The two are **mutually exclusive** — `build.rs` panics if
both features are set.

## Build & run

```bash
cargo build --profile release-smp-shared --features smp-shared
SMP=2 cargo run --profile release-smp-shared --features smp-shared   # -smp 2 in QEMU
```

`SMP=N` (cargo_runner.sh) sets QEMU `-smp N`. The primary test image is
**devbox-smoltcp** (the default devbox: native smoltcp stack, built-in ssh dropped,
real SMP) — see `scripts/build_devbox_smoltcp.sh` and
[`../build-profiles.md`](../build-profiles.md).

## Design (approved roadmap)

- **Coexist** with the multikernel (new feature, default build untouched).
- **Big-Kernel-Lock first**, then fine-grained. The BKL upgrades the kernel's
  pervasive single-core invariant — `with_irqs_disabled` (IRQ masking) gives mutual
  exclusion only on one core; the worst offenders are the 218+
  `lookup_process() -> &'static mut Process` sites (`process/table.rs`,
  `process/children.rs`) — from "IRQs off" to "BKL held" in one stroke. The lock
  (`akuma_exec::sync::KernelLock`, driven via `akuma_exec::bkl`) is an owner-tracked,
  idempotent spinlock: **held iff a core is in EL1**, reconciled at EL transitions, so
  no per-thread depth crosses context switches. Zero-cost no-op unless
  `cfg(kernel_smp_shared)`. M1 wires it on the syscall path (`rust_sync_el0_handler`
  wrapper + `enter_user_mode`); the IRQ/scheduler reconciliation is M2.
- **Single global run queue + lock**; each core's current thread is `TPIDRRO_EL0`
  (already per-core hardware). The single `ThreadPool.current_idx`/`round_robin_idx`
  do not generalize and are being retired.
- **TLB (M3, done):** the page-table *modification* flushes (`flush_tlb_all`/`_asid`/
  `_page`/`_range_all_asid`) broadcast over the inner-shareable domain (`...is`) under
  `kernel_smp_shared`, so a user address space edited on one core is coherent on peers
  running it. Real per-AS ASIDs are deferred (a perf optimization; all use ASID 0 today,
  which is correct given private per-core TLBs + full local switch-flush + IS edits).

## Boot / bringup

1. `probe_dtb(dtb_ptr)` (from `kernel_main`, before heap init) parses `/cpus` + `/psci`,
   stashes MPIDRs by `aff0 = mpidr & 0xff`.
2. `bringup_secondaries()` (after `gic::init`) PSCI `CPU_ON`s each secondary at the
   `secondary_entry_shared` trampoline (asm, `.text.boot`), which loads the BSP's
   **shared** boot `TTBR0`/`TTBR1` and tail-calls `secondary_shared_start`.
3. `secondary_shared_start(_ctx, core_idx)` (M2c) adopts its boot context as this core's
   idle thread (`adopt_current_as_core_idle` → `TPIDRRO_EL0`, `register_core_idle`),
   inits its per-PE GIC receive path (CPU interface + redistributor + scheduler SGI 0 +
   timer PPI 27; device space is identity-mapped via boot L1[0]), installs the shared
   `exception_vector_table`, arms the shared 10 ms CNTV tick, enables IRQs, and enters an
   idle loop (release-BKL / WFI / re-acquire, preemption enabled). From there the timer
   tick's self-SGI preempts idle onto any READY thread in the shared scheduler.

Self-tests (`process_tests.rs`): `test_smp_shared_cores_online` asserts online count ==
`probed_core_count - 1`; `test_smp_shared_scheduler` spawns kernel workers and asserts
they run on more than one core.

## Status

| Milestone | State |
|---|---|
| M0 — cores online on shared kernel | ✅ SMP=2/4 verified |
| M1 — BKL primitive + syscall-path wiring | ✅ BSP verified, no deadlock/regression |
| M2a — IRQ/scheduler-path BKL + eret reconcile | ✅ BSP verified |
| M2b — SMP-safe scheduler: per-core idle | ✅ BSP verified |
| M2c — secondaries run the shared scheduler | ✅ threads on 2 & 4 cores |
| M3 — userspace on secondaries (+ inner-shareable TLB) | ✅ userspace on 2 & 4 cores |
| M4 — migration + cross-core wakeups + hardening | next |
| M5 — fine-grained locking (real ASIDs, split BKL) | planned |
| M3 — userspace on secondaries | planned |
| M4 — migration + cross-core wakeups | planned |
| M5 — fine-grained locking | planned |

## Background

- [`../../archive/SMP_SHARED.md`](../../archive/SMP_SHARED.md) — full progress log.
- [`smp.md`](smp.md) — the multikernel (the other, share-nothing SMP model).
- [`scheduler.md`](scheduler.md) — the base scheduler this extends.
