# The hooked scheduler preempts kernel threads

Date: 2026-11-10. Branch `akuma-multiarch-attempt-0`, on the way to making the hooked scheduler
the AArch64 default and deleting the SGI path (`docs/archive/AKUMA_MULTIARCH_ATTEMPT_0.md`).

## What was cooperative

The hooked IRQ entry (`akuma_exceptions::irq_hooked`) switched threads only when the scheduler
SGI had interrupted EL0: `preempt = from_el0 && irq == SGI_SCHEDULER`. A kernel thread kept the
core until it yielded. The SGI path has always preempted EL1 as well.

Four workarounds had grown around that gap, each correct for a cooperative kernel and each one
more way the two paths differed:

- `idle_halt` parked a non-idle caller with `sleep_us(1 ms)` instead of halting. A halted
  non-idle thread (a socket waiter, the async-main loop) relies on the tick to displace it, and
  there was no such tick (`docs/archive/HOOKED_IDLE_CPU_BURN.md`).
- The netpoll loop's hooked branch halted only when `x86_runnable_exists()` said nothing else
  could run, then yielded every lap.
- The `_across_preemption` NEON/FP boot tests passed vacuously.
- `x86_yield_now`'s BKL release before a switch was justified by the absence of preemption.

## Change

1. **`irq_hooked` preempts EL1** when `threading::el1_preemptible()` says so: the per-thread
   preemption count is zero (`PreemptGuard`, `disable_preemption`, the SGI scheduler's own
   gate); `POOL` is not held, because it is a plain spinlock, the switch takes it, and the
   interrupted code may hold it (the SGI handler's `try_lock` skips for the same reason); and
   the thread is `RUNNING`. A thread that has just published `WAITING` or `TERMINATED` is on
   its way into its own switch, and taking the core first would skip what it does in between.
2. **The AArch64 hooked switch masks IRQs** (`x86_yield_now` saves `DAIF`, masks, restores on
   resume), so a tick cannot land half-way through a voluntary switch and re-enter it. The
   switch already saved and restored `DAIF` per context, and a fresh thread starts at `DAIF = 0`.
   amd64 does not preempt ring 0 and is unchanged.
3. **The workarounds go.** `idle_halt` is one body on both paths again (non-idle halters
   `wfi` preemptibly). The netpoll loop takes the SGI path's `leave_kernel; wfi; enter_kernel`
   on hooked too, with or without `smp-shared`. The test and comment text says what is true now.

What is *not* a cooperative leftover, and stays: the hooked yield is a function call, so
caller-saved FP registers do not survive one (`neon_regs_across_yield` and
`fpcr_fpsr_across_yield` stay SGI-only), and a masked yield is fine there
(`yield_now_detects_masked_yield` stays SGI-only).

## The switch did not publish its root

Gating the change turned up an older defect. `pmm_conserved_across_spawn_exit_reap` failed
1 run in 3 at SMP=2 with exactly 91 pages "leaked". The free count was flat across every cycle;
the *baseline* moved. Two address spaces sat in the deferred-free queue
(`mmu::pending_ttbr_free_stats`: 166 user + 16 table frames, 2 x 91), and the gate holding them
was `any_core_on_l0`: core 0 and core 1 each still named a dead process's L0, while kernel
threads ran on the boot tables.

The hooked `switch_to` installed `TTBR0_EL1` with `akuma_switch::install_ttbr0_flush` and never
published the transition to the per-core live-root registry (`ACTIVE_L0`/`PREV_L0`), which the
SGI switch and `activate`/`deactivate` all bracket with `publish_l0_begin`/`publish_l0_end`. So:

- the registry went on naming whatever the core last `activate`d, pinning that address space
  after its process died, until the core next ran `activate`; the passing runs show the same
  queue, freed later: their free count rose by exactly 270 pages (3 x 90) during the first cycle;
- the reverse was the dangerous half. A root the switch installed was invisible to the free
  gate while it was live: the running slot's own record is consumed at switch-in, so neither
  gate covered it. That is the F8 class: a freed L0 installed into `TTBR0_EL1`
  (`docs/archive/BUG_FIX_LIST.md`, "F8"; the amd64 spelling is
  `docs/archive/AKUMA_AMD64_SSH_WEDGE_CONTEXT_SWITCH_PF.md`).

Fix: `switch_to` publishes around the install. After it the test passed 8 of 8 at SMP=2 with no
baseline jump; dead address spaces are freed at once.

The test also drives `drain_pending_ttbr_frees` after each forced reap, as it already drove the
reap, and on failure now prints per-cycle free counts, the heap and slot-stack totals, the
deferred-free queue and which core or slot gates each entry
(`mmu::pending_ttbr_free_l0s`, new).

`thread_slot_reclaim_on_spawn` took its cooldown baseline after the spawn loop. With thread 0
preemptible a tick during a long fill runs the threads already spawned, so the earliest
termination is the spawn loop's start; the baseline moved there.

## Verification

See the multiarch log, section "The hooked scheduler preempts EL1".

## Background

- `docs/archive/HOOKED_IDLE_CPU_BURN.md`: the park this replaces.
- `docs/archive/SECOND_LISTENER_SMP1_FREEZE.md`: why a non-idle halter must stay preemptible.
- `docs/archive/TRIM_FAT_COOPERATIVE_SCHEDULING.md`: the SGI path's own cooperative class,
  removed 2026-08-11.
- `amd64/src/sched.rs` `preempt_if_needed`: why amd64 still does not preempt ring 0 (its heap
  lock does not mask interrupts).
