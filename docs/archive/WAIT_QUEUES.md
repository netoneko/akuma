# Wait Queues and Blocking Syscalls

This document describes the wait queue implementation that allows syscalls like
`nanosleep` to yield instead of busy-waiting.

## Overview

Long-running syscalls (like `nanosleep`) previously blocked the entire system by
busy-waiting in the kernel. The new wait queue implementation allows threads to
yield during syscalls, enabling concurrent execution while processes sleep.

## Key Components

### 1. Per-Thread Exception Stacks

Each kernel thread has its own exception stack area for storing trap frames during
syscall handling. This allows safe context switching during syscalls because each
thread's user state is isolated.

**Stack Layout:**

```
|------------------| <- stack_top (highest address)
| Exception area   |  1KB (EXCEPTION_STACK_SIZE) for UserTrapFrame
|------------------|
| Kernel stack     |  Rest of stack for normal kernel code
|------------------| <- stack_base (lowest address)
```

**Exception Area Contents (1KB):**

| Offset from top | Size | Content |
|-----------------|------|---------|
| 0 to -280 | 280 bytes | UserTrapFrame (x0-x30, SP_EL0, ELR, SPSR, padding) |
| -280 to -512 | 232 bytes | Scratch space for handler |
| -512 to -1024 | 512 bytes | Reserved for nested IRQs |

### 2. WAITING Thread State

A new thread state `WAITING` indicates a thread is blocked waiting for a timer
or event:

```rust
pub mod thread_state {
    pub const FREE: u8 = 0;
    pub const READY: u8 = 1;
    pub const RUNNING: u8 = 2;
    pub const TERMINATED: u8 = 3;
    pub const INITIALIZING: u8 = 4;
    pub const WAITING: u8 = 5;  // Blocked on timer/event
}
```

WAITING threads are skipped by the scheduler until their wake condition is met.

### 3. Timer Queue

Each `ThreadSlot` has a `wake_time_us` field:

```rust
pub struct ThreadSlot {
    // ... existing fields ...
    pub wake_time_us: u64,        // When to wake (0 = not sleeping)
    pub exception_stack_top: u64, // Per-thread exception stack
}
```

The scheduler's `wake_sleeping_threads()` function checks all WAITING threads
and marks them READY when their wake time has elapsed.

### 4. schedule_blocking() Function

```rust
pub fn schedule_blocking(wake_time_us: u64) {
    let tid = current_thread_id();
    
    // Set wake time
    pool.slots[tid].wake_time_us = wake_time_us;
    
    // Mark as WAITING
    THREAD_STATES[tid].store(thread_state::WAITING, Ordering::SeqCst);
    
    // Yield - will return when woken
    yield_now();
}
```

## How It Works

1. **User calls `sleep()`** → SVC exception
2. **`sync_el0_handler`** saves user registers to per-thread exception stack
3. **`sys_nanosleep`** calculates deadline, calls `schedule_blocking(deadline)`
4. **`schedule_blocking`** sets `wake_time_us`, marks thread WAITING, yields
5. **Scheduler** switches to another READY thread
6. **Timer tick** → scheduler's `wake_sleeping_threads()` checks all WAITING threads
7. **When deadline passes**, thread is marked READY
8. **Scheduler** picks thread, context switches back
9. **`schedule_blocking`** returns, `sys_nanosleep` loops and sees deadline passed
10. **Returns 0** to user

## Context Switch Safety

The per-thread exception stack ensures safe context switching during syscalls:

- Each thread's UserTrapFrame is saved to its own exception stack
- `TPIDR_EL1` (Thread Pointer ID Register) stores the current exception stack
- When switching back to a thread, its trap frame is still intact

**Why TPIDR_EL1?** Using a CPU register instead of a global variable eliminates
race conditions and memory access issues. The exception handler reads directly
from the register with a single `mrs` instruction - no memory loads needed.

**Critical:** The exception stack must be updated **BEFORE** `switch_context()`:

```rust
// In sgi_scheduler_handler:
set_current_exception_stack(pool.slots[new_idx].exception_stack_top); // Sets TPIDR_EL1
switch_context(old_ptr, new_ptr);
```

This is because new threads jump directly to `thread_start_closure` via their saved
`x30` register and never return to `sgi_scheduler_handler`. If we set the exception
stack after `switch_context`, new threads would run with the wrong exception stack.

## TTBR0 Handling During Syscall Blocking

When a user process makes a syscall that blocks (e.g., `nanosleep`), TTBR0 contains
the user's page tables. When yielding during a syscall, we must switch to kernel TTBR0.

**Critical:** The user TTBR0 must be saved in `ThreadSlot.saved_user_ttbr0`, NOT in a
local variable. Local variables don't survive context switches through IRQ handlers
because the IRQ handler's push/pop of registers can clobber stack-relative values.

```rust
// In schedule_blocking:
let user_ttbr0 = read_ttbr0();                    // Read user TTBR0
switch_to_kernel_ttbr0();                          // Use kernel page tables FIRST
pool.slots[tid].saved_user_ttbr0 = user_ttbr0;    // Save in ThreadSlot (survives context switch)
yield_now();                                       // Context switch happens
let restored = pool.slots[tid].saved_user_ttbr0;  // Read from ThreadSlot
restore_ttbr0(restored);                           // Restore user TTBR0
```

This ensures:
1. `switch_context` saves kernel TTBR0 (not user TTBR0) to the thread context
2. When the thread resumes, kernel code has full access to kernel memory
3. The user TTBR0 is safely preserved in the ThreadSlot across context switches
4. Before returning from `schedule_blocking`, user TTBR0 is restored

## Moving Exception Stacks Elsewhere

Currently, exception stacks are reserved at the top of each kernel thread stack.
To move them to separate allocations:

1. **Allocate separate memory** per thread (1KB each)
2. **Store pointer** in `ThreadSlot.exception_stack_top`
3. **Update on init** during `ThreadPool::init()` and `allocate_stack_for_slot()`
4. **No assembly changes needed** - exception handler reads from `TPIDR_EL1`

Benefits of separate allocation:
- Cleaner stack layout
- Could use guard pages for overflow detection
- More flexible sizing

## Related Files

- `src/threading.rs` - Thread pool, scheduler, `schedule_blocking()`
- `src/exceptions.rs` - Exception handler, `set_current_exception_stack()` uses TPIDR_EL1
- `src/syscall.rs` - `sys_nanosleep` using wait queue
- `src/config.rs` - Stack size constants

## Testing

1. **Basic sleep**: `hello 10 1000` should allow SSH connections during sleep
2. **Multiple processes**: Run several sleeping processes concurrently
3. **Ctrl+C**: Should still interrupt sleeping processes
4. **Long sleep**: SSH should remain responsive during 10+ second sleeps
