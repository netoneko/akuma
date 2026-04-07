# Process Table Architecture

## Date

2026-04-07

## Overview

The process table manages all user processes in Akuma. It maps PIDs to Process
structs and provides lookup, iteration, and lifecycle management.

## Data Structure

```
PROCESS_TABLE: RwSpinlock<BTreeMap<Pid, Arc<Spinlock<Process>>>>
```

- **Outer lock**: `RwSpinlock` (reader-writer spinlock). Readers (lookups,
  iterations) proceed concurrently; only insert/remove takes the write lock.
- **Inner lock**: Each Process is wrapped in `Arc<Spinlock<Process>>` for
  per-process mutual exclusion. Multiple threads can hold `Arc` handles to
  different processes simultaneously without contending on the table lock.
- **Ownership**: The table holds one `Arc` reference per process. When
  `unregister_process` removes it, the `Arc` is returned to the caller. When
  the last `Arc` drops, `Process::drop()` runs, freeing the `UserAddressSpace`
  and all physical pages.

### Supporting Tables

- `THREAD_PID_MAP: Spinlock<BTreeMap<usize, Pid>>` -- Maps kernel thread IDs to
  PIDs for `CLONE_THREAD` children (they share the parent's ProcessInfo page).
- `LAZY_REGION_TABLE: Spinlock<BTreeMap<Pid, BTreeMap<usize, LazyRegion>>>` --
  Demand-paged VM regions, keyed by address-space owner PID. Separate from
  Process to avoid aliasing issues.

### Files

- `crates/akuma-exec/src/process/table.rs` -- Table statics, register/unregister/get
- `crates/akuma-exec/src/process/children.rs` -- lookup_process, current_process,
  list_processes, get_current_process
- `crates/akuma-exec/src/sync.rs` -- RwSpinlock implementation

## API

### New (safe) API

```rust
// Returns Arc handle; caller locks per-process as needed
get_process(pid: Pid) -> Option<Arc<Spinlock<Process>>>
get_current_process() -> Option<Arc<Spinlock<Process>>>
```

### Backward-compatible shims

```rust
// Returns &'static mut Process via data_ptr() -- same unsafety as before.
// These exist so 218+ existing call sites don't need changes yet.
lookup_process(pid: Pid) -> Option<&'static mut Process>
current_process() -> Option<&'static mut Process>
```

The shims bypass the per-process `Spinlock` via `data_ptr()` to match the old
behavior. They are transitional -- new code should use `get_process()`.

### Table operations

```rust
register_process(pid: Pid, proc: Box<Process>)  // wraps in Arc<Spinlock<>>
unregister_process(pid: Pid) -> Option<Arc<Spinlock<Process>>>
```

## Lock Ordering

```
IRQ-disable (outermost)
  > PROCESS_TABLE RwSpinlock (read or write)
    > Per-process Spinlock<Process>
      > SharedFdTable lock
      > SharedSignalTable lock
      > fault_mutex
      > terminal_state lock
    > LAZY_REGION_TABLE
    > FUTEX_WAITERS
    > THREAD_PID_MAP
    > CHILD_CHANNELS
```

**Critical invariant**: Never hold PROCESS_TABLE while calling
`schedule_blocking()`. The get-Arc-then-release pattern preserves this.

## Diagnostics

### Lock-hold-time tracking (`diag.rs`)

- `lock_timer_start()` / `lock_timer_end(caller, t0)` -- wraps lock sites
- Logs `[PTLOCK] {caller}: held {elapsed}us` when held > 100us
- Compile-time gated: `LOCK_TIMING_ENABLED`

### Borrow-aliasing detector (`diag.rs`)

- `borrow_inc(pid)` injected into `lookup_process()` -- monotonic counter
- Logs `[BORROW-ALIAS] pid={} count={}` when same PID has 2+ concurrent lookups
- `BorrowGuard` RAII type for new code that wants paired inc/dec
- Compile-time gated: `BORROW_TRACKING_ENABLED`

### Futex compliance logging (`config.rs` + `sync.rs`)

- `FUTEX_DBG_ENABLED: bool` in `src/config.rs` (default false)
- Traces `[futex-dbg] WAIT/WOKE/WAKE/REQUEUE` with tid, addr, timestamps
- Zero cost when disabled (const-false branch elimination)

## History

### Before (single Spinlock)

```
PROCESS_TABLE: Spinlock<BTreeMap<Pid, Box<Process>>>
```

- `lookup_process()` locked table, extracted `*mut Process` via unsafe cast,
  released table, returned `&'static mut Process`
- `list_processes()` held lock while cloning `String`/`Vec<String>` for every
  process -- caused `ps` hangs under load
- No reader concurrency: every lookup, every iteration, every register/unregister
  contended on the same exclusive lock

### After (RwSpinlock + per-process Arc)

- Readers proceed concurrently (all syscall handlers doing `lookup_process`)
- Writers only needed for register/unregister (fork, exit)
- `list_processes()` collects PIDs under read lock, builds ProcessInfo2 outside
- Per-process `Spinlock` enables future call-site migration away from the
  unsafe `&'static mut` shim

### Future: Stage C (lock-free array)

Replace `RwSpinlock<BTreeMap>` with fixed-size atomic arrays:
```
SLOT_STATES: [AtomicU8; MAX_PROCESSES]
PROCESS_SLOTS: [AtomicPtr<Spinlock<Process>>; MAX_PROCESSES]
```

Migration from B to C is mechanical: only `table.rs` changes. Per-process
`Arc<Spinlock<Process>>` and all call sites remain unchanged. See
`proposals/FIX_PROCESS_TABLE.md` for the full design.

## Tests

### Deadlock fix: Writer-priority RwSpinlock (2026-04-07)

The initial RwSpinlock implementation had no writer priority: `lock_exclusive`
only succeeded when state == 0 (no readers). Under Go's combined_stress workload
(150+ goroutine threads doing continuous syscalls), readers could continuously
arrive and prevent the writer (fork's `register_process`) from ever acquiring
the lock. This caused the second forktest run to hang at "Launching child 1...".

**Fix:** Redesigned state encoding with writer-priority bit:
- Bit 31 (`WRITER_BIT` = `0x8000_0000`): writer pending or active
- Bits 0-30: reader count

`lock_exclusive` now has two phases:
1. Set `WRITER_BIT` via `fetch_or` — this immediately blocks new readers
2. Wait for existing readers to drain (spin until state == `WRITER_BIT`)

`lock_shared` checks `WRITER_BIT` first — if set, spins instead of acquiring.

Added spin-count diagnostic: if write lock spins > 10M iterations, prints
`[RWLOCK] write lock stuck: state={:#x} readers={} writer_bit={}` to help
debug future deadlocks.

### Host-level (`cargo test`)

In `crates/akuma-exec/src/sync.rs` (11 tests):
- `rwspinlock_read_then_write` -- basic read/write lifecycle
- `rwspinlock_multiple_readers` -- 3 concurrent readers
- `rwspinlock_try_write_fails_while_read_held` -- exclusion verification
- `rwspinlock_try_read_fails_while_write_held` -- exclusion verification
- `rwspinlock_try_write_fails_while_write_held` -- writer exclusion
- `rwspinlock_write_after_readers_drop` -- write succeeds after readers release
- `rwspinlock_read_after_write_drops` -- read succeeds after writer releases
- `rwspinlock_with_btreemap` -- integration with BTreeMap (mirrors table usage)
- `rwspinlock_state_encoding_writer_priority` -- raw AtomicU32 state transitions with writer bit
- `rwspinlock_try_read_blocked_by_pending_writer` -- readers blocked when writer pending
- `rwspinlock_writer_priority_blocks_new_readers` -- writer priority integration test

### Kernel-level (boot tests in `process_tests.rs`)

- `list_processes_does_not_hold_lock_during_clone` -- two-phase list works
- `rwspinlock_table_concurrent_reads` -- two read locks simultaneously
- `process_table_register_get_unregister` -- Arc lifecycle: register, get, clone, unregister
- `lookup_process_shim_returns_valid_ref` -- backward-compat shim works
- `borrow_tracker_increments` -- diag borrow counter fires on lookup
- `get_current_process_returns_arc` -- None in kernel context (no user process)
