# Syscall Blocking and Context Switching Issues

## Problem Summary

Long-running syscalls (like `nanosleep`) block the entire system because context switching during syscall handling is unsafe on ARM64.

## Symptoms

When running `hello 10 1000` (10 outputs, 1 second delay):
- SSH connections are blocked for the duration
- The main network loop (thread 1) doesn't run
- Accept timeouts stop accumulating
- New SSH connections hang until the process exits

## Root Cause: ARM64 Exception Architecture

### How Syscalls Work

1. User code (EL0) calls `SVC` instruction
2. CPU automatically:
   - Saves user's PC to `ELR_EL1`
   - Saves user's PSTATE to `SPSR_EL1`
   - Switches to EL1 (kernel mode)
3. `sync_el0_handler` saves user context to trap frame
4. Syscall handler runs (e.g., `sys_nanosleep`)
5. Handler returns, trap frame is restored
6. `ERET` returns to user code

### The Problem with Nested Interrupts

ARM64 has only ONE set of `ELR_EL1`/`SPSR_EL1` registers. When a timer interrupt fires during a syscall:

```
User code (EL0)
  └─> SVC (syscall)
        ELR_EL1 = user_pc    ← Saved by sync_el0_handler to trap frame
        └─> sys_nanosleep running in EL1
              └─> Timer IRQ fires!
                    CPU OVERWRITES: ELR_EL1 = kernel_pc
                    └─> IRQ handler
                          └─> Context switch attempt
                                └─> PROBLEM: Stack frames are nested
```

### Why Context Switching Fails

When we try to context switch from within the IRQ handler:

1. **Nested exception frames**: We have sync_el0_handler's frame, then irq_handler's frame
2. **Stack state mismatch**: The other thread's stack doesn't have matching frames
3. **ELR_EL1 confusion**: Which return address should be used?

Even though `sync_el0_handler` saves user's ELR/SPSR to the trap frame, the nested IRQ handler complicates the stack unwinding.

## Current Workaround

The `sys_nanosleep` syscall busy-waits in the kernel:

```rust
fn sys_nanosleep(seconds: u64, nanoseconds: u64) -> u64 {
    while uptime_us() < deadline {
        // Check for Ctrl+C
        if is_interrupted() { return EINTR; }
        
        // Busy-wait in 10ms chunks
        delay_us(10_000);  // BLOCKS THE CPU
    }
    0
}
```

This works but blocks thread scheduling for the entire sleep duration.

## Attempted Solutions

### 1. Short Kernel Sleeps (Failed)

**Idea**: Sleep max 1ms in kernel, loop in userspace

**Implementation**:
```rust
// Kernel
fn sys_nanosleep(...) -> u64 {
    let sleep_us = total_us.min(1_000); // Max 1ms
    delay_us(sleep_us);
    0
}

// Userspace  
fn sleep_ms(ms: u64) {
    for _ in 0..ms {
        syscall(NANOSLEEP, 0, 1_000_000, ...); // 1ms per call
    }
}
```

**Result**: Caused crashes (data abort at 0x72). The frequent syscall transitions exposed other bugs.

### 2. Preemption During Syscalls (Not Attempted)

**Idea**: Enable preemption explicitly in `sys_nanosleep`

**Problem**: The exception state (ELR_EL1, SPSR_EL1, stack frames) makes this unsafe without careful saving/restoring.

## Proper Solution: Wait Queues

How Linux solves this:

### 1. Explicit Preemption Points

Syscalls call `schedule()` at well-defined points where the kernel stack is in a clean state:

```rust
fn sys_nanosleep(duration: u64) -> u64 {
    let deadline = now() + duration;
    
    loop {
        if now() >= deadline {
            return 0;
        }
        
        // Set up wake condition
        current_thread.state = WAITING;
        timer_queue.insert(deadline, current_thread);
        
        // SAFE preemption point - stack is clean
        schedule();
        
        // Woken up - check if done
    }
}
```

### 2. Wait Queues

Instead of busy-waiting:
1. Add thread to a wait queue
2. Mark thread as `WAITING` (not schedulable)
3. Call `schedule()` to switch to another thread
4. Timer/event wakes the thread by marking it `READY`
5. Scheduler eventually runs it again

### 3. Proper Context Save

The scheduler saves the FULL kernel context at the `schedule()` call site, including:
- All general-purpose registers
- The return path back to userspace
- Stack pointer

## Implementation Requirements

To implement proper wait queues in Akuma:

1. **Thread states**: Add `WAITING` state
2. **Wait queues**: Data structure to track sleeping threads
3. **Timer queue**: Sorted list of (wake_time, thread_id)
4. **`schedule()` function**: Safe context switch from known points
5. **Wake mechanism**: Timer interrupt checks queue and wakes threads

## Current Limitations

- `nanosleep` blocks the calling thread's CPU time
- Long-running processes block SSH accept loop
- No true concurrent execution during blocking syscalls

## Workarounds for Users

1. **Use short delays**: `hello 10 100` (100ms) is less disruptive than 1000ms
2. **Accept the blocking**: For a toy OS, this is acceptable
3. **Avoid blocking operations**: Design userspace to yield frequently

## Related Files

- `src/syscall.rs` - Syscall handlers
- `src/exceptions.rs` - Exception vector table
- `src/threading.rs` - Thread scheduler
- `userspace/libakuma/src/lib.rs` - Userspace sleep functions
