# Process Table Architecture

## Date

2026-04-07

## Overview

The process table manages all user processes in Akuma. It uses a fixed-size
array of atomic pointers with per-slot IRQ-disabled reads for safe pointer
dereference on single-core.

## Data Structure

```
MAX_PROCESSES = 256

SLOT_STATES:   [AtomicU8; 256]            // FREE=0 or ACTIVE=1
PROCESS_SLOTS: [AtomicPtr<Process>; 256]   // raw heap pointers (from Box::into_raw)
```

- **Reads use `with_irqs_disabled`**: `for_each_process`, `find_process`,
  `collect_pids`, `get_process_ptr` all wrap slot reads in `with_irqs_disabled`
  to prevent preemption between atomic load and pointer dereference. Without
  this, `unregister_process` on another thread (via timer interrupt + context
  switch) could free the Process between load and use (use-after-free). This
  caused a SIGSEGV at PC=0x20000000 during testing.
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
for_each_process(|p: &Process| { ... })     // visit all, IRQs disabled — MUST NOT allocate
find_process(|p| -> Option<T>) -> Option<T>  // find first, IRQs disabled — MUST NOT allocate
collect_pids(|p| -> bool) -> Vec<Pid>        // scan with IRQs disabled into stack buf, then Vec
collect_process_info(|p| -> Option<T>) -> Vec<T>  // generic version of collect_pids
get_process_ptr(pid) -> Option<*mut Process> // single PID lookup, IRQs disabled
```

**Critical rule**: `for_each_process` and `find_process` run their callback
with IRQs disabled. Callbacks MUST NOT allocate on the heap. For iteration
that needs allocation, use `collect_pids` (scans into a `[u32; 256]` stack
buffer with IRQs disabled, then copies to Vec with IRQs enabled) or
`collect_process_info` (same pattern, generic type).

### Two-Phase Pattern

Functions like `list_processes` that need to clone Strings from Process fields
use a two-phase approach:

1. `collect_pids(|_| true)` — collect all PIDs (IRQs disabled, stack buffer)
2. For each PID, `lookup_process(pid)` — dereference individually (brief IRQ disable)
3. Clone fields with IRQs enabled (safe to allocate)

### Supporting Tables

- `THREAD_PID_MAP: Spinlock<BTreeMap<usize, Pid>>` — Maps kernel thread IDs to
  PIDs for `CLONE_THREAD` children (they share the parent's ProcessInfo page).
- `LAZY_REGION_TABLE: Spinlock<BTreeMap<Pid, BTreeMap<usize, LazyRegion>>>` —
  Demand-paged VM regions, keyed by address-space owner PID.

### Files

- `crates/akuma-exec/src/process/table.rs` — Array, register/unregister/iteration
- `crates/akuma-exec/src/process/children.rs` — lookup_process, current_process, list_processes
- `crates/akuma-exec/src/sync.rs` — RwSpinlock (kept for future use, not used by table)

## API

```rust
// Lookup (IRQs disabled during scan, returns raw pointer)
lookup_process(pid) -> Option<&'static mut Process>
current_process() -> Option<&'static mut Process>

// Table operations
register_process(pid: Pid, proc: Box<Process>)   // CAS to claim slot
unregister_process(pid: Pid) -> Option<Box<Process>>  // swap to null
```

## History

### Stage D (instrumentation)

- Added `diag.rs` with lock-hold-time tracking and borrow-aliasing detector
- Added `FUTEX_DBG_ENABLED` const-gated futex trace logging
- Fixed `list_processes()` to two-phase (collect PIDs, then build info)

### Stage B (RwSpinlock + Arc<Spinlock<Process>>) — reverted

Changed table to `RwSpinlock<BTreeMap<Pid, Arc<Spinlock<Process>>>>`. This
introduced three issues:

1. **Writer starvation**: The initial RwSpinlock had no writer priority. Under
   Go's combined_stress (150+ goroutine threads), readers continuously prevented
   the writer (fork's register_process) from acquiring the lock. Fixed by adding
   writer-priority bit.

2. **Per-process Spinlock deadlock**: Iteration code called `proc_arc.lock()` on
   each process while holding the table read lock. The `lookup_process` shim used
   `data_ptr()` to bypass the per-process lock. This mismatch caused hangs.

3. **SIGSEGV from Arc drop**: Goroutine threads exiting caused the parent's
   Process struct to be freed mid-execution (Arc refcount dropped to 0).

### Stage C (atomic array) — current

Replaced the entire `RwSpinlock<BTreeMap<Pid, Arc<Spinlock<Process>>>>` with
a fixed-size `[AtomicPtr<Process>; 256]` array. Back to `Box<Process>` ownership
(no Arc, no per-process Spinlock). Matches the thread pool pattern
(`THREAD_STATES: [AtomicU8; MAX_THREADS]`).

**Use-after-free fix**: Initial lock-free implementation had no IRQ protection
during reads. On single-core with preemptive scheduling, a timer interrupt
between the atomic pointer load and the dereference allowed `unregister_process`
to free the Process on another thread, causing SIGSEGV at PC=0x20000000.
Fixed by wrapping reads in `with_irqs_disabled`.

**Allocation-under-IRQ-disable**: Wrapping the entire scan callback in
`with_irqs_disabled` caused `Vec::push` (heap allocation) to run with IRQs
disabled, which can stall or deadlock the allocator. Fixed by using
stack-buffer collection (`collect_pids` with `[u32; 256]`) for predicates,
and the two-phase pattern for anything that needs heap allocation.

## Diagnostics

### Lock-hold-time tracking (`diag.rs`)

- `lock_timer_start()` / `lock_timer_end(caller, t0)`
- Logs `[PTLOCK] {caller}: held {elapsed}us` when held > 100us
- Compile-time gated: `LOCK_TIMING_ENABLED`

### Borrow-aliasing detector (`diag.rs`)

- `borrow_inc(pid)` injected into `lookup_process()` — monotonic counter
- Logs `[BORROW-ALIAS] pid={} count={}` when same PID has 2+ concurrent lookups
- Compile-time gated: `BORROW_TRACKING_ENABLED`

### Futex compliance logging (`config.rs` + `syscall/sync.rs`)

- `FUTEX_DBG_ENABLED: bool` in `src/config.rs` (default false)
- Traces `[futex-dbg] WAIT/WOKE/WAKE/REQUEUE` with tid, addr, timestamps

### RwSpinlock write-lock stuck diagnostic (`sync.rs`)

- Logs `[RWLOCK] write lock stuck: state={:#x}` after 10M spin iterations
- Helps debug deadlocks if the RwSpinlock is used in the future

## Tests

### Host-level (`cargo test`, 93 tests in akuma-exec)

In `crates/akuma-exec/src/sync.rs` (11 tests):
- RwSpinlock read/write lifecycle, multiple readers, exclusion, writer priority,
  state encoding, BTreeMap integration

### Kernel-level (boot tests in `process_tests.rs`, 9 tests)

- `list_processes_does_not_hold_lock_during_clone` — two-phase list works
- `lock_free_table_concurrent_reads` — simultaneous lookups succeed
- `process_table_register_get_unregister` — register/lookup/unregister lifecycle
- `lookup_process_shim_returns_valid_ref` — lookup returns usable &mut Process
- `borrow_tracker_increments` — diag borrow counter fires on lookup
- `current_process_none_in_kernel_ctx` — returns None without user context
- `lock_free_iteration` — for_each_process, find_process, collect_pids
- `slot_recycling` — slots are reused after unregister
- `kill_process_notifies_child_channel` — kill path notifies CHILD_CHANNELS for wait4
