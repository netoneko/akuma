# Real (shared-kernel) SMP

Classic symmetric multiprocessing: ONE shared kernel across all cores — one
page-table set, one PMM/heap, one global run queue, real cross-core locking. Source:
`src/smp_shared.rs`. Behind `cfg(kernel_smp_shared)` (the `smp-shared` feature, paired
with the `release-smp-shared` profile); the default build compiles none of it.

> **Stability: C (active development).** M0–M4 done as of 2026-07-19 (userspace runs +
> migrates across cores; one open boot item, below). Progress log:
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
  `process/children.rs`) — from "IRQs off" to "BKL held" in one stroke. The lock
  (`akuma_exec::sync::KernelLock`, driven via `akuma_exec::bkl`) is an owner-tracked,
  idempotent lock: **held iff a core is in EL1**, reconciled at EL transitions, so
  no per-thread depth crosses context switches. Contended acquisition is a **fair FIFO
  ticket wait** (M5c) so no core is starved — the reentrant/reconcile acquires take no
  ticket, keeping the counters balanced across context switches. Zero-cost no-op unless
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
| M4 — migration + hardening (cross-core wakeup deferred) | ✅ 1 thread on 4 cores |
| M5a — network SMP-safety, devbox boots to sshd under SMP | ✅ SMP=2 clean, SMP=4 works |
| M5b — BKL-free user page-fault path (per-AS `as_lock`) | ✅ Stages 1–3 + 4a: fault PTE edits under `as_lock`, file read/install split, **file-fault block I/O runs BKL-dropped** (A/B measured a BKL-wait reduction on a busybox ELF-fault storm). 4b (full flip) deferred |
| M5b BKL-hold profiler | ✅ Gated (`set_profiling`, default off). **Finding: IRQ/scheduler ≈70 % of contended BKL time, faults ≈20 %** under multi-process load |
| M5c — split the run-queue lock out of the BKL | ✅ Step 1 (POOL covers the whole context switch — switch atomic on POOL alone) done + verified SMP=2/4. Step 2 (scheduler runs BKL-free on EL0 preemption) implemented + its SMP≥4 deadlock **fixed** (lldb-confirmed 2026-07-20 cross-core circular deadlock, *not* the earlier "fairness/monopoly" guess): a BKL-free secondary claimed a thread `RUNNING` without the BKL while the BSP held the BKL in a cooperative `yield_now` wait. Two-part fix, both landed: (1) don't hold the BKL across a cooperative wait — `idle_halt` in `exec_with_io_cwd` + `test_parallel_processes`; (2) a **fair FIFO ticket `KernelLock`** to kill the residual livelock. Validated 3/3 at SMP=4 (`test_smp_shared_cooperative_wait`) + fixed the pre-existing SMP=4 `parallel_processes` race. **The step-2 toggle (`sched_bklfree_el0`) is now default-on (2026-07-20)** — validated on the self-test suite (3/3 SMP=4) and on the full devbox-smoltcp boot to sshd at **SMP=2** (SSH-in clean, 0 `[BKL] stuck`). See `debug-smp.md` §"M5c step-2". |
| M5d — blocking waits drop the BKL (`threading::blocking_relax`) | ✅ A thread parked in a blocking poll-wait must not hold the BKL, or it freezes every peer core. Fixed the **socket recv/accept/connect/send-space wait** (`akuma_net::socket::wait_until`), the **DNS-resolve wait** (`smoltcp_net.rs`, fires first on connect-by-hostname), the **demand-paging fault-slot spin** (`children.rs`), the **rump tap read** (rump-only), and hardened two fatal spawn parks (`spawn.rs`) to `mark_current_terminated` first. All route through one `blocking_relax()` (= `yield_now` then, under smp-shared, `idle_halt`); off smp-shared it is a plain `yield_now` (default build byte-identical). Regression test: `test_smp_shared_blocking_wait_peer_progress` (every core parked in `blocking_relax`, BSP still exec+reaps; 5/5 SMP=4). **Root cause**: the meow→LLM freeze — meow `connect`ed to the LLM then sat in the recv holding the BKL, freezing all 4 cores (thousands of `[BKL] stuck`). **After the fix**: meow streams full LLM responses at SMP=4 with **0** `[BKL] stuck`. |
| M5 — fine-grained locking (real ASIDs, split BKL, cross-core wakeup) | in progress (see M5b/M5d) |

> **Residual (→ M5 fine-grained locking):** the coarse BKL does not scale cleanly past
> 2 cores. `SMP=2` is contention-clean; `SMP=4` has a *nondeterministic* contention wedge:
> individual operations complete (meow streams several full LLM responses, the self-test suite
> passes, forktest spawns children), but **under sustained multi-core network load `SMP=4` can
> still wedge on the coarse BKL** after a while (observed: meow works for several exchanges then
> hangs). This is distinct from — and remains after — the M5d blocking-wait fix, which removed
> the *deterministic first-recv* freeze. Splitting the BKL into per-subsystem locks (a real
> NETWORK lock out of the BKL) is the fix; the profiler points at the scheduler/IRQ path
> (≈70 % of contended BKL time), which `sched_bklfree_el0` (M5c step 2, now default-on) attacks.
> **Use `SMP=2` for sustained/interactive workloads until the BKL is split.**
>
> The earlier "full devbox-smoltcp boot to sshd stalls under active secondaries" item was the
> M4 open item and is **resolved** by M5a (see below) — boot-to-sshd works at SMP=2 (reliable)
> and SMP=4 (works, subject to the residual above).

## Background

- [`../../archive/SMP_SHARED.md`](../../archive/SMP_SHARED.md) — full progress log.
- [`smp.md`](smp.md) — the multikernel (the other, share-nothing SMP model).
- [`scheduler.md`](scheduler.md) — the base scheduler this extends.
