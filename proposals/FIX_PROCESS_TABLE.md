# Process Table Locking Refactor: D -> B -> C

## Date

2026-04-07

## Context

Recurring `ps` hangs and locking bugs stem from `PROCESS_TABLE` being a single `Spinlock<BTreeMap<Pid, Box<Process>>>` that is held during heap allocations (String/Vec clones in `list_processes()`), with no lock ordering enforcement. Additionally, `lookup_process()` returns `&'static mut Process` via unsafe pointer escape after releasing the lock, creating aliasing UB potential across 218+ call sites. This plan implements three stages: D (immediate instrumentation + fix), B (structural refactor to RwLock + per-process locks), and C (lock-free array, future work).

**We implement D and B now. C is documented for later.**

---

## Stage D: Targeted Instrumentation + Minimal Fixes

### D1. Fix `list_processes()` to not allocate under lock

**File:** `crates/akuma-exec/src/process/children.rs:543-569`

Rewrite to two-phase: collect PIDs under lock, then build ProcessInfo2 per-PID outside the lock.

```rust
pub fn list_processes() -> Vec<ProcessInfo2> {
    // Phase 1: collect PIDs under lock (single ~256 byte Vec<u32> allocation)
    let pids: Vec<Pid> = with_irqs_disabled(|| {
        PROCESS_TABLE.lock().keys().copied().collect()
    });
    // Phase 2: build info outside the lock
    let mut result = Vec::with_capacity(pids.len());
    for pid in pids {
        if let Some(proc) = lookup_process(pid) {
            result.push(ProcessInfo2 {
                pid, ppid: proc.parent_pid, box_id: proc.box_id,
                name: proc.name.clone(), state: ..., args: proc.args.clone(),
                current_syscall: proc.current_syscall.load(Relaxed),
                last_syscall: proc.last_syscall.load(Relaxed),
            });
        }
    }
    result
}
```

### D2. Lock-hold-time tracking

**New file:** `crates/akuma-exec/src/process/diag.rs`

- `const LOCK_TIMING_ENABLED: bool = true;` (compile-time gate)
- `const LOCK_HOLD_THRESHOLD_US: u64 = 100;`
- `with_process_table_timed(caller: &str, f: F) -> T` — wraps `PROCESS_TABLE.lock()` with `(runtime().uptime_us)()` before/after, logs `[PTLOCK] {caller}: held {elapsed}us` if over threshold
- Migrate high-value call sites: `list_processes`, `lookup_process`, `register_process`, `unregister_process`

### D3. Borrow-aliasing detector

**Same file:** `crates/akuma-exec/src/process/diag.rs`

- `static BORROW_COUNTS: [AtomicU32; 256]` — per-PID outstanding borrow counter
- `const BORROW_TRACKING_ENABLED: bool = true;`
- `borrow_inc(pid)` / `borrow_dec(pid)` — atomic inc/dec, log `[BORROW-ALIAS] pid={} count={}` if count reaches 2+
- Inject `borrow_inc` into `lookup_process()`. Counter monotonically increases (no dec at existing call sites). This detects concurrent/overlapping lookups to same PID.
- New `lookup_process_tracked(pid) -> Option<(&'static mut Process, BorrowGuard)>` with RAII dec — use for new code going forward

### D4. Futex compliance logging

**File:** `src/config.rs` — add `pub const FUTEX_DBG_ENABLED: bool = false;`
**File:** `src/syscall/sync.rs` — add const-gated `[futex-dbg]` traces at:
- FUTEX_WAIT entry: `tid, tgid, addr, val, timestamp`
- FUTEX_WAIT exit: `tid, addr, result (0/EAGAIN/EINTR/ETIMEDOUT), timestamp`
- FUTEX_WAKE: `addr, max, woken_count, timestamp`
- FUTEX_REQUEUE: `addr, addr2, woken, requeued, timestamp`

Zero cost when `const false` (LLVM eliminates branches).

### D5. Add `pub mod diag` to module tree

**File:** `crates/akuma-exec/src/process/mod.rs` — add `pub mod diag;`

### Files changed (Stage D)

| File | Change |
|------|--------|
| `crates/akuma-exec/src/process/diag.rs` | **NEW** — lock timing, borrow tracker |
| `crates/akuma-exec/src/process/mod.rs` | Add `pub mod diag;` |
| `crates/akuma-exec/src/process/children.rs` | Rewrite `list_processes()`, inject `borrow_inc` into `lookup_process()` |
| `crates/akuma-exec/src/process/table.rs` | Wire `register_process`/`unregister_process` through `with_process_table_timed` |
| `src/config.rs` | Add `FUTEX_DBG_ENABLED` const |
| `src/syscall/sync.rs` | Add futex trace logging |

---

## Stage B: RwSpinlock + Per-Process Arc<Spinlock<Process>>

### B1. Implement RwSpinlock

**New file:** `crates/akuma-exec/src/sync.rs`

`lock_api` is already in the dependency tree (via `spinning_top`). Implement `RawRwSpinlock` (~50 lines) using `AtomicU32`:
- 0 = unlocked, u32::MAX = write-locked, 1..MAX-1 = reader count
- `lock_shared`: CAS loop increment if < MAX-1
- `lock_exclusive`: CAS 0 -> MAX
- Implement `lock_api::RawRwLock` trait

Export:
```rust
pub type RwSpinlock<T> = lock_api::RwLock<RawRwSpinlock, T>;
pub type RwSpinlockReadGuard<'a, T> = lock_api::RwLockReadGuard<'a, RawRwSpinlock, T>;
pub type RwSpinlockWriteGuard<'a, T> = lock_api::RwLockWriteGuard<'a, RawRwSpinlock, T>;
```

**File:** `crates/akuma-exec/src/lib.rs` — add `pub mod sync;`

Add `lock_api = "0.4"` as explicit dependency in `crates/akuma-exec/Cargo.toml`.

### B2. Change PROCESS_TABLE type

**File:** `crates/akuma-exec/src/process/table.rs`

```rust
// Before:
pub static PROCESS_TABLE: Spinlock<BTreeMap<Pid, Box<Process>>> = ...;

// After:
pub static PROCESS_TABLE: RwSpinlock<BTreeMap<Pid, Arc<Spinlock<Process>>>> = ...;
```

Update functions:
- `register_process(pid, proc: Box<Process>)` — `Arc::new(Spinlock::new(*proc))`, use `.write()` for insert
- `unregister_process(pid) -> Option<Arc<Spinlock<Process>>>` — use `.write()` for remove
- **New** `get_process(pid) -> Option<Arc<Spinlock<Process>>>` — use `.read()`, clone Arc

### B3. Add new safe API + backward-compatible shims

**File:** `crates/akuma-exec/src/process/children.rs`

New functions:
```rust
pub fn get_process(pid: Pid) -> Option<Arc<Spinlock<Process>>> {
    with_irqs_disabled(|| { PROCESS_TABLE.read().get(&pid).cloned() })
}

pub fn get_current_process() -> Option<Arc<Spinlock<Process>>> {
    let tid = crate::threading::current_thread_id();
    let thread_pid = with_irqs_disabled(|| { THREAD_PID_MAP.lock().get(&tid).copied() });
    if let Some(pid) = thread_pid { return get_process(pid); }
    let pid = read_current_pid()?;
    get_process(pid)
}
```

Shims (preserving old API for all 218+ call sites):
```rust
pub fn lookup_process(pid: Pid) -> Option<&'static mut Process> {
    let arc = get_process(pid)?;
    // SAFETY: Same unsafety as before. Arc keeps Process alive while in table.
    // Per-process Spinlock bypassed (not locked) to match old behavior.
    // This is a transitional shim — callers should migrate to get_process().
    let ptr = Spinlock::data_ptr(&*arc);
    Some(unsafe { &mut *ptr })
}

pub fn current_process() -> Option<&'static mut Process> {
    // ... same logic, delegates to lookup_process shim
}
```

Key: `Spinlock::data_ptr(&self) -> *mut T` is provided by `lock_api::Mutex` and returns the raw data pointer without acquiring the lock. This is identical in safety to the current code.

### B4. Update `list_processes()` for new table type

Still uses two-phase from Stage D, but phase 1 now uses `.read()`:
```rust
let pids: Vec<Pid> = with_irqs_disabled(|| {
    PROCESS_TABLE.read().keys().copied().collect()
});
```

### B5. Update direct `PROCESS_TABLE.lock()` call sites

These 23 call sites must change from `.lock()` to `.read()` or `.write()`:

| File | Call sites | Change |
|------|-----------|--------|
| `crates/akuma-exec/src/process/signal.rs` | `kill_process`, `kill_thread_group` iteration | `.read()` |
| `crates/akuma-exec/src/process/mod.rs` | `kill_box`, `return_to_kernel` child/sibling scans | `.read()` |
| `crates/akuma-exec/src/process/stats.rs` | `dump_running_process_stats` | `.read()` |
| `crates/akuma-exec/src/process/children.rs` | `list_processes`, `find_pid_by_thread` | `.read()` |
| `src/syscall/proc.rs:1167` | wait4 zombie scan | `.read()` |
| `src/process_tests.rs:3037` | test code | `.read()` |

For iteration patterns that currently do `table.get_mut(&pid).map(|p| ...)`, these become `table.get(&pid).map(|arc| arc.lock())` under read lock.

### B6. Update test files

**Files:** `src/process_tests.rs`, `src/tests.rs`

Tests that create `Box<Process>` and insert into table need to wrap in `Arc<Spinlock<>>`. Tests that read from table need `.read()` and `.lock()` on the arc.

### B7. Lock ordering (documented, not enforced yet)

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

Critical invariant: **Never hold PROCESS_TABLE while calling `schedule_blocking()`**. The shim's get-Arc-then-release pattern preserves this.

### Files changed (Stage B)

| File | Change |
|------|--------|
| `crates/akuma-exec/src/sync.rs` | **NEW** — RwSpinlock implementation |
| `crates/akuma-exec/src/lib.rs` | Add `pub mod sync;` |
| `crates/akuma-exec/Cargo.toml` | Add `lock_api = "0.4"` |
| `crates/akuma-exec/src/process/table.rs` | Change table type, update register/unregister, add `get_process` |
| `crates/akuma-exec/src/process/children.rs` | Add `get_process`/`get_current_process`, rewrite shims, update iterations |
| `crates/akuma-exec/src/process/signal.rs` | `.lock()` -> `.read()` |
| `crates/akuma-exec/src/process/mod.rs` | `.lock()` -> `.read()` / `.write()` |
| `crates/akuma-exec/src/process/stats.rs` | `.lock()` -> `.read()` |
| `src/syscall/proc.rs` | `.lock()` -> `.read()` at line 1167 |
| `src/process_tests.rs` | Update for new table type |
| `src/tests.rs` | Update for new table type |

---

## Stage C: Lock-Free Process Array (future, not implemented now)

Replace `RwSpinlock<BTreeMap<Pid, Arc<Spinlock<Process>>>>` with fixed-size atomic arrays:

```rust
const MAX_PROCESSES: usize = 64;
static SLOT_STATES: [AtomicU8; MAX_PROCESSES] = ...;  // Free/Active
static PROCESS_SLOTS: [AtomicPtr<Spinlock<Process>>; MAX_PROCESSES] = ...;
```

- `allocate_pid()`: CAS loop on SLOT_STATES (same as thread pool's `claim_free_slot`)
- `register_process`: `Box::into_raw` -> `AtomicPtr::store`
- `lookup_process`: `AtomicPtr::load` -> dereference (lock-free read, no table lock)
- `list_processes`: iterate 64 slots, check SLOT_STATES (lock-free scan)
- `unregister_process`: swap pointer to null, reconstruct Box for drop

No epoch-based reclamation needed: unregister only runs after thread exit (no concurrent readers for dead process). Arc from Stage B provides safe reclamation if SMP added later.

Migration from B to C was mechanical: only `table.rs` and `children.rs` changed.
All 218+ `lookup_process` call sites remained unchanged.

**Decision:** Sequential PIDs (NEXT_PID monotonic, never recycled) with linear
scan for PID-to-slot lookup. 256 slots, ~2us worst-case scan on ARM64.

---

## What Actually Happened (2026-04-07)

Stage B was implemented but caused two deadlock classes:
1. Writer starvation in the RwSpinlock (no writer priority)
2. Per-process Spinlock vs data_ptr() shim mismatch during iteration

Stage B was reverted in favor of Stage C (atomic array). Stage C itself
went through three iterations:

1. **Pure lock-free** (no IRQ protection): SIGSEGV at PC=0x20000000 from
   use-after-free — preemption between atomic load and dereference let
   `unregister_process` free the Process on another thread.

2. **Full-scan IRQ protection** (`with_irqs_disabled` around entire scan):
   Children stuck as "running" after SIGKILL — `Vec::push` heap allocation
   inside IRQ-disabled callback deadlocked the allocator.

3. **Per-slot IRQ protection + stack buffer** (current): `for_each_process`
   and `find_process` run callbacks with IRQs disabled (MUST NOT allocate).
   `collect_pids` scans into `[u32; 256]` stack buffer with IRQs disabled,
   copies to Vec with IRQs enabled. Two-phase pattern for `list_processes`.

The RwSpinlock primitive was kept in `sync.rs` for future use.
Stage D instrumentation (diag.rs, futex logging) was kept.

## Implementation Order (as executed)

1. **D1** — Fix `list_processes()` (immediate `ps` hang fix)
2. **D2-D5** — Instrumentation (diag module, lock timing, borrow tracker, futex logging)
3. **B1** — RwSpinlock primitive with writer priority (kept in sync.rs)
4. **B2-B5** — RwSpinlock + Arc<Spinlock<Process>> (caused deadlocks, reverted)
5. **C** — Atomic `[AtomicPtr<Process>; 256]` array with per-slot IRQ protection
6. **Bug #31** — `kill_process_with_signal` missing child channel notify (wait4 hang)

## Verification

1. `cargo check` after each stage
2. `cargo test --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)` for host-runnable crate tests
3. `cargo run --release` — boot in QEMU, verify:
   - `ps` does not hang
   - Run `forktest_parent -duration 10s` — all children exit cleanly
   - Check serial output for `[PTLOCK]` slow-lock warnings
   - Check serial output for `[BORROW-ALIAS]` aliasing warnings
   - Enable `FUTEX_DBG_ENABLED = true`, run Go binary, verify futex traces appear
4. After Stage B: verify no regression in all above tests
