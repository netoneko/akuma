# Unified Context Architecture

## Overview

This document describes the unified context system implemented in January 2026 to eliminate the dual-context architecture that was causing heap corruption and exit code corruption bugs.

## Problem: Dual Context System

Previously, the kernel had two separate context save/restore mechanisms:

1. **THREAD_CONTEXTS** - Used by the scheduler for context switching between threads
2. **KernelContext** - Used by `run_user_until_exit` to return after a user process exits

Both saved the same registers (x19-x30, sp) but were used independently:

```
Before:
┌─────────────────────────────────────────────────────────┐
│ Thread 8 runs execute()                                 │
│   └─> run_user_until_exit() saves KernelContext         │
│         └─> ERET to user mode                           │
│               │                                         │
│               ▼                                         │
│         [User code runs]                                │
│               │                                         │
│         Timer IRQ fires                                 │
│               └─> sgi_scheduler_handler saves to        │
│                   THREAD_CONTEXTS[8]                    │
│                     └─> Switch to Thread 9              │
│                           ...                           │
│                     └─> Switch back to Thread 8         │
│                           └─> Restore from              │
│                               THREAD_CONTEXTS[8]        │
│               │                                         │
│         [User code continues]                           │
│               │                                         │
│         User calls exit()                               │
│               └─> return_to_kernel() restores from      │
│                   KernelContext (STALE?)                │
└─────────────────────────────────────────────────────────┘
```

This created race conditions where:
- Context switches could corrupt the relationship between saved contexts
- Exit codes appeared as heap addresses
- FAR=0x5 crashes occurred

## Solution: Unified Context

The new architecture uses only `THREAD_CONTEXTS` for all context management:

```
After:
┌─────────────────────────────────────────────────────────┐
│ spawn_process_with_channel()                            │
│   └─> Spawns closure on Thread 8                        │
│         └─> execute() activates user address space      │
│               └─> enter_user_mode() ERETs to user       │
│                     │                                   │
│                     ▼                                   │
│               [User code runs]                          │
│                     │                                   │
│               Timer IRQ - normal context switch via     │
│               THREAD_CONTEXTS[8]                        │
│                     │                                   │
│               [User code continues]                     │
│                     │                                   │
│               User calls exit()                         │
│                     └─> return_to_kernel():             │
│                           1. Set exit code on channel   │
│                           2. Deactivate address space   │
│                           3. Mark thread TERMINATED     │
│                           4. yield_now() forever        │
│                                                         │
│ Thread 0 cleanup reclaims the slot                      │
└─────────────────────────────────────────────────────────┘
```

## Key Changes

### 1. Context Struct Extended

```rust
pub struct Context {
    pub magic: u64,           // 0xDEAD_BEEF_1234_5678 for integrity
    pub x19: u64,             // Callee-saved registers
    // ... x20-x28 ...
    pub x29: u64,             // Frame pointer
    pub x30: u64,             // Link register
    pub sp: u64,              // Stack pointer
    pub daif: u64,            // Interrupt mask
    pub elr: u64,             // Exception link register
    pub spsr: u64,            // Saved program status
    pub ttbr0: u64,           // User address space
    pub user_entry: u64,      // User PC (0 for kernel threads)
    pub user_sp: u64,         // User SP (0 for kernel threads)
    pub is_user_process: u64, // 1 for user process threads
}
```

### 2. KernelContext Removed

- Deleted `KernelContext` struct
- Deleted `run_user_until_exit` assembly function
- `kernel_ctx` field removed from `Process` struct

### 3. execute() Never Returns

```rust
pub fn execute(&mut self) -> ! {
    // Set up process
    register_process(self.pid, self as *mut Process);
    self.address_space.activate();
    crate::irq::enable_irqs();
    
    // Enter user mode - never returns
    unsafe { enter_user_mode(&self.context); }
}
```

### 4. return_to_kernel() Terminates Thread

```rust
pub extern "C" fn return_to_kernel(exit_code: i32) -> ! {
    let tid = crate::threading::current_thread_id();
    
    // Unregister process
    if let Some(proc) = current_process() {
        unregister_process(proc.pid);
    }
    
    // Notify async caller via channel
    if let Some(channel) = remove_channel(tid) {
        channel.set_exited(exit_code);
    }
    
    // Cleanup
    crate::mmu::UserAddressSpace::deactivate();
    crate::threading::mark_current_terminated();
    
    // Yield forever - scheduler reclaims thread
    loop { crate::threading::yield_now(); }
}
```

### 5. Synchronous Execution via Polling

```rust
pub fn exec_with_io(path: &str, args: Option<&[&str]>, stdin: Option<&[u8]>) 
    -> Result<(i32, Vec<u8>), String> 
{
    let (thread_id, channel) = spawn_process_with_channel(path, args, stdin)?;
    
    // Poll until complete
    loop {
        if channel.has_exited() || is_thread_terminated(thread_id) {
            break;
        }
        yield_now();
    }
    
    // Collect output
    let stdout = channel.read_all();
    cleanup_terminated();
    
    Ok((channel.exit_code(), stdout))
}
```

## Integrity Checks

The scheduler validates context magic before switching:

```rust
// In sgi_scheduler_handler
if !new_ctx.is_valid() {
    console::print("[SGI CORRUPT] Context magic invalid\n");
    THREAD_STATES[new_idx].store(thread_state::TERMINATED, Ordering::SeqCst);
    return;  // Don't switch to corrupted context
}
```

## Assembly Offset Changes

The `switch_context` assembly was updated for the new struct layout:

| Field | Old Offset | New Offset |
|-------|------------|------------|
| magic | N/A | 0 |
| x19, x20 | 0 | 8 |
| x21, x22 | 16 | 24 |
| x23, x24 | 32 | 40 |
| x25, x26 | 48 | 56 |
| x27, x28 | 64 | 72 |
| x29, x30 | 80 | 88 |
| sp | 96 | 104 |
| daif | 104 | 112 |
| elr | 112 | 120 |
| spsr | 120 | 128 |
| ttbr0 | 128 | 136 |

## Benefits

1. **Single source of truth** - Only THREAD_CONTEXTS manages thread state
2. **No stale context** - Context is always current after switch
3. **Simpler flow** - No complex register restoration in return_to_kernel
4. **Better debugging** - Magic value detects context corruption early
5. **Cleaner async model** - All process execution uses ProcessChannel

## Migration Notes

- `execute()` now returns `!` (never) instead of `i32`
- Direct calls to `process.execute()` must be replaced with `exec_with_io()` or `spawn_process_with_channel()`
- Tests updated to use the new API
