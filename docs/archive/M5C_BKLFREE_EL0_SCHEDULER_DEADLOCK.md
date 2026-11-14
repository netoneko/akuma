# M5c step-2: the BKL-free EL0 scheduler's deadlock and its fix (2026-07-20)

**Status: history.** The BKL-free EL0-preempt scheduler path (`sched_bklfree_el0`), its
toggle and the ticket-free acquire it needed were removed on 2026-11-11 with the SGI
scheduler they belonged to ([`SGI_SCHEDULER_RESIDUE.md`](SGI_SCHEDULER_RESIDUE.md)). The two
fixes this found stay: cooperative waits drop the BKL (`idle_halt`), and the BKL is a fair
FIFO ticket lock. The reproduction steps below no longer apply. Moved here from
`docs/runbooks/debug-smp.md`.

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

## Background

- [`SMP_SHARED.md`](SMP_SHARED.md) — the M0–M5 log.
- [`../reference/subsystems/locking.md`](../reference/subsystems/locking.md) — the ticket
  invariant.
