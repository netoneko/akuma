# Process Table Architecture

## Date

2026-04-07

## Overview

The process table manages all user processes in Akuma. It uses a lock-free
fixed-size array of atomic pointers for zero-contention reads.

## Data Structure

```
SLOT_STATES:   [AtomicU8; 256]        // FREE=0 or ACTIVE=1
PROCESS_SLOTS: [AtomicPtr<Process>; 256]  // raw heap pointers (from Box::into_raw)
```

- **No locks for reads**: `lookup_process`, `list_processes`, `find_pid_by_thread`
  all scan the array with atomic loads. No spinlock, no RwLock, no IRQ disabling.
- **CAS for writes**: `register_process` claims a slot via `compare_exchange` on
  SLOT_STATES. `unregister_process` swaps the pointer to null and marks FREE.
- **Ownership**: Processes are heap-allocated via `Box`. `register_process` takes
  ownership via `Box::into_raw`. `unregister_process` returns `Box::from_raw`.
  The Box drop triggers `UserAddressSpace::drop()` to free all physical pages.
- **PID scheme**: PIDs are monotonically increasing (from `NEXT_PID: AtomicU32`),
  never recycled. Slots are recycled. PID-to-slot lookup is a linear scan of
  256 entries (~2us worst case on ARM64).

### Iteration API

```rust
for_each_process(|p: &Process| { ... })     // visit all active processes
find_process(|p| -> Option<T>) -> Option<T>  // find first matching
collect_pids(|p| -> bool) -> Vec<Pid>        // collect PIDs by predicate
get_process_ptr(pid) -> Option<*mut Process> // single PID lookup
```

All are lock-free. No `with_irqs_disabled` needed for reads.

### Supporting Tables

- `THREAD_PID_MAP: Spinlock<BTreeMap<usize, Pid>>` -- Maps kernel thread IDs to
  PIDs for `CLONE_THREAD` children (they share the parent's ProcessInfo page).
- `LAZY_REGION_TABLE: Spinlock<BTreeMap<Pid, BTreeMap<usize, LazyRegion>>>` --
  Demand-paged VM regions, keyed by address-space owner PID.

### Files

- `crates/akuma-exec/src/process/table.rs` -- Lock-free array, register/unregister/iteration
- `crates/akuma-exec/src/process/children.rs` -- lookup_process, current_process, list_processes
- `crates/akuma-exec/src/sync.rs` -- RwSpinlock (kept for future use, no longer used by table)

## API

```rust
// Lock-free lookup (returns raw pointer, valid while process is registered)
lookup_process(pid) -> Option<&'static mut Process>
current_process() -> Option<&'static mut Process>

// Table operations (CAS-based, no locks)
register_process(pid: Pid, proc: Box<Process>)
unregister_process(pid: Pid) -> Option<Box<Process>>
```

## History

### Stage D (instrumentation)

- Added `diag.rs` with lock-hold-time tracking and borrow-aliasing detector
- Added `FUTEX_DBG_ENABLED` const-gated futex trace logging

### Stage B (RwSpinlock + Arc<Spinlock<Process>>) — reverted

Changed table to `RwSpinlock<BTreeMap<Pid, Arc<Spinlock<Process>>>>`. This
introduced two deadlock classes:

1. **Writer starvation**: The initial RwSpinlock had no writer priority. Under
   Go's combined_stress (150+ goroutine threads), readers continuously prevented
   the writer (fork's register_process) from acquiring the lock. Fixed by adding
   writer-priority bit, but revealed the second issue.

2. **Per-process Spinlock deadlock**: Iteration code called `proc_arc.lock()` on
   each process while holding the table read lock. The `lookup_process` shim used
   `data_ptr()` to bypass the per-process lock. This mismatch caused hangs when
   iteration and shim-based access interacted through the scheduler.

### Stage C (lock-free array) — current

Replaced the entire `RwSpinlock<BTreeMap<Pid, Arc<Spinlock<Process>>>>` with
a fixed-size `[AtomicPtr<Process>; 256]` array. Zero locks for reads, CAS for
writes. Back to `Box<Process>` ownership (no Arc, no per-process Spinlock).

This matches the thread pool pattern (`THREAD_STATES: [AtomicU8; MAX_THREADS]`)
already proven in the codebase.

## Diagnostics

### Lock-hold-time tracking (`diag.rs`)

- `lock_timer_start()` / `lock_timer_end(caller, t0)`
- Logs `[PTLOCK] {caller}: held {elapsed}us` when held > 100us
- Compile-time gated: `LOCK_TIMING_ENABLED`

### Borrow-aliasing detector (`diag.rs`)

- `borrow_inc(pid)` injected into `lookup_process()` — monotonic counter
- Logs `[BORROW-ALIAS] pid={} count={}` when same PID has 2+ concurrent lookups
- Compile-time gated: `BORROW_TRACKING_ENABLED`

### Futex compliance logging (`config.rs` + `sync.rs`)

- `FUTEX_DBG_ENABLED: bool` in `src/config.rs` (default false)
- Traces `[futex-dbg] WAIT/WOKE/WAKE/REQUEUE` with tid, addr, timestamps

## Tests

### Host-level (`cargo test`, 93 tests in akuma-exec)

In `crates/akuma-exec/src/sync.rs` (11 tests):
- RwSpinlock read/write lifecycle, multiple readers, exclusion, writer priority,
  state encoding, BTreeMap integration

### Kernel-level (boot tests in `process_tests.rs`, 8 tests)

- `list_processes_does_not_hold_lock_during_clone` -- list works after refactor
- `lock_free_table_concurrent_reads` -- simultaneous lookups succeed
- `process_table_register_get_unregister` -- register/lookup/unregister lifecycle
- `lookup_process_shim_returns_valid_ref` -- lookup returns usable &mut Process
- `borrow_tracker_increments` -- diag borrow counter fires on lookup
- `current_process_none_in_kernel_ctx` -- returns None without user context
- `lock_free_iteration` -- for_each_process, find_process, collect_pids
- `slot_recycling` -- slots are reused after unregister
