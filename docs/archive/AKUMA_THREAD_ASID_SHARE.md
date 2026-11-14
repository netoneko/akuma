# Thread churn: ASIDs held by threads, and a process table that panicked when full

*2026-11-11, branch `akuma-multiarch-attempt-0`.*

## Symptom

`futextest` step 6 (500 thread create/join rounds) failed on AArch64 with `[asid] EXHAUSTED` and
`pthread_create` returning `EAGAIN`. While it lasted, `sys_spawn` failed too (`Failed to create
address space`), so a new ssh session could not start a shell and the box looked dead for a moment.
The kernel from before the multiarch slices (`mrg-a64.elf`) and the sockets-slice kernel
(`sock5-a64.elf`) behave the same: it is not a regression from the branch.

`userspace/forktest/c_stress/thrchurn.c` reproduces it on its own: create a thread, join it, repeat.

| run (AArch64, HVF, SMP=4) | failed |
|---|---|
| `thrchurn 300 20000` (20 ms pause between rounds) | 0 |
| `thrchurn 2000 2000` (2 ms pause) | 0 |
| `thrchurn 300 0` (no pause) | 18, first at round 251 (`EAGAIN`) |
| `thrchurn 300 0`, again | 28 |

Nothing fails when the rounds are spaced out, however many there are, so the ASIDs are not
leaked: they are held in flight and come back late.

## Cause

Three fixed pools of about 256 entries each, and an exited thread held a place in all three for
two 10 ms cooldowns:

| Pool | Size | An exited thread held it until |
|---|---|---|
| thread slots | 248 | the slot recycle, 10 ms after exit (`THREAD_CLEANUP_COOLDOWN_US`) |
| process-table rows | 256 | the row's reclaim, 10 ms after that (`PROCESS_RECLAIM_COOLDOWN_US`) |
| ASIDs | 256 | the `UserAddressSpace` drop, in that same reclaim |

A join returns as soon as the thread exits, so a no-pause loop creates about 250 threads per
cooldown window and empties whichever pool is checked first.

The ASID was not even used. `UserAddressSpace::new_shared` allocated a fresh ASID for every
`CLONE_THREAD` view, but `clone_thread` hands the child the parent's `ttbr0` verbatim, so every
thread ran under its parent's ASID. The view's own ASID was flushed and freed in its drop and
never installed anywhere. That bookkeeping had a second fault: an owner dropping before its
threads freed **its** ASID at once, while the threads were still running under it, and the next
`alloc` could hand it to another process. All TLB maintenance in the tree is already all-ASID
(one L0 used to live under several ASIDs), which narrowed the window, but the ASID was reused
under live translations.

### The second limit, found by fixing the first

With threads sharing their group's ASID, the next run of `thrchurn 300 0` panicked the kernel:

```
[threads] slots exhausted (live=4 terminated=244 ceiling=248) — reclaimed 11 and retrying
!!! PANIC !!!
Location: crates/akuma-exec/src/process/table.rs:73
Message: Process table full (256 slots, 11 reclaimable RETIRED)
```

The ASIDs had been failing first and returning `EAGAIN`. Behind them the process table filled
with the rows of exited threads, and `register_process` panics on a full table. This was a
recorded OPEN item in `docs/reference/subsystems/memory.md`. The comment in
`akuma-syscalls-glue`'s `clone_thread` said amd64 had already hit it. The table finds out it is
full only at the end of `spawn_child_thread_and_publish`, after the child's thread slot, its amd64
task row, its pid-map entry and its channels exist. Nothing there could be unwound, so it
panicked.

## Fix

1. **Threads share their group's ASID** (`crates/akuma-mmu`). The new
   `UserAddressSpace::new_thread_view(l0, owner_asid)` registers in `SHARED_L0_TABLE` like
   `new_shared` but allocates nothing, and marks the view `asid_owned = false`. Changes to
   `Drop`:
   - An owner that drops while views are alive parks its ASID in the entry
     (`deferred_asid`) beside its deferred frames.
   - The view that brings the count to zero flushes and frees it.
   - A view frees an ASID of its own only if it allocated one.
   - If the views all go between the owner's check and its store, nothing is parked, and the owner
     frees its own ASID.

   `clone_thread` uses `new_thread_view`; `vfork_process` and the boot tests keep `new_shared`.
   The x86 type has no ASID, so its `new_thread_view` is `new_shared`.
2. **A process-table row is reserved before the child is built** (`process::table`).
   - `reserve_process_slot()` grants a `ProcessSlot` only while `occupied + reserved <
     MAX_PROCESSES`. It reads and raises the count under one lock.
   - `register_reserved` claims, then returns the reservation. Every other change to that sum
     lowers it, so its claim cannot miss.
   - `fork_process`, `vfork_process` and `clone_thread` reserve at entry, AArch64 `spawn` before
     boxing the process, and amd64 `sys_spawn` and `init` before making the task. A full table
     is refused there with `PROCESS_TABLE_FULL`, while there is nothing to undo.
   - Plain `register_process` keeps its panic for boot and tests.
3. **A full table is waited out** (`akuma_syscalls_glue::proc::retry_while_full`). When `clone`,
   `fork` or `vfork` is refused with `PROCESS_TABLE_FULL`, or with `NO_FREE_USER_SLOTS` (now
   public in `akuma-threading`), the call parks 1 ms and runs again, draining retired rows before
   every attempt. It gives up after 100 ms; then the caller sees `EAGAIN`, or `ENOMEM` from amd64
   `fork`. Linux does not fail `pthread_create` because earlier threads are still being cleaned
   up, and here that capacity always comes back within the two cooldowns. Spawn is not retried.

## Verification

- `thrchurn` (no pause) on both kernels: 0 failures for `300 0 1`, `2000 0 1` and `300 0 10`
  (3000 rounds). AArch64 is HVF at SMP=4; amd64 is the TCG herd VM at SMP=2. No panic in either
  log.
- `futextest` (all phases, including step 6), `thrprobe` (8 × 12 threads) and `cloneflags`
  (matches Linux) pass on both kernels.
- New boot test `process_slot_reservation_bounds` (AArch64): it grants exactly the free rows,
  refuses the next, and accepts again once the reservations are dropped. It passes at SMP 1/2/4.
- AArch64 HVF boot suite: 316/324/322 at SMP 1/2/4, the earlier 315/323/321 plus the new test.
  The remaining `FAILED` lines are the thread-ceiling test's own `SPAWN FAILED` diagnostic, as
  before.
- amd64 TCG boot suite: 836/846/846 at SMP 1/2/4, and 846 for `headless`, unchanged.
- Clippy `-D warnings` is clean on AArch64, amd64 (default, `no-tests`) and Taoxichuan (x86_64
  and aarch64). Host tests pass for `akuma-mmu`, `akuma-exec`, `akuma-threading` and
  `akuma-syscalls-glue`. The `unsafe` budget is unchanged.

## Open

- A thread created *by a vfork child* runs under the vfork child's own ASID, which that child
  frees when it drops. This predates the fix and is unchanged by it. POSIX allows a vfork child
  only `exec` or `_exit`.
- `spawn` has no wait: a spawn into a table full of cooling rows returns `EAGAIN` (AArch64:
  `Err` from `spawn_process`). Nothing spawns at thread-churn rates today.
- The two cooldowns are still the ceiling on sustained churn: about 250 threads per 20 ms. Past
  that, `retry_while_full` waits rather than fails, so heavy churn is slowed rather than refused.

Background: [`AKUMA_MULTIARCH_ATTEMPT_0.md`](AKUMA_MULTIARCH_ATTEMPT_0.md) "Slice 6: threads";
[`BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md`](BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md) for why a row is
reclaimed late.
