# SMP=4 fork/exec process-state corruption — investigation dossier

**Status: CLOSED 2026-07-31.** Restored to `docs/archive/` 2026-08-14: it was
deleted from `docs/runbooks/` in `c4f16a8e` ("clean up old runbooks"), correctly —
a debugging dossier is not an action-first runbook — but the same commit preserved
two comparable dossiers by renaming them into `archive/`
([`FORKTEST_GO_HANG_FIX.md`](FORKTEST_GO_HANG_FIX.md),
[`SMP_GO_STRESS_CORRUPTION_FIX.md`](SMP_GO_STRESS_CORRUPTION_FIX.md)), and this one
was still cited as the **"Full dossier"** by
[`../runbooks/debug-smp.md`](../runbooks/debug-smp.md)'s triage matrix and by two
grade-carrying reference docs. Text below is verbatim as deleted.

## Corrections found after this doc was written — read these first

Each is also marked **inline at the claim it corrects**, as a `⚠️ CORRECTION N`
callout, so a reader who lands mid-document from a search does not act on a
retracted claim.

1. **The "three-mechanism combination, all load-bearing" claim is wrong on
   mechanism 3.** `COW_FAULT_LOCK` (`src/pmm.rs`) provided **no mutual exclusion
   at all** — a per-PA counter incremented and decremented around the break that
   nothing ever read and nothing ever waited on, so it excluded no one. That is
   F3 of [`COW_PILE_AUDIT.md`](COW_PILE_AUDIT.md) §5; it has since been **deleted**
   (`grep -rn COW_FAULT_LOCK src crates` → 0 hits). What actually makes the
   cross-process CoW break safe is the `released_last_va` gate in
   `complete_cow_break`. Mechanisms 1 (`LifecycleGuard`) and 2 (the `demote_range_to_ro`
   DSB) stand.
2. **The mid-document "Tree state now" block contradicts the top-of-file update.**
   It says `LifecycleGuard` is "a documented no-op on every build"; the code calls
   `disable_preemption()` under `cfg(kernel_smp_shared)` — it is **active**. The
   top-of-file update takes precedence
   ([`BKL_PROCESS_CARVE_OUT.md`](BKL_PROCESS_CARVE_OUT.md) §8.1).
3. **The "3 boots × 10 rounds, 0 faults" validation line is not corroborated by
   the harness it cites** and should be re-derived before being relied on
   ([`BKL_PROCESS_CARVE_OUT.md`](BKL_PROCESS_CARVE_OUT.md) §9.8).

Since this closed, `no-bkl-process` has been promoted into the `smp-shared`
feature set (2026-07-31) — see `BKL_PROCESS_CARVE_OUT.md` §9.9.

> **UPDATE 2026-07-31: VALIDATED — the fix combination holds.**
>
> > ⚠️ **CORRECTION 3 (2026-08-14): this validation is not corroborated by the
> > harness it cites** — `BKL_PROCESS_CARVE_OUT.md` §9.8 re-ran it and the numbers
> > below do not reproduce. Re-derive before relying on them.
>
> A fork-hammer
> validation at SMP=4 (3 boots × 10 rounds × 8 concurrent SSH connections, each
> running `for i in 1..8; do busybox true; done`) produced **0 SIGSEGV / WILD-DA /
> DA-MISS / PANIC / ppid=0** fault signatures across all boots and rounds. The
> herd concurrent bringup window (the primary trigger — every service is
> fork+exec'd simultaneously across cores) was clean on all 3 boots.
>
> The fix is a **three-mechanism combination**, all of which are load-bearing:
>
> 1. **`LifecycleGuard` (active `disable_preemption`)** —
>    `crates/akuma-exec/src/process/lifecycle.rs`. Acquired at the top of
>    `fork_process` (`mod.rs:1491`), `vfork_process` (`mod.rs:2219`), and inside
>    `replace_image`/`replace_image_from_path` (`image.rs:48,129`). Prevents
>    involuntary timer preemption from exposing half-mutated process/CoW state
>    to non-lifecycle EL1 readers. `schedule_indices` (`threading/mod.rs:2204`)
>    returns `None` for involuntary entries when preemption is disabled, which
>    blocks writer #8 (`sgi_scheduler_handler_with_sp` at `threading/mod.rs:2749`)
>    from saving `THREAD_CONTEXTS` during the guarded window — closing
>    hypothesis 2's torn-read window.
> 2. **DSB barrier in `demote_range_to_ro`** — `crates/akuma-exec/src/mmu/mod.rs:1782`.
>    Guarantees PTE writes are globally visible before `flush_tlb_all()` under
>    `cfg(kernel_smp_shared)`, closing the cached-RW-TLB-entry race window
>    (hypothesis 4).
> 3. ~~**Per-physical-page CoW fault serialization (`COW_FAULT_LOCK`)** —
>    `src/pmm.rs:815`. Prevents parent and child (different PIDs) from
>    concurrently breaking CoW on the same shared frame (hypothesis 4).~~
>
>    > ⚠️ **CORRECTION 1 (2026-08-14): this mechanism never existed.**
>    > `COW_FAULT_LOCK` was a per-PA counter incremented and decremented around
>    > the break that **nothing ever read and nothing ever waited on** — it
>    > excluded no one, so it prevented nothing. F3 of
>    > [`COW_PILE_AUDIT.md`](COW_PILE_AUDIT.md) §5; deleted since
>    > (`grep -rn COW_FAULT_LOCK src crates` → 0 hits). What actually makes the
>    > cross-process CoW break safe is the `released_last_va` gate in
>    > `complete_cow_break`. **So the combination is two mechanisms, not three** —
>    > 1 and 2 below stand.
>
> The BKL remains held across all fork/exec/fault paths and is still required:
> it prevents concurrent EL1 on other cores. The `LifecycleGuard` + BKL together
> form the two-mechanism correctness envelope documented in
> `docs/archive/BKL_PROCESS_CARVE_OUT.md` §5 — neither is redundant.
>
> **Caveat:** the userspace sshd (`/bin/sshd` over smoltcp) exhausts after ~3
> rounds of 8 concurrent connections (a pre-existing sshd robustness issue, not
> fork corruption). The fork-hammer therefore exercises ~24 fork+exec cycles
> per boot plus the boot bringup itself (~30 fork+execs from herd). The
> runbook's original harness (16 concurrent × 8 rounds, with the in-kernel SSH
> server) would be a stronger stress if re-run; but the boot bringup alone was
> the primary trigger ("reproduces within one boot most of the time") and it
> is now clean across 3 consecutive boots.

> **UPDATE 2026-07-21 (evening): cross-core CoW/TLB protocol bugs FIXED.** Two critical
> issues in the CoW share/demote/break protocol were found and fixed:
>
> - **Missing DSB barrier in `demote_range_to_ro`:** PTE writes were not guaranteed
>   globally visible before `flush_tlb_all()`, creating a race window where cached RW
>   TLB entries could persist after PTEs were demoted to RO. Fixed by adding `dsb ish`
>   at the end of `demote_range_to_ro` under `cfg(kernel_smp_shared)`.
> - **CoW fault serialization was per-PID, not per-physical-page:** Parent and child
>   (different PIDs) could fault on the same shared page concurrently, leading to
>   double-free of the shared frame. Fixed by adding global per-physical-page CoW fault
>   serialization via `COW_FAULT_LOCK` in `src/pmm.rs` + updated CoW fault handler.
>   ⚠️ **See CORRECTION 1 — `COW_FAULT_LOCK` locked nothing and has been deleted.**
>
> Both fixes address the cross-core CoW/TLB coherence issues identified in hypothesis
> 4. Testing with the fork-hammer harness is needed to confirm the WILD-DA FAR=0x0
> crashes are eliminated. See `docs/archive/SMP_SHARED.md` "Cross-core CoW/TLB protocol
> fixes" for full technical details.
>
> Earlier progress (2026-07-21 morning): `LifecycleGuard` is now a real per-thread
> preemption-disable guard; two liveness bugs it exposed are fixed (one pre-existing);
> the fault population CHANGED but SMP=4 fork-hammer was still not clean. Where it
> stood after that session's five instrumented hammer runs:
>
> - The **mixed-EL context corruption** (user PC = kernel text, SPSR=EL0t —
>   hypothesis 2) stopped appearing in the final runs, and three POISON tripwires
>   (below) now stand guard for it. Not yet provable as fixed — it was
>   intermittent — but it no longer dominates.
> - The surviving crashes are the **null-deref family** (valid busybox PC reads
>   `FAR=0x0` ~1 s into shell life, `last_sc=ppoll`): DATA corruption, i.e.
>   **hypothesis 4 (cross-core CoW/TLB coherence) is now the lead** — the shells
>   that fork children lose an owned pointer value. Next session should attack
>   the CoW share/demote/break protocol under concurrent EL0 (see hypothesis 4
>   and the demote-then-flush window in `fork_process`).
>
> What changed, and what was found on the way:
>
> - **The fix:** `LifecycleGuard::acquire()` now calls
>   `threading::disable_preemption()` under `cfg(kernel_smp_shared)` (released on
>   drop; explicit `release()` retained in the no-return teardown fns). This keeps
>   exactly the property the whole-op DAIF experiment proved sufficient (no
>   involuntary switch can expose half-mutated lifecycle state mid-op) while
>   avoiding both DAIF failure modes: IRQs stay enabled (timer/device IRQs and
>   block-I/O completion still run) and voluntary yields still switch
>   (`schedule_indices` only gates `!voluntary` entries), so ops that read ELFs
>   or wait cooperatively cannot deadlock the box. Full rationale:
>   `crates/akuma-exec/src/process/lifecycle.rs` module docs.
> - **Defense-in-depth:** thread-slot recycling resets the per-tid
>   preemption-disable counter (a leaked count would permanently starve the
>   slot's next occupant); `disable_preemption()` is `#[track_caller]` and the
>   preemption watchdog prints the culprit `file:line` of the oldest disable.
> - **NEW BUG FOUND while validating (the hammer wedged the box with 0
>   SIGSEGVs): the BKL fair-FIFO ticket accounting can leak a ticket with
>   `sched_bklfree_el0` OFF** — same family as the known M5c step-2 leak, but on
>   the default configuration. lldb on the wedged instance (gdbstub :1235, all
>   cores halted): `KERNEL_LOCK = {owner: 0, next_ticket: 114074, now_serving:
>   114069}` with all four cores' backtraces parked in the BKL acquire spin (3×
>   `rust_irq_handler_with_sp+864`, 1× `rust_sync_el0_handler+352`) — five
>   tickets in flight, four living waiters, the served ticket's taker gone ⇒
>   `now_serving` can never advance ⇒ permanent 4-core wedge. Preemption
>   counters were clean (only an `idle_halt` WFI hold), so this is NOT a guard
>   leak — the guard's scheduling shift just makes the pre-existing hole easy to
>   hit under fork-hammer churn.
> - **Mitigation landed:** `KernelLock::acquire` is now self-healing
>   (`crates/akuma-exec/src/sync.rs`): (a) if the lock stays FREE while
>   `now_serving` sits frozen short of our ticket for ~20M consecutive spins,
>   the waiter CAS-advances `now_serving` one step; (b) a waiter whose ticket
>   `now_serving` moved PAST re-takes a fresh ticket; (c) the ownership take is
>   a CAS (not a blind store) so a recovery race cannot mint two owners. Every
>   recovery prints `[BKL] RECOVERED (<kind>) by core N` — **each such line is a
>   live sighting of the still-unfixed accounting leak; root-causing it is the
>   open follow-up** (start from thread-migration-while-in-EL1 and the
>   reconcile-acquire paths).
>
> Original dossier below (mechanism confirmation, disproven approaches, repro).
>
> ---
>
> **UPDATE 2026-07-21 (later same day): mechanism CONFIRMED, fix scope identified,
> tree returned to a clean baseline.** Two decisive experiments were run on real
> SMP=4 QEMU after the LifecycleLock was disproven (see the status block below):
>
> - **SMP=1 control, same harness:** 39 forks + 39 busybox execs, **0 crashes,
>   0 `[BKL] stuck`** across 12 hammer rounds. SMP=4 crashes on round 1. ⇒ this is a
>   **true cross-core race**, not a fork/exec logic bug. (The doc had asserted SMP=1
>   stability; now confirmed with *this* harness.)
> - **Whole-op per-core preemption disable** (mask `DAIF.I` for the entire body of
>   every lifecycle op, replacing the cross-core spinlock): **0 SIGSEGV across a full
>   hammer run** — so the fault class *is* preemption-mid-operation exposure (to
>   non-lifecycle readers too, which is why serializing only lifecycle-vs-lifecycle
>   didn't help). BUT it **hard-deadlocked**: the ops (and the freshly-exec'd child's
>   first `[IA-DP]` ELF code-page fault) cooperatively yield / wait on async block-I/O
>   completion that a *different* thread must pump; with preemption masked that thread
>   never runs, the I/O never completes, and the BKL holder never releases → all cores
>   wedge. Wedged exactly at the child's first code-page fault.
>
> **Validated fix direction (TODO):** the *mechanism* (disable preemption during the
> mutation) is right; the *scope* (whole op) is wrong. Disable preemption only around
> the **synchronous, non-yielding, non-blocking memory-mutation windows** — never
> across a lock-wait, a cooperative yield, block I/O, or an `eret` to userspace:
> `replace_image`'s `mmap_regions/lazy_regions.clear()` + AS-swap + repopulate middle;
> `fork_process`'s child-publish (context write + table register + mark schedulable);
> the `THREAD_CONTEXTS[tid]` writes; the trap-frame capture. Pin the exact non-yielding
> boundaries with an lldb watchpoint on `Process.parent_pid` / `THREAD_CONTEXTS[tid].pc`.
>
> **Tree state now:**
>
> > ⚠️ **CORRECTION 2 (2026-08-14): superseded, and wrong about the code.**
> > This block is from an earlier same-day revision; the top-of-file update takes
> > precedence. `LifecycleGuard` calls `disable_preemption()` under
> > `cfg(kernel_smp_shared)` (`lifecycle.rs:85–86`) — it is **active**, not a
> > no-op (`BKL_PROCESS_CARVE_OUT.md` §8.1).
>
> `crates/akuma-exec/src/process/lifecycle.rs` `LifecycleGuard` is a
> documented **no-op** on every build (both the spinlock's BKL-stall regression and the
> whole-op deadlock removed; behavior == pre-66e09bf). The 11 `LifecycleGuard::acquire()`
> call sites are retained as no-ops marking where the narrow guards belong. SMP=4 boots
> clean to sshd (0 `[BKL] stuck`, 0 watchdog) and still crashes under the hammer (the
> original open bug, now with a much sharper diagnosis).
>
> ---
>
> **Status: LifecycleLock fix (commit 66e09bf) EMPIRICALLY DISPROVEN 2026-07-21.**
> A real SMP=4 QEMU run (fresh `--profile release-smp-shared --features
> devbox-smoltcp,no-tests` on `devbox.img`/4096MB, lock confirmed active in the
> binary) + the fork-hammer **still crashes on boot 1, hammer round 1**: 12
> SIGSEGVs, 10× user-PC-in-kernel-text, `ppid=0`-clobbered processes — the same
> signatures as before the fix. Idle boot is clean; the crash fires the instant
> the hammer runs. Per the decision tree below, this puts us squarely in
> **hypotheses 2/4** (THREAD_CONTEXTS aliasing / TLB coherence) and rules out the
> lifecycle-op-vs-lifecycle-op race the lock serialized. **Two concrete new facts
> from that run:**
>
> 1. **The clobbered user PC resolves to `rust_sync_el0_handler_inner + 0x0`**
>    (`0x4011d22c` in this binary; the doc's earlier `0x4011d004` was a different
>    build) **and the fault SPSR is `0x0` (EL0t).** A context that was saved while
>    the thread executed the syscall/fault handler **at EL1** is being restored and
>    `eret`'d **as an EL0 context** — i.e. an EL-confused / aliased
>    `THREAD_CONTEXTS[tid]` slot, written by the **preemption context-save path,
>    which is NOT a lifecycle op and takes no lock.** That is precisely why the
>    LifecycleLock (which only serializes fork/exec/exit/spawn against each other)
>    cannot touch this bug. **Hypothesis 2 is now the lead; start at the SGI/timer
>    EL1-preemption context-save in `src/exceptions.rs` and every writer of
>    `THREAD_CONTEXTS` in `crates/akuma-exec/src/threading/mod.rs`.**
> 2. **The fix introduced a REGRESSION:** the pre-fix run had "0 `[BKL] stuck`";
>    this run has 8× `[BKL] stuck` (`owner=3 waiter=1/2/4`) plus a `[WATCHDOG]
>    Preemption disabled 140ms`. The lock is held across preemption and never
>    dropped at EL transitions, so it contends with the BKL. If the lock is kept as
>    defense-in-depth it needs a lock-ordering audit against the BKL first.
>
> The `LifecycleLock` itself (`crates/akuma-exec/src/process/lifecycle.rs`) is
> correctly implemented and wired into all 11 named lifecycle ops; it is a no-op on
> non-`kernel_smp_shared` builds. It just does not address the actual fault class.
> Original handoff doc below, unchanged for context.
>
> Companion: [`../runbooks/debug-smp.md`](../runbooks/debug-smp.md) (general shared-kernel SMP debugging)
> and [`../reference/subsystems/smp-shared.md`](../reference/subsystems/smp-shared.md).

## One-paragraph summary

Under **`cfg(kernel_smp_shared)` at SMP=4**, during the **high-concurrency bringup window**
(secondaries onlining while `herd` fork+execs every service, plus a fork-hammer of
`busybox` over ssh), processes **SIGSEGV with heterogeneous signatures** — the hallmark of
**memory corruption of `Process` / saved-context / page-table state**, not a single logic
bug. It hits **both freshly-forked children *and* already-running processes** (e.g. `/bin/sshd`
faults at 2.89 s uptime). The kernel itself stays alive (0 `[BKL] stuck`, heartbeats
continue) — this is a userspace-visible fault caused by corrupted per-process state. A
**settled** instance is stable (survives 30×20 concurrent fork rounds); the corruption is
specific to the concurrent bringup window.

## Exact repro

```bash
# Build the SMP devbox image (no-tests, userspace sshd, smoltcp, real SMP):
cargo build --profile release-smp-shared --features devbox-smoltcp,no-tests

# Auto-repro harness: reboots at SMP=4, waits for sshd, fork-hammers it, greps for the fault.
# (Harness lives in scratchpad; reproduced verbatim at the bottom of this doc.)
SMP=4 python3 sshd_crash_hunt.py
# Writes sshd_crash_HUNT_RESULT.txt / _PROGRESS.txt / sshd_hunt_boot.log in the repo root.
```

It reproduces **within one boot** most of the time (caught on boot 1/20 in the last run).
The fork-hammer is: 16 concurrent ssh connections, each running
`for i in 1 2 3 4 5 6 7 8; do busybox true; done` — i.e. a burst of `fork`+`execve("/bin/busybox")`.

## The signatures (heterogeneous ⇒ corruption, not one logic bug)

All observed in a **single** boot:

- **Null / near-null deref in userspace.** `FAR=0x0` or `FAR=0x120` (a struct-field offset off
  a null base), `ELR` a *valid* busybox code address, `x0=0`. musl/busybox dereferences a
  pointer that should be valid but reads as 0.
- **User PC = a *kernel* address.** `[WILD-DA] pid=22 FAR=0x0 ELR=0x4011d004` with `SPSR=0x0`
  (EL0t). The kernel is based at `0x40100000`, so `0x4011d004` is *kernel text* running as the
  thread's EL0 PC — the saved user context's `pc` was clobbered with a kernel value.
- **Clobbered / half-built `Process`.** `[DA-MISS] pid=23 ppid=0 … checked 0 mmap_regions`,
  `[DA-MISS] pid=8 ppid=0 va=0x120 parent_lr=0`. A process whose `Process.parent_pid == 0`
  (no real parent) and/or empty `mmap_regions` — fields that fork/exec set to non-zero for a
  real process. **NB pid 8 is an already-running service, not a fresh child** — its `parent_pid`
  was zeroed *after* it was healthy.

Raw dump (drops-OFF run, boot 1):

```
[T2.16] [Fault] Data abort from EL0 at FAR=0x0, ELR=0x100b5ff4, ISS=0x47
[Fault]  x0=0x0 x1=0x10120460 x2=0x10124220 x3=0x13
[Fault]  x19=0x20124620 x20=0x10120000 x29=0x202ffff2f0 x30=0x100b6254
[Fault] Process 21 (/bin/busybox) SIGSEGV after 0.06s
[DA-MISS] pid=22 ppid=5 va=0x0 lr_count=6 parent_lr=6 parent_has_va=false
...
[T2.17] [WILD-DA] pid=22 FAR=0x0 ELR=0x4011d004 last_sc=...   # user PC = KERNEL addr
[Fault] Process 22 (/bin/busybox) SIGSEGV after 0.06s
[DA-MISS] pid=23 ppid=0 va=0x0 lr_count=5 parent_lr=0 parent_has_va=false   # ppid=0
[T2.17] [DP] eager miss: pid=23 va=0x0 checked 0 mmap_regions               # empty mmap_regions
```

## THE decisive experiment (already run — do not repeat)

**Hypothesis tested:** the two BKL-drop optimizations (execve ELF read + file-fault block I/O)
open a window where concurrent EL1 corrupts shared state.

**Method:** forced **both** drops OFF at boot (see the temporary edit in `src/main.rs` right
after the `no-tests` `bringup_secondaries()` call — marked *DO NOT COMMIT*):

```rust
smp_shared::set_exec_bkl_drop_enabled(false);
smp_shared::set_fault_bkl_drop_enabled(false);
```

**Result: the crash STILL fires on boot 1/20, all three signatures present.**

**Why this is decisive.** `rust_sync_el0_handler` (`src/exceptions.rs:2116`) wraps the *entire*
syscall+fault path in `bkl::enter_kernel()` / `bkl::leave_kernel()` (lines 2117 / 2123). The
**only** BKL releases *inside* an excursion are the two drops we just disabled. So with the
drops off, **every EL1 excursion holds the BKL end-to-end → no two cores ever execute EL1 at
the same instant.** The corruption persists anyway ⇒ **it is NOT caused by concurrent EL1
execution and NOT by the BKL-drop windows.** (This overturns the earlier working theory in
`debug-smp.md` that said "audit the BKL-drop sites.")

## Ruled OUT (with evidence — don't re-chase)

1. **BKL-drop windows** (execve ELF read `src/syscall/proc.rs:645`; file-fault block I/O
   `src/exceptions.rs:2774,3303`). Disproven by the decisive experiment above. Also: each drop
   is scoped to touch only *private / not-yet-installed* frames — no live `&mut Process` or
   process-table mutation crosses the drop.
2. **CoW share / break / eviction / refcount.** Cross-core *defended*: `pmm::free_page`
   (`src/pmm.rs:569`) is refcount-aware via `cow_ref_dec` (only frees at count 0); the CoW-break
   fault handler copies the source frame *before* decrementing (`src/exceptions.rs:2584-2600`);
   the PTE edit is under the per-AS `as_lock`. `try_evict_ro_page` → `free_page`
   (`crates/akuma-exec/src/mmu/mod.rs:823`, `.../process/children.rs:537`) is safe for the same
   reason — it can drop this AS's ref on a shared frame without freeing it under a peer.
3. **Fork never drops the BKL / never yields.** `handle_oom` (`src/allocator.rs:52`) grows the
   heap synchronously or returns `Err`; no allocation inside `fork_process` yields. A single
   `fork_process` is therefore *instantaneously* atomic w.r.t. EL1 (but see the preemption
   caveat below — it is **not** atomic across preemption).
4. **Thread-slot-reuse context zeroing** (cleanup zeroing a slot spawn just filled). Guarded by a
   `TERMINATED → INITIALIZING` SeqCst CAS (`crates/akuma-exec/src/threading/mod.rs:910-950`).
5. **ext2 / block read path.** Properly SMP-locked: `state: RwSpinlock<Ext2State>` +
   `block_cache: Spinlock<BlockCache>` + `BLOCK_DEVICE: Spinlock` (`crates/akuma-ext2/src/ext2.rs:529-547`,
   `src/block.rs:226`). Concurrent reads serialize correctly.

## The structural hole (where the bug almost certainly lives)

The process/threading subsystem's cross-core safety rests on **single-CPU invariants upgraded to
"the BKL serializes EL1"**, *not* on locks protecting the data:

- **`THREAD_CONTEXTS`** — the per-thread saved register file (`UnsafeCell`, no lock). Its safety
  comment literally reads *"3. We're single-CPU, so no concurrent access is possible"*
  (`crates/akuma-exec/src/threading/mod.rs:1377-1385`). Accessed with only `with_irqs_disabled`.
- **The process table** (`crates/akuma-exec/src/process/table.rs`) hands out
  `&'static mut Process` to 218+ sites via `current_process()` / `lookup_process()` /
  `get_process_ptr()`, guarded only by `with_irqs_disabled` (`table.rs:112-143`). The safety
  comment says valid "while IRQs are disabled **or** no other thread can call
  `unregister_process`" — a single-core statement. `with_irqs_disabled` takes **no cross-core
  lock**; it only masks local IRQs.

**The critical realization about the BKL's guarantee.** IRQs are **enabled** during the
syscall/fault handler (`src/exceptions.rs:174`, `msr daifclr, #2`). So a thread can be
**preempted mid-`fork`/`execve`/`exit`**. On that preemption the IRQ path reconciles the BKL to
the frame it `eret`s into — and if it switches to an **EL0** thread it **releases the BKL**
(`src/exceptions.rs:1512,1543`; the eret in `rust_sync_el0_handler` releases at line 2123 *before*
the asm restores registers). Therefore:

> The BKL guarantees no two cores run EL1 **at the same instant**. It does **NOT** make a
> multi-step kernel operation (`fork_process`, `do_execve`/`replace_image`, exit/teardown)
> **atomic across preemption**. A half-mutated global (`THREAD_CONTEXTS[tid]`, a `Process`
> mid-construction, a process-table slot mid-registration, a `Process` mid-`replace_image` with
> `mmap_regions` already `.clear()`ed) is exposed at every preemption point to whatever EL1 code
> the next-scheduled thread runs — including on another core.

And separately, **EL0 runs with no BKL at all** (genuine parallelism): two `busybox` children
execute userspace simultaneously on different cores over frames the fork **CoW-shared** between
parent and child (`crates/akuma-exec/src/process/mod.rs:1555-1726`). Correctness there depends on
the demote-to-RO + `flush_tlb_all()` (`mod.rs:1717-1726`) being coherent before either side runs,
and on the CoW-break protocol being atomic across the two *separate* per-AS `as_lock`s.

## Narrowed hypothesis space (rank-ordered for the next debugger)

1. **`replace_image` (execve) is not atomic across preemption.** The tell is strong: crashes
   cluster right after `[FORK-DBG] replace_image: … AS swapped` / `trampoline ENTRY`, and
   `replace_image` **`.clear()`s `mmap_regions`/`lazy_regions`** mid-flight
   (`crates/akuma-exec/src/process/image.rs:49,124`) and swaps the address space
   (`deactivating old AS` → `swapping AS`). If the exec'ing thread is preempted between the
   clear/AS-swap and repopulation, and *anything* reads that `Process` (a signal, a
   `for_each_process` sweep, its own re-entry, a sibling), it sees a half-built image →
   `checked 0 mmap_regions`, `ppid`/context garbage. **Start here.**
2. **`THREAD_CONTEXTS[tid]` clobbered → user PC = kernel addr. ⭐ Strongest concrete lead.**
   `0x4011d004` **resolves exactly to `akuma::exceptions::rust_sync_el0_handler_inner + 0x0`**
   (via `llvm-nm -nC` on `target/aarch64-unknown-none/release-smp-shared/akuma`) — the entry of
   the syscall/fault handler. Crucially, **that function's address is never taken as a value in
   the source** — it is only `bl`'d (`src/exceptions.rs:178`) / called
   (`src/exceptions.rs:2122`). So the value in the saved-context `pc` slot is **not** a legitimate
   stored function pointer; it arrived by **memory corruption / aliasing of the context memory**
   itself (a `THREAD_CONTEXTS[tid]` entry or the on-kernel-stack `UserTrapFrame` being reused,
   freed-and-reallocated, or overlapped by another live structure and then read back as a
   `UserContext`). Trace every writer of `THREAD_CONTEXTS` (`update_thread_context`
   `threading/mod.rs:2556`; the SGI context-save; `get_saved_user_context` `:3349`; the fork
   capture `process/mod.rs:1963-2013`) and every place a `UserContext`/`UserTrapFrame`'s backing
   memory could be aliased or reused across a tid-index collision or a preemption. The exact
   `+0x0` value argues against a stack-return-address leak (those land mid-function) and for a
   whole-struct overwrite / index-collision. **`0x100d9ea0` / `0x100b5ff4` etc. are the *user*
   PCs — busybox text; only the `0x401xxxxx` values are kernel.**
3. **`&'static mut Process` aliasing across preemption.** `fork_process` holds
   `let parent = current_process()` (`process/mod.rs:1376`) across its whole body with IRQs
   enabled. If the parent (or a co-owner of that `Process`) runs EL1 after a preemption while
   this `&mut` is live, that is aliasing UB + a data race. Audit long-lived `&'static mut Process`
   held across any point where IRQs are on.
 4. **TLB / instruction-cache coherence of the fork CoW demotion + child first-run.**
    **FIXED (2026-07-21 evening).** Two cross-core CoW/TLB protocol bugs were found and
    fixed:
    - **Missing DSB in `demote_range_to_ro`:** PTE writes were not guaranteed globally
      visible before `flush_tlb_all()`, creating a race window where cached RW TLB entries
      could persist after PTEs were demoted to RO. Fixed by adding `dsb ish` at the end of
      `demote_range_to_ro` under `cfg(kernel_smp_shared)`.
      (`crates/akuma-exec/src/mmu/mod.rs:1745`)
    - **CoW fault serialization was per-PID, not per-physical-page:** Parent and child
      (different PIDs) could fault on the same shared page concurrently, leading to
      double-free of the shared frame. Fixed by adding global per-physical-page CoW fault
      serialization via `COW_FAULT_LOCK` in `src/pmm.rs` + updated CoW fault handler in
      `src/exceptions.rs`. See `docs/archive/SMP_SHARED.md` "Cross-core CoW/TLB protocol
      fixes" for full details.
      ⚠️ **See CORRECTION 1 — `COW_FAULT_LOCK` locked nothing and has been deleted.**

    Testing with the fork-harness is needed to confirm the WILD-DA FAR=0x0 crashes are
    eliminated.

## Suggested next experiments

- ~~Resolve `0x4011d004`~~ **DONE:** it is `rust_sync_el0_handler_inner+0x0` (see hypothesis 2) —
  a corrupted/aliased context, not a legitimate pointer store.
- ~~SMP=1 control with the same harness~~ **DONE:** Confirmed clean — 0 kernel fault lines
  (connection failures under the 16-way flood are graceful `No available user threads`
  exhaustion, not a kernel bug). Confirms this is a true cross-core / preemption race.
- ~~Cross-core CoW/TLB protocol fixes~~ **DONE (2026-07-21 evening):** Fixed two bugs:
  1. Missing DSB barrier in `demote_range_to_ro` (PTE writes not globally visible before
     `flush_tlb_all`)
  2. CoW fault serialization was per-PID, not per-physical-page (double-free race when
     parent and child fault concurrently on same shared page). See hypothesis 4 details
     above and `docs/archive/SMP_SHARED.md` for full fix documentation.
- **Validate fixes with fork-hammer harness:** Run `sshd_crash_hunt.py` (see Appendix) to
  confirm the WILD-DA FAR=0x0 crashes are eliminated. Success bar: 3 boots × 12 rounds at
  SMP=4 with 0 fault lines and no wedge, plus an SMP=1 control.
- ~~Live lldb over the gdbstub~~ Deferred in favor of protocol fixes; still useful for
  frozen-state inspection if crashes persist after fixes.

## Environment / build facts the next debugger needs

- Feature/profile: `--profile release-smp-shared --features devbox-smoltcp,no-tests`.
- `devbox.img` (1 GiB ext2) + `MEMORY=4096` default; QEMU `-smp 4` via `SMP=4`.
- BKL model: owner-tracked, idempotent, **held iff a core is in EL1**, reconciled at EL
  transitions; contended acquire is a **fair FIFO ticket** wait
  (`crates/akuma-exec/src/sync.rs`, driven via `akuma_exec::bkl`).
- The temporary drops-off experiment edit is in `src/main.rs` (marked **DO NOT COMMIT**); revert
  it before shipping. It does not need to stay for debugging — the bug reproduces with drops on
  or off.
- All SMP-shared code is `cfg(kernel_smp_shared)`-gated; default/size/extreme/multikernel builds
  compile none of it.

## Appendix: repro harness (`sshd_crash_hunt.py`)

Reboots devbox-smoltcp at SMP=4 up to 20 times; per boot: wait for `Started sshd` (or a
boot-time crash), then 10 rounds × 16 concurrent ssh connections each running a `busybox true`
fork loop; grep the boot log for `SIGSEGV` / `abort from EL0`; on a hit, dump the surrounding
fault lines to `sshd_crash_HUNT_RESULT.txt` and stop. Full script is in the session scratchpad
(`scratchpad/sshd_crash_hunt.py`); it shells out to `overlays/devbox/run-smoltcp.sh` with
`SMP=4`.
