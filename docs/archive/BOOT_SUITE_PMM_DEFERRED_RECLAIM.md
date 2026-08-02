# Boot-suite PMM assertions vs. deferred process reclaim — 2026-08-02

`test_mmap_file_oom_survives` panicked the kernel at its PMM-conservation
assert (`PMM not reclaimed after kill: before=35777 after=35456`, a 321-page
deficit) and halted the whole suite at 37 PASSED — deterministically, on
`--release` at the default 256 MB with a >RAM model file on disk. The deficit
was not a leak. It was the boot-suite environment observing, for the first
time at this mapping size, that post-Phase-7e process teardown is a chain of
deferred collectors — none of which run during the suite.

## 1. Wrong hypothesis first: the fs cache

The failure appeared right after `fs-cache` became default (7cf9348), and the
test streams 507 MB through ext2, so the obvious suspect was block-cache
growth claiming PMM pages via kernel-heap `handle_oom` spans. Instrumenting
the test with `allocator::claimed_span_report()` falsified this cleanly:
`heap committed 0 -> 0 pages`. The cache's 13→35 MB growth fit entirely inside
the pre-claimed heap arena and never touched PMM. (The accounting stays in the
test anyway — on other profiles/RAM sizes mid-test span claims are possible
and must not read as a leak.)

## 2. The double-run experiment

Running `/bin/mmap_file` twice in a row classified the deficit:

| round | exit | free before → after | deficit |
|---|---|---|---|
| 0 | 0 (clean) | 35777 → 35456 | **321 pages** |
| 1 | −11 (OOM-SIGSEGV) | 35456 → **15** | **35441 pages** |

Round 1 left the kernel with 15 free pages; the rest of the suite ran
memory-starved (71 PASSED vs 240, exec failures, "No current user process").
Adding `reclaim_retired_processes_force()` to the poll loop made both rounds
recover in **one poll** — round 0's loop also collected **22 retired zombie
processes from earlier tests** that were silently holding pages
(deficit −824). Nothing leaks; everything is reclaim backlog.

## 3. The actual teardown chain (clean exit, kernel-spawned process)

1. `sys_exit_group` marks the process Zombie, closes fds, notifies the
   channel (this is what wakes `exec_with_io`), tears down user frames via
   `kill_thread_group`, marks its thread TERMINATED, and parks in
   `yield_now()` forever. It deliberately does **not** `unregister_process`
   ("leave as zombie for wait4"). The `Process` — and its `UserAddressSpace`
   holding every **page-table frame** — stays ACTIVE in the table. For a
   507 MB mapping that is ~321 pages (≈254 L3 tables + L2/L0 + ancillary).
2. Thread-slot recycle (`cleanup_terminated`, cooldown-gated ~10 ms) fires
   `on_thread_cleanup(tid)` → `unregister_process(pid)` → slot RETIRED.
   Kernel-spawned processes have no wait4 parent; this hook IS their reaper.
3. `reclaim_retired_processes` (cooldown-gated) does the `Box::drop` →
   `UserAddressSpace::drop` → page tables + L0 + ASID freed. Its only
   steady-state caller is **netpoll_maint** — which is spawned *after* the
   boot suite (`run_async_main_preemptive` comes after
   `process_tests::run_all_tests()` in `main.rs`).

On the fault-kill path (OOM SIGSEGV), step 1's eager user-frame teardown does
not happen the same way — `kill_process` unregisters immediately, and the
ENTIRE address space (user frames included) waits on step 3. That is why the
OOM-killed round stranded ~35 K pages: with no collector running, ~138 MB sat
in a RETIRED slot indefinitely.

**Thread-dump breadcrumb** (`dump_thread_resume_points` in the FAIL path):
`tid=8 st=T pid=31 sc=94` with `retired=0 active=1` — a TERMINATED thread
whose exit_group'd process is still ACTIVE is the signature of step 2 never
having run.

## 4. Consequences for boot-suite tests

Any PMM-conservation assertion in the boot suite must drive the whole chain
itself, force variants of both collectors (the cooldowns outlast fast poll
loops):

```rust
akuma_exec::threading::cleanup_terminated_force();
akuma_exec::process::table::reclaim_retired_processes_force();
crate::allocator::reclaim_to_pmm();
```

— both before snapshotting `free_before` (earlier tests' zombies depress the
baseline; 22 were pending at this point in the suite) and inside the recovery
poll loop. `pmm_conserved_across_spawn_exit_reap` already did this, which is
why it passed (drift=0) while the mmap test failed. With the chain in place
the mmap test conserves exactly: `free 36922 -> 36922 pages (1 reclaim poll)`.

The test's assert was also converted to a non-fatal `[FAIL]` print (with span
report, retired/active counts, and `mmu::shared_l0_stats()` — a new
diagnostic for frames parked in `SHARED_L0_TABLE`'s deferred-free entries) so
one failing test can no longer halt the suite.

## 5. Open question worth keeping

In production the chain is fine (netpoll_maint runs every 100 ms). But the
round-1 shape generalizes: a large process OOM-killed while `netpoll_maint`
is starved, wedged, or not yet started strands its whole address space, and
memory pressure is exactly when that reclaim matters most. There is no
pressure-driven reclaim trigger — `handle_oom`/eviction do not attempt
`reclaim_retired_processes` (deliberately, per the lock-context reasoning in
`table.rs::register_process`). If a real workload ever reproduces the
"suite-collapse" shape, a dedicated low-ambient-lock reclaim call site under
PMM pressure is the direction.

## Background

- [`BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md`](BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md)
  — why unregister defers the free (RETIRED slots, BKL-dropped windows).
- [`CURRENT_TRAP_FRAME_STALE_ON_EXIT.md`](CURRENT_TRAP_FRAME_STALE_ON_EXIT.md)
  — the adjacent teardown-window family, same 2026-08-02 session.
- [`BKL_RUSTC_SCALING_BASELINE.md`](BKL_RUSTC_SCALING_BASELINE.md) — the
  workload context in which the flakiness was noticed.
