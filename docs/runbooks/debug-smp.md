# Debug SMP (shared-kernel multiprocessing)

Symptom-driven debugging for **real (shared-kernel) SMP** — `cfg(kernel_smp_shared)`,
the `smp-shared` feature on the `release-smp-shared` profile. For architecture see
[`../reference/subsystems/smp-shared.md`](../reference/subsystems/smp-shared.md); for the
running dev log see [`../archive/SMP_SHARED.md`](../archive/SMP_SHARED.md). This is the
*inverse* of the share-nothing multikernel ([`../reference/subsystems/smp.md`](../reference/subsystems/smp.md),
`cfg(kernel_smp)`); the two are mutually exclusive.

> **Stability: C (active development).** M0–M4 + M5a/M5b/M5c done. The BKL is now a **fair
> FIFO ticket lock**, which (with `idle_halt` on cooperative waits) fixed the former
> nondeterministic SMP=4 contention hang — the self-test suite now reaches the
> `test_mmap_file_oom` baseline reliably at SMP=4. Run devbox single-core unless you are
> specifically working on SMP.

## Mental model in 60 seconds

- **One Big Kernel Lock (BKL)** serializes all kernel (EL1) execution across cores. It is
  **owner-tracked, idempotent, binary**: "held by a core iff that core is in EL1",
  reconciled at every EL transition rather than balanced (`akuma_exec::sync::KernelLock`,
  driven via `akuma_exec::bkl`). Entry from EL0 acquires; `eret` to EL0 releases; a nested
  EL1 exception and its return leave it held. So there is exactly one release per
  excursion — no per-thread lock depth crosses a context switch.
- **The IRQ path holds the BKL across the whole excursion** (`rust_irq_handler_with_sp`),
  including the scheduler decision + context save, then **reconciles** the BKL to the EL of
  the frame it will `eret` into (release for EL0, keep/acquire for EL1) using the SPSR at
  `IRQ_FRAME_SPSR_OFFSET`. After a context switch that frame is the *incoming* thread's.
- **Userspace holds no BKL** — genuine parallelism. Faults' block I/O is dropped off the
  BKL (M5b Stage 4a); PTE edits are serialized by a per-address-space lock
  (`Process::as_lock`) so a fault and an `munmap`/`mmap`/`mprotect` on the same address
  space exclude correctly.
- **The shared run queue** is `POOL: Spinlock<ThreadPool>` (`threading/mod.rs`); each
  core's current thread is `TPIDRRO_EL0` (hardware, per-core).

## Build & run

```bash
cargo build --profile release-smp-shared --features smp-shared
SMP=2 cargo run --profile release-smp-shared --features smp-shared     # -smp 2
SMP=4 cargo run --profile release-smp-shared --features smp-shared     # -smp 4
```

`SMP=N` (cargo_runner.sh) sets QEMU `-smp N`. Boot self-tests
(`src/process_tests.rs::test_smp_shared_*`) run when tests are compiled in (no
`no-tests`). The devbox image is `--features devbox-smoltcp` (adds `no-tests`).

## Symptom → cause → fix

| Symptom / signature | Cause | Fix / next step |
|---|---|---|
| `[BKL] stuck: owner=N waiter=M` **flood that never stops**, no forward progress | A core is wedged in EL1 holding the BKL (real deadlock) — OR the BSP panicked while holding it (see the panic dump just above the flood). | If preceded by a panic dump, it's that panic — fix the panic, the flood is an artifact. Else attach gdb (below) and find what `owner`'s core is spinning on. |
| A few dozen **transient** `[BKL] stuck` during a spawn-heavy / FS test, then progress | Coarse-BKL contention — a core held the BKL >10 M spins (~10 ms) while peers waited. Expected today. | Profile it (below). The dominant holder is usually the scheduler. Not a bug. |
| `SMP=4` works for several exchanges then **wedges under sustained fork/exec + network load** (e.g. meow hangs on ~turn 4), flood is `owner=0` (unowned!) and never stops | **ROOT-CAUSED (2026-07-20): a `sched_bklfree_el0` ticket leak.** `owner=0` in the stuck line means the lock is *unowned* (`owner` stores `core_id+1`) yet waiters spin — the fair `KernelLock`'s `next_ticket` ran ahead of `now_serving` with no owner to advance it. The BKL-free EL0-preempt path is the only one that `reconcile`-acquires without a paired `enter_kernel`, so it can take a ticket with no matching release. | **FIXED by reverting `sched_bklfree_el0` to default-OFF.** A/B: flag-ON wedges in seconds under `forktest -combined_stress` + busybox fork loop + meow; flag-OFF runs the same load 13/13 clean, 0 stuck. To re-enable, fix the reconcile-path ticket accounting first. To confirm an imbalance live: `lldb`, `memory read --size 4 --count 4 &KERNEL_LOCK` → `next_ticket` > `now_serving` with `owner==0`. |
| SMP=4 **nondeterministically wedges** in a spawn/timing-heavy test (`parallel_processes`, "Mixed cooperative") — a re-run passes | **FIXED (2026-07-20).** Was a cooperative wait-loop (`yield_now` only) holding the BKL while a child stranded RUNNING on a BKL-blocked peer, the unfair test-and-set then livelocking recovery. | Fixed by `idle_halt` on the wait loops (drop the BKL) + the fair FIFO ticket BKL. If a *new* wedge appears, check for a yield-only wait holding the BKL (see "M5c step-2" below). |
| `[BKL] stuck: owner=1` flood, workload frozen, **only** with `sched_bklfree_el0` ON | The M5c step-2 **cross-core circular deadlock** (re-root-caused 2026-07-20 with lldb — the earlier "monopoly on an unfair lock" note was wrong; it's a hard hang, not a re-grab race). A BKL-free secondary claims a thread `RUNNING` without the BKL while the BSP holds the BKL in a cooperative yield-wait for it. | The two-part fix (idle_halt on cooperative waits + fair ticket BKL) fixes *this* deadlock — but step-2 has a **separate ticket-leak bug under load** and stays **default-OFF** (see the `owner=0` row above + the dedicated section below). |
| `owner=N waiter=N` (same core) self-deadlock | A non-idempotent re-acquire, or holding a spinlock then taking the BKL and being re-entered. | `KernelLock::acquire` handles nested `enter_kernel` from a timer IRQ two ways: a reentrant fast path (`owner==me` → return, no ticket) and **masking local IRQs across the ticket wait** so a nested exception can't take a second ticket. If you added a lock that nests with the BKL, check ordering: `BKL > as_lock > {PMM, page tables, block, ...}`. |
| `[SGI] POOL contended, skipped N ticks` | The scheduler SGI used `POOL.try_lock()` and the current thread already held POOL (mid-op). One preemption tick skipped — harmless; the next tick retries. | Only worry if it never stops (a leaked POOL guard). |
| Boots at SMP=1 but hangs/deadlocks at SMP≥2 in a subsystem | That subsystem isn't SMP-safe: an inner spinlock (`NETWORK`, `SOCKET_TABLE`, block, VFS) is held across a context switch, so under the BKL a peer spins on it while holding the BKL. | Wrap the critical section in a `PreemptGuard` (akuma-net pattern) so the inner lock is never held across a switch; or fold it under the BKL/`as_lock` ordering. |
| Text interleaves per-char on the console (`[SM[PS-sMP...`) | Multiple cores writing the UART with no cross-core console lock. | Cosmetic. Use `safe_print!` (heap-free, secondary-safe). |

## Profiling BKL contention (who holds the lock?)

`crates/akuma-exec/src/sync.rs` has a **BKL-hold profiler**, gated off by default
(`set_profiling(true/false)` — it false-shares a per-core tag line on every kernel entry,
which perturbs timing, so keep it off outside a measurement window):

- `set_holder_tag(core, tag)` is called at each kernel entry (syscall number / `HOLD_TAG_FAULT`
  / `HOLD_TAG_IRQ`) to record what a core is doing while it holds the BKL.
- A waiter samples the blocking owner's tag on first contention and adds its spin count to
  `WAIT_BY_HOLDER[tag]` — so `wait_by_holder(tag)` tells you which excursions made peers wait.
- `contention_spins()` / `reset_contention_spins()` give the total cross-core BKL wait (a
  wait-time proxy) for A/B measurement.

`test_smp_shared_fault_parallelism` (process_tests.rs) is the template: it toggles an
optimization, runs a workload, and dumps the top holders. **Finding (2026-07):** under a
multi-process (spawn) workload the **IRQ/scheduler path holds ~70 %** of contended BKL
time, faults ~20 %, syscalls the rest — so the scheduler is the dominant contention source.

To A/B a change safely, measure at **SMP=2** (fewer cores → less timing noise), enabling the
profiler only around the window.

### Runtime toggles (A/B / debugging)

- `smp_shared::set_fault_bkl_drop_enabled(bool)` — the M5b Stage 4a optimization (drop the
  BKL around a file-fault's block I/O). Default **on**.
- `smp_shared::set_exec_bkl_drop_enabled(bool)` — the M5c hold-shortening optimization (drop
  the BKL around execve's ELF reads: the main-binary read in `do_execve` **and** the
  dynamic-interpreter read in the ELF loader, the latter via the `ExecRuntime.exec_bkl_drop_enabled`
  hook). Default **on**. Safe: each read runs on a not-yet-installed image/AS and uses only
  VFS/ext2/`blk` + the self-locked heap allocator, none BKL-protected — the same profile as the
  fault drop. This is the "shorten BKL holds" lever (execve ELF-load was the dominant coarse-BKL
  hold). A/B-measured by the `smp_shared_exec_parallelism` self-test (SMP=2): busybox exec storm,
  ~26–63% fewer BKL spins with the drop ON across boots.
- `smp_shared::set_sched_bklfree_el0_enabled(bool)` — the M5c step-2 optimization (run the
  scheduler SGI BKL-free when it preempted EL0). Default **off** and **not safe to enable under
  load**: it fixes the *cooperative-wait* deadlock (two-part fix below) but has a **separate
  ticket-accounting leak** in its reconcile path that hard-deadlocks SMP≥4 under fork/exec
  churn (`owner=0` flood — see the symptom table). Fix that leak before re-enabling. The M5c
  step-1 foundation (POOL covers the whole context switch) is always active regardless.
- `sync::set_profiling(bool)` — the BKL-hold profiler. Default **off**.

## M5c step-2 (BKL-free EL0 scheduler): the deadlock and its fix

**Re-root-caused 2026-07-20 with lldb over the QEMU gdbstub.** The earlier (2026-07-19)
"monopoly on an unfair lock" explanation was **wrong** — reasoned, never confirmed at the
instruction level. It is a **hard cross-core circular deadlock**, not a fairness/re-grab
problem. Enabling `sched_bklfree_el0` and booting `SMP=4` reproduces a `[BKL] stuck:
owner=1` flood (~3000–5000 events, all `owner=1` = the BSP, vs. ~102 toggle-off) with the
workload permanently wedged. To reproduce: add
`smp_shared::set_sched_bklfree_el0_enabled(true)` right after `bringup_secondaries()` in
`main.rs` (do not commit), build, `SMP=4 GDB=1 INSTANCE=1 cargo run …`, grep the log for
`BKL] stuck`, then attach lldb on `:1235`.

> **Watch out — two overlapping symptoms.** The *terminal* state everyone sampled before is
> a red herring: it is the pre-existing `test_mmap_file_oom` **panic** → the panic's
> semihosting `HLT #0xF000` (`0xd45e0000`) → `EC=0x0` → `rust_sync_el1_handler`'s terminal
> `loop{wfe}` on core 0 *holding the BKL* (per the table row above: "if preceded by a panic
> dump, the flood is an artifact"). Sample the **pre-panic** flood (`grep -c PANIC` must be
> `0`) to see the real step-2 bug.

**Mechanism (confirmed by live register/memory state at the pre-panic wedge).**

1. The BSP (core 0) holds the BKL and is spinning in `exec_with_io_cwd`'s cooperative
   wait-loop — `while !channel.has_exited() && !is_thread_terminated(tid) { yield_now() }` —
   waiting for the EL0 child it spawned (backtrace: `run_all_tests → exec_with_io →
   exec_with_io_cwd → yield_now → …irq_handler → sgi_scheduler_handler_with_sp`). The loop
   runs entirely in EL1, so the BKL is never reconciled-to-EL0-released; the BSP holds it
   throughout.
2. The child thread is state **`RUNNING`**, and it is the current thread (`TPIDRRO_EL0`) of a
   **secondary** — but that secondary is frozen in the inlined `KernelLock::acquire` spin
   inside `enter_kernel` (`rust_irq_handler_with_sp`, the `ldar/cmp/b.eq` loop). It took a
   syscall/device IRQ while running the child and now waits on the BKL the BSP holds.
3. The BSP's `ThreadPool::schedule_indices` only ever selects a **READY, non-idle** thread.
   The child is `RUNNING` (skipped); the only READY thread is a per-core idle
   (`IDLE_SLOT_FOR_CORE`, skipped by `IS_IDLE_THREAD`). So the scan returns `None`, and the
   BSP spins its yield loop forever holding the BKL. **Circular wait:** BSP holds BKL & waits
   for child → child is RUNNING on a secondary → secondary needs the BKL → BSP holds it.

**Why the toggle is the trigger.** The BKL-free EL0-preempt path lets a secondary **claim a
READY thread and mark it `RUNNING` without acquiring the BKL**, while the BSP holds the BKL
in a cooperative wait for that thread. Once the child is `RUNNING` on a secondary the BSP
won't migrate it (it moves only READY threads) and the secondary freezes the instant the
child needs EL1. With the toggle **off** a secondary can only claim a thread by first taking
the BKL — impossible while the BSP holds it — so the child stays **READY** and the **BSP
itself** runs it, reconciling to EL0 and releasing the BKL. No deadlock. (The old note's
"per-tick circulation" is really "secondaries can't steal a thread out from under a
BKL-holding cooperative waiter.")

**The fix is TWO parts, both required — both now DONE** (validated at SMP=4 with the
40-iteration exec-and-wait stress `test_smp_shared_cooperative_wait`, 3/3 clean where the
unfixed build hung ~100%):

1. **A kernel thread must not hold the BKL across a cooperative wait-loop.** Drop +
   re-acquire the BKL around the `yield_now` wait — done for `exec_with_io_cwd` and
   `test_parallel_processes` via `idle_halt`. This alone cut the hang from ~100% to a
   **~25% residual**.
2. **A fair / queued BKL.** The residual ~25% was a *livelock*: after the waiter dropped the
   BKL (idle_halt WFI), the unfair test-and-set let the other spinning secondaries and the
   waiter's own next-tick re-grab starve the one secondary holding the BKL-free-stolen
   child, so it never got the BKL to un-strand it. (Attaching gdb *hides* this — the stub's
   periodic all-core halts perturb the race enough to let it through; it reproduces without
   gdb.) **Done:** `KernelLock` (`crates/akuma-exec/src/sync.rs`) is now a **FIFO ticket
   lock** — a contended acquirer takes a ticket and waits its turn (masking local IRQs for
   the wait so a nested exception can't take a second, un-serviceable ticket); the owner's
   release advances `now_serving` by one. Reentrant/reconcile acquires take no ticket, so the
   counters stay balanced across the non-lexical acquire/release of context switches.

**Result:** step-2 is now safe to enable at SMP≥4 (left defaulting off only because flipping
the kernel-wide default is a separate call). As a bonus, the same idle_halt + fair-BKL combo
**also cured the pre-existing nondeterministic SMP=4 `parallel_processes` hang** (same
yield-wait-under-BKL root cause) — the full self-test suite now reaches the `test_mmap_file_oom`
baseline reliably at SMP=4.

## Live debugging with gdb/lldb

The host has no `gdb`; attach `lldb` to QEMU's gdbstub. See
[`../../docs/`](../reference/subsystems/smp-shared.md) and the memory note on
lldb+gdbstub. Recipe:

```bash
INSTANCE=1 GDB=1 SMP=2 cargo run --profile release-smp-shared --features smp-shared   # gdbstub on :1235
# in another shell:
lldb target/aarch64-unknown-none/release-smp-shared/akuma \
  -o "gdb-remote localhost:1235" -o "thread backtrace all"
```

For the devbox image add `DISK=devbox.img` and `--features devbox-smoltcp,no-tests`.
`thread backtrace all` shows every core; the BKL `owner` in a `[BKL] stuck` line is
`aff0 + 1`, so `owner=1` is core 0 (the BSP).

## Waiting for boot / SSH (never block on the QEMU job)

QEMU runs forever. Poll the log; never `job_output --wait` the QEMU process:

```bash
until grep -q "SSH Server\] Listening" run.log 2>/dev/null; do sleep 2; done
```

The self-test suite halts on a failed threading test
(`!!! THREADING TESTS FAILED - HALTING !!!`, an intentional semihosting `hlt`, *not* a
crash). `parallel_processes` timing out here used to be the SMP=4 race; that is fixed (fair
BKL + idle_halt) — if it recurs, treat it as a fresh regression, not "re-run and hope".

## Gotchas

- **Console garble** makes `grep` miss lines. Sanitize first:
  `LC_ALL=C tr -d '\r' < run.log | LC_ALL=C tr -c '[:print:]\n' ' '`.
- **`as_lock` must never be held across alloc / block I/O / a context switch** — the PMM
  OOM/reclaim path can re-enter it. Hold it only for short PTE-edit windows (IRQs off).
  Allocate before, free after.
- The default / size / extreme / multikernel builds compile **none** of this — every
  `bkl`/`as_lock`/profiler entry is a zero-cost no-op unless `cfg(kernel_smp_shared)`.
- All SMP work is gated; if a change regresses a non-SMP build, you broke a `cfg` gate.
