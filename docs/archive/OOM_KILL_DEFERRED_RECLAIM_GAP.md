# No pressure-driven reclaim of RETIRED processes — 2026-08-02

**Status: CLOSED 2026-08-02 — §5's candidates 1 and 2 both landed.** The
mechanism is `process::reclaim` (a `request_retired_reclaim()` /
`drain_retired*()` split, so the trigger can live anywhere while the drop runs
only at vetted sites) wired into the exit-path terminal parks, the idle loops,
and `alloc_page_zeroed_user`'s ladder above the live-page eviction rung.
Current-state description:
[`../reference/subsystems/memory.md`](../reference/subsystems/memory.md) ->
"Reclaim under memory pressure". §4's constraint was respected, not worked
around: no reclaim from `register_process`, cooldown untouched, `_force` still
tests-only. §5 candidate 3 (making `register_process` fail gracefully instead
of panicking) is **still open** — it needs a fallible `register_process` and
was deliberately left out of that change.

The text below is the original risk statement, kept verbatim.

---

**Status: OPEN RISK.** Not yet observed in production. Observed and measured in
a controlled boot-suite experiment (below); the production-shaped trigger is
plausible and lands exactly when the kernel can least afford it. Split out of
[`BOOT_SUITE_PMM_DEFERRED_RECLAIM.md`](BOOT_SUITE_PMM_DEFERRED_RECLAIM.md) §5.

## 1. The gap, in one paragraph

Since Phase 7e's "Free" half, a dead process's memory is returned to the PMM
only when `reclaim_retired_processes` drops the RETIRED `Process`
(`UserAddressSpace::drop` frees every user + page-table frame). That function
has exactly one steady-state caller: the `netpoll_maint` arm of the main loop,
on a 100 ms cadence. Nothing in the memory-pressure machinery —
`handle_oom`, `try_evict_ro_page`, the OOM-kill path itself — ever attempts
it. So the reclaim that matters most under pressure is scheduled by a
component that pressure can starve.

## 2. Measured consequence (boot-suite experiment, 2026-08-02)

`/bin/mmap_file` on a 507 MB file at 256 MB RAM, run before `netpoll_maint`
exists (the boot suite runs ahead of `run_async_main_preemptive` in
`main.rs`), OOM-SIGSEGV'd under pressure:

- The fault-kill path (`kill_process`) unregisters immediately → the slot is
  RETIRED holding the **entire address space**: ~35,441 pages (~138 MB).
- PMM free fell to **15 pages** (≈ `USER_PAGE_RESERVE`) and stayed there
  through 500 reap/yield polls — the poll loop drove thread-slot recycling
  and heap-span reclaim, but nothing retired-process reclaim.
- The remainder of the suite ran memory-starved: 71 PASSED instead of 240,
  exec failures ("Kernel memo…" truncated OOM error), "No current user
  process" cascades.

Adding one `reclaim_retired_processes_force()` call to the loop recovered all
of it in a single poll. The memory was never lost — it was parked with no one
scheduled to unpark it.

## 3. Production-shaped triggers

`netpoll_maint` normally closes this within ~100 ms + cooldown. The gap bites
when that assumption fails:

1. **Collector starvation under the very pressure the reclaim would relieve.**
   The maintenance loop shares the system with everything else; under a
   PMM-exhaustion event it can block on allocation (net buffer pools), lose
   scheduling to an OOM-kill storm, or sit behind BKL contention. A large
   process OOM-killed while the collector is stalled strands its whole
   address space — and each stranded AS deepens the pressure stalling the
   collector. The feedback loop only breaks if the collector eventually runs.
2. **Kill storms faster than the cadence.** N large processes killed within a
   100 ms window hold N address spaces simultaneously; peak PMM demand is the
   sum, not the max.
3. **Process-table exhaustion panics, not fails.** `register_process` claims
   only FREE slots and deliberately does not reclaim on a miss (see §4); a
   table full of RETIRED zombies (`MAX_PROCESSES` = 256) makes the next spawn
   **panic the kernel** ("Process table full"), even though every one of
   those slots is reclaimable.
4. **Anything that runs process workloads before the main loop starts** — the
   boot suite today, early-boot init workloads tomorrow.

## 4. Why there is no on-demand reclaim (the real constraint)

This is not an oversight; it is a documented lock-context hazard.
`reclaim_retired_processes` runs arbitrary `Process::drop` code — frees
page-table frames (PMM lock), releases an ASID (`ASID_ALLOCATOR`), drops fd
tables. `register_process`'s comment records the boot hang that ruled out
calling it from a full-table miss: the caller is deep inside fork/clone/spawn
paths whose lock context it doesn't control, and a caller already holding one
of the drop-path locks self-deadlocks the instant reclaim retakes it
(docs/archive/BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md, "the on-demand reclaim
that wasn't"). Any fix must add a call site with **known, minimal ambient
lock context** — not sprinkle reclaim into allocation failure paths.

The RETIRE cooldown itself must also survive: it is what guarantees no peer
core still holds a raw `*Process` from a BKL-dropped window. A pressure
trigger should call the cooldown-honoring `reclaim_retired_processes`, never
the `_force` variant (test-only).

## 5. Direction, if this ever bites for real

Candidate call sites, in order of preference:

1. **The OOM-kill path itself, after teardown completes.** The fault handler
   that decides to SIGSEGV a process for PMM exhaustion is a known context:
   it is about to return to the scheduler, holds no drop-path locks at that
   boundary, and is by definition the moment demand exceeds supply. A
   cooldown-honoring reclaim attempt there converts "strand until the
   collector runs" into "strand for at most one cooldown".
2. **`alloc_page_zeroed_user`'s eviction fallback**, mirroring
   `allocator::reclaim_to_pmm`'s `try_lock`-and-bail discipline: when free is
   at reserve and nothing is evictable, attempt reclaim before declaring OOM.
   Riskier — allocation is called from many lock contexts — so it would need
   the same reentrancy bail-outs (`try_lock` on every drop-path lock, skip on
   contention) rather than blocking.
3. **`register_process` full-table miss**: still ruled out as a direct call
   (§4), but the panic could at least become a graceful spawn failure when
   `retired_process_count() > 0`, signalling "collector starved" instead of
   halting the kernel.

## 6. Detection signature

- PMM free pinned at ~`USER_PAGE_RESERVE` while
  `akuma_exec::process::table::retired_process_count() > 0` — reclaimable
  memory exists, collector isn't running.
- `[PSTATS]` `pmm=` field flat at the floor across 30 s dumps → the 100 ms
  collector has missed hundreds of cadences.
- `dump_thread_resume_points()`: for a *clean*-exit zombie the signature is a
  `st=T` thread whose pid is still ACTIVE (unregister never ran — thread-slot
  recycle is its trigger); for a fault-kill it is `retired > 0` that never
  drains.
- Boot-suite reproduction recipe: run `/bin/mmap_file` on a >RAM file twice
  in a row from a kernel test without any force-reclaim calls (this is
  exactly the experiment in
  [`BOOT_SUITE_PMM_DEFERRED_RECLAIM.md`](BOOT_SUITE_PMM_DEFERRED_RECLAIM.md) §2).

## Background

- [`BOOT_SUITE_PMM_DEFERRED_RECLAIM.md`](BOOT_SUITE_PMM_DEFERRED_RECLAIM.md) —
  the investigation this risk was extracted from; full teardown-chain walk.
- [`BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md`](BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md)
  — why the free is deferred at all, and the self-deadlock that constrains
  call sites.
- [`BKL_PHASE7_AUDIT.md`](BKL_PHASE7_AUDIT.md) §2.1/§2.1.1 — the
  raw-pointer-liveness hazard the cooldown protects against.
