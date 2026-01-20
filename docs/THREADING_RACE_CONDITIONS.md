# Threading Race Conditions and CURRENT_THREAD Analysis

This document analyzes potential race conditions, deadlocks, and the role of thread ID storage in the kernel's threading implementation.

## Status: FIXED

The issues documented below have been fixed. See "Changes Made" section at the end.

## Bug 5 Root Cause Analysis

### Symptoms
```
EC=0x25, ISS=0x4
ELR=0x40090618, FAR=0xfffffffffffffffd
SPSR=0x80000345 (correct EL1h)
Thread=0, only 1 thread exists
```
- `FAR=0xfffffffffffffffd` = -3 in signed representation (corrupted pointer)
- Crash very early in boot (37K allocations vs 3M+ in normal runs)
- Test output: `kthreads check: waiting (tid1=8 found=false, tid2=9 found=false)`
- SPSR is correct (previous fixes working), indicating a different bug

### Likely Root Cause: Dangling pool_ptr

In `sgi_scheduler_handler`, there's a use-after-lock-release pattern:

```rust
let (switch_info, pool_ptr) = {
    let mut pool = POOL.lock();              // Lock acquired
    let ptr = &mut *pool as *mut ThreadPool; // Pointer taken
    (pool.schedule_indices(voluntary), ptr)
};  // ← Lock RELEASED here

// ... later ...
let pool = &mut *pool_ptr;  // ← DANGLING POINTER ACCESS
```

The lock is released at the end of the block, but `pool_ptr` continues to be dereferenced. Although on single-CPU with IRQs masked this is technically "safe," it violates Rust's aliasing rules and could cause issues if assumptions change.

## Race Conditions Identified

### 1. Unsafe `POOL.data_ptr()` Without Lock

**Location**: `spawn_system_thread_fn` (line 1677), `spawn_user_thread_fn_internal` (line 1791)

```rust
let pool = unsafe { &mut *POOL.data_ptr() };
```

**Problem**: 
- No lock is held while modifying `pool.slots[slot_idx]`
- The scheduler reads slots while holding the lock
- Comment claims "We own this slot (it's INITIALIZING)" but the invariant is fragile

**Risk**: If `with_irqs_disabled` fails silently or an NMI occurs, corruption is possible.

### 2. CURRENT_THREAD vs pool.current_idx Desynchronization

**Location**: `schedule_indices` (lines 1168-1169)

```rust
CURRENT_THREAD.store(next_idx, Ordering::SeqCst);
self.current_idx = next_idx; // Keep in sync for context access
```

**Problem**: These two stores are not atomic together. Code observing state between them sees inconsistent values.

### 3. Context Switch Timing Window

**Sequence**:
1. `CURRENT_THREAD = new_thread` (atomic store)
2. `pool.current_idx = new_thread` (inside lock)
3. `switch_context(old, new)` (changes CPU state)

**Problem**: Before `switch_context` completes, `CURRENT_THREAD` already points to the new thread, but we're still on the **old thread's stack**. Any code reading `CURRENT_THREAD` during this window gets wrong information.

## Potential Deadlock Scenarios

### 1. POOL Lock + Allocator Lock (Previously Fixed)

**Bug 4 from CONTEXT_SWITCH_BUGS.md**: `cleanup_terminated_internal` acquired `POOL.lock()` without disabling IRQs. If a timer fired while holding the lock, the SGI handler would deadlock.

**Fix Applied**: Added `IrqGuard` around `POOL.lock()` acquisitions.

### 2. Nested Lock in Corruption Recovery (Safe But Fragile)

**Location**: `sgi_scheduler_handler`

```rust
let (switch_info, pool_ptr) = {
    let mut pool = POOL.lock();  // First lock (released at block end)
    // ...
};

// Later, in corruption recovery:
if new_idx < 8 && !is_new_thread && is_user_spsr {
    let mut pool = POOL.lock();  // Second lock (OK - first was released)
    // ...
}
```

This is **not a deadlock** because the first lock is released before the second is acquired. However, the pattern is fragile and easy to break during refactoring.

## CURRENT_THREAD Analysis

### Current Usage

| Location | Purpose |
|----------|---------|
| `schedule_indices()` | Know which thread is currently running |
| `disable_preemption()` | Access per-thread preemption counter |
| `enable_preemption()` | Access per-thread preemption counter |
| `is_preemption_disabled()` | Check thread-local preemption state |
| `check_preemption_watchdog()` | Monitor preemption duration |
| `cleanup_terminated_internal()` | Only allow cleanup from thread 0 |
| `mark_current_terminated()` | Mark current thread as terminated |
| `current_thread_id()` | Public API for current thread ID |

### Can We Eliminate CURRENT_THREAD?

**Short answer: No.**

**Reasons**:

1. **Lock-Free Access Required**: Many call sites (`current_thread_id()`, preemption checks) need lock-free access. Requiring `POOL.lock()` would introduce deadlocks in IRQ handlers.

2. **Performance**: `CURRENT_THREAD.load()` is a single atomic read. Acquiring a spinlock is much heavier.

3. **Circular Dependency**: The scheduler needs to know the current thread to make scheduling decisions, but accessing `pool.current_idx` requires the lock that the scheduler already holds.

### Alternative: CPU Register Storage

ARM64 provides `TPIDRRO_EL0` (Thread Pointer ID Register, Read-Only from EL0) which could store the current thread ID:

```rust
// On context switch:
unsafe { core::arch::asm!("msr tpidrro_el0, {}", in(reg) new_thread_id); }

// Fast read from anywhere:
pub fn current_thread_id() -> usize {
    let tid: usize;
    unsafe { core::arch::asm!("mrs {}, tpidrro_el0", out(reg) tid); }
    tid
}
```

**Benefits**:
- Eliminates atomic load overhead
- Hardware-guaranteed per-CPU storage
- No memory contention

**Consideration**: `TPIDR_EL1` is already used for exception stack pointers.

## Recommendations

### Priority 1: Fix Dangling pool_ptr (Critical)

Hold the lock during the entire context switch operation:

```rust
// In sgi_scheduler_handler
let switch_info = {
    let mut pool = POOL.lock();
    let info = pool.schedule_indices(voluntary);
    
    if let Some((old_idx, new_idx)) = info {
        // All pool access here while lock is held
        let (old_ptr, new_ptr) = pool.get_context_ptrs(old_idx, new_idx);
        set_current_exception_stack(pool.slots[new_idx].exception_stack_top);
        
        unsafe { switch_context(old_ptr, new_ptr); }
        
        // IRQs masked throughout, lock held until here
    }
    info
};
```

Since IRQs are already masked during SGI handling, holding the lock during `switch_context` is safe on single-CPU.

### Priority 2: Fix Unsafe POOL.data_ptr() Usage (High)

Replace `POOL.data_ptr()` with proper lock acquisition:

```rust
// Instead of:
let pool = unsafe { &mut *POOL.data_ptr() };

// Use:
let mut pool = POOL.lock();
```

The lock is cheap when there's no contention (we're already in `with_irqs_disabled`), and this eliminates the unsafe invariant.

### Priority 3: Move Context Storage Out of POOL (Medium-term)

Create a separate static array for contexts, similar to `THREAD_STATES`:

```rust
static THREAD_CONTEXTS: [UnsafeCell<Context>; MAX_THREADS] = ...;
```

**Benefits**:
- Context access without locking POOL
- Thread ID becomes truly independent of pool
- Reduces lock contention

### Priority 4: Consider CPU Register for Thread ID (Long-term)

Use `TPIDRRO_EL0` to store current thread ID:

```rust
// switch_context epilogue (in assembly):
msr tpidrro_el0, x_new_thread_id

// current_thread_id() implementation:
mrs x0, tpidrro_el0
```

This provides the fastest possible thread ID lookup.

## Summary Table

| Issue | Severity | Status | Recommendation |
|-------|----------|--------|----------------|
| Dangling `pool_ptr` after lock release | Critical | **FIXED** | Hold lock during switch_context |
| `POOL.data_ptr()` without lock | High | **FIXED** | Use `POOL.lock()` instead |
| `CURRENT_THREAD` timing race | Medium | **FIXED** | Use CPU register (TPIDRRO_EL0) |
| POOL + allocator deadlock | High | Fixed | IrqGuard around POOL.lock() |
| Can we remove `CURRENT_THREAD`? | N/A | **DONE** | Yes - replaced with CPU register |

## Files Affected

- `src/threading.rs`: Main threading implementation
- `src/exceptions.rs`: Exception handlers using thread state
- `src/config.rs`: Thread configuration constants

## Related Documentation

- `docs/CONTEXT_SWITCH_BUGS.md`: Previous context switch bug analysis
- `docs/LOCK_FREE_THREADING.md`: Lock-free thread state design

---

## Changes Made (Implementation)

The following changes were implemented to fix the documented issues:

### 1. Separate THREAD_CONTEXTS Array

Thread contexts are now stored in a separate static array outside of `ThreadPool`:

```rust
static THREAD_CONTEXTS: [SyncContext; config::MAX_THREADS] = { ... };
```

**Benefits:**
- Context access no longer requires POOL lock
- Eliminates dangling pointer issues
- Makes synchronization invariants explicit

### 2. CPU Register for Thread ID (TPIDRRO_EL0)

Current thread ID is now stored in the ARM64 `TPIDRRO_EL0` register:

```rust
fn set_current_thread_id(tid: usize) {
    unsafe { core::arch::asm!("msr tpidrro_el0, {}", in(reg) tid as u64); }
}

pub fn current_thread_id() -> usize {
    let tid: u64;
    unsafe { core::arch::asm!("mrs {}, tpidrro_el0", out(reg) tid); }
    tid as usize
}
```

**Benefits:**
- Zero-cost reads (single register read, no memory access)
- Hardware-guaranteed per-CPU storage
- No atomic operations needed
- `CURRENT_THREAD` atomic variable removed

### 3. Fixed sgi_scheduler_handler

The handler now copies all required data before releasing the POOL lock:

```rust
let switch_info = {
    let mut pool = POOL.lock();
    pool.schedule_indices(voluntary).map(|(old_idx, new_idx)| {
        // Copy out all metadata we need
        let old_stack_base = pool.stacks[old_idx].base;
        let new_stack_base = pool.stacks[new_idx].base;
        let new_tpidr = pool.slots[new_idx].exception_stack_top;
        (old_idx, new_idx, old_stack_base, new_stack_base, new_tpidr)
    })
};  // Lock released - but we have copies of everything
```

**Benefits:**
- No dangling pointers
- Lock is released before context switch
- All data is copied, not referenced

### 4. Removed context from ThreadSlot

The `context` field was removed from `ThreadSlot`:

```rust
pub struct ThreadSlot {
    // NOTE: context removed - use THREAD_CONTEXTS[idx] instead
    pub cooperative: bool,
    pub start_time_us: u64,
    pub timeout_us: u64,
    pub exception_stack_top: u64,
}
```

### 5. Safety Invariants

The new design enforces these invariants:

1. **Context access only when IRQs masked**: The scheduler (with IRQs masked) is the only code that modifies contexts during switch
2. **Context only accessed when thread not running**: A thread's context is only read/written when that thread is suspended
3. **Context initialized before READY**: The INITIALIZING state blocks the scheduler until context is fully set up
4. **Context zeroed on FREE**: Cleanup zeros the context before marking the slot as FREE
