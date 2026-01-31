# Process Memory Cleanup

This document explains how memory is cleaned up when a process or thread terminates in Akuma.

## Overview

Akuma uses Rust's RAII (Resource Acquisition Is Initialization) pattern for memory cleanup. When a process exits, its memory is freed through a chain of `Drop` implementations triggered by dropping the `Box<Process>`.

## Memory Systems

Akuma has two distinct memory systems that are cleaned up differently:

| Memory Type | What It Contains | Cleanup Mechanism |
|-------------|------------------|-------------------|
| User Address Space | Physical pages for code, data, stack, mmap | `UserAddressSpace::drop()` calls `pmm::free_page()` |
| Kernel Heap | Process struct, Vecs, Strings, BTreeMaps | Talc allocator via Rust's Drop |

## Cleanup Flow

```
┌─────────────────────────────────────────────────────────────────┐
│                    Process Exit Path                             │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  User calls exit() syscall                                       │
│           │                                                      │
│           ▼                                                      │
│  Exception handler detects proc.exited = true                    │
│           │                                                      │
│           ▼                                                      │
│  return_to_kernel(exit_code)                                     │
│           │                                                      │
│           ├─► cleanup_process_sockets(proc)                      │
│           │      Close all open sockets/FDs                      │
│           │                                                      │
│           ├─► remove_channel(tid)                                │
│           │      Remove ProcessChannel from registry             │
│           │                                                      │
│           ├─► UserAddressSpace::deactivate()                     │
│           │      Restore boot TTBR0 (CRITICAL: before drop!)     │
│           │                                                      │
│           ▼                                                      │
│  let _dropped = unregister_process(pid)                          │
│           │                                                      │
│           ▼                                                      │
│  _dropped goes out of scope → Box<Process> dropped               │
│           │                                                      │
│           ├─► Process::drop()                                    │
│           │      Free dynamic_page_tables                        │
│           │                                                      │
│           └─► UserAddressSpace::drop()                           │
│                  ├─► Free all user_frames (code, data, stack)    │
│                  ├─► Free all page_table_frames (L1, L2, L3)     │
│                  ├─► Free l0_frame                               │
│                  ├─► Free ASID                                   │
│                  └─► Flush TLB for ASID                          │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

## Key Code Locations

### Process Registration/Unregistration

```rust
// src/process.rs

/// Process table: maps PID to owned Process
static PROCESS_TABLE: Spinlock<BTreeMap<Pid, Box<Process>>> = ...;

/// Register a process (takes ownership)
fn register_process(pid: Pid, proc: Box<Process>) {
    PROCESS_TABLE.lock().insert(pid, proc);
}

/// Unregister and return ownership (for dropping)
fn unregister_process(pid: Pid) -> Option<Box<Process>> {
    PROCESS_TABLE.lock().remove(&pid)
}
```

### UserAddressSpace Drop

```rust
// src/mmu.rs

impl Drop for UserAddressSpace {
    fn drop(&mut self) {
        // Free all user pages (code, data, stack, heap, mmap)
        for frame in &self.user_frames {
            pmm::free_page(*frame);
        }

        // Free all page table frames (L1, L2, L3)
        for frame in &self.page_table_frames {
            pmm::free_page(*frame);
        }

        // Free L0 table
        pmm::free_page(self.l0_frame);

        // Free ASID
        ASID_ALLOCATOR.lock().free(self.asid);

        // Flush TLB for this ASID
        flush_tlb_asid(self.asid);
    }
}
```

### return_to_kernel Cleanup

```rust
// src/process.rs

pub extern "C" fn return_to_kernel(exit_code: i32) -> ! {
    // ... socket cleanup ...
    
    // CRITICAL: Deactivate TTBR0 BEFORE dropping Process
    // If we drop first, TTBR0 would point to freed page tables!
    UserAddressSpace::deactivate();
    
    // Drop the process - this triggers all cleanup
    if let Some(pid) = pid {
        let _dropped_process = unregister_process(pid);
        // _dropped_process goes out of scope here, triggering Drop
    }
    
    // Mark thread terminated and yield forever
    mark_current_terminated();
    loop { yield_now(); }
}
```

## Kill Process Path

When `kill_process(pid)` is called externally:

```
kill_process(pid)
    │
    ├─► Set channel.interrupted (allow blocked syscalls to abort)
    ├─► Yield a few times (let process handle interrupt)
    ├─► cleanup_process_sockets(proc)
    ├─► Set proc.exited = true, exit_code = 137 (SIGKILL)
    ├─► unregister_process(pid) → Box dropped → memory freed
    ├─► remove_channel(thread_id)
    └─► mark_thread_terminated(thread_id)
```

## What Gets Freed

| Resource | Tracked In | Freed By |
|----------|-----------|----------|
| Code pages | `user_frames` | `UserAddressSpace::drop()` |
| Data pages | `user_frames` | `UserAddressSpace::drop()` |
| Stack pages | `user_frames` | `UserAddressSpace::drop()` |
| mmap pages | `user_frames` | `UserAddressSpace::drop()` |
| L1/L2/L3 page tables | `page_table_frames` | `UserAddressSpace::drop()` |
| L0 page table | `l0_frame` | `UserAddressSpace::drop()` |
| Dynamic page tables | `dynamic_page_tables` | `Process::drop()` |
| ASID | `asid` | `UserAddressSpace::drop()` |
| Kernel heap (Process struct) | Talc allocator | Rust Drop |
| ProcessChannel buffers | Arc reference | Last Arc dropped |
| Socket buffers | Socket table | `cleanup_process_sockets()` |

## Verification

There is **no explicit runtime verification** that Drop is called. The code relies on Rust's RAII guarantees.

### Enabling Debug Tracking

To verify memory is being freed:

1. **Enable PMM frame tracking** in `src/pmm.rs`:
   ```rust
   pub const DEBUG_FRAME_TRACKING: bool = true;
   ```

2. **Check for leaks**:
   ```rust
   let leak_count = pmm::leak_count();
   let stats = pmm::tracking_stats();
   ```

3. **Add debug prints to Drop** (temporary):
   ```rust
   impl Drop for UserAddressSpace {
       fn drop(&mut self) {
           safe_print!(64, "[MMU] Dropping ASID={}\n", self.asid);
           // ... rest of drop
       }
   }
   ```

### Empirical Verification

Monitor `pmm::stats()` to verify allocated pages return to baseline:

```rust
let (total, allocated, free) = pmm::stats();
// Run process
// Check allocated count returns to previous value
```

## Potential Memory Leak Scenarios

### 1. Process Not Found on Exit

If `current_process()` returns `None` in `return_to_kernel`, the process stays in `PROCESS_TABLE` forever.

### 2. Double Termination Race

If `kill_process` and normal exit race, the `already_terminated` check prevents double-free but requires both paths to be correct.

### 3. Thread Terminated Without Process Exit

If a thread is killed before calling `unregister_process`, the process may remain in the table.

## Critical Ordering

The order of operations in cleanup is critical:

1. **Deactivate TTBR0 first** - Before dropping `UserAddressSpace`, switch back to boot page tables. Otherwise TTBR0 points to freed memory.

2. **Socket cleanup before unregister** - Must access `fd_table` before Process is dropped.

3. **Drop triggers physical page free** - The `pmm::free_page()` calls happen inside Drop, not explicitly in cleanup code.

## Related Documentation

- `docs/USERSPACE_MEMORY_MODEL.md` - How userspace memory works
- `docs/MEMORY_LAYOUT.md` - Physical and virtual memory layout
- `docs/CONCURRENCY.md` - Lock ordering for safe cleanup
- `docs/UNIFIED_CONTEXT_ARCHITECTURE.md` - Thread/process relationship
