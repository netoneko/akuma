# SMP=4 fork/exec process-state corruption — handoff / debugging dossier

> **UPDATE 2026-07-21 (evening): major progress, NOT closed. `LifecycleGuard` is
> now a real per-thread preemption-disable guard; two liveness bugs it exposed are
> fixed (one pre-existing); the fault population CHANGED but SMP=4 fork-hammer is
> still not clean.** Where it stands after this session's five instrumented hammer
> runs:
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
> **Tree state now:** `crates/akuma-exec/src/process/lifecycle.rs` `LifecycleGuard` is a
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
> Companion: [`debug-smp.md`](debug-smp.md) (general shared-kernel SMP debugging)
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
4. **TLB / instruction-cache coherence of the fork CoW demotion + child first-run.** Lower
   probability (drops-off keeps EL1 serialized and `flush_tlb_all` is a broadcast
   `tlbi vmalle1is`), but a stale RW TLB entry on the core that runs the child first — before the
   demotion flush is observed — would let parent and child both write a shared frame with no
   fault. Check the ordering between `flush_tlb_all()` (`mod.rs:1726`) and the child becoming
   schedulable, and whether the child's first `eret` (`entry_point_trampoline` →
   `enter_user_mode`) guarantees an ISB/TLB-clean on the running core.

## Suggested next experiments

- ~~Resolve `0x4011d004`~~ **DONE:** it is `rust_sync_el0_handler_inner+0x0` (see hypothesis 2) —
  a corrupted/aliased context, not a legitimate pointer store.
- **SMP=1 control with the same harness** — expected clean (confirms it's a true cross-core /
  preemption race, not a fork/exec logic bug). The docs assert SMP=1 stability; confirm with
  *this* harness to remove all doubt.
- **Make `fork`/`execve`/exit atomic across preemption**, as a correctness-first bisection: mask
  IRQs (or hold a dedicated *process-lifecycle* spinlock that is NOT dropped on context switch —
  distinct from the BKL) for the whole `fork_process` / `do_execve`+`replace_image` /
  teardown critical section, so no preemption can expose their half-built state. If the crash
  vanishes, the bug is a not-atomic-across-preemption lifecycle op (hypotheses 1/3) and the
  proper fix is real locking on the process table / `Process` fields / `THREAD_CONTEXTS`. If it
  persists, it's the parallel-EL0 / TLB path (hypothesis 4). **Note:** IRQs-off across the ELF
  *read* would re-introduce the block-I/O stall the drops were added to avoid — mask only around
  the state-mutation, do block I/O into private buffers first (the split already exists in the
  fault path; mirror it).
- **Live lldb over the gdbstub** (`INSTANCE=1 GDB=1 SMP=4 …`, attach on `:1235`; see
  `debug-smp.md`). A watchpoint on the victim `Process.parent_pid` / its `THREAD_CONTEXTS[tid].pc`
  catches the corrupting write in the act. Caveat (from the memory notes): the stub's periodic
  all-core halts perturb tight races — the coarse fork-hammer here should still trip.

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
