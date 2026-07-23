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
| M5c — split the run-queue lock out of the BKL | ✅ Step 1 (POOL covers the whole context switch — switch atomic on POOL alone) done + verified SMP=2/4. Step 2 (scheduler runs BKL-free on EL0 preemption) implemented + its SMP≥4 deadlock **fixed** (lldb-confirmed 2026-07-20 cross-core circular deadlock, *not* the earlier "fairness/monopoly" guess): a BKL-free secondary claimed a thread `RUNNING` without the BKL while the BSP held the BKL in a cooperative `yield_now` wait. Two-part fix, both landed: (1) don't hold the BKL across a cooperative wait — `idle_halt` in `exec_with_io_cwd` + `test_parallel_processes`; (2) a **fair FIFO ticket `KernelLock`** to kill the residual livelock. Validated 3/3 at SMP=4 (`test_smp_shared_cooperative_wait`) + fixed the pre-existing SMP=4 `parallel_processes` race. **⚠️ The step-2 toggle (`sched_bklfree_el0`) defaults OFF** — it was briefly flipped on (2026-07-20) and reverted the same day: under heavy fork/exec churn at SMP≥4 the BKL-free path **leaks a fair-`KernelLock` ticket** (it is the only path that `reconcile`-acquires the BKL without a paired `enter_kernel`), drifting `next_ticket`/`now_serving` until the lock hard-deadlocks with `owner==0`. lldb-confirmed; A/B: flag-ON wedges within seconds under mixed fork/exec+meow load, flag-OFF runs 13/13 meow turns clean. Re-enabling needs the ticket-accounting leak fixed first (M5 follow-up). See `debug-smp.md` §"M5c step-2". |
| M5d — blocking waits drop the BKL (`threading::blocking_relax`) | ✅ A thread parked in a blocking poll-wait must not hold the BKL, or it freezes every peer core. Fixed the **socket recv/accept/connect/send-space wait** (`akuma_net::socket::wait_until`), the **DNS-resolve wait** (`smoltcp_net.rs`, fires first on connect-by-hostname), the **demand-paging fault-slot spin** (`children.rs`), the **rump tap read** (rump-only), and hardened two fatal spawn parks (`spawn.rs`) to `mark_current_terminated` first. All route through one `blocking_relax()` (= `yield_now` then, under smp-shared, `idle_halt`); off smp-shared it is a plain `yield_now` (default build byte-identical). Regression test: `test_smp_shared_blocking_wait_peer_progress` (every core parked in `blocking_relax`, BSP still exec+reaps; 5/5 SMP=4). **Root cause**: the meow→LLM freeze — meow `connect`ed to the LLM then sat in the recv holding the BKL, freezing all 4 cores (thousands of `[BKL] stuck`). **After the fix**: meow streams full LLM responses at SMP=4 with **0** `[BKL] stuck`. |
| M5e — deferred sibling kill at the EL1→EL0 boundary | ✅ `kill_thread_group` PHASE 1 no longer hard-marks siblings `TERMINATED` under smp-shared (which stranded spinlocks held by a sibling preempted mid-EL1 — lldb-confirmed: a forktest child died holding `BLOCK_DEVICE`, freezing every later disk I/O = the sshd "freeze"). It posts a per-thread pending-kill (`threading::request_thread_kill`), leaves the sibling schedulable, and grace-waits; each sibling self-terminates at its next kernel-exit boundary (`take_thread_kill_request`, checked in `rust_sync_el0_handler`'s BKL wrapper) where every lock has been released. Single-core keeps the direct mark (default build unchanged). Host tests + `test_deferred_kill_does_not_strand_locks`. |
| M5 — fine-grained locking (real ASIDs, split BKL, cross-core wakeup) | in progress (see M5b/M5d/M5e) |

> **State of SMP=4 under load (2026-07-20):** with `sched_bklfree_el0` **OFF** (the default) +
> the M5d blocking-wait fix, `SMP=4` now survives sustained mixed load — a fork/exec storm
> (`forktest -combined_stress`) + a busybox fork loop + **13/13 meow→LLM turns**, **0 `[BKL]
> stuck`**. The earlier "meow hangs after a few turns" wedge was the `sched_bklfree_el0` ticket
> leak (above), not a generic coarse-BKL limit — reverting the flag resolved it.
>
> **Remaining M5 work:** (1) fix the `sched_bklfree_el0` ticket-accounting leak so the
> scheduler-off-BKL optimization can ship (the profiler puts the scheduler/IRQ path at ≈70 % of
> contended BKL time — the biggest lever); (2) split a real NETWORK lock out of the BKL. Coarse
> BKL contention still shows as *transient* `[BKL] stuck` bursts during spawn-heavy work, but
> these recover. `SMP=2` remains the most contention-clean config.
>
> **Update 2026-07-21 (see the same-dated entry in the progress log):** the fork/exec
> corruption work landed a real `LifecycleGuard` (per-thread preemption disable around the
> lifecycle mutation windows), fixed a pre-existing stolen-yield race (`VOLUNTARY_SCHEDULE`
> is now per-core), and made the BKL's ticket FIFO **self-healing** after lldb proved it can
> leak a ticket and hard-deadlock all cores even with `sched_bklfree_el0` OFF —
> `[BKL] RECOVERED` log lines are live sightings of that still-unfixed leak. The fork-hammer's
> surviving crash family is now null-deref data corruption (CoW/TLB, hypothesis 4 in
> [`../../runbooks/debug-smp-fork-corruption.md`](../../runbooks/debug-smp-fork-corruption.md)).
>
> The earlier "full devbox-smoltcp boot to sshd stalls under active secondaries" item was the
> M4 open item and is **resolved** by M5a (see below) — boot-to-sshd works at SMP=2 (reliable)
> and SMP=4 (works, subject to the residual above).

### Separate issues surfaced 2026-07-20 (NOT the BKL/SMP-lock work)

Stress-testing with `forktest` and `rustc` turned up two userspace-visible failures. Both leave
the **kernel alive** (0 `[BKL] stuck`, console/heartbeat live) — neither is a BKL wedge. Tracked
separately:

- **`forktest_parent` hangs only when launched via the userspace `/bin/sshd` (devbox-smoltcp),
  NOT a kernel-commit regression and NOT the ticket leak.** It hangs at "Launching child 0": the
  parent's Go runtime parks in a `futex` and never reaches its own duration deadline (children
  close their report pipes with `write_count=0`). **Controlled comparison (all SMP=1, flag off):**

  | Build / launcher | forktest |
  |---|---|
  | `main` kernel, in-kernel ssh server | ✅ passes (~5 s, all children reaped) |
  | branch `--profile release-smp-shared --features smp-shared`, in-kernel ssh | ✅ passes |
  | branch `devbox-smoltcp` (`userspace-sshd`, `/bin/sshd`) | ❌ hangs |

  The **same smp-shared kernel** runs forktest fine under the in-kernel ssh server and hangs under
  the userspace `/bin/sshd`. So the variable is the **launch environment** — the deep userspace
  fork chain `sshd → shell → forktest_parent → forktest_child` under `userspace-sshd` — not the
  BKL/SMP kernel code. (An earlier "regression vs main" note here was a *confounded* comparison:
  `main` was tested via the in-kernel ssh server, the branch via userspace `/bin/sshd`.) It is
  **not** the `sched_bklfree_el0` ticket leak: that needs SMP≥2 contention + the flag on; this
  reproduces at SMP=1 with the flag off. Likely a Go-runtime futex/timer/signal or pipe-`EPOLLHUP`
  issue exposed by that fork chain. `rustc` (`hello.rs`) compiles + runs on the branch (RC=0,
  ~68 s SMP=1), so fork/exec/mmap themselves are fine.

- **Forked children `SIGSEGV` under SMP=4 — memory corruption in the fork/exec/process-creation
  path during the high-concurrency BRINGUP window.** First seen as an intermittent `/bin/sshd`
  SIGSEGV; **reproduced** by fork-hammering **during boot** (`scratchpad/sshd_crash_hunt.py`:
  reboot + immediate concurrent fork load). Key timing finding: it fires only in the **first few
  seconds** (secondaries onlining + herd forking every service at once) — a settled instance
  survives 30×20 concurrent fork rounds cleanly. So it is a **bringup-window concurrency race**,
  not steady-state fork load. Children crash **right after `[FORK-COW] shared N pages`**.

  **It is not one bug — the crash signature is heterogeneous across boots**, which is the tell of
  memory corruption rather than a single logic error:
  - `x0..x19` all zero (inherited callee-saved `x19` lost) → null-deref;
  - child's user PC = a **kernel** address (`rust_sync_el0_handler_inner`) with other regs correct;
  - a fresh process writing to `0x0` during musl startup (`set_tid_address`/`sigprocmask`);
  - `[DA-MISS] pid=N ppid=0 … checked 0 mmap_regions` — a process with an **empty mmap-region list
    and no parent** (a half-initialized / clobbered `Process`).

  **Hypotheses ruled out by instrumentation:** (1) missing TLB flush — `flush_tlb_all()` is a
  correct cross-core broadcast (`tlbi vmalle1is; dsb ish; isb`); (2) the zeroed-GPR fallback in
  `get_saved_user_context` — a `[CTXBUG]` probe on the fallback-for-current-thread case **never
  fired**; (3) capture-time kernel PC — a `[CAPBUG]` probe on `fork`'s captured `parent_ctx.pc`
  **never fired** (the kernel PC appears *after* capture). So the corruption is post-capture and
  affects `Process`/context/page-table/mmap-region state broadly.

  **Updated conclusion (2026-07-21): NOT the BKL-drops.** A decisive experiment forced **both**
  block-I/O BKL-drops OFF (so every EL1 excursion holds the BKL end-to-end → EL1 fully serialized
  across cores) and the crash **still fired on boot 1/20**, all signatures present. So the cause
  is **not** concurrent EL1. The exposure is that a multi-step lifecycle op (`fork_process`,
  `do_execve`/`replace_image`, exit/teardown) is **not atomic across preemption** — IRQs are on
  during the handler (`exceptions.rs:174`), so a thread preempted mid-op has the BKL reconciled
  away, exposing half-built `Process` / `THREAD_CONTEXTS` / process-table state — and/or
  genuinely-parallel **EL0** over fork-CoW-shared frames. The user-PC-=-kernel-addr signature
  resolves exactly to `rust_sync_el0_handler_inner+0x0`, a value never stored as a pointer in the
  source ⇒ context-memory corruption/aliasing. **Full dossier + rank-ordered hypotheses + repro:
  [`../../runbooks/debug-smp-fork-corruption.md`](../../runbooks/debug-smp-fork-corruption.md).**

## Background

- [`../../archive/SMP_SHARED.md`](../../archive/SMP_SHARED.md) — full progress log.
- [`smp.md`](smp.md) — the multikernel (the other, share-nothing SMP model).
- [`scheduler.md`](scheduler.md) — the base scheduler this extends.
