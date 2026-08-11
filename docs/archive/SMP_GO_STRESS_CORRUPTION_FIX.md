# SMP Go combined_stress: phantom-SVC corruption + BKL teardown wedge — Investigation Prompt

> **STATUS 2026-07-22 (same day, follow-up session): bugs 1 and 2 FIXED — see
> "Resolution" at the bottom. Bug 3 (ticket leak) still open; two new residuals
> (sshd post-session freeze, missing SSH exit-status) documented there.**

## Problem Statement

With the waitid/pidfd fix in (see `FORKTEST_GO_HANG_FIX.md`), the Go forktest
runs under shared-kernel SMP — and immediately exposes two kernel bugs:

1. **Phantom-SVC exception misclassification** (silent data corruption, present
   from SMP=2 up).
2. **Hard BKL wedge at the SIGTERM/teardown phase** (SMP=4).

## Repro

```bash
SMP=4 overlays/devbox/run-smoltcp.sh    # devbox-smoltcp, release-smp-shared
ssh -p 2222 root@localhost '/bin/forktest_parent -num_children=3 -duration=15s -combined_stress'
```

Scaling (same kernel, one run each, 2026-07-22):

| SMP | `[SPURIOUS-SVC]` | `WILD-DA` | outcome |
|-----|-----------------|-----------|---------|
| 1   | 0               | 0         | PASS |
| 2   | 8               | 0         | PASS — **but silently corrupting** |
| 4   | 25              | 2         | one child SIGSEGV, then hard BKL wedge at the 15 s deadline (~7.5k `[BKL] stuck: owner=2`, 0 RECOVERED, 0 WATCHDOG) |

Reference logs from the session that found this: `forktest_smp{1,2,4}_fixed.log`,
`forktest_smp2_final.log` (repo root).

## Evidence chain (bug 1 — phantom SVC)

Typical storm entry:

```
[SPURIOUS-SVC] stale-icache: nr=10200780808 elr=0x86840 insn@elr-4=0xcb050021 (not svc) x0=0x261bed000 — IC flush + replay #2
...
[SPURIOUS-SVC] giving up at elr=0x86840 after 20 replays — dispatching nr=113
```

Disassembly of `userspace/forktest/forktest_child` at the two recurring ELRs:

- `0x86840` = **`dc zva, x0`** inside `runtime.memclrNoHeapPointers` (Go bulk-zeroing
  freshly allocated spans).
- `0x45db4/0x45db8` = **`ldar x0,[x2]` / `ldrsb x27,[x0]`** in `runtime.(*spanSet).push`.

Pattern decode:
- `x0` is always a **page-aligned, fresh Go-heap address** → these PCs take
  **demand-paging data aborts** (first touch of a lazy page), constantly and legally.
- ELR points **at** the instruction (data-abort semantics), so `insn@elr-4` is
  never an `svc` — a real SVC would have ELR = svc+4.
- "nr" is whatever `x8` last held: 113 (`clock_gettime`, Go's hottest real
  syscall) or a Go heap pointer (0x2_60xx_xxxx ≈ 10.2e9). i.e. **no syscall setup
  ever happened** — the trap is not an SVC.

So: an EL0 **data abort is being classified as EC_SVC64**. It is NOT stale
icache (the guard's original theory): IC flush + replay does not heal it at the
same ELR (up to 20 replays), and the sites are data, not code, dependent.

### Prime suspect (code-confirmed window, not yet fix-verified)

`src/exceptions.rs`:

- `rust_sync_el0_handler` (the BKL wrapper, ~line 2117) calls
  `bkl::enter_kernel()` **first**. Under contention this spins with **IRQs
  enabled** (deliberate — see M2c bug #2 in `docs/archive/SMP_SHARED.md`), so a
  timer SGI can nest, context-switch, run other threads (whose traps overwrite
  the per-PE `ESR_EL1`/`FAR_EL1`), and/or resume this thread on a **different
  core** whose ESR holds a stale SVC syndrome.
- `rust_sync_el0_handler_inner` (~line 2128) only THEN reads `mrs esr_el1` and
  classifies the exception.

Reading `ESR_EL1`/`FAR_EL1` after any preemptible window is unsound: the
syndrome registers are per-PE state valid only until the next trap on that PE.
This fits the scaling perfectly — the misclassification rate tracks BKL-spin
time (zero uncontended at SMP=1).

Note: something already captures an `entry_esr` "at the very top of
rust_sync_el0_handler" (used near line 992 for an interrupted-syscall check) —
find where that snapshot happens and why the inner handler still re-reads live.

### Corruption amplifier (fix regardless of root cause)

The guard's give-up path **dispatches the garbage nr anyway**
(`giving up ... — dispatching nr=10201055240`). That writes ENOSYS into `x0` of
a thread that was mid-`memclr`/`spanSet.push` with a live pointer in `x0` →
guaranteed register corruption → the observed downstream `WILD-DA FAR=0x20000001a`
(a corrupted spanSet spine pointer) and Go heap corruption. Give-up must never
dispatch: deliver SIGSEGV/SIGILL to the thread (or refault) instead.

## Fix sketch (bug 1)

1. Snapshot `ESR_EL1` + `FAR_EL1` in the **vector asm at exception entry**,
   while PSTATE.I is still masked from the trap, into the `UserTrapFrame`; make
   the whole handler chain (SVC arm, both fault arms, the JIT-replay and
   VERIFY_SVC guards) consume the frame copies. Audit EVERY late `mrs esr_el1` /
   `mrs far_el1` in `src/exceptions.rs` (`grep -n "mrs.*esr\|mrs.*far"`).
2. Same audit for the EL1-sync and IRQ paths (any syndrome read after a
   yield/spin/enable-IRQ point).
3. Replace the give-up dispatch with signal delivery.
4. Keep a `SPURIOUS-SVC` counter; assert it is 0 in the boot self-tests and
   after acceptance stress runs. (Per `feedback_kernel_tests`: kernel changes
   need a `src/process_tests.rs` self-test — the race itself is hard to force,
   but the counter assertion + a Go-stress acceptance check covers regression.)

## Bug 2 — BKL wedge at teardown (SMP=4)

Onset is **exactly the parent's duration deadline** (T33.1x in the log): the
parent wakes, SIGTERMs 3 multithreaded Go children (cross-core
`pend_signal_for_thread` + `interrupt_thread` + group teardown), and the box
wedges: core 2 owns the BKL forever, all peers spin (`owner=2` constant), **zero
`[BKL] RECOVERED`** (so NOT the ticket leak — the owner is genuinely stuck in
EL1), zero `[WATCHDOG]` (no long preemption-disable), workload frozen.

May be downstream of bug-1 corruption (a Go child with a corrupted heap dying
mid-teardown) — so fix bug 1 first, then re-run the repro 5×. If the wedge
persists:

- Attach lldb BEFORE the wedge: `INSTANCE=1 GDB=1 SMP=4 overlays/devbox/run-smoltcp.sh`
  (gdbstub :1235; see `akuma_lldb_gdbstub_debugging` memory / `debug-smp.md`),
  let it wedge, halt, and backtrace **core 2** — the stuck owner is directly
  observable. Dump `KERNEL_LOCK` (owner/next_ticket/now_serving) and check for a
  cooperative wait loop or inner-lock spin under the BKL in the
  signal/teardown path (the M5a `PreemptGuard` discipline may be missing on a
  signal-delivery or exit-path lock).

## Related single-core anomaly (bug 3, low priority but diagnostic gold)

`forktest_smp1_fixed.log` has ONE `[BKL] stuck: owner=0 waiter=1` +
`[BKL] RECOVERED (advanced-lost) by core 1` — at **SMP=1**. The ticket leak
reproduces with a single core, so the standing lead suspect ("thread migrating
cores mid-EL1-hold") is wrong or incomplete. A single-core repro is a much
easier root-cause target for the open BKL accounting leak.

## Using forktest stress modes for lock-granularity (M5) work

The child's flags isolate subsystems, so each mode is a per-subsystem BKL
contention probe (combine with the BKL-hold profiler,
`akuma_exec::sync::set_profiling` + `WAIT_BY_HOLDER`, and
`sync::contention_spins` for A/B):

| Flag | Subsystem exercised | Lock-split work it measures |
|------|--------------------|-----------------------------|
| `-mmap_test -mmap_alloc_mb=N` | mmap/munmap + demand paging + PMM | `as_lock` (M5b), PMM/heap split |
| `-file_io` | VFS + block I/O under BKL | VFS/block split, Pass-B BKL-drop (M5b-4a) |
| `-goroutine_stress` | futex, timers, SIGURG preemption signals, scheduler | run-queue split (M5c), futex/signal locking |
| `-combined_stress` | all of the above | whole-system regression gate |
| `-use_c_child` | same stresses, pure C musl child | kernel-vs-Go-runtime disambiguation |

Suggested loop for each M5 split: profile with the matching single mode
(baseline `WAIT_BY_HOLDER` + `contention_spins`) → implement split → re-profile
same mode → gate with `-combined_stress` at SMP=2 and SMP=4. `scripts/quick_forktest.py`
(invoked from the repo root, like the other `scripts/*.py` harnesses) already
automates boot+run+log-scrape; extend it to sweep modes.

## Success Criteria

- `grep -c SPURIOUS-SVC` == 0 on SMP=2 and SMP=4 combined_stress runs.
- SMP=4 `-num_children=3 -duration=15s -combined_stress` EXIT=0, 5/5 runs, no
  wedge at the deadline, 0 WILD-DA / 0 WATCHDOG / (transient RECOVERED only if
  bug 3 still open).
- `sshd_crash_hunt.py` (SMP=4, 3 boots) stays clean — no regression from the
  ESR-capture change.
- Boot self-tests green at SMP=1/2/4; 125+ akuma-exec host tests; clippy clean.

## Files

- `src/exceptions.rs` — vector asm + `rust_sync_el0_handler{,_inner}`, fault
  arms, VERIFY_SVC guard (~line 2100), `entry_esr` capture (~line 992).
- `crates/akuma-exec/src/sync.rs` — KernelLock, ticket self-heal, profiler.
- `crates/akuma-exec/src/process/signal.rs` + `src/syscall/proc.rs` (tkill/kill
  paths), teardown in `process/lifecycle.rs` — bug 2 territory.
- `src/config.rs` — `FUTEX_DBG_ENABLED`, `DEADLOCK_THREAD_DUMP_ENABLED` debug
  levers (flip for instrumented runs; revert before handing back).

## Resolution (2026-07-22)

### Bug 1 — phantom SVC: FIXED (ESR/FAR entry snapshot)

The prime suspect was confirmed, but the window is even earlier than the BKL
spin: `sync_el0_handler`'s **vector asm itself enables IRQs** (`msr daifclr, #2`)
before `bl rust_sync_el0_handler`, so *any* syndrome read from Rust was already
after a preemptible window.

Fix (all in `src/exceptions.rs`):

1. The vector asm snapshots `ESR_EL1`→`x1` and `FAR_EL1`→`x2` **before**
   `daifclr`, while PSTATE.I is still masked from the trap, and passes them as
   arguments: `rust_sync_el0_handler(frame, esr, far)`. The entire inner chain
   consumes the snapshot; no live `mrs esr_el1/far_el1` remains on the EL0 sync
   path. The fault arms take ELR from the **trap frame** too — the live
   `ELR_EL1` is consumed by any intervening IRQ's `eret`, so a late read
   returns a kernel resume address, not the faulting user PC.
2. EL1-sync paths (`rust_sync_el1_handler`, CoW/user-copy fast paths) keep
   live reads — sound because that asm never unmasks IRQs before the handler.
3. The spurious-SVC guard's give-up path **never dispatches**. It sets a flag;
   after the QEMU DC-ZVA / STP-XZR misroute emulations get their chance (they
   legitimately reach give-up — their ELR-4 is not an svc either), an
   unclaimed phantom SVC gets SIGILL at the trap PC (or kills the group).
4. `SPURIOUS_SVC_COUNT` counter + `spurious_svc_count()`; boot self-test
   `test_no_spurious_svc_traps` runs LAST in the suite and asserts 0.

Verified: SPURIOUS-SVC 0 (was 8) at SMP=2, 0 (was 25) at SMP=4, across 4 boots /
13 combined_stress runs. The `[BKL] stuck owner=N` deadline wedge (bug 2) never
reappeared (was ~7.5k lines) — it was downstream of bug-1 corruption as
hypothesized.

### Bug 2b (new, found by the fix) — exit/exit_group returned to EL0: FIXED

With the phantom storm gone, the remaining WILD-DA pair at every SIGTERM
deadline decoded to `FAR=0x0 ELR=0x4aa0c` = `str xzr,[x0]` with `x0=0` inside
**`runtime.fatalthrow`** — Go's *deliberate* crash-store when `exit` returns.
Kernel chain: group teardown unregisters the process while a CLONE_VM sibling
still runs on another core (10 ms preemption window) → its `write`s hit EBADF
(fds already closed by exit_group) → it calls `exit_group`, `current_process()`
is None, `sys_exit_group` falls through and **returns the exit code to EL0** →
Go crashes on purpose. Same fall-through existed in `sys_exit`.

Fix: `src/syscall/mod.rs` dispatch arms for `nr::EXIT`/`nr::EXIT_GROUP` call
`return_to_kernel(code)` if the sys fn comes back (it handles the
process-already-gone case and parks the thread). exit/exit_group can now never
return to EL0. Verified: 0 WILD-DA in 8+ subsequent SMP=4 runs.

### Scaling table after the fixes (same repro)

| SMP | `[SPURIOUS-SVC]` | `WILD-DA` | BKL deadline wedge | outcome |
|-----|-----------------|-----------|--------------------|---------|
| 2   | 0 (was 8)       | 0         | none               | PASS |
| 4   | 0 (was 25)      | 0 (was 2) | none (was ~7.5k spins) | forktest PASS (in-band exit 0); residuals below |

### Bugs 2 + 3 — BKL wedge AND ticket leak: one root cause, FIXED (lldb-confirmed)

The wedge was NOT purely downstream of bug 1 — it reappeared intermittently
mid-run (not at the deadline) after the ESR fix, now reliably reproducible
(~1 boot in 2). lldb on a live wedge (gdbstub, `GDB=1`):

- **All four cores** parked at the same PC inside `KernelLock::acquire`'s
  ticket wait — no owner doing work anywhere.
- Lock state: `owner=4`, `next_ticket=958866`, `now_serving=958861`; the four
  spinners held tickets 958862–65. The *served* ticket belonged to no living
  waiter, and the lost-ticket self-heal can't fire because `owner != 0`.
- Smoking gun: the spinner physically on CPU3 had **`me=2`** in its registers.

Root cause — same class as bug 1: `bkl::enter_kernel()`/`leave_kernel()` read
`current_core_id()` (MPIDR) in **preemptible context** (the vector asm enables
IRQs before the handler; `acquire` masks only later, inside the wait). A
preemption + migration between the MPIDR read and the lock operation runs the
op with a stale core identity, which breaks both directions:

- stale-`me` **acquire** skips the reentrant `owner == me` fast path → the
  thread takes a ticket and spins IRQ-masked on a core whose own (logical)
  hold it can never observe → that core can never release → `owner` frozen
  nonzero, every core piles up = **the hard wedge** (0 RECOVERED).
- stale-`me` **release** is a silent CAS no-op → the hold leaks and
  `now_serving` stops advancing = **the whole bug-3 "ticket leak" family**
  that the `advanced-lost`/`reticket` self-heals were papering over.

Fix (`crates/akuma-exec/src/bkl.rs`): `enter_kernel`, `leave_kernel`, and
`reconcile_for_spsr` now mask IRQs **around** the `current_core_id()` read +
lock op (`sync.rs` `irq_save_mask`/`irq_restore` made `pub(crate)`), making
the core identity migration-atomic with the operation.

The SMP=1 `RECOVERED (advanced-lost)` sighting is consistent with a false
positive of the frozen-`now_serving` heuristic during a host stall (QEMU vCPU
descheduled) — not necessarily the same leak.

### Still open

- **Terminated-thread lock leak — the sshd "freeze" root cause: FIXED
  (deferred kill at the EL1→EL0 boundary).** After the BKL fix, 7/7 stress runs
  passed and then run 8's ssh hung: sshd (single-threaded cooperative
  multiplexer, `userspace/sshd`) was parked in syscall 301 (`SPAWN`) forever
  while the kernel stayed healthy. lldb: its core executing
  `spawn_process_with_channel_ext → resolve_symlinks → ext2 read_inode →
  block::read_bytes`, PC at the `isb; ldrb; cbnz` spin loop of
  **`BLOCK_DEVICE.lock()`** (`src/block.rs` `Spinlock`). All forktest threads
  were already recycled ⇒ a stress-run child thread died holding the
  block-device spinlock. Mechanism: `kill_thread_group` PHASE 1
  (`process/mod.rs`) marked sibling threads TERMINATED unconditionally — a
  sibling parked mid-EL1 (cooperative disk wait during demand paging, holding
  `BLOCK_DEVICE`) never ran again, so the lock leaked and every later
  disk-dependent path (sshd's next spawn) spun forever.

  **Fix (deferred kill):** under `cfg(kernel_smp_shared)`, `kill_thread_group`
  PHASE 1 no longer hard-marks siblings `TERMINATED`. It posts a per-thread
  pending-kill flag (`threading::request_thread_kill`) and leaves the sibling
  schedulable, then grace-waits (yielding, which drops the BKL under smp-shared)
  for every sibling to reach `TERMINATED`. Each sibling runs to its next
  **EL1→EL0 boundary** — the point in `rust_sync_el0_handler` (the BKL wrapper)
  where the syscall/fault call stack has unwound and released every kernel lock —
  sees the flag (`threading::take_thread_kill_request`), and self-terminates
  there (`mark_thread_terminated` + yield loop; the proven `sys_exit` pattern).
  Only then does PHASE 2 clean up fds/channels/unregister. A 2 s grace bounds
  the wait; a sibling stuck in a non-yielding EL1 loop (a separate bug) is
  hard-terminated as a last resort. Single-core / non-smp-shared builds keep the
  direct `mark_thread_terminated` (safe: the caller is the sole EL1 thread), so
  the default build is byte-for-byte unchanged. Host unit tests
  (`threading::pending_kill_tests`) + kernel self-test
  (`test_deferred_kill_does_not_strand_locks`) guard the primitive.

  **Validated (2026-07-23, SMP=4 devbox-smoltcp):** 15/15 forktest
  `-combined_stress` runs clean (old freeze onset was run 8), 8/8 concurrent
  busybox fork-hammer clean, sshd responsive throughout and after. Log: 0
  SPURIOUS-SVC, 0 WILD-DA, 0 SIGSEGV, 0 `[BKL] RECOVERED`, 0 `[WATCHDOG]`,
  0 "grace expired" (siblings self-terminate promptly — the grace-wait never
  hit the 2 s timeout). One transient `[BKL] stuck` (250 ms, recovered) during
  a spawn burst — the expected coarse-BKL contention, not the freeze. Hunt aid
  retained: `DEADLOCK_THREAD_DUMP_ENABLED` also dumps on the 30 s PSTATS cadence
  in `src/main.rs`.
- **SMP=4 boot self-test suite wedges — RESOLVED (2026-07-23, follow-up
  session).** The "nondeterministic wedge" was two DETERMINISTIC test bugs in
  sequence, each panicking/halting core 0 mid-suite so the peers' `[BKL]
  stuck: owner=1` storm buried the real error (hence "the wedge point
  varies" — it was whichever bug the run reached, and both storms look alike
  from the log tail). Neither was an SMP race — both reproduced at SMP=1 once
  the suite actually ran the failing tests (a disk with a >RAM `/models` file
  un-skips `test_mmap_file_oom_survives`):
  1. `test_mmap_file_oom_survives` asserted PMM free-count recovery the
     instant `exec_with_io` returned. Post-exit reclaim is asynchronous by
     design — `set_exited` fires before `unregister_process`, and a dying
     thread cannot free its own kernel stack/slot resources (freed at slot
     recycle by `cleanup_terminated`). Instrumentation showed a 144–321-page
     deficit that recovers after ONE reap+yield, zero heap claimed-span
     growth, converging to the same free-count every run: a lag, not a leak.
     Fix: the test polls (bounded, 500 iterations) before judging.
  2. `test_kill_thread_group_reaps_futex_blocked_sibling` fabricated a
     futex-parked sibling from a bare `claim_test_thread_slots` slot forced
     into WAITING — a state no real thread can be in (WAITING with no saved
     context). The deferred-kill fix's `request_thread_kill` WAKES parked
     siblings, so the scheduler dispatched the context-less slot:
     `[SGI-S FATAL] new_sp=0x0` halt, 4/4 runs, SMP=1..4. Fix: the sibling is
     now a real initialized thread (`spawn_user_thread_initializing`) whose
     trampoline performs the boundary dance (`take_thread_kill_request` →
     `mark_current_terminated`), making the test exercise the deferred-kill
     wake→schedule→self-terminate flow end-to-end. (Kernel-side hygiene was
     already sound: `PENDING_KILL` is cleared at slot recycle.)

  After both fixes: suite runs END-TO-END green at SMP=1 and SMP=4 (3/3 runs),
  0 FATAL / 0 PANIC, only ~55 transient recovered `[BKL] stuck` (the known
  coarse-BKL contention). Remaining suite failures are the two pre-existing
  SMP-independent ones: `fs_error_to_errno_mapping` (below) and
  `stp_xzr_ec15_handler_fires` (QEMU/HVF generates EC=0x25 instead of 0x15 —
  environment-dependent, also FAILED in old boottest_smp1 logs).
- Pre-existing suite failure (SMP-independent, also in old full-suite logs):
  `fs_error_to_errno_mapping: PermissionDenied → got -13 (EACCES), expected
  -1 (EPERM)`.
- (Fixed in passing: `test_mmap_file_oom_survives` panicked the suite at small
  RAM — its lazy-path expectation of `-11` predated clean file-page eviction,
  which now legitimately lets the oversized-mmap process finish with exit 0.
  The test accepts both; the kernel-stays-up assertion is unchanged.)
- **~~ssh always exits 255~~** (FIXED, commit `e54eba9`): `userspace/sshd` now
  sends `SSH_MSG_CHANNEL_EXIT_STATUS` + `CHANNEL_CLOSE` (`protocol.rs:392,400`),
  so `ssh` returns the real exit code. Harnesses that still gate on in-band
  markers (`echo INBAND_EXIT=$?`) are harmless — the exit code is now also
  correct. Older disk images without the rebuilt sshd still return 255.
