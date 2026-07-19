# Real (shared-kernel) SMP — progress log

Running log of the real symmetric-multiprocessing effort: ONE shared kernel across
all cores (one page-table set, one PMM/heap, one run queue, real cross-core locking).
This is the **inverse** of the multikernel in `MULTIKERNEL.md` (one-kernel-per-core,
share-nothing). For the current-state design see
[`../reference/subsystems/smp-shared.md`](../reference/subsystems/smp-shared.md).

Everything is behind `cfg(kernel_smp_shared)` (the `smp-shared` feature, paired with
the `release-smp-shared` profile). `build.rs` makes `smp` and `smp-shared` mutually
exclusive. The default build compiles none of it.

Design decisions (user-approved plan): coexist with the multikernel as a new feature;
incremental milestones M0..M5; **Big-Kernel-Lock first**, fine-grained later; single
global run queue + lock; per-core current thread via `TPIDRRO_EL0`. Developed and
verified on the **devbox-smoltcp** image (native smoltcp stack, built-in ssh dropped),
which is now the default devbox — rump_server work is deferred.

---

## M0 — Cores online on the shared kernel ✅ (2026-07-19)

**Goal:** bring N cores up on the *shared* boot page tables, PMM, and heap — no
isolation, no partitions. Each secondary reports online and parks in `WFE`.

**Implementation** (`src/smp_shared.rs`, new; ~300 lines):
- Build wiring: `smp-shared` feature + `cfg(kernel_smp_shared)` (build.rs), the
  `release-smp-shared` profile (Cargo.toml), a 4 MB size branch in
  `scripts/cargo_runner.sh`.
- Topology probe (`probe_dtb`, called from `kernel_main` before heap init): parses
  `/cpus` + `/psci`, stashes MPIDRs indexed by `aff0 = mpidr & 0xff`. Compact copies
  of the multikernel's `read_mpidr`/`psci_call`/`resolve_dtb`/`psci_is_hvc` so
  `src/smp.rs` stays byte-for-byte untouched.
- Bringup (`bringup_secondaries`, after `gic::init`): PSCI `CPU_ON` each secondary at
  the new `secondary_entry_shared` trampoline, then a bounded spin until every one
  bumps the shared `ONLINE_COUNT`.
- Trampoline (`secondary_entry_shared`, `global_asm!` in `.text.boot`): mirrors
  `crate::smp::secondary_entry` + boot.rs MMU setup, but loads the BSP's **shared**
  boot `TTBR0`/`TTBR1` (`boot_ttbr0_addr`/`boot_ttbr1_addr`, published MMU-off by
  boot.rs) — never a restricted per-core table — and tail-calls
  `secondary_shared_start`. Own 16 KiB/core stacks in `.bss.smp_shared`.
- `secondary_shared_start(_ctx, core_idx)`: `ONLINE_COUNT.fetch_add(1)`, `safe_print!`
  "core N online", then `WFE` park.

**Key realizations:**
- The trampoline's shared-table load is exactly what real SMP wants — the multikernel
  only diverges into an isolated table *after* the trampoline, so no `smp.rs` change
  was needed, just a fork of the post-trampoline path.
- The boot page tables identity-map device space via the L1[0] 1 GiB block
  (0x0–0x40000000), so a secondary on the boot tables reaches the shared UART (and
  later its own GIC redistributor) directly — **no boot.rs device-map change needed
  for M0.** The planned "map all GICR frames" step is deferred to M2, when secondaries
  actually enable interrupts.
- A plain kernel `static` (e.g. `ONLINE_COUNT`) is genuinely shared cross-core in this
  build (not replicated) — the whole premise, and it works.

**Verification** (`release-smp-shared`, MEMORY=256M):
- `SMP=2`: `[SMP-shared] ✓ 1 secondary core(s) online (shared kernel)`;
  boot self-test `[Test] smp_shared_cores_online PASSED (1/1 ...)`.
- `SMP=4`: `✓ 3 secondary core(s) online`; `PASSED (3/3 ...)`.
- Boot self-test in `src/process_tests.rs::test_smp_shared_cores_online`, run FIRST in
  `run_all_tests` so it is observed even though an unrelated, pre-existing
  memory-pressure test (`test_mmap_file_oom`, process_tests.rs:4690) aborts the suite
  later at 256M — confirmed pre-existing: identical panic with `SMP=1`, no secondary
  running.

**Known M0 cosmetic:** multiple cores writing the shared UART concurrently interleave
characters (`[SMP[S-sMP-sharedhared]`), because there is no cross-core console lock
yet. Harmless; addressed when the console is serialized (M1/M2).

**Files:** `src/smp_shared.rs` (new), `src/main.rs` (module + probe/bringup calls),
`build.rs`, `Cargo.toml`, `scripts/cargo_runner.sh`, `src/process_tests.rs`.

---

## M1 — Big Kernel Lock scaffolding (next)

Add a nesting-aware `KERNEL_LOCK` in `akuma-exec`, acquired on kernel entry and
released around blocking/context-switch points, upgrading the pervasive
`with_irqs_disabled` single-core invariant (esp. the 218+
`lookup_process() -> &'static mut Process` sites) from "IRQs off" to "BKL held".
Retire `ThreadPool.current_idx` as the current-thread source of truth. Not yet
running two cores through the scheduler.

## M2..M5 (planned)

See the approved plan for the full roadmap: M2 shared scheduler (SMP-safe SGI handler,
per-core idle, inner-shareable TLB flushes `...is`, real per-AS ASIDs, map all GICR
frames), M3 userspace on secondaries, M4 migration + cross-core wakeups, M5
fine-grained locking (PMM/heap out first, then the `&'static mut Process` refcount
rework, then VFS/net).
