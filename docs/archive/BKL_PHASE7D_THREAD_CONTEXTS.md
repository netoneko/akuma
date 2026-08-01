# Phase 7d: `THREAD_CONTEXTS` ownership proof

**Status: DONE, 2026-08-01.** `BKL_PHASE7_AUDIT.md` §5 posed this as a fork: either
establish that `POOL`'s state machine already guarantees "not running on any CPU"
(cheap — a corrected SAFETY comment plus a host test), or add real per-slot ownership
(expensive — a new lock). The audit here found the former: the invariant already
holds on the live call graph, on atomics already in place, independent of the BKL. It
also found one real gap — not in the invariant itself, but in dead code that would have
violated it had anything ever called it — and removed that code rather than fix it.

## 1. What was being asked

`crates/akuma-exec/src/threading/mod.rs`'s `THREAD_CONTEXTS: [SyncContext; MAX_THREADS]`
is a bare `[UnsafeCell<Context>; 64]` with a hand-written `unsafe impl Sync`. Its old
SAFETY comment:

```
/// 1. Each context is only modified by the scheduler with IRQs masked
/// 2. A context is only accessed when its thread is not running on any CPU
/// 3. We're single-CPU, so no concurrent access is possible
```

Clause 3 is false under `smp-shared` — that's the premise of the whole Phase 7 effort.
Clause 2 is the real invariant, but as written it was an assertion, not a proof: nothing
in the code enforced it independent of the BKL serializing all EL1 execution today. Phase
7d's job was to find out whether removing the BKL from thread lifecycle code (spawn,
fork's context capture, the scheduler switch) would still leave that invariant standing,
or whether it currently only holds because the BKL happens to still be there.

## 2. Method

Every `get_context`/`get_context_mut` call site was traced to its caller and classified
by *why* concurrent access to that slot can't happen, independent of the BKL. Three real
categories emerged, covering every live site:

1. **Scheduler switch.** `ThreadPool::get_context_ptrs`'s callers (the SGI dispatch path,
   `threading/mod.rs` ~2500) touch both the outgoing and incoming slot's context while
   `POOL` (a real cross-core `Spinlock<ThreadPool>`) is held across the *entire* switch —
   decision, context save, context load — per the existing M5c comment at the call site:
   *"The Big Kernel Lock provided that atomicity before; holding POOL across the whole
   switch makes it hold on POOL alone, so the scheduler SGI no longer needs the BKL."*
   That comment was already correct and already shipped; Phase 7d's contribution is
   confirming it actually covers every context-switch access, and that the scheduler
   (`schedule_indices`) never selects a slot whose state isn't READY — so a slot mid-setup
   is structurally never a switch target, regardless of what the BKL is doing.

2. **Spawn / reclaim setup.** Every live spawn path (`claim_free_slot`, used by
   `spawn_user_thread_fn_internal`, `spawn_system_thread_fn`,
   `spawn_user_closure_initializing` via `spawn_user_thread_initializing`, and
   `adopt_current_as_core_idle`) claims a slot with `THREAD_STATES[i].compare_exchange(
   FREE, INITIALIZING, SeqCst, SeqCst)` before writing `THREAD_CONTEXTS[i]`. The CAS is a
   real cross-core primitive — exactly one caller can ever win it for a given index — and
   the scheduler ignores INITIALIZING slots (case 1), so nothing else touches that context
   until the owner publishes it. `reclaim_terminated_slots` mirrors this on the tear-down
   side with a `TERMINATED -> INITIALIZING` CAS before it zeros the context — the "CRITICAL"
   comment already in that function names the exact race this prevents (a naive
   `TERMINATED -> FREE` would let a concurrent spawn claim the slot and get its fresh
   context zeroed out from under it).

   Publication is `mark_thread_ready`'s plain `Ordering::SeqCst` store. This is the piece
   `BKL_PHASE7_AUDIT.md` §2.2 flagged as an *unwritten* argument ("`fork_process` steps
   5–8 write `THREAD_CONTEXTS` for a thread that is not yet published, relying on the BKL
   for the publication ordering"). It turns out not to need the BKL: a `SeqCst` store is
   at minimum a release, and the scheduler's `THREAD_STATES[idx].load(SeqCst)` is at
   minimum an acquire, so any core that observes READY via that load is guaranteed — by
   the atomic memory model alone, not by any lock — to also observe every write to
   `THREAD_CONTEXTS[idx]` that preceded the store in the writer's program order. Proven
   with real concurrent hardware, not argued from the model: see §4.

3. **Self-read of the live thread.** `get_saved_user_context(parent_tid)` in
   `fork_process`/`clone_thread`/vfork's child-context capture is always called with
   `parent_tid == current_thread_id()` — a thread reading its own slot. This can't race
   because there is exactly one execution of "this thread" at a time by construction; no
   lock is needed or taken.

What's deliberately **not** covered by this proof: `dump_thread_resume_points` and
`list_kernel_threads` read arbitrary threads' contexts — including ones RUNNING on a peer
core — with no synchronization at all. This is accepted, not fixed: both are debug/stat
paths (the `kthreads` command, the heartbeat hang-dump), the reads are single aligned
`u64` fields (no torn multi-word reads, no memory unsafety), and a stale value is
tolerable for display. Nothing that feeds a correctness decision uses this pattern.

## 3. The gap that was found

Four `ThreadPool` methods used a **plain load** instead of a CAS to find a free slot:
`spawn`, `spawn_with_stack_size`, `spawn_system_closure`, `spawn_user_closure` (plus their
free-function wrappers `spawn`, `spawn_cooperative`, `spawn_with_options`,
`spawn_with_stack_size`, `spawn_user_thread`). Their search ranges overlap the CAS-based
paths' ranges (`spawn`/`spawn_with_stack_size` scanned the *entire* `1..MAX_THREADS`,
covering both the system and user thread sub-ranges that `spawn_system_thread_fn` and
`spawn_user_thread_fn_internal` claim via `claim_free_slot`). A plain-load spawn holding
`POOL` and a lock-free CAS spawn on a peer core could both observe the same slot as FREE
before either commits a state change — the loser's context write clobbers the winner's,
and the loser's final `THREAD_STATES[i].store(READY, ...)` can stomp an INITIALIZING slot
still being set up, publishing a half-written context to the scheduler. `ThreadPool::reclaim`
and `cleanup_terminated` had the mirror bug on tear-down: direct `TERMINATED -> FREE` with
no INITIALIZING intermediate and no context zeroing, re-opening the exact race
`reclaim_terminated_slots`'s "CRITICAL" comment documents and works around.

**None of this was reachable.** A full-workspace grep found zero callers of
`ThreadPool::spawn`/`spawn_with_stack_size`/`spawn_system_closure`/`spawn_user_closure`/
`reclaim`/`cleanup_terminated`, and zero callers of the five dead free-function wrappers,
anywhere outside their own definitions. (`spawn_cooperative`, the boot self-test's
`test_spawn_cooperative`, calls `spawn_fn_cooperative` — a different, CAS-based function
that only looks similar by name.) The comment on the now-deleted `spawn_user_thread`
even claimed "Used by fork_process to spawn the child thread," which was false — fork
uses `spawn_user_thread_initializing`.

Because the race was real *only in the sense that nothing ever exercised it*, the fix was
deletion, not a lock: keeping dead code around whose only property was "safe because
unreachable" would have made the SAFETY comment's proof conditional on that code staying
unreachable forever — a landmine for the next person who wires it up. Removed:
`ThreadPool::spawn`, `spawn_with_stack_size`, `spawn_system_closure`, `spawn_user_closure`,
`reclaim`, `cleanup_terminated`, and (once orphaned by the first two) `reallocate_stack`;
free functions `spawn`, `spawn_cooperative`, `spawn_with_options`, `spawn_with_stack_size`,
`spawn_user_thread`. ~340 lines net removed.

## 4. Host test: proving it on real concurrent hardware, not just arguing it

`crates/akuma-exec/src/threading/mod.rs`, `mod thread_contexts_invariant_tests` (bottom of
file), real `std::thread`s standing in for cores, confined to slots 50..64 so it doesn't
collide with other tests' fixed tids under `cargo test`'s default parallelism:

- `claim_free_slot_never_double_claims_under_contention` — 8 threads × 500 iterations,
  each claiming a slot via `claim_free_slot`, stamping `THREAD_CONTEXTS[idx].x19` with a
  thread-tagged marker, yielding (widening the window for a would-be clobberer), then
  re-reading and asserting the stamp survived. Proves case 2's mutual exclusion under
  genuine contention, not just "the CAS API looks right."
- `ready_transition_publishes_context_writes_without_a_lock` — 2000 iterations, each
  spawning a reader thread that spins on `THREAD_STATES[idx].load(SeqCst) == READY` and,
  the instant it observes that, immediately reads `THREAD_CONTEXTS[idx].x19` and asserts
  the writer's marker is already there. This is the exact scenario §2.2 worried about
  ("relying on the BKL for publication ordering") — reproduced without any lock in the
  loop at all, on the host's real multicore hardware.

Both pass. `cargo test -p akuma-exec --target aarch64-apple-darwin`: **158 passed, 0
failed** (up from the Phase 7 audit's 156 — +2 for these tests), clean clippy.

## 5. Boot verification

`cargo build --profile release-smp-shared --features devbox-smoltcp,bkl-profile`, booted
on a private QEMU instance (own disk clone not needed — no other instance was running) at
`MEMORY=2048`:

| | SMP=2 | SMP=4 |
|---|---|---|
| Boot self-test suite | 240+ PASSED, 2 FAILED (both pre-existing/unrelated — `PermissionDenied -> EPERM` errno mapping, `stp_xzr_ec15_handler_fires` QEMU-dependent) | same two, same count |
| `Threading Tests` block | ALL PASSED | ALL PASSED |
| PANIC / WILD | 0 / 0 | 0 / 0 |
| `[BKL] RECOVERED` | 0 | 0 |
| stale dropped-window heals | 0 | 0 |
| devbox regimen (herd/httpd/sshd) | ran cleanly 60+s, steady PSTATS | ran cleanly 65+s, steady PSTATS |

No contention measurement here — this phase changes a comment, deletes unreachable code,
and adds host tests; it doesn't touch a hot path or drop the BKL anywhere new, so there is
nothing for a same-binary A/B to measure (same reasoning as `BKL_MM_CARVE_OUT.md` §5).

## 6. What this unblocks, and what it doesn't

This closes 7d as "cheap" per the audit's framing: `THREAD_CONTEXTS` access is now backed
by a written, tested proof instead of a stale single-core comment, on every path Phase 7f
would touch when it starts moving `clone`/`fork_process` syscalls into the per-syscall
BKL opt-out list. It does **not** touch the process table (7e) — `with_process`,
`lookup_process`, `for_each_process` still rely on `with_irqs_disabled` alone, and that
remains the real blocker for a whole-`clone`/`execve` carve-out per
`BKL_PHASE7_AUDIT.md` §2.1. 7d only ever covered `THREAD_CONTEXTS`; the process struct
itself is a separate structure with a separate (much larger) migration.

## Background

- [`BKL_PHASE7_AUDIT.md`](BKL_PHASE7_AUDIT.md) §2.2 (the original finding), §5 (the 7a–7f
  decomposition, 7d's charter).
- [`../reference/subsystems/locking.md`](../reference/subsystems/locking.md) — updated
  with this phase's status.
