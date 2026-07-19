# Real (shared-kernel) SMP

Classic symmetric multiprocessing: ONE shared kernel across all cores — one
page-table set, one PMM/heap, one global run queue, real cross-core locking. Source:
`src/smp_shared.rs`. Behind `cfg(kernel_smp_shared)` (the `smp-shared` feature, paired
with the `release-smp-shared` profile); the default build compiles none of it.

> **Stability: C (active development).** M0 only as of 2026-07-19. Progress log:
> [`../../archive/SMP_SHARED.md`](../../archive/SMP_SHARED.md).

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
  `process/children.rs`) — from "IRQs off" to "BKL held" in one stroke.
- **Single global run queue + lock**; each core's current thread is `TPIDRRO_EL0`
  (already per-core hardware). The single `ThreadPool.current_idx`/`round_robin_idx`
  do not generalize and are being retired.
- **TLB (M2):** switch the local `vmalle1`/`aside1`/`vaae1` flushes to inner-shareable
  `...is` (hardware broadcast) and give each `UserAddressSpace` a real ASID (today all
  use ASID 0) so shared address spaces don't alias cross-core.

## Boot / bringup (M0)

1. `probe_dtb(dtb_ptr)` (from `kernel_main`, before heap init) parses `/cpus` + `/psci`,
   stashes MPIDRs by `aff0 = mpidr & 0xff`.
2. `bringup_secondaries()` (after `gic::init`) PSCI `CPU_ON`s each secondary at the
   `secondary_entry_shared` trampoline (asm, `.text.boot`), which loads the BSP's
   **shared** boot `TTBR0`/`TTBR1` and tail-calls `secondary_shared_start`.
3. `secondary_shared_start(_ctx, core_idx)` bumps the shared `ONLINE_COUNT`, prints
   over the shared UART (device space is identity-mapped via the boot L1[0] block), and
   parks in `WFE`. GIC/timer/scheduler join in later milestones.

Self-test: `process_tests.rs::test_smp_shared_cores_online` asserts online count ==
`probed_core_count - 1`.

## Status

| Milestone | State |
|---|---|
| M0 — cores online on shared kernel | ✅ SMP=2/4 verified |
| M1 — Big Kernel Lock scaffolding | next |
| M2 — shared scheduler (SGI/idle/TLB-IS/ASID) | planned |
| M3 — userspace on secondaries | planned |
| M4 — migration + cross-core wakeups | planned |
| M5 — fine-grained locking | planned |

## Background

- [`../../archive/SMP_SHARED.md`](../../archive/SMP_SHARED.md) — full progress log.
- [`smp.md`](smp.md) — the multikernel (the other, share-nothing SMP model).
- [`scheduler.md`](scheduler.md) — the base scheduler this extends.
