# Lock-Free Threading and SSH Concurrency

This document summarizes the findings from debugging SSH session concurrency issues, lock-free threading implementation, and proper async yielding.

## Overview

The Akuma kernel uses a hybrid threading model:
- **Thread 0**: Runs the embassy async executor (cooperative)
- **Threads 1-7**: System threads for SSH sessions (preemptive)
- **Threads 8-31**: User threads for process execution (preemptive)

SSH sessions use a custom `block_on()` executor that polls async futures, while thread 0 uses the full embassy executor.

## Issue 1: Thread Pool Lock Contention

### Problem
The original `ThreadPool` used a `Spinlock` for all operations. Functions like `spawn()`, `cleanup_terminated()`, `thread_count()`, etc. all acquired this lock. When multiple SSH sessions ran simultaneously, they contended for this lock, causing:
- System hangs
- Input staggering
- Unresponsive SSH sessions

### Solution: Lock-Free Thread States

Implemented lock-free thread state management using atomic operations:

```rust
/// Atomic thread states - lock-free access
static THREAD_STATES: [AtomicU8; MAX_THREADS] = [...];

/// Current running thread - lock-free access
static CURRENT_THREAD: AtomicUsize = AtomicUsize::new(0);

pub mod thread_state {
    pub const FREE: u8 = 0;
    pub const READY: u8 = 1;
    pub const RUNNING: u8 = 2;
    pub const TERMINATED: u8 = 3;
    pub const INITIALIZING: u8 = 4;
}
```

Key changes:
- **Slot claiming**: Uses `compare_exchange` to atomically claim FREE → INITIALIZING
- **State transitions**: Scheduler uses atomic loads/stores instead of lock
- **Counting functions**: `thread_count()`, `system_threads_available()` are now lock-free

### The INITIALIZING State

Critical fix for a race condition:

```
Before: claim_free_slot → READY → (race!) → scheduler runs uninitialized thread
After:  claim_free_slot → INITIALIZING → setup context → READY → scheduler runs
```

If we set state to READY immediately, the scheduler might switch to the thread before its context is set up, causing crashes (FAR=0x0).

## Issue 2: Embassy-net RefCell Panics

### Problem
Embassy-net uses `RefCell` internally for its network stack. When multiple SSH sessions accessed the network simultaneously:

```
Thread 2: polls future → borrows RefCell
Timer interrupt → preemption
Thread 3: polls future → tries to borrow same RefCell
PANIC: RefCell already borrowed
```

### Solution: Disable Preemption During Poll

In `block_on()`:

```rust
fn block_on<F: Future>(mut future: F) -> F::Output {
    loop {
        // CRITICAL: Disable preemption during poll
        crate::threading::disable_preemption();
        let poll_result = future.as_mut().poll(&mut cx);
        crate::threading::enable_preemption();

        match poll_result {
            Poll::Ready(output) => return output,
            Poll::Pending => {
                crate::threading::yield_now();
                // spin delay...
            }
        }
    }
}
```

This ensures:
- No timer preemption while RefCell is borrowed
- Other threads can still run between polls (during yield)
- Network operations complete atomically

## Issue 3: Async Yielding in `block_on` Context

### Problem
`exec_async` needed to wait for a user process to complete. Various approaches failed:

| Approach | Problem |
|----------|---------|
| `Timer::after(10ms).await` | Embassy timers only advance in thread 0's executor, not in `block_on` |
| `yield_now()` in tight loop | Floods scheduler with SGIs, causes lock contention |
| Spin-wait with preemption toggling | Complex, error-prone, caused hangs |
| `Poll::Pending` forever | Never returns, hangs |

### Solution: `YieldOnce` Future

Created a future that yields exactly once:

```rust
struct YieldOnce(bool);

impl Future for YieldOnce {
    type Output = ();
    
    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        if self.0 {
            Poll::Ready(())  // Second poll: done
        } else {
            self.0 = true;
            Poll::Pending    // First poll: yield
        }
    }
}
```

Usage in `exec_async`:

```rust
loop {
    if channel.has_exited() || is_thread_terminated(thread_id) {
        break;
    }
    
    // Yield once - returns Pending, block_on yields, then we're re-polled
    YieldOnce::new().await;
}
```

Flow:
1. `exec_async` checks if process done
2. Not done → `YieldOnce::new().await` returns `Pending`
3. `block_on` sees `Pending`, yields to scheduler
4. User thread gets scheduled, runs process
5. Timer preempts, `block_on` gets scheduled again
6. `block_on` re-polls, `YieldOnce` returns `Ready`
7. Loop continues, check again

This properly integrates with `block_on`'s yield logic without manual preemption manipulation.

## Key Invariants

1. **THREAD_STATES is the source of truth** for thread states. `pool.slots[i].state` is kept in sync for compatibility but atomics are authoritative.

2. **Preemption must be disabled** when accessing embassy-net's RefCell (during poll in `block_on`).

3. **Never set READY before context is set up** - use INITIALIZING state.

4. **Async functions in `block_on` context** must use `YieldOnce` or similar to properly yield, not embassy timers.

## Thread State Transitions

```
FREE ──[claim_free_slot]──> INITIALIZING ──[context setup]──> READY
                                                                 │
                          ┌─────────────[scheduler]──────────────┘
                          │
                          v
                       RUNNING ──[mark_current_terminated]──> TERMINATED
                          │                                       │
                          └────────[scheduler]────────────────────┘
                                                                  │
                       ┌──────────[cleanup_terminated]────────────┘
                       v
                     FREE
```

## Performance Characteristics

| Operation | Before | After |
|-----------|--------|-------|
| `thread_count()` | Lock + iterate | Atomic reads |
| `spawn_*()` | Lock for entire spawn | Lock-free claim + brief lock for context |
| `cleanup_terminated()` | Lock + iterate + cleanup | Atomic CAS loop |
| `current_thread_id()` | Lock | Atomic load |
| Scheduler state check | Lock | Atomic loads |

## Files Modified

- `src/threading.rs`: Lock-free state management, INITIALIZING state
- `src/ssh/server.rs`: `block_on()` with preemption protection
- `src/process.rs`: `YieldOnce` future for proper async yielding
