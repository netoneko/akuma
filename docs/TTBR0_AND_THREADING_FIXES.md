# TTBR0 and Threading Bug Fixes

This document summarizes the critical bugs discovered and fixed related to thread spawning,
page tables (TTBR0), and thread state management.

## Overview

The system was experiencing intermittent crashes with symptoms including:
- `[Exception] Sync from EL1: EC=0x25` (Data Abort from kernel)
- Kernel accessing user-space addresses (FAR=0x3fffffa0)
- `[Exception] Unknown from EL0: EC=0x0, ISS=0x0`
- `[return_to_kernel] ERROR: no current process!`
- Zombie threads accumulating and not being recycled

## Root Cause: TTBR0 Corruption

### The Bug

When spawning new threads, the code captured the **current** TTBR0:

```rust
// OLD - BUG!
let boot_ttbr0: u64;
unsafe { core::arch::asm!("mrs {}, ttbr0_el1", out(reg) boot_ttbr0); }
pool.slots[slot_idx].context.ttbr0 = boot_ttbr0;
```

If a user process was running at the time of spawning, `boot_ttbr0` would be the 
**user process's page tables**, not the kernel's boot page tables.

### Symptoms

When the new thread later ran:
1. Its TTBR0 pointed to a user process's page tables
2. If that user process had exited, the page tables might be stale/freed
3. Kernel code trying to access user-space addresses would fault
4. Crash with EC=0x25 (Data Abort) accessing FAR=0x3fffffa0 (user stack)

### The Fix

Use the **stored** boot TTBR0 value that was saved by the boot code:

```rust
// NEW - CORRECT!
// Added to src/mmu.rs:
pub fn get_boot_ttbr0() -> u64 {
    unsafe {
        let addr: u64;
        core::arch::asm!(
            "adrp {tmp}, boot_ttbr0_addr",
            "add {tmp}, {tmp}, :lo12:boot_ttbr0_addr",
            "ldr {out}, [{tmp}]",
            tmp = out(reg) _,
            out = out(reg) addr,
        );
        addr
    }
}

// In spawn functions:
let boot_ttbr0 = crate::mmu::get_boot_ttbr0();
```

### Files Changed

- `src/mmu.rs`: Added `get_boot_ttbr0()` function
- `src/threading.rs`: Updated all 6 spawn functions to use `get_boot_ttbr0()` instead of reading current TTBR0

## Zombie Thread Cleanup

### The Problem

With `DEFERRED_THREAD_CLEANUP = true`, only thread 0 can clean up terminated threads.
But thread 0's idle loop wasn't calling `cleanup_terminated()`, so zombies accumulated.

### The Fix

Updated thread 0's idle loop in `src/main.rs`:

```rust
loop {
    loop_counter = loop_counter.wrapping_add(1);
    
    // Clean up every 10 iterations
    if loop_counter % 10 == 0 {
        let cleaned = threading::cleanup_terminated();
        if cleaned > 0 {
            console::print(&format!("[Thread0] Cleaned {} terminated threads\n", cleaned));
        }
    }
    
    // Heartbeat every 1000 iterations
    if loop_counter % 1000 == 0 {
        console::print(&format!("[Thread0] loop={} | zombies={}\n", 
            loop_counter, threading::thread_stats().2));
    }
    
    threading::yield_now();
}
```

## Thread State Consolidation

### The Problem

Thread state was stored in TWO places:
1. `THREAD_STATES[i]` - Global atomic array (u8)
2. `slots[i].state` - Field in ThreadSlot struct (ThreadState enum)

These had to be kept in sync manually, which was error-prone.

### The Fix

Removed the duplicate `state` field from `ThreadSlot`, keeping only `THREAD_STATES` 
as the single source of truth:

**Before:**
```rust
pub struct ThreadSlot {
    pub state: ThreadState,  // REMOVED
    pub context: Context,
    pub cooperative: bool,
    // ...
}
```

**After:**
```rust
pub struct ThreadSlot {
    // NOTE: state removed - use THREAD_STATES[idx] instead
    pub context: Context,
    pub cooperative: bool,
    // ...
}
```

Updated all methods that read `slot.state` to use `THREAD_STATES[i]` instead:
- `ThreadPool::thread_stats()`
- `ThreadPool::thread_count()`
- `has_ready_threads()` (now lock-free!)

## Enhanced Diagnostics

Added better error reporting for debugging:

### EL1 Sync Exceptions (`src/exceptions.rs`)
```
[Exception] Sync from EL1: EC=0x25, ISS=0x10, ELR=..., FAR=..., SPSR=...
  Thread=X, TTBR0=0x..., TTBR1=0x..., SP=0x...
  WARNING: Kernel accessing user-space address!
```

### EL0 Unknown Exceptions
```
[Exception] Unknown from EL0: EC=0x0, ISS=0x0
  Thread=X, ELR=..., FAR=..., SPSR=...
  TTBR0=..., SP=...
  WARNING: TTBR0 looks like boot page tables, not user process!
```

### return_to_kernel Errors
```
[return_to_kernel] ERROR: no current process!
  Thread=X, raw_pid_at_0x1000=Y, TTBR0=...
  ELR=..., SPSR=..., SP=...
  TTBR0 looks like boot page tables - no user process active!
```

### SSH Status Logging
```
[SSH Status] active=X fallback=Y sys_avail=Z max=4
```

## Testing

After these fixes:
1. No more TTBR0 corruption crashes
2. Zombie threads are properly cleaned up
3. `kthreads` command shows healthy thread states
4. SSH sessions work correctly
5. User processes (hello, stackstress) run without issues

## Configuration Options

Related configuration in `src/config.rs`:

```rust
/// Enable deferred thread cleanup (only thread 0 cleans up)
pub const DEFERRED_THREAD_CLEANUP: bool = true;

/// Cooldown before recycling a terminated thread slot (10ms)
pub const THREAD_CLEANUP_COOLDOWN_US: u64 = 10_000;
```

## Lessons Learned

1. **Never capture current TTBR0 for new threads** - Always use the stored boot value
2. **Single source of truth** - Don't duplicate state in multiple places
3. **Atomic state for schedulers** - Lock-free scheduling requires atomic state arrays
4. **Cleanup responsibility** - Someone must be responsible for cleanup, and they must run
