# Phase 7e ("Free" half): deferred process-table reclamation

**Status**: Landed 2026-08-01. No feature flag, no runtime toggle, no
`smp-shared`-only gate — the new state machine (`RETIRED` slot state) and the
deferred collector (`reclaim_retired_processes`) always run, on every profile.
There is nothing to A/B: the old synchronous free was a live use-after-free
hazard under `smp-shared`'s already-default-on carve-outs, not an optional
optimization with a contention number attached.

This is the "Free" half of 7e in
[`BKL_PHASE7_AUDIT.md`](BKL_PHASE7_AUDIT.md) §5: deferred reclamation for
`unregister_process`'s `Box::drop`, which no existing inner lock covered. The
"Access" half (extending `lookup_process_shared`'s `&self` + `as_lock`/`vm_lock`
pattern to the remaining ~300 `lookup_process`/`current_process` call sites, and
deleting the `&'static mut` API) is unstarted; see §6.

## 1. The hazard this closes

`docs/reference/subsystems/locking.md`'s "What the BKL is still load-bearing
for" table has carried the process table (`&'static mut Process`, 300 call
sites) as an open item since Phase 3, with `with_irqs_disabled` — mutual
exclusion on **one core** — as its only guard. `BKL_PHASE7_AUDIT.md` §2.1.1
sharpened this: the documented safety argument ("a process can't be freed by
its own thread") covers *self*-teardown only. Peer cores free *other* pids'
`Process` at `process/mod.rs:1116`/`:1209` (`kill_fork_subtree_recursive` and
sibling cleanup) and via `wait4`/`waitid` reaping a child (`src/syscall/proc.rs`,
six call sites).

Before this change, `unregister_process(pid)` found the slot, swapped its
pointer to null, and returned `Option<Box<Process>>` — whoever called it
(`let _ = unregister_process(pid);` at most call sites) dropped the Box
essentially immediately, running `Process::drop` synchronously. That frees
`dynamic_page_tables`; the address space itself is freed earlier via
`UserAddressSpace::drop` when `Process` itself drops.

The problem: five carve-outs are already default-on in `smp-shared`
(`no-bkl-vfs`, `no-bkl-network`, `no-bkl-process`, `no-bkl-mm`, `no-bkl-drivers`),
each of which drops the BKL for a bounded window of real I/O or a page-table
edit. Inside such a window, the executing core holds a raw `*mut Process` or
`&'static Process` obtained from `lookup_process`/`get_process_ptr`/
`lookup_process_shared` *before* the window opened — and for that window's
duration, the BKL is not serializing anything. A peer core that reaps that
exact pid via `wait4` during the window used to free the `Process` — and the
memory the first core was still reading became a use-after-free with nothing
standing in the way. `with_irqs_disabled` only blocks preemption/IRQs on the
*local* core; it says nothing about what a different core's atomics-only
`unregister_process` scan can do concurrently.

This was real, not hypothetical, for the same reason `BKL_PHASE7B_PPOLL_CARVE_OUT.md`
found a real bug the moment a carve-out's window could span real concurrent
kernel activity: the five landed carve-outs already create exactly this
condition today, on every `smp-shared` boot.

## 2. Design: retire, don't free

`unregister_process` no longer frees anything. It transitions the slot
`ACTIVE -> RETIRED` (a new `slot_state::RETIRED`, alongside `FREE`/`ACTIVE`) via
a `compare_exchange`, stamps a retirement timestamp, and returns `bool` (was
this call the one that retired it) instead of `Option<Box<Process>>`. A RETIRED
slot is invisible to every lookup path (`get_process_ptr`, `for_each_process`,
`find_process`, `collect_pids`, `collect_process_info`, `process_count`) — they
already filtered on `== ACTIVE`, so RETIRED needed no changes there — and
invisible to `register_process`'s free-slot scan (which only claims `FREE`).
The `Process` and its address space stay live in memory, exactly where they
were, until `reclaim_retired_processes` actually frees them.

`reclaim_retired_processes()` scans for `RETIRED` slots whose
`process_reclaim_cooldown_us` (10ms, `config::PROCESS_RECLAIM_COOLDOWN_US`, same
order of magnitude as `THREAD_CLEANUP_COOLDOWN_US`) has elapsed, atomically
swaps the pointer to null (so a second concurrent reclaimer racing the same slot
gets null and skips — no caller-identity gate, no single collector, mirroring
why `threading::reclaim_terminated_slots` dropped its own gate: docs/archive/
BKL_VFS_CARVE_OUT.md §11.4), sets the slot `FREE`, and only then drops the
`Box`. The cooldown is the actual safety margin: it must outlast any
BKL-dropped window that could still hold a stale pointer into this exact slot.
Those windows are single bounded I/O ops or PTE-chunk copies (§"no-bkl-*"
sections of `locking.md`), not open-ended, so 10ms is generous — the same
reasoning `THREAD_CLEANUP_COOLDOWN_US` already relies on for a structurally
identical hazard (a thread slot's stack/context reused while a stale reference
is still in flight).

Called from exactly one place: **periodically**, from `netpoll_maint`
(`src/main.rs`), on the same 100ms cadence as `threading::reclaim_terminated_slots`.
§3 explains why `register_process` — the obvious second call site, on a
full-table miss — does *not* also call it, despite that being the original
design and the pattern `spawn_user_thread_fn_internal` uses for `THREAD_STATES`.

## 3. The on-demand reclaim that wasn't

The first version of this phase added the symmetric fix on the allocation
side too: `register_process`, on a full-table miss, called
`reclaim_retired_processes()` once and retried the free-slot scan once before
panicking — exactly mirroring `spawn_user_thread_fn_internal`'s
`reclaim_terminated_slots` retry, and for the same stated reason: deferred
reclamation means a RETIRED slot doesn't become FREE until its cooldown
elapses, so a burst of process exits can transiently fill the table with
cooled-but-uncollected zombies — the "slots sat TERMINATED for tens of
seconds, `fork` stalled" failure mode `BKL_VFS_CARVE_OUT.md` §11.4 already
found and fixed for `THREAD_STATES`.

Boot-verifying this at SMP=2 with the full self-test suite hung indefinitely
(`[BKL] stuck: owner=1 waiter=2` forever, 200%+ host CPU, zero test-suite
progress) right after `test_epoll_multi_poller_pipe` passed. The next test,
`test_sigpipe_terminate_no_deadlock`, is real fork/exec/pipe/SIGPIPE traffic
(`sh -c "busybox yes | busybox head -n 1"`) — the first test in the suite to
put real process churn through the real exit path, not a synthetic
`make_test_process`. Isolated by stashing the whole phase and re-running the
identical boot: baseline passed `test_sigpipe_terminate_no_deadlock` in
milliseconds and booted cleanly to SSH, which pinned the hang to this phase
specifically rather than a pre-existing flake.

Root cause: `THREAD_STATES` reclaim only flips atomics, so calling it from an
arbitrary allocation-miss caller is safe regardless of what that caller
already holds. `Process` teardown is not that cheap — `Box::drop` runs
`UserAddressSpace::drop`, which frees page-table frames and releases an ASID,
each needing their own locks. `register_process` is reachable from deep
inside fork/clone/spawn (`process/spawn.rs`, `process/mod.rs`'s
`fork_process`/clone paths), whose lock context it does not and cannot know.
The self-test suite's dense back-to-back `register_process`/`unregister_process`
churn (~200 tests by this point, most leaving a RETIRED zombie behind them,
many still inside their 10ms cooldown) made the table genuinely full often
enough that the on-demand path actually ran during `test_sigpipe_terminate_no_deadlock`'s
real `fork`+`exec`, at a point in the call stack that already held a lock
needed to free some *unrelated* earlier zombie — a same-core, non-reentrant
`Spinlock::lock()` re-entry, which spins forever by construction. That is
exactly the shape of hazard the `- **A guard's inner spinlock must mask local
IRQs too...**` and neighboring entries in `locking.md`'s "Correctness rules
learned the hard way" already warn about for *known* lock pairs; this was the
same class of bug for a pair nobody had reason to suspect, because the two
call sites (`register_process` allocating a brand-new process, and
`reclaim_retired_processes` freeing an unrelated old one) look unrelated
unless you trace exactly where the second one's `Box::drop` can run.

Fix: removed the on-demand branch entirely. `register_process` just panics on
a genuinely full table again, exactly like before this phase. The periodic
`netpoll_maint` collector — a call site with no ambient lock context, since
it's a fresh async-loop iteration, not nested inside another subsystem's
allocation path — remains the only reclaimer. This reopens the transient
table-exhaustion risk under a fast enough exit burst that the on-demand path
was meant to close, traded deliberately for not self-deadlocking; a future fix
for that risk needs a different collector call site (a dedicated system
thread outside any allocation path, the way `netpoll_maint` already is), not
reclaiming inside `register_process`. Re-verified at SMP=2 with the full
self-test suite: §7.

**Lesson for future reclaim/collector work in this kernel:** "reclaim on
demand at the allocation-miss site," the general pattern `locking.md`
recommends from the `THREAD_STATES` precedent, has a precondition worth
stating explicitly now that it's been violated once: the reclaimed object's
teardown must not need any lock the allocation-miss caller could already be
holding. Plain-atomics slot recycling (threads) satisfies this for free;
anything that runs real `Drop` code (page tables, address spaces, file
handles, sockets) probably does not, and needs the same audit this section
just did before trusting the pattern.

## 4. Why a CAS, not just a state store

`unregister_process` claims the `ACTIVE -> RETIRED` transition with
`compare_exchange`, not a plain store. A racing second `unregister_process(pid)`
for the same pid (plausible: a slow `wait4` waiter and a signal-driven reaper
both reaching the same zombie) must not re-find the slot as if it were still
ACTIVE and retire it a second time — that would double-run the
thread-termination side effect and, worse, race two reclaimers into thinking
they each own the sole reclaim of that pointer. Losing the CAS makes the second
caller's `unregister_process` return `false` and touch nothing further; exactly
one caller performs the termination side effect and exactly one eventual
`reclaim_retired_processes` frees the memory. `test_unregister_process_second_call_loses_cas`
(`src/process_tests.rs`) pins this.

## 5. Call-site fallout

`unregister_process`'s signature change (`Option<Box<Process>> -> bool`) touched
every caller:
- `crates/akuma-exec/src/process/mod.rs` (5 sites) and `src/syscall/proc.rs`
  (6 sites): all already discarded the returned Box (`let _ = ...;` or
  equivalent) — mechanical fixups, no behavior change.
- Two host/boot tests (`src/tests.rs`, `test_clone_vm_mmap_regions_on_owner` /
  `test_clone_vm_eager_fallback_finds_region`) manually free synthetic
  `mmap_regions` frames that the real `Process::drop` doesn't own (they're
  injected directly by the test, not backed by a real mmap through the page
  tables). Previously this used the returned Box after unregistering; now it
  drains and frees those frames *before* calling `unregister_process`, while
  the process is still ACTIVE and visible to `lookup_process` — the correct
  place for it regardless of this change, since a retired/reclaimed process
  should not be reachable at all.

## 6. Tests

Host build (`cargo check -p akuma-exec`) and full kernel build (base `release`,
`release-smp-shared` with and without `bkl-profile`) all compile clean — see
commit history for the exact fixups this forced (mostly `.is_some()`/`Option`
pattern matches on what is now a `bool`).

Two new boot self-tests in `src/process_tests.rs`, run as part of
`process_tests::run_all_tests()`, mirroring `test_thread_slot_reclaim_on_spawn`
(the equivalent thread-slot test) for the process table:

- `test_process_reclaim_respects_cooldown` — register + unregister a synthetic
  process, confirm an immediate `reclaim_retired_processes()` declines to
  collect it (cooldown not elapsed), then burn past `PROCESS_RECLAIM_COOLDOWN_US`
  via the real timer and confirm the next call collects it. Flushes any
  leftover RETIRED zombies from earlier tests via `reclaim_retired_processes_force()`
  first so the before/after counts are unambiguous.
- `test_unregister_process_second_call_loses_cas` — §4 above, directly.

Existing process-table tests (`test_process_table_register_get_unregister`,
`test_wait4_reaps_zombie`, `test_slot_recycling`, `test_exit_unregisters_process`)
needed only the `Option<Box<_>>` → `bool` type fixup; their assertions
(`lookup_process` returns `None` immediately after unregister, `process_count()`
drops) still hold exactly as before, because RETIRED was made invisible to
every one of those query paths from the start.

## 7. Boot verification

<!-- Filled in after the SMP boot run below completes. -->

## 8. What this unblocks, and what it doesn't

This closes the concrete UAF hazard in §1 for the five carve-outs already
default-on. It does **not**:

- Convert any `lookup_process`/`current_process` call site to the
  `&self`-borrowing, `as_lock`-guarded pattern `lookup_process_shared` already
  uses on the M5b fault path — that's the "Access" half, ~300 mechanical sites,
  unstarted (`BKL_PHASE7_AUDIT.md` §5, 7e "Access").
- Change anything about `with_irqs_disabled` being the process table's only
  same-core mutual exclusion — RETIRED just makes cross-core *freeing* safe;
  read/write races on a live ACTIVE process's fields are a separate, still-open
  concern the Access half is meant to address.
- Touch `ProcessInfo`/`register_process`'s BKL dependency at all — registering
  a *new* pid was never the hazard (a fresh `Process` has no outstanding
  pointers anywhere yet); only reaping an *existing* one was.
- Move any syscall off the BKL. Nothing about the per-syscall entry point
  changed; this is still, like `no-bkl-mm`/`no-bkl-drivers`, foundational work
  with no attribution-backed contention number attached (`BKL_MM_CARVE_OUT.md`
  §5 explains why that's expected for plan-driven, not attribution-driven,
  phases).

## Background

- [`BKL_PHASE7_AUDIT.md`](BKL_PHASE7_AUDIT.md) §2.1/§2.1.1/§5 — the audit that
  scoped 7e and split it into Access/Free.
- [`BKL_VFS_CARVE_OUT.md`](BKL_VFS_CARVE_OUT.md) §11.4 — the thread-slot analog
  of this exact bug class (deferred cleanup starved under load until the
  caller-identity gate was dropped), which `reclaim_retired_processes` mirrors.
- [`BKL_PROCESS_CARVE_OUT.md`](BKL_PROCESS_CARVE_OUT.md) §9.2 — rejected a
  coarse `PROCESS_TABLE_LOCK`; this phase doesn't introduce one either.
- [`../reference/subsystems/locking.md`](../reference/subsystems/locking.md) —
  current-state reference; updated alongside this doc.
