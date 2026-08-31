# Kernel locking: the BKL and how carve-outs work

> **Stability: B (verify behaviour).** The rules below are extracted from six
> completed carve-outs (`no-bkl-network`, `no-bkl-vfs`, `no-bkl-process`,
> `no-bkl-mm`, `no-bkl-drivers`, `no-bkl-irq`) and are consistent
> across all of them, but the underlying BKL-removal effort is itself grade **C** —
> check `smp-shared.md` and the archive docs below for what has landed since
> this was written.

This is the distilled "how to add a lock, or remove one, without re-deriving
the lessons from scratch" reference. It is not a history — for the blow-by-blow
of any specific fix, follow the links into `docs/archive/`.

## The model

Under `smp-shared` (real shared-kernel SMP, `cfg(kernel_smp_shared)`), one
kernel runs across all cores. Correctness is anchored by a single **Big Kernel
Lock** (`akuma_exec::sync::KernelLock`, driven via `akuma_exec::bkl`):
**held iff a core is in EL1**, reconciled at every EL transition
(`reconcile_for_spsr`, called from IRQ/exception epilogues), with a fair FIFO
ticket wait so no core starves. Off `smp-shared` it's a zero-cost no-op.

The BKL upgraded the kernel's old single-core invariant (`with_irqs_disabled`,
which only gives mutual exclusion on *one* core) to something SMP-safe in one
stroke, at the cost of serializing everything. The ongoing work is **carving
subsystems out from under it** — not by inventing a new lock hierarchy, but by
noticing that most kernel state already has its own fine-grained lock (fd
table, socket table, ext2 superblock/BGD, block cache, network stack), and the
BKL is simply *redundant* for syscalls that only touch that state. See
`smp-shared.md` for the milestone-by-milestone history (M0–M5).

Six carve-outs exist today: `no-bkl-network` (all smoltcp net syscalls +
socket `read`/`write`), `no-bkl-vfs` (ext2 read paths, `mmap`, `unlinkat`,
`openat`, `renameat`/`renameat2`, `mkdirat`, `fchmodat`), `no-bkl-process`
(`fork_process`'s CoW page-copy window), `no-bkl-mm`
(`mprotect`/`madvise`/`munmap`/`mremap`/`mmap`), `no-bkl-drivers`
(`getrandom`, `/dev/urandom` read/pread, `/dev/dsp` write), and `no-bkl-irq` (the timer IRQ's dispatch in
`rust_irq_handler_with_sp`). All six are default-on in `smp-shared`
(since 2026-08-01). `no-bkl-process` is the first that is not about I/O: it overlaps a
CPU-bound page copy with peer-core EL1 rather than a disk or wire wait, and it is
the first to lean on a *page-table* inner lock (`as_lock`) rather than a
subsystem's own state locks. `no-bkl-mm` is the first phase picked by the plan
rather than by attribution (no mm syscall has ever been measured as a significant
BKL holder) — see `BKL_MM_CARVE_OUT.md` §5 — and the first where the audit found
real gaps (an unguarded free-list, a page-table mutation with no `as_lock`) rather
than only rediscovering an existing lock. `no-bkl-irq` is the first carve-out
that is not a *syscall* excursion at all — the timer IRQ never calls
`enter_kernel` in the first place on this path, so there is no dropped-BKL
"window" to open/close, only a single `if` at the dispatch site; see
[`BKL_PHASE7A_TIMER_IRQ_CARVE_OUT.md`](../../archive/BKL_PHASE7A_TIMER_IRQ_CARVE_OUT.md).

### The FIFO ticket invariant

The fair wait is a ticket lock: `acquire` takes `next_ticket`, spins until `now_serving`
reaches it, and the owner's `release` advances `now_serving` by one. The invariant that
keeps it live is **not** "the two counters stay equal in aggregate" — it is:

> Every **allocated ticket** must be matched by exactly one **ownership episode** that
> advances `now_serving` past it.

Aggregate equality is strictly weaker and admits a wedge: a ticket value that no core will
ever hold still has to be *passed*, and nothing advances past it, so the next acquirer
waits on a `now_serving` that can never arrive. Two ways to break the real invariant, with
opposite signatures:

| Break | `now_serving` ends up | Log signature |
|---|---|---|
| an advance with no allocation behind it | **ahead** of `next_ticket` | `RECOVERED (reticket-skipped)` bursts; fair lock degrades to test-and-set |
| an allocation with no ownership episode behind it | **behind** — a lost slot | `[BKL] stuck owner=0` bursts, then `RECOVERED (advanced-lost)` after 20M spins |

**A re-ticket is not automatically a leak.** `reticket-skipped` — a waiter finds
`now_serving` has already advanced past its ticket — is benign rebalancing: the release
that advanced `now_serving` past the waiter's slot is exactly what consumed that
allocation, so re-ticketing costs nothing (`sync.rs:608-611`). The kind that actually
leaked was `reticket-owned`: a waiter reaching `serving == my_ticket` found someone else
already owning the lock at that exact turn and abandoned its ticket for a fresh one —
and nothing else ever consumed the abandoned allocation, so `now_serving` ended one short
of `next_ticket` until the 20M-spin `advanced-lost` self-heal forced it. That path has
been **removed**: at HEAD, a waiter that loses the ownership CAS at its own turn falls
through and keeps its ticket, spinning in place until the holder's release lands it in
the `reticket-skipped` case above (`sync.rs:595-612`).

The out-of-band acquirer is what forces re-tickets. `acquire_no_ticket` (the BKL-free
EL0-preempt reconcile, `reconcile_for_spsr_no_ticket`, **default-on** via
`SCHED_BKLFREE_EL0_ENABLED`) takes ownership without occupying a queue slot. It must
therefore leave the FIFO **completely** untouched — no ticket on acquire, and no advance on
release (a per-core `barged` flag on `KernelLock` tells `release` which kind of hold it is
ending). Anything less consumes a live waiter's slot and forces that waiter to re-ticket.
An earlier compensating `next_ticket.fetch_add(1)` in the barge kept the counters equal and
still wedged, for exactly the phantom-ticket reason above.

**Reading the diagnostics.** `[BKL] stuck` prints `owner=`, `waiter=`, `tag=`:

- **`owner=0` means the lock is FREE** while cores spin — a lost ticket, *not* a holder
  that won't let go. Chase the ticket accounting, not the "owner".
- `tag=` is meaningless unless the build has `bkl-profile`: `set_profiling` is only enabled
  there (`src/bkl_profile.rs:48`), every tag-writing function early-returns when it is off,
  and `HOLDER_TAG` initialises to `HOLD_TAG_UNKNOWN`, so **`tag=511` on a normal build just
  means "profiler off"**. It narrows nothing. See [Attribution tooling](#attribution-tooling).
- `RECOVERED (advanced-lost)` is the only kind that means a ticket was genuinely lost;
  `reticket-skipped` is benign rebalancing. They are counted separately —
  `kernel_lock_lost_ticket_recoveries()` vs the aggregate `kernel_lock_recoveries()` — and
  an assertion that wants "the accounting is sound" must watch the former. The real failure
  mode of watching the aggregate instead is **false alarms**, not missed leaks: the fix
  above converts what used to leak into benign `reticket-skipped` recoveries, and those
  bump the aggregate on a perfectly healthy, contended run (`sync.rs:1616-1618`).

Regression: `sync::tests::kernel_lock_barge_against_waiters_does_not_leak_serving_slots`
(host). Note that asserting *final drift* does not catch this class — the `advanced-lost`
self-heal repairs the counters, so a leaking build still ends balanced (it just pays a 20M
spin freeze per leak). The leak-specific signal is `kernel_lock_lost_ticket_recoveries()`
— non-zero there means a ticket was genuinely lost, full stop. The boot self-test
`test_no_bkl_ticket_recoveries` (`src/process_tests.rs:874`) currently asserts the
*aggregate* `kernel_lock_recoveries()` instead, which — per the false-alarm note just
above — can fail on a healthy run that merely saw `reticket-skipped` bursts under real
contention; treat a failure there as "go check `kernel_lock_lost_ticket_recoveries()`",
not as proof of a leak by itself.

## The carve-out playbook

For a given syscall or subsystem:

1. **Scope the window as narrowly as possible.** Wrap only the on-disk/on-wire
   work in the guard (`VfsBklGuard` / `NetBklGuard`), not the whole function.
   Keep outside the window: user-string copies, fd-table-only lookups, and
   every early-error return (`EBADF` etc.) — those don't touch the shared
   state the carve-out is protecting, so they shouldn't pay for a BKL
   drop+reacquire.
2. **Don't invent a new coarse lock.** The first instinct — wrap the syscall
   in one new `NETWORK_LOCK`/`VFS_LOCK` — doesn't work for anything that
   blocks: `accept`/`connect`/`recv` and friends yield via `blocking_relax()`,
   and holding a coarse lock across that yield serializes *everything* behind
   one blocked call, which is strictly worse than the BKL (which *is* dropped
   during the wait). Instead, drop the BKL for the syscall's duration and rely
   on the fine-grained locks that state already has. A coarse
   ordering-enforcement scaffold (`akuma_net::locks`) existed for this until
   **2026-08-30**, when it was deleted: nothing ever called it, and it could not
   have worked — `acquire_network_lock` bound the spinlock to a `_guard` that
   dropped at function return, so it granted no exclusion at all, and its
   `HELD_LOCKS` ordering state was one global `AtomicU32` that two cores would
   corrupt under `smp-shared`. If you are tempted to rebuild it, read
   [`../../archive/REDIS_ROUND_TRIP_STAGE_TRACE.md`](../../archive/REDIS_ROUND_TRIP_STAGE_TRACE.md)
   §2 first — it preserves the code and measures why finer granularity is the
   wrong lever here.
3. **Write a boot self-test that drives the real syscall entry point**
   (`handle_syscall(...)`), covering: the on-disk work happening inside the
   window, dirfd/cwd-relative resolution, early-error paths staying balanced,
   and any fast-path branches (device nodes, etc.) that must stay outside the
   window.
4. **Boot-verify correctness at SMP=2**: full self-test suite green, 0 PANIC/
   WILD, 0 stale dropped-window-ledger heals.
5. **Measure contention with a same-binary A/B**, not just a before/after
   commit comparison — build with the `bkl-profile` feature (`cfg(
   kernel_bkl_profile)`, `src/bkl_profile.rs`), run the identical workload
   twice at SMP=4 with only the new guard toggled, and compare the syscall's
   `[BKLPROF]` cumulative/peak share and total workload spin count. A
   same-binary A/B is what tells "this conversion measurably helped" apart
   from "this syscall was never contended in the first place" — see
   `BKL_VFS_CARVE_OUT.md` §13.3 for a worked example.

   Two rules the campaign learned the hard way about *how* to toggle and *what*
   to sum (`BKL_VFS_CARVE_OUT.md` §17):

   - **Toggle in source, not in cargo features.** Swap the guard out
     (`git show HEAD:src/syscall/fs.rs > …`) and keep the feature set byte-
     identical across both sides. A source edit forces a recompile, so the ELF
     is always newer than the `.bin`; alternating feature sets was what made
     `cargo_runner.sh`'s old `[ "$ELF" -nt "$BIN" ]` guard boot the *other*
     side's kernel behind a "Finished in 0.1s" cargo line. That guard is gone
     (objcopy now always regenerates), but the habit is still the safer one —
     and if you must alternate features, verify the boot is the kernel you
     think it is by grepping the `.bin` for a string only one side contains.
   - **Sum only the workload's windows.** `analyze.py`'s default is whole-boot
     and gets diluted by idle/teardown; filtering by spin magnitude instead of
     by time is worse still, since it counts service bringup as workload on a
     boot whose regimen starts early. Take `drive.py`'s REGIMEN START/DONE (or
     the first/last regimen `execve` in the serial log) and sum `[BKLPROF]`
     per-tag spins over the `t=` windows spanning that interval, on both sides.
6. **Verify data integrity, not just "didn't crash."** A syscall that early-
   returns an error on every call (broken, not fast) can look identical to a
   correctly BKL-free one in a profiler — `rm -f`/`cp` swallow errors. Check
   real content: sha256 digests across the carve-out, `ls`/`e2fsck` after
   deletes, etc.
7. **Let attribution — not intuition — pick the next target.** Phase 0's
   estimate ("scheduler/IRQ holds ~70% of contended time") was wrong for the
   real workload; measured, it was 27%, and a single syscall (`unlinkat`) was
   72.6%. Converting `unlinkat` didn't just remove its share — it promoted the
   next-largest holder (`openat`, 36.6%) into visibility. Don't convert the
   next syscall on a checklist; profile, then convert whatever the profile
   names. This kept paying off down to small holders: `renameat` (2.8%),
   `mkdirat` (2.6%), and `fchmodat` (1.8%) only showed up at all once the
   driving workload was extended with a `mkdir`+`rename`+`chmod` phase — the
   standing regimen never exercised them.

   **Corollary, learned by getting it wrong: re-measure before quoting a share,
   and never quote one across a profiler change.** This rule used to conclude
   here that `irq/sched` was 66–73% of remaining spin once every named syscall
   was converted. That figure came from `BKL_VFS_CARVE_OUT.md` §16, whose
   profiler over-credited `irq/sched` (per-core instead of per-thread
   attribution); §18 re-measured the same workload at **88.8% → 23.0%** after
   fixing it, and two carve-outs landed after that. Measured fresh at HEAD with
   Phases 2–6 all on, `irq/sched` is **~21–23%** and the largest remaining
   holders are the process-lifecycle syscalls, which have no inner lock at all
   (`execve` ~22%, `clone` ~10–13%). See
   [`BKL_PHASE7_AUDIT.md`](../../archive/BKL_PHASE7_AUDIT.md) §1 for the current
   numbers and §5 for where the effort belongs now. Phase 7a (`no-bkl-irq`,
   below) then moved it again: a same-binary A/B on the same regimen measured
   **24.7% (BKL-held) → 10.2% (`no-bkl-irq` on)** — see
   [`BKL_PHASE7A_TIMER_IRQ_CARVE_OUT.md`](../../archive/BKL_PHASE7A_TIMER_IRQ_CARVE_OUT.md)
   §5. Same rule applies to *this* number too: it is one A/B on one regimen,
   not a new standing baseline — re-measure before quoting it elsewhere.

## Correctness rules learned the hard way

Each of these was a real bug found during a carve-out, not a hypothetical.

- **Latch the guard's decision at construction; never re-read a runtime
  toggle in `drop()`.** `VfsBklGuard`/`NetBklGuard` consult a runtime on/off
  toggle (for A/B and as a kill switch), and that toggle is genuinely flipped
  while guards are live. A guard that reads it in both `new()` and `drop()`
  can drop the BKL under "on" and then decline to re-acquire under "off" —
  unbalancing the syscall wrapper's `leave_kernel` and corrupting the ticket
  FIFO for every other core. Fix: latch the decision once, in a field, at
  construction. (`BKL_VFS_CARVE_OUT.md` §2.4; host test
  `vfs_bkl_guard_latched_arm_stays_balanced_across_toggle_flip`.)

- **Mask IRQs/preemption per *attempt*, never across an unbounded wait.**
  `read_state`/`write_state`-style acquisition loops can spin for a long time
  (they carry orphaned-lock recovery paths for exactly that reason). Masking
  IRQs across the whole wait starves this core's timer for the entire
  contended window — and if the current holder is a thread *on this core*,
  nothing can ever run to release it. Take the preempt/IRQ guard immediately
  before the non-blocking `try_lock()`, keep it only on success, drop it
  before the backoff spin. (§3.1.)

- **Field order in a guard struct is load-bearing.** When a guard carries both
  an inner lock guard and a preemption/IRQ-mask guard, declare the lock field
  *before* the hold field, so the lock releases before preemption/IRQs are
  restored on drop. The reverse order reopens, for an instant, exactly the
  window the guard exists to close. (§3.)

- **An interrupt landing inside a dropped-BKL window will silently convert it
  back to BKL-held, unless you track that on purpose.** The reconcile
  invariant ("BKL held iff EL1") is enforced at every `eret` against the
  *interrupted frame's* SPSR — which is EL1 — so a timer IRQ during a dropped
  window makes the epilogue re-take the BKL and hold it for the rest of the
  window, silently re-serializing what was supposed to be BKL-free. Fixed
  with a **per-thread depth of open dropped-BKL windows** consulted at every
  `eret`: "target is EL1 *and* the resumed thread has an open window" also
  means release. Thread-scoped, not core-scoped, because windows survive
  preemption and cross-core migration. Any new dropper (a new guard type, a
  new blocking-wait site) must go through this same
  `bkl::dropped_window_open()/close()` pair, not a bare `leave_kernel`/
  `enter_kernel`. (§9; `akuma_exec::bkl::DroppedWindowLedger`.)

- **A guard's inner spinlock must mask local IRQs too, if it's reachable from
  a BKL-free window.** Otherwise: the window holds the inner lock, a nested
  IRQ does an unconditional `enter_kernel()` hard-spin wanting the BKL, and if
  the BKL's owner is spinning on that same inner lock, the two cores deadlock
  AB-BA. (Network Phase 2, `PreemptGuard` now masks IRQs for exactly this
  reason.)

- **Never hold a lock (BKL or inner) across a blocking/cooperative wait.** A
  thread parked in `blocking_relax()`/`schedule_blocking()` while holding
  *anything* freezes every peer core that wants it — this was the literal
  cause of the meow→LLM freeze (recv held the BKL across the wait). Route
  every blocking wait through `blocking_relax()`, which itself is `yield_now`
  (or, under smp-shared, `idle_halt`) with no lock held across it. (`smp-
  shared.md` M5d.)

- **Raise signals after releasing locks, not while holding them.** `pipe_write`
  used to call `send_sigpipe()` while holding the global `PIPES` spinlock;
  default disposition terminates the writer inline, which re-enters
  `pipe_close_write` and tries to re-acquire the same lock the caller is still
  holding — a same-core self-deadlock that then starves every peer piled up
  on the BKL. (Network Phase 2, `test_sigpipe_terminate_no_deadlock`.)

- **The allocator is a lock re-entrant too: never grow an unbounded buffer
  inside a lock the OOM path takes.** The same `PIPES` lock had a second,
  subtler instance of the rule above. `pipe.buffer` was an unbounded
  `VecDeque` and `pipe_write` extended it while holding `PIPES` with IRQs
  disabled, so a failing allocation ran `alloc_error_handler` *inline* with
  `PIPES` still held → `return_to_kernel` → `cleanup_process_fds` →
  `pipe_close_write` → same non-reentrant lock. Note the pattern: it isn't
  only *your* code that can re-enter — any allocation inside a locked section
  is a call into the OOM path, which is allowed to tear down processes. The fix
  is to bound the buffer (`PIPE_CAPACITY`, 64 KiB, matching Linux) so the
  allocator is never reached there; growing to ~2 GB also starved the reader
  outright, since each multi-hundred-MB realloc-and-copy ran with IRQs off and
  the BKL held. Bounding pipes means writes can now be **short** — see
  `pipe_write`'s contract and `pipe_write_all_blocking` for framed in-kernel
  callers, which silently truncated frames until they were audited.
  (`test_pipe_write_caps_at_capacity`.)

- **Wake blocked writers on the last-reader close, symmetrically with readers
  on the last-writer close.** Once writers could block (above), the missing
  mirror image of `pipe_close_write`'s EOF wake became a permanent hang: a
  writer parked on a full pipe learns the pipe broke only by retrying and
  seeing `read_count == 0`, so nothing woke it and it never took the EPIPE that
  raises SIGPIPE. Any state a sleeper can only observe by re-polling needs a
  wake on *every* transition into it, not just the one you had a sleeper for
  when you wrote the code.
  (`test_pipe_close_read_wakes_blocked_writer`.)

- **Release your own resources before notifying anyone who could reap you.**
  `sys_exit_group` closed its fd table *after* `notify_child_channel_exited`,
  which wakes the parent's `wait4`; the parent could reap the process on a peer
  core and `unregister_process` → `mark_thread_terminated` stopped the exiting
  thread mid-syscall, before it ever released its fds. This survived only by
  accident for years: the reap freed the `Process` synchronously and
  `SharedFdTable`'s `Drop` closes the table, so the reaper did it on the
  victim's behalf. Phase 7e deferred that drop and the accident stopped
  working — during the boot self-test suite the collector never runs at all, so
  `head`'s pipe read end was held forever and `yes` slept forever on a full
  pipe. Don't let releasing your own state depend on winning a race, and treat
  "a `Drop` somewhere else happens to clean this up" as a latent bug, not a
  design. (`test_sigpipe_terminate_no_deadlock`; same ordering hazard the
  `kill_thread_group`-before-notify comment already documents.)

- **Don't hard-terminate a sibling thread mid-EL1.** Marking a sibling
  `TERMINATED` while it's preempted inside a kernel excursion stranded
  whatever spinlock it was holding (a forktest child died holding
  `BLOCK_DEVICE`, freezing all later disk I/O). Fix: post a pending-kill
  request, leave the sibling schedulable, and let it self-terminate at its
  own next kernel-exit boundary, where every lock is guaranteed released.
  (`smp-shared.md` M5e.)

- **Route special fd kinds before the guard opens, not through it.**
  Pipe-backed `UnixSocket` fds must not run through the BKL-free socket path
  — dispatch on fd kind before the `NetBklGuard`/`VfsBklGuard` window starts.

- **A leaked dropped-window depth on a recycled thread slot is catastrophic**
  (unrelated EL1 code would run BKL-free). Two independent safety nets: the
  EL1 fault-kill path force-clears the ledger (its destructors never run —
  the kernel stack is abandoned), and EL0 entry with a nonzero depth is a
  tripwire that heals and logs rather than trusting the stale value.

- **Deferred cleanup gated to "must run from thread 0" has no recovery path
  under load.** Slot/resource reclamation that only runs from one specific
  core's idle loop starves the moment that core is never idle — which is
  exactly when reclamation is needed. Prefer: reclaim on demand at the
  allocation-miss site (with a retry), plus a steady-state collector on
  whatever thread runs a busy poll loop, gated by a *time cooldown*
  (protects against recycling a slot too early) rather than a *caller-identity*
  gate (which provides no actual safety — a CAS on the state transition does).
  (§11.4/§11.7.)

- **Deferring a `Drop` also invalidates every `Arc::strong_count`-based
  liveness test on what the dropped object held.** `cleanup_process_fds` closed
  a CLONE_FILES-shared fd table when `Arc::strong_count(&proc.fds) == 1` —
  correct while reaping freed a dead sibling's `Process` (and its `Arc` clone)
  synchronously. Phase 7e's deferred reclamation keeps RETIRED zombies' clones
  alive for ≥10ms (and, during the synchronous boot self-test phase, forever),
  so an externally-killed multithreaded group never reached count 1 and its
  pipe read ends were released only by the deferred collector — the same
  hung-peer shape as the exit_group close-after-notify entry above. Fix:
  decide liveness by scanning for other *live* (not-exited, still-ACTIVE)
  processes sharing the same `Arc` (`for_each_process` + `Arc::ptr_eq`), not by
  refcount. General form: after deferring a drop, audit not just the side
  effects the `Drop` provided (the entry above) but every *predicate* that
  used the object's prompt destruction as a signal.
  (`test_external_kill_closes_shared_fds`;
  [`BKL_PHASE7E_ACCESS_HALF.md`](../../archive/BKL_PHASE7E_ACCESS_HALF.md) §3.)

- **"Reclaim on demand at the allocation-miss site," the rule just above, has a
  precondition the THREAD_STATES case happened to satisfy and the process table
  didn't: the reclaimed object's teardown must not need any lock the caller
  could already be holding.** `THREAD_STATES` reclaim only flips atomics — no
  such risk. `Process` teardown (`Box::drop` → `UserAddressSpace::drop`) frees
  page-table frames and releases an ASID, which needs its own locks; calling it
  from inside `register_process` — reachable from deep in fork/clone/spawn,
  whose lock context `register_process` doesn't control — self-deadlocked the
  instant a caller already held one of those locks (found via a real boot hang
  landing Phase 7e's process-table reclaim, `test_sigpipe_terminate_no_deadlock`
  exercising real fork/exec/pipe/SIGPIPE traffic). Fixed by *not* reclaiming at
  the allocation-miss site for this one: `register_process` just panics on a
  genuinely full table, and the periodic collector (a call site with no ambient
  lock context) is the only reclaimer. See
  [`BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md`](../../archive/BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md).

- **Every caller of a lock must agree on IRQ discipline — a struct with two
  independent spinlocks doesn't get that for free.** `ProcessChannel` keeps
  `buffer` and `pollers` as two *separate* `Spinlock`s (unlike `KernelPipe`,
  which protects both under one lock). Every pre-existing `pollers` access
  (`add_poller`, the wake loops in `write`/`write_stdin`/`set_exited`,
  `is_poller_registered`) took it with IRQs *enabled*. Adding
  `check_set_writer` (exec-channel write backpressure) needed `buffer` and
  `pollers` locked *together*, under `with_irqs_disabled`, to close its own
  TOCTOU against a concurrent drain — making it the first caller ever to take
  `pollers` with IRQs *disabled*. Mixing the two disciplines on the same lock
  is a live deadlock, not just untidy: if a thread holding `pollers` (IRQs
  enabled) is preempted by a timer tick, and the preempting thread then spins
  on `pollers` with IRQs disabled, that spinner can never itself be preempted
  back off — so the original holder never resumes to release the lock.
  Permanent freeze, no panic, and the timer tick itself stops firing, so even
  a periodic debug heartbeat goes silent. Reproduced within seconds of real
  throughput (a producer child outrunning a 1 KiB-at-a-time reader). Fixed by
  making *every* access to both locks consistently `with_irqs_disabled`, not
  by special-casing the new call site. General form: before adding a caller
  that needs a stricter discipline (masked IRQs, nested locking) than a lock's
  existing callers use, audit every existing caller — don't assume a "small"
  new critical section is safe just because it's short.
  (`test_process_channel_write_bounded_backpressure`;
  [`EXEC_CHANNEL_LARGE_OUTPUT_TRUNCATION.md`](../../../userspace/sshd/docs/EXEC_CHANNEL_LARGE_OUTPUT_TRUNCATION.md)
  §7.2.)

## Syscall → lock map

Ground truth as of 2026-08-01, verified against `src/syscall/{fs,net,mem,proc,fb}.rs`
and the inner-lock call sites in `crates/akuma-ext2/src/ext2.rs`,
`crates/akuma-exec/src/process/fd.rs`, `src/block.rs`, `src/vfs/mod.rs`,
`src/{rng,audio}.rs`, and `crates/akuma-net/src/{socket,smoltcp_net}.rs` — not transcribed from the
archive doc's prose. Re-derive rather than trust this table once it's more
than a few months old; grep the guard names below to check it hasn't drifted.

### `no-bkl-vfs` — `src/syscall/fs.rs` / `src/syscall/mem.rs`

| syscall | guard scope | inner lock(s) held while BKL dropped |
|---|---|---|
| `sys_read` (File arm, `fs.rs:390`) | inside `File` match arm | fd table `Spinlock` → ext2 `read_state` (`read_at`, `ext2.rs:1918`) → block cache + `BLOCK_DEVICE` `Spinlock` (`block.rs:244`) |
| `sys_pread64` (`fs.rs:714`) | inside `File` arm | same as `sys_read` |
| `sys_pwrite64` (`fs.rs:782`) | inside `File` arm | ext2 `write_state` (`write_at`, `ext2.rs:1962`) |
| `sys_write` (`fs.rs:807`, `new_if`) | whole fn (match nested in per-chunk loop) | ext2 `write_state` (`write_at`) |
| `sys_lseek` (`fs.rs:1540`, `new_if`) | whole fn | ext2 `read_state` (`metadata`, `ext2.rs:2268`, via `file_size`) |
| `sys_fstat` (`fs.rs:1590`) | inside `File` arm | ext2 `read_state` (`metadata`) |
| `sys_newfstatat` (`fs.rs:1716`) | after path resolution | ext2 `read_state` (`is_symlink`, `read_symlink`, `metadata`) |
| `sys_statx` (`fs.rs:1964`) | after path resolution | ext2 `read_state` (`is_symlink`, `metadata`) |
| `sys_getdents64` (`fs.rs:2448`) | after `File`-kind match | ext2 `read_state` (`read_dir`, `ext2.rs:1825`) |
| `sys_openat` (`fs.rs:1323`) | before `resolve_symlinks`, i.e. before the `/dev/*`/`/proc/self/exe` fast paths (see Phase 7c) | ext2 `read_state` (`resolve_symlinks`'s `read_symlink`, `exists`/`lookup_path`), then `write_state` for `O_CREAT`/`O_TRUNC` (`write_file`, `ext2.rs:1869`) + `chmod` |
| `sys_mkdirat` (`fs.rs:2214`) | after dirfd/base-path resolution | ext2 `write_state` (`create_dir`, `ext2.rs:2045`) |
| `sys_fchmodat` (`fs.rs:1845`) | after dirfd/base-path resolution | ext2 `read_state` (`resolve_symlinks`) then `write_state` (`chmod`, `ext2.rs:2286`) |
| `sys_unlinkat` (`fs.rs:2268`) | after dirfd/base-path resolution | ext2 `write_state` (`remove_file` `ext2.rs:2126`, `remove_dir` `ext2.rs:2162`) — the multi-second-hold case (§ archive doc §7.2) |
| `sys_renameat` (`fs.rs:2309`) | whole fn after path-arg copies | ext2 `write_state` (`rename`, `ext2.rs:2234`) |
| `sys_renameat2` (`fs.rs:2343`) | whole fn, incl. `RENAME_NOREPLACE` exists probe | ext2 `read_state` (`exists`) then `write_state` (`rename`) |
| `sys_mmap` eager fill (`mem.rs:360`) | around per-frame disk-fill loop, before install | ext2 `read_state` (`read_at`) |
| `sys_mmap` lazy inode-resolve (`mem.rs:296`) | around `resolve_inode` only | ext2 `read_state` (`lookup_path`) |
| `mmap_eager_to_lazy_fallback` (`mem.rs:175`) | around `resolve_inode` only | same |

**Not converted (still fully BKL-held):** `sys_dup`/`sys_dup3` (`fs.rs:1121`,
`1146`), `sys_close`/`sys_close_range` (`fs.rs:1429`, `1476`), `sys_fstatfs`
(`fs.rs:1081`, synthesized — no real VFS touch but no guard either),
`sys_fcntl` (`fs.rs:2107`), `sys_fchmod` (`fs.rs:1797`, calls `write_state`
BKL-held), `sys_fallocate`/`sys_ftruncate`/`sys_truncate` (`fs.rs:1863/1880/1894`,
call `write_state` BKL-held), `sys_faccessat2` (`fs.rs:2046`), `sys_getcwd`
(`fs.rs:2088`, no I/O — cached `proc.cwd`, no guard needed), `sys_symlinkat`
(`fs.rs:2361`), `sys_linkat` (`fs.rs:2380`, copy not hardlink), `sys_readlinkat`
(`fs.rs:2394`), `sys_fchdir`/`sys_chdir` (`fs.rs:2508`, `2529`).

**Footnote for whoever converts `truncate` next:** `Ext2Filesystem::truncate`
(`ext2.rs:2331-2355`) takes only `read_state()` — a shared guard — even though
it mutates the inode via `write_inode`. Works today because `write_inode`/
`write_block` only need `&Ext2State` (the block-cache `Spinlock` guards the
actual mutation, not the `RwSpinlock`), but it means truncate's mutation isn't
serialized against concurrent readers the way `write_file`/`create_dir` are.
Not currently exposed to a BKL-free window — audit this before converting
`truncate`/`ftruncate`.

### The per-syscall BKL opt-out list — Phase 7f (`src/smp_shared.rs` + `src/exceptions.rs`)

Since 2026-08-01, `rust_sync_el0_handler` acquires the BKL **unless the trapped
syscall's number is on the opt-out list** (`SYSCALL_BKL_OPTOUT`, a 512-bit atomic
bitmap seeded from the compile-time `SYSCALL_BKL_OPTOUT_SEED`) — the §7.3 "invert the
default" mechanism from the fine-grained-locking plan. A listed syscall's whole
excursion runs as ONE open dropped-BKL window: opened at entry *without* an acquire,
closed at exit *without* a re-acquire (`bkl::dropped_window_close_no_reacquire`), the
decision latched at entry. IRQ epilogues consult the ledger exactly as for guard
windows, the body's now-redundant carve-out guard nests as a depth-2 window (guards
are deliberately left in place — removing a syscall from the list at runtime via
`set_syscall_bkl_optout(nr, false)`, the per-syscall kill switch, restores the
guard-scoped behaviour without a rebuild), and the cold shared paths that genuinely
need BKL-held execution pause the window (`bkl::DroppedWindowPause`): the
`handle_syscall` interrupted-arm plain-field write, the phantom-SVC give-up path, the
STP-XZR-misroute demand-page path. `return_to_kernel` force-clears the ledger at its
top (mirroring `return_to_kernel_from_fault`) so never-return exits inside an
opted-out excursion cannot leak depth; `exit`/`exit_group`/`rt_sigreturn` are on a
structural deny list. The EL0-entry heal tripwire now feeds a counter
(`exceptions::stale_window_heal_count()`) asserted 0 by the boot suite.

Currently listed — tranche 1 (all previously whole-fn-guarded, so conversion only
deleted the entry/exit lock round-trip): `socket`, `bind`, `listen`, `accept`,
`accept4`, `connect`, `getsockname`, `getpeername`, `setsockopt`, `getsockopt`,
`resolve_host`, `getrandom`. Tranche 2: `rt_sigprocmask` (per-thread mask, no
process-table touch) and `nanosleep` (blocking, cleared by the blocking-window
analysis below). Tranche 3: `futex` — the first entry with **no** pre-existing guard,
so conversion moved its whole body BKL-free; cleared once gate 2 below was closed for
`FUTEX_WAITERS`. **`sendto`/`recvfrom`/`sendmsg`/`recvmsg` must not be listed
wholesale** — their unix-socket routing arm runs before their guard and stays
BKL-held (the AB-BA note below). **`getcwd` must not be listed** either, despite the
VFS map calling it guardless: it *reads* `proc.cwd`, a plain `String` `Process`
field that `chdir` writes BKL-held (see the load-bearing table). `KernelLock`/
`reconcile_for_spsr`/the ledger/the guards must survive until the un-converted set
is empty. See
[`BKL_PHASE7F_OPTOUT_LIST.md`](../../archive/BKL_PHASE7F_OPTOUT_LIST.md).

**Two gates every further conversion must clear** (Phase 7f tranche 2, §4 of the
archive doc):

1. **No `Process`-derived reference may live across `schedule_blocking()`.** Bounded
   lookup-then-use is fine — that is what 7e's RETIRED + 10 ms cooldown covers. A
   reference held across an indefinite park is not. `sys_read`/`sys_write` violated
   this (bound `proc` before the arm match, deref'd it after the park) — a live
   pre-existing bug, since the BKL never covered those references either; fixed in
   tranche 3 by **scoping** the binding to the fd-table clone and re-resolving at the
   post-park derefs. Prefer scoping to a comment: it stops the next arm someone adds
   from reintroducing it.
2. **Every inner lock the excursion takes must mask local IRQs**, or a nested IRQ's
   unconditional `enter_kernel()` deadlocks AB-BA against a peer holding the BKL and
   waiting on that lock. `PIPES`, `FUTEX_WAITERS` (`syscall/sync.rs`) and
   `EPOLL_TABLE` (`syscall/poll.rs`) all comply as of tranche 3. **Still
   non-compliant: `Process::terminal_state` and its nested `input_waker`** — those
   sites use `disable_preemption()`, which stops the scheduler but does *not* mask
   IRQs, and `write_to_process_stdin`/`close_process_stdin` take the same lock
   BKL-held. That is what blocks converting `read`, whose Stdin arm holds it across a
   park. Surface: `syscall/term.rs` (~12 sites), `syscall/fs.rs`, `process/mod.rs`.

   **Before masking a lock, check whether it can stop being a lock.** `UTC_OFFSET_US`
   (`src/timer.rs`) was a bare `Spinlock<Option<u64>>` reachable BKL-free through
   `futex(FUTEX_WAIT_BITSET|CLOCK_REALTIME)`; because it guarded one scalar with no
   companion state it became an `AtomicU64` instead of a masked hold, which takes it
   off this inventory rather than adding another hold that must keep its discipline.
   That generalizes into phase **7g** — see
   [`BKL_FINE_GRAINED_LOCKING_PLAN.md`](../../archive/BKL_FINE_GRAINED_LOCKING_PLAN.md)
   §7.3a for the classification table. It does **not** apply to `FUTEX_WAITERS` or
   `EPOLL_TABLE`: they guard `BTreeMap`s, where an atomic trades contention for torn
   reads.

   Masking is also not sufficient on its own — the hold must contain nothing that can
   block. `sys_epoll_ctl` held `EPOLL_TABLE` across `validate_user_ptr`, which
   demand-pages a file-backed lazy page through the VFS; the user copy had to be
   **hoisted out** of the hold (with a membership probe kept ahead of it so `EBADF`
   still precedes `EFAULT`), not merely masked. Console writes are the same class: the
   epoll-ctl `tprint!`s moved out too.

**The dropped-window ledger is thread-id-indexed, so a recycled slot must be
cleared.** A thread killed while parked inside a converted syscall never reaches its
window close; without a clear, the next occupant of that tid inherits the depth and
runs BKL-free until the EL0-entry tripwire heals it.
`threading::reclaim_terminated_slots` now calls
`bkl::clear_dropped_windows_for_dead_thread(tid)` before the slot goes `FREE`
(ledger-only — a TERMINATED thread has no invariant to restore). Listing `nanosleep`
reproduced this deterministically (3 heals/boot); it is a prerequisite for every
further blocking conversion.

### `no-bkl-network` — `src/syscall/net.rs`

`NetBklGuard` is purely compile-time gated (no runtime toggle, no latching
needed — unlike `VfsBklGuard`).

| syscall | guard scope | inner lock(s) held while BKL dropped |
|---|---|---|
| `sys_socket` (`net.rs:123`) | whole fn | fd table; `SOCKET_TABLE` `Spinlock` (`akuma-net/src/socket.rs:256`) |
| `sys_bind`/`sys_listen` (`net.rs:239`, `269`) | whole fn | `SOCKET_TABLE`; `NETWORK` `Spinlock` under `PreemptGuard` (`smoltcp_net.rs:106`) |
| `sys_accept`/`sys_accept4` (`net.rs:286`, `332`) | whole fn, incl. blocking wait (no lock held across it) | `SOCKET_TABLE`; `NETWORK` |
| `sys_connect` (`net.rs:388`) | whole fn | same |
| `sys_getsockname`/`sys_getpeername` (`net.rs:426`, `455`) | whole fn | `SOCKET_TABLE`; `NETWORK` |
| `sys_sendto`/`sys_recvfrom` (`net.rs:508`, `594`) | after the unix-socket pipe-routing check (deliberately BKL-held to avoid an AB-BA with a nested IRQ) | `SOCKET_TABLE`; `NETWORK` |
| `sys_setsockopt`/`sys_getsockopt` (`net.rs:723`, `807`) | whole fn | `SOCKET_TABLE`(; `NETWORK` for getsockopt) |
| `sys_sendmsg`/`sys_recvmsg` (smoltcp build, `net.rs:901`, `1051`) | after unix-socket iovec check | `SOCKET_TABLE`; `NETWORK` |
| `sys_resolve_host` (DNS, `net.rs:1317`) | whole fn | `NETWORK` |
| `sys_read`/`sys_write` on `Socket` fd (`fs.rs:417`, `913`) | inside `Socket` match arm | `SOCKET_TABLE`; `NETWORK` |

Not converted / correctly guardless: `sys_socketpair` (AF_UNIX, pipe-only,
never touches `SOCKET_TABLE`), `sys_shutdown` (hardcoded no-op), `sendmsg`/
`recvmsg` on the non-smoltcp rump-sysproxy build (pipe-only path).

### Adjacent BKL-drop sites outside the named carve-outs

These call `akuma_exec::bkl::dropped_window_open()/close()` directly (same
ledger primitive, no `VfsBklGuard`/`NetBklGuard` struct):

| site | toggle | scope | inner lock |
|---|---|---|---|
| `sys_execve` ELF read (`proc.rs:649`, inside `sys_execve` `proc.rs:544`) | `exec_bkl_drop_enabled()` (`smp_shared.rs:87`) | whole-file `read_file` — **only** in the single-image smp-shared build (`not(kernel_profile_extreme)`); the size profile reads just a 256-byte shebang header, which never takes this window | ext2 `read_state` |
| Data-Abort demand-page fill (`exceptions.rs:2964`/`3017`) | `fault_bkl_drop_enabled()` (`smp_shared.rs:68`) | per-page fill loop only; page-table install is separate, BKL-held | ext2 `read_state` |
| Instruction-Abort demand-page fill (`exceptions.rs:3487`/`3533`) | same | mirror of Data-Abort path | same |
| `netpoll_drain`'s `smoltcp_net::poll()` loop (`main.rs`, `BKL_VFS_CARVE_OUT.md` §19–20) | none — unconditional under the cfg gate | the whole burst-drain `while poll() {}` loop | `NETWORK`/`SOCKET_TABLE` |
| `sys_epoll_pwait`/`sys_pselect6`/`sys_ppoll`'s per-iteration `smoltcp_net::poll()` call (`poll.rs:605`/`819`/`925`, Phase 7b piece 1, `BKL_PHASE7B_PPOLL_CARVE_OUT.md`) | none — unconditional under the cfg gate, same as `netpoll_drain` | one `poll()` call per readiness-loop iteration | `NETWORK`/`SOCKET_TABLE` |

### `no-bkl-process` — `crates/akuma-exec/src/process/mod.rs`

Phase 3, landed 2026-07-31 and **on by default in `smp-shared`** since the same
day, like the other two.

Contention-confirmed by a same-source `bkl-profile` A/B at SMP=4 on the standing
regimen: **`clone` 19.5% → 2.5%** of workload-window contended time (23.9M → 2.8M
spins, 8.6×), dropping it from the #2 holder to a minor one, with total workload
spins down 9% and 6/6 digests exact on both sides.
`ProcessBklGuard` (`process/bkl_guard.rs`) mirrors `VfsBklGuard`: runtime toggle
`process_bkl_drop_enabled()` (default on, latched at construction), same
dropped-window ledger.

| site | guard scope | inner lock(s) held while BKL dropped |
|---|---|---|
| `fork_process` CoW share/demote pass | the copy loop only — after the `for_each_process` sibling scan, the `LAZY_REGION_TABLE` snapshot, and `propagate_lazy_regions_to_child` (all hoisted BKL-held ahead of it); before steps 5–8 | thread-group **leader's** `Process::as_lock` in 64-page chunks (`FORK_AS_CHUNK_PAGES`, IRQ-masked via `AsLockHold`), plus `COW_REFCOUNTS` `Spinlock` and the PMM/allocator locks |

Two things about this one differ from the fs/net pattern and are easy to get
wrong:

- **It's the thread-group leader's `as_lock`, not the forking thread's.**
  `CLONE_THREAD` siblings each get a fresh `Spinlock` in their own `Process`
  while sharing one address space; the fault handler picks its owner with
  `address_space_owner_pid_for_fault()` (TTBR0 → the non-shared process owning
  that L0), so fork resolves the same way. Taking `parent.as_lock` from a worker
  thread would hold a lock nothing else waits on.
- **The hold is chunked, and the PTE read + `cow_ref_inc` + demote + range-flush
  are one hold.** `AsLockHold` masks IRQs, so it cannot span the copy (timer
  starvation, and the AB-BA against a BKL-holding `munmap` that wants
  `as_lock`); but the four steps above must be atomic against a peer's CoW fault
  or fork can `cow_ref_inc` a frame that fault just freed. That constraint is
  what merged the demote into the share pass — it used to be a separate second
  walk.

**Still fully BKL-held in `fork_process`:** steps 1–3 (allocation only, nothing
to relieve) and steps 5–8 — `ProcessInfo` write, `get_saved_user_context` /
`update_thread_context`, `spawn_user_thread_initializing`, `register_process` +
`mark_thread_ready` (the publication point). As of Phase 7d, `THREAD_CONTEXTS`
itself is no longer why this window needs the BKL — it has a real proof
independent of it (§"`no-bkl-process`'s `THREAD_CONTEXTS`" below). `ProcessInfo`
and `register_process` still are: they touch the process table, whose
*registration* path is still BKL-serialized (the Access half, landed
2026-08-01, retired the `&'static mut` accessors; it did not add a
registration lock) — the original audit's finding there stands for writers. The process table's *reaping*
path (`unregister_process`) got its own fix in Phase 7e's "Free" half (landed
2026-08-01, see below) — a different hazard (peer-core free racing an in-flight
BKL-dropped reader), not this one. Also still BKL-held: the
eager-copy (non-CoW) fork branch (unreachable, `COW_FORK_ENABLED = true`) and
`sys_clone`'s routing layer (`VFORK_WAITERS`, nanosecond-scale). `execve`'s
`replace_image` destructive window remains the single most dangerous carve-out
target in the space and is untouched.

Background: [`BKL_PROCESS_CARVE_OUT.md`](../../archive/BKL_PROCESS_CARVE_OUT.md)
— §§1–8 are the original audit (which concluded no carve-out was possible), §9
is what actually landed and why §2.4a's "the parent's page tables have no lock"
was wrong: the CoW fault handler was already editing those same PTEs BKL-free
under `as_lock`, so the inner lock existed and fork just wasn't taking it.

#### `no-bkl-process`'s `THREAD_CONTEXTS` (Phase 7d, landed 2026-08-01)

Not a carve-out (no new guard, no runtime toggle) — a correctness proof for a structure
`fork_process` steps 5–8 write to. `THREAD_CONTEXTS: [SyncContext; MAX_THREADS]`
(`threading/mod.rs`) used to justify its `unsafe impl Sync` with "we're single-CPU,"
false under `smp-shared`. Phase 7d classified every live access site into one of three
cases that hold without the BKL: the scheduler switch (real cross-core `POOL` Spinlock
held across the whole switch), spawn/reclaim setup (the `THREAD_STATES[idx]`
`FREE`/`TERMINATED -> INITIALIZING` `compare_exchange` gives exclusive ownership, and
`mark_thread_ready`'s plain `Ordering::SeqCst` store is — provably, not just
plausibly — sufficient to publish the write to a peer without any lock), and self-read
of the live thread's own slot (fork/clone capturing `current_thread_id()`'s context,
which can't race itself). The audit also found four `ThreadPool` methods
(`spawn`/`spawn_with_stack_size`/`spawn_system_closure`/`spawn_user_closure`) and two
more (`reclaim`/`cleanup_terminated`) that used a plain load instead of a CAS and would
have violated case 2 under real contention — dead code, zero callers anywhere in the
workspace, deleted rather than fixed. New host tests
(`threading::thread_contexts_invariant_tests`) hammer the CAS and the publish-without-a-lock
claim with real `std::thread`s. See
[`BKL_PHASE7D_THREAD_CONTEXTS.md`](../../archive/BKL_PHASE7D_THREAD_CONTEXTS.md).

This does not touch `ProcessInfo`/`register_process` — registering a *new* pid
remains fully BKL-held (see the Access-half section below).

#### `no-bkl-process`'s process-table `Free` half (Phase 7e, landed 2026-08-01)

Not a carve-out either (no new guard, no runtime toggle, no `smp-shared`-only
gate — it changes behavior on every profile). Closes a real use-after-free: the
five carve-outs already listed above each drop the BKL for a bounded window,
during which the executing core can be holding a raw `*mut Process`/
`&'static Process` obtained from `lookup_process`/`get_process_ptr`/
`lookup_process_shared` *before* the window opened. `unregister_process` used to
free that memory synchronously the instant a peer core reaped the same pid
(`wait4`, sibling teardown) — a use-after-free with nothing but
`with_irqs_disabled` (single-core-only exclusion) in the way, live on every
`smp-shared` boot once those five carve-outs landed, not merely hypothetical.

Fix: `unregister_process` now transitions the slot `ACTIVE -> RETIRED` (a new
third `slot_state`, alongside `FREE`/`ACTIVE`) via `compare_exchange` instead of
freeing — RETIRED is invisible to every lookup path and to `register_process`'s
free-slot scan, exactly like FREE is invisible to lookup and ACTIVE is invisible
to allocation, but the `Process` stays live in memory. `reclaim_retired_processes`
is the deferred collector: it frees a RETIRED slot only after
`process_reclaim_cooldown_us` (10ms) has elapsed since retirement — a cooldown
generous relative to the bounded I/O-or-PTE-chunk windows it needs to outlast —
called periodically from `netpoll_maint` (100ms cadence, mirroring
`threading::reclaim_terminated_slots`). **Deliberately not** called on-demand
from `register_process` on a full-table miss, unlike the spawn paths'
`reclaim_terminated_slots` retry-once pattern this was originally modeled on
(`spawn_user_thread_fn_internal`, `spawn_system_thread_fn`, and since
2026-08-03 `spawn_user_thread_initializing` too — see
[`thread-lifecycle.md`](thread-lifecycle.md) §2.1): `register_process` is reachable from
deep inside fork/clone/spawn, whose lock context it doesn't control, and
`Process` teardown (`Box::drop` → `UserAddressSpace::drop`) needs its own
locks (page-table frame free, ASID release) — unlike a thread slot's plain
atomics. The first version of this phase *did* add that retry and it
self-deadlocked the moment a caller already held one of those locks, found via
a real boot hang (`test_sigpipe_terminate_no_deadlock`, real fork/exec/pipe/
SIGPIPE traffic) — see the "Correctness rules learned the hard way" entry
above this table and §"the on-demand reclaim that wasn't" in the archive doc.
One consequence: deferred reclamation means cooled-but-uncollected zombies
*can* transiently fill the table under a fast enough exit burst, the same
shape `THREAD_STATES` starved under before `reclaim_terminated_slots` dropped
its caller-identity gate (`BKL_VFS_CARVE_OUT.md` §11.4) — accepted for now;
if it bites in practice the fix is a different collector call site, not
reclaiming inside `register_process`. The `ACTIVE -> RETIRED` CAS
(not a plain store) also fixes a smaller pre-existing race: two callers
unregistering the same pid concurrently used to both find it and both attempt
to free it; now exactly one wins the transition and the other's call is a no-op.
Two new boot self-tests (`test_process_reclaim_respects_cooldown`,
`test_unregister_process_second_call_loses_cas`, `src/process_tests.rs`) pin
both properties, mirroring `test_thread_slot_reclaim_on_spawn`. See
[`BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md`](../../archive/BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md).

This does **not** touch same-core field races on a live ACTIVE process, or
`ProcessInfo`/`register_process`'s BKL dependency — see the Access half, below.

#### `no-bkl-process`'s process-table `Access` half (Phase 7e, landed 2026-08-01)

The `&'static mut Process` accessors (`lookup_process`, `current_process`) are
**deleted**. Every former call site (~250 in production code) now uses
`lookup_process_shared`/`current_process_shared` for reads and `&self` methods,
`table::with_process`/`with_current_process` (IRQ-masked closure, no allocation)
for short plain-field writes, `Process::with_address_space` (an
`as_lock`-holding closure handing out `&mut UserAddressSpace`, replacing the
open-coded `AsLockHold` + disjoint-field-borrow pattern) for page-table
mutators, or — at exactly two enumerated lifecycle sites, `sys_execve`'s
`replace_image*` tail and `spawn::run_registered_process` — the `unsafe`
`table::with_process_exclusive`, whose exclusivity is structural (own process,
BKL-held path) and whose call-site list is Phase 7f's `execve`/`clone`
worklist. Three signal-path PTE edits that ran with no `as_lock` at all
(`ensure_cow_page_writable`, `try_resolve_el1_cow_fault`,
`try_deliver_signal`'s RX fix-up) were folded under `with_address_space` in
the process. This is behaviour-preserving per site and proceeds with the BKL
in place: cross-core exclusion for plain `Process` fields is **still the BKL**
(the row in the load-bearing table below stands for same-core-races/writers);
what's gone is the aliasing UB of two cores materializing `&mut` to one
`Process`, and the `'static` lifetime that outlived RETIRED→FREE reclamation.
See [`BKL_PHASE7E_ACCESS_HALF.md`](../../archive/BKL_PHASE7E_ACCESS_HALF.md),
including the `cleanup_process_fds` fd-liveness fix it forced (the
"correctness rules" entry below).

### `no-bkl-mm` — `src/syscall/mem.rs`

Phase 5, landed 2026-08-01, promoted to `smp-shared` default-on 2026-08-01.
Picked by the plan, not by attribution (no mm syscall has ever measured as a
significant BKL holder).

| syscall | guard scope | inner lock(s) held while BKL dropped |
|---|---|---|
| `sys_mprotect` (`mem.rs`) | after `proc` resolves | `Process::as_lock` (PTE flag edits); `LAZY_REGION_TABLE` |
| `sys_madvise` (`mem.rs`) | after `proc` resolves, per match arm | `Process::as_lock` (prefault install / zero-mapped); `LAZY_REGION_TABLE`; PMM (`MADV_WILLNEED`'s batch alloc) |
| `sys_munmap` (`mem.rs`) | after `proc` resolves | `Process::vm_lock` (region lookup/removal + free-list); `Process::as_lock` (PTE unmap); `SHARED_FILE_MAPPINGS`; `LAZY_REGION_TABLE`; PMM (frame free) |
| `sys_mremap` (`mem.rs`) | after `proc` resolves once (not re-looked-up inside the window) | `Process::vm_lock`; `Process::as_lock`; `LAZY_REGION_TABLE`; PMM |
| `sys_mmap` (`mem.rs`) | after `proc` resolves | `Process::vm_lock` (free-list alloc via `vm_alloc_mmap`); `Process::as_lock` (PTE install); `LAZY_REGION_TABLE`; PMM; `SHARED_FILE_MAPPINGS`; nested `VfsBklGuard` windows for on-disk fill (ledger depth-counted, safe to nest) |

Two gaps closed as a prerequisite (not present before this phase, not a "nothing to
build" finding like net/vfs/process):

- **`ProcessMemory::free_regions`/`alloc_mmap()`** was a plain unguarded `Vec` —
  every mm syscall now goes through `Process::vm_alloc_mmap`/`vm_free_mmap`, which
  fold it under the existing `vm_lock` (same IRQ-disabled, pure-bookkeeping
  discipline `vm_lock` already enforces for `mmap_regions`).
- **`sys_mmap`'s OOM/reclaim fallback** (`reclaim_clean_file_pages` →
  `try_evict_ro_page`) mutated page tables with no `as_lock` hold at all — fixed
  with a per-page (not per-sweep) `as_lock_hold`.

Background: [`BKL_MM_CARVE_OUT.md`](../../archive/BKL_MM_CARVE_OUT.md) — §1 is the
audit (the two gaps above), §3 is verification (boot self-test at SMP=2/4, the
`mmap_stress`/`mmap_file`/`mmapsum`/`fpfault`/`neonfault` + `llama-bench` suite,
the contention regimen), §4 is a same-binary Akuma-vs-native-Linux tok/s comparison
(bonus, not part of the carve-out itself), §5 explains why this phase has no
before/after contention number the way `unlinkat`/`netpoll_drain` do.

### `no-bkl-drivers` — `src/syscall/{fs,proc,fb}.rs`

Phase 6, landed 2026-08-01, promoted to `smp-shared` default-on 2026-08-01.
Plan-driven (Phase 6 of the locking plan), like `no-bkl-mm` — no device-driver
syscall has ever measured as a significant BKL holder. The audit found that
virtio-blk and virtio-net were already BKL-free via `no-bkl-vfs` and
`no-bkl-network` (their `BLOCK_DEVICE`/`NETWORK` Spinlocks are the inner locks
credited in those phases' syscall→lock maps); this phase covers the remaining
drivers. All virtio devices are polling-based (no IRQ handlers registered
except the timer IRQ 27, which is scheduler-coupled and belongs to Phase 7).
`DriverBklGuard` (`src/syscall/fs.rs`, `pub(super)`) mirrors `MmBklGuard`:
runtime toggle `drivers_bkl_drop_enabled()` (default on, latched at
construction), same dropped-window ledger.

| site | guard scope | inner lock(s) held while BKL dropped |
|---|---|---|
| `sys_getrandom` (`proc.rs`, after `validate_user_ptr`) | whole chunked-read loop | `RNG_DEVICE` `Spinlock` (`rng.rs:498`) |
| `sys_read` → `DevUrandom` (`fs.rs`) | `fill_bytes` + `copy_to_user_safe` | `RNG_DEVICE` |
| `sys_pread64` → `DevUrandom` (`fs.rs`, same) | same | `RNG_DEVICE` |
| `sys_write` → `DevDsp` (`fs.rs`) | `audio::play` call | `SOUND_DEVICE` `Spinlock` (`audio.rs:205`) |

The `sys_fb_*` framebuffer syscalls were three more rows here, guarded by
ramfb's `FB_STATE` `Spinlock`. They were removed 2026-08-31
([`FRAMEBUFFER_REMOVED.md`](../../archive/FRAMEBUFFER_REMOVED.md)), which briefly
cost this carve-out its only *real* guarded path in `test_drivers_bkl_drop`:
`sys_fb_init` took dimensions rather than pointers, so it was the one driver
syscall the boot test could call without a mapped user page.

Recovered the same day. `sys_getrandom` now serves as that leg, using
`BYPASS_VALIDATION` to get a kernel-stack buffer past `validate_user_range` —
which means the guard is constructed and the whole chunked `fill_bytes` +
`copy_to_user` body runs inside the dropped window. Read the branch tag in the
test's PASSED line: `[filled]` is full coverage, `[EIO]` (no virtio-rng) means
the window still opened and closed but the fill assertion did not run.

**This kernel has no graphics output at all.** virtio-gpu never existed here
(zero matches for `gpu`/`GPU`), and ramfb is gone.

Background: [`BKL_DRIVERS_CARVE_OUT.md`](../../archive/BKL_DRIVERS_CARVE_OUT.md)
— §1 is the full driver audit (which found most work already done by preceding
phases), §2 explains why the plan's IRQ-handler goal belongs to Phase 7
(scheduler), §3 confirms virtio-gpu's absence.

## What the BKL is still the only lock for

Audited 2026-08-01 with all five carve-outs on
([`BKL_PHASE7_AUDIT.md`](../../archive/BKL_PHASE7_AUDIT.md) §2). These are the structures
where the BKL is not redundant — it *is* the cross-core lock, because the only other
guard is `with_irqs_disabled` (mutual exclusion on one core). **Do not remove the BKL
from syscall entry while any of these stands.** (The audit's §2.3 entry, `ALARM_QUEUE` +
`critical_section`'s process-global nesting counter, is no longer on this list: Phase 7a
gave the queue a real `Spinlock` and removed the `critical_section` dependency — see
[`BKL_PHASE7A_TIMER_IRQ_CARVE_OUT.md`](../../archive/BKL_PHASE7A_TIMER_IRQ_CARVE_OUT.md).)

| structure | where | current guard | measured BKL share |
|---|---|---|---|
| process table — plain `Process` fields (writers via `with_process`/`with_current_process`, IRQ-mask only) | `process/table.rs` (`with_process`, `with_process_exclusive`, `get_process_ptr`, `for_each_process`, …), `process/children.rs` (`lookup_process_shared`, `current_process_shared`) | `with_irqs_disabled` only, for same-core field races on a live ACTIVE process — the Access half (landed 2026-08-01) deleted the `&'static mut` accessors (`lookup_process`/`current_process`) and routed page-table mutation under `as_lock` (`with_address_space`), but plain-field writes still have no cross-core lock beyond the BKL. The cross-core **free** path is no longer bare `with_irqs_disabled`: `unregister_process` retires (RETIRED slot state) instead of freeing, and `reclaim_retired_processes` defers the actual free past a cooldown (Phase 7e "Free" half, landed 2026-08-01 — closes the peer-free UAF for the five carve-outs' *bounded* windows specifically; does **not** cover a `schedule_blocking()`-spanning window, see the Phase 7b note just below, which is a different hazard shape). Known since Phase 3 (`BKL_PROCESS_CARVE_OUT.md` §7 "(b)"); peer cores free *other* PIDs at `process/mod.rs:1116`/`:1209` | underlies `execve`/`clone`/`wait4`/`netpoll_maint` |
| `replace_image` destructive window | `process/image.rs:29`, `:121` | `LifecycleGuard` = per-thread `disable_preemption()`, **not a lock** | `execve` ~22% |
| `fork_process` steps 5–8 | `process/mod.rs` | none for `ProcessInfo`/`register_process` (process-table row above). `THREAD_CONTEXTS` moved off this list in Phase 7d — see "`no-bkl-process`'s `THREAD_CONTEXTS`" above | `clone` ~10–13% |
| console UART | `src/console.rs:60` | bare `static UART` | `netpoll_herd` ~1% |

**The process-table row above is not hypothetical — a Phase 7b A/B hit it.** A
whole-syscall `ppoll`/`epoll_pwait` BKL-free window (spanning `schedule_blocking()`, i.e.
real scheduler activity, unlike every other carve-out's single bounded I/O op) produced
one intermittent data-corruption run out of two in the standing regimen, alongside a
`[BKL] stale dropped-window depth 1 healed at EL0 entry` line and a thread crash whose
signature matches a known physical-page-reuse-race pattern. That guard was reverted; see
[`BKL_PHASE7B_PPOLL_CARVE_OUT.md`](../../archive/BKL_PHASE7B_PPOLL_CARVE_OUT.md) §3–4.
**Any future BKL-free window that can span a `schedule_blocking()`/context-switch point
(not just a single bounded I/O op) should be treated as touching this row, whether or not
it looks like it does.**

**Refinement (Phase 7f tranche 2, archive doc §4.1) — what the BKL actually covers
here.** `KERNEL_LOCK` is a **per-core** lock, and `bkl::reconcile_for_spsr` reconciles
it for *the thread the core is about to run*. So when a thread blocks and the core
switches away, that thread's hold simply evaporates; nothing restores it until the
thread is resumed and its own eret epilogue re-acquires. `schedule_blocking` never
calls `leave_kernel` because it does not need to. Three consequences worth having
written down:

- A "BKL-held" blocking syscall is BKL-held during each *runnable stretch*, never
  across the wait. This is true of every syscall in the kernel, converted or not.
- Converting a blocking syscall therefore does not newly expose the wait — it changes
  the serialization of the runnable stretches, exactly as converting a non-blocking
  syscall does.
- Any reference carried *across* a wait is already unprotected today. The row above
  still stands, but the hazard it names is "reference outlives the 10 ms reclaim
  cooldown", not "window spans a scheduler".

Corroboration: `schedule_blocking`'s own wait loop calls
`process::is_current_interrupted()` → `current_process_shared()` on every `wfi`
iteration, and `accept`/`connect`/`resolve_host` have run blocking BKL-free windows
since Phase 2's whole-fn `NetBklGuard`. A bounded process-table lookup inside a
blocking BKL-free window is production-proven, not novel.

By contrast these hold the BKL but already have a real inner lock, so they are ordinary
carve-out candidates: `ppoll`/`epoll_*`'s `EPOLL_TABLE` and per-fd-type locks (only the
`smoltcp_net::poll()` call itself is carved so far — see the "Adjacent BKL-drop sites"
table above; the whole-syscall carve is blocked on the process-table row above per the
finding just noted), pipes (`PIPES`), futex (`FUTEX_WAITERS`), eventfd/timerfd, and
`idle_halt`'s post-WFI bookkeeping (`POOL`).

**The process-table migration is done on both axes** (see the two Phase 7e
sections above): the *free* path defers reclamation (RETIRED + cooldown
collector), and the *Access* half extended `lookup_process_shared`'s
`&self` + `as_lock`/`vm_lock` pattern to every call site and deleted the
`&'static mut` API — with **no** `PROCESS_TABLE_LOCK`, which
`BKL_PROCESS_CARVE_OUT.md` §9.2 rejected as exactly the coarse lock the
playbook warns against. What remains for 7f: plain-field writers still lean on
the BKL (the table row above), and the two `with_process_exclusive` lifecycle
windows (`replace_image`, first-run) are the `execve`/`clone` conversions.

## Attribution tooling

`bkl-profile` (cargo feature → `cfg(kernel_bkl_profile)`, `src/bkl_profile.rs`)
turns on a per-tag BKL-hold profiler for the whole boot and prints a **delta**
histogram every 10s from the async-main loop, crediting spins to whatever the
*owning* core was doing when a waiter first observed contention. It's
measurement-only and perturbs timing, which is why it's a separate feature
rather than part of `smp-shared` — never compare absolute spin counts across
sessions, only shares/ranks within one same-binary run, and prefer the
`[BKL] stuck` threshold counter (present in every build) as the
profiler-independent cross-check.

**Attribution is thread-scoped** (since 2026-07-31, `BKL_VFS_CARVE_OUT.md` §18).
A kernel excursion belongs to a thread — it survives preemption and can resume
on another core — so the authoritative tag lives in a per-thread table
(`sync::ThreadTagTable`, indexed by `current_thread_id()`), and the per-core
`HOLDER_TAG` the waiter samples is a *cache* of it. Three operations maintain
the invariant `HOLDER_TAG[c] == THREAD_TAG[thread running on c]`:
`set_holder_tag` (kernel entry) writes both; `set_core_tag_transient` (the IRQ
dispatch stamp) writes only the core cache, so the interrupted thread's tag is
never clobbered; `load_thread_tag_to_core` reloads the cache wherever the
current thread changes (`set_current_thread_register`, which every switch path
funnels through) and at the IRQ epilogue.

This replaced a per-core-only tag that was **not** trustworthy: a tick that
context-switched handed the incoming thread the "irq/sched" label, and a thread
preempted inside a long BKL-held syscall never re-entered the kernel to correct
itself, so it ran that syscall's whole remainder labelled "irq/sched". Because
the long excursions are the ones that get preempted, the artifact pooled in one
bucket — on the campaign's SMP=4 regimen, `irq/sched` measured **88.8%** before
the fix and **23.0%** after, on a matched same-disk A/B. Do not compare a
pre-2026-07-31 `irq/sched` share with a later one.

Buckets: `0..=499` syscall numbers, `500` fault, `501` irq/sched, `502` idle,
`503` netpoll, `511` unknown. `idle`/`netpoll` name the two long-lived kernel
threads that hold the BKL with no syscall or fault entry point (the per-core
idle loops; the async-main smoltcp drain) — before they were named, their holds
inherited whatever their core last did, which is how they fed `irq/sched`.
`unknown` now means "held by a thread that never passed a tagging site", a real
answer — except in a build without `bkl-profile`, where `tag=511` everywhere
just means the profiler isn't compiled in.

Read attribution over the **workload windows**, not whole-boot
(`BKL_VFS_CARVE_OUT.md` §17.2): `scripts/bkl_smp_regimen/analyze_workload.py
--auto <serial.log>` derives the interval from the regimen's own `execve`
footprint and sums per-tag spins over just those windows. `analyze.py`'s default
whole-boot view is diluted by bringup and idle.

```
cargo build --profile release-smp-shared --features devbox-smoltcp,no-tests,bkl-profile
```

Driving workload: `net4` (concurrent downloads → net + ext2 write), `read4`
(concurrent reads with a working set bigger than the block cache), `cp2`
(read+write in one process), `rm` (mutating syscalls). See
`BKL_VFS_CARVE_OUT.md` §11.1 for the exact harness.

## Background

- [`BKL_VFS_CARVE_OUT.md`](../../archive/BKL_VFS_CARVE_OUT.md) — the VFS
  carve-out, worked in full: guard latching, IRQ-mask-per-attempt, the
  dropped-window ledger root-cause/fix, and the `unlinkat`/`openat`
  attribution + conversion sessions.
- [`BKL_PROCESS_CARVE_OUT.md`](../../archive/BKL_PROCESS_CARVE_OUT.md) — the
  Phase 3 audit of `clone`/`fork_process`/`execve`: why no carve-out was
  implemented (no inner lock on process/CoW state; the BKL is the lock), and
  what prerequisites would unblock one.
- [`BKL_MM_CARVE_OUT.md`](../../archive/BKL_MM_CARVE_OUT.md) — the Phase 5
  `mprotect`/`madvise`/`munmap`/`mremap`/`mmap` carve-out: the two real locking
  gaps it found and closed, and why (unlike the other three) it has no
  attribution-backed contention number.
- [`BKL_DRIVERS_CARVE_OUT.md`](../../archive/BKL_DRIVERS_CARVE_OUT.md) — the
  Phase 6 device-driver carve-out: the full driver audit (which found most work
  already done by `no-bkl-vfs`/`no-bkl-network`), why the plan's IRQ-handler
  goal belongs to Phase 7, and why virtio-gpu is a no-op (it does not exist).
- [`BKL_PHASE7_AUDIT.md`](../../archive/BKL_PHASE7_AUDIT.md) — the Phase 7
  audit: why BKL removal is **not** executable yet (§1 corrects the `irq/sched`
  share, §2 is the load-bearing inventory above, §4 the ticket-accounting bug it
  found and fixed, §5 the 7a–7f decomposition).
- [`BKL_PHASE7A_TIMER_IRQ_CARVE_OUT.md`](../../archive/BKL_PHASE7A_TIMER_IRQ_CARVE_OUT.md)
  — Phase 7a: `ALARM_QUEUE`'s real `Spinlock`, the `critical_section` removal,
  and the BKL-free timer-IRQ dispatch (`no-bkl-irq`), with the same-binary A/B
  (`irq/sched` 24.7%→10.2%).
- [`BKL_PHASE7D_THREAD_CONTEXTS.md`](../../archive/BKL_PHASE7D_THREAD_CONTEXTS.md)
  — Phase 7d: the `THREAD_CONTEXTS` ownership proof (three cases covering every
  live access site, none needing the BKL), the dead plain-load spawn/reclaim
  methods it found and deleted, and the real-concurrency host tests that check
  the CAS exclusivity and publish-without-a-lock claims on actual hardware.
- [`BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md`](../../archive/BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md)
  — Phase 7e "Free" half: the peer-free use-after-free the five default-on
  carve-outs already created, and the RETIRED-slot-state + cooldown-collector
  fix (mirroring `reclaim_terminated_slots`).
- [`BKL_PHASE7E_ACCESS_HALF.md`](../../archive/BKL_PHASE7E_ACCESS_HALF.md)
  — Phase 7e "Access" half: the `lookup_process`/`current_process`
  (`&'static mut`) deletion, the replacement accessor surface
  (`*_shared`/`with_process`/`with_address_space`/`with_process_exclusive`),
  the unlocked signal-path PTE edits it folded under `as_lock`, and the
  fd-liveness fix + boot self-test.
- [`BKL_FINE_GRAINED_LOCKING_PLAN.md`](../../archive/BKL_FINE_GRAINED_LOCKING_PLAN.md)
  — the overall phased plan. §"Phase 2" has the network carve-out's design
  notes (why a coarse `NETWORK_LOCK` doesn't work, the SIGPIPE self-deadlock,
  the AB-BA nested-IRQ fix). **§7 is the replanned Phase 7**, and §7.3 is its
  canonical approach: *don't remove the BKL, invert its default* via a
  per-syscall opt-in list seeded empty, so it withers into provably-dead code
  instead of being deleted in one step — note the constraint that
  `reconcile_for_spsr` and the dropped-window ledger must survive the whole
  traversal.
- [`SMP_SHARED.md`](../../archive/SMP_SHARED.md) — full shared-kernel SMP
  progress log (M0–M5).
- [`smp-shared.md`](smp-shared.md) — current-state milestone status for
  shared-kernel SMP.
- [`../../runbooks/debug-smp.md`](../../runbooks/debug-smp.md) and
  [`../../archive/SMP_FORK_EXEC_CORRUPTION_FIX.md`](../../archive/SMP_FORK_EXEC_CORRUPTION_FIX.md)
  — action-first procedures for BKL wedges and fork/CoW corruption.
