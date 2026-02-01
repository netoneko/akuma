# Thread Scheduling Investigation

## Summary

Investigation into why spawning multiple `meow` instances causes hangs, and why terminated user threads aren't being cleaned up properly.

## Timeline of Issues

### Issue 1: Memory Leak in realloc (FIXED)

**Symptom**: meow crashes with "OUT OF MEMORY!" after ~60MB allocated but only ~3.6MB net memory.

**Root cause**: `libakuma`'s `HybridAllocator::realloc` was not calling `mmap_dealloc` on the old memory block after copying to a new allocation.

**Fix**: Added `mmap_dealloc` call in realloc. However, this caused hangs (see Issue 2).

### Issue 2: Hang When Calling munmap from realloc (WORKAROUND)

**Symptom**: After fixing the memory leak, meow hangs after the first LLM response.

**Root cause**: Making `munmap` syscalls during allocation context causes issues, possibly due to:
- Scheduler interaction during realloc
- Re-entrancy in exception handling
- Lock ordering problems

**Workaround**: Disabled `mmap_dealloc` in realloc, accepting memory leaks during reallocations. Process exit cleans up properly.

**TODO**: Investigate why munmap syscall from realloc causes hangs.

### Issue 3: Proper munmap Implementation (FIXED)

**Symptom**: Kernel's `sys_munmap` was a no-op.

**Fix**: Implemented proper munmap:
- Added `mmap_regions: Vec<(usize, Vec<PhysFrame>)>` to `Process` struct
- `sys_mmap` now calls `record_mmap_region()` to track VA-to-frame mappings
- `sys_munmap` now properly:
  - Unmaps pages from page table
  - Frees physical frames via PMM
  - Removes from `UserAddressSpace::user_frames` to prevent double-free

### Issue 4: Second meow Instance Never Starts (CURRENT)

**Symptom**: 
- First meow spawns on thread 10, works correctly
- Second meow spawns on thread 11, `[thread_closure] START` never prints
- Both meows appear stuck
- `kthreads` shows both threads 10 and 11 as "ready"

**Debug findings**:

```
[spawn_user_internal] claimed slot 11
[spawn_user_internal] tid=11 READY: elr=0x400803e0 sp=0x42402f50 x19=0x400807dc
[spawn_process] spawned thread 11 pid 13 for /bin/meow
[SGI-S] 3 -> 8 (user)
[SGI-S] 3 -> 9 (user)
```

Thread 11 is NEVER scheduled! No `[SGI-S] X -> 11` appears.

**Root cause**: Round-robin scheduler starvation

The scheduler's round-robin algorithm:
```rust
let mut next_idx = (current_idx + 1) % config::MAX_THREADS;
loop {
    let state = THREAD_STATES[next_idx].load(Ordering::SeqCst);
    if state == thread_state::READY || state == thread_state::RUNNING {
        break;  // Stops at FIRST ready thread!
    }
    next_idx = (next_idx + 1) % config::MAX_THREADS;
}
```

When SSH session (thread 3) is preempted:
1. Scheduler checks 4, 5, 6, 7 (not ready)
2. Checks 8 (herd) - READY, **breaks immediately**
3. Never reaches threads 9, 10, 11

For thread 11 to be scheduled, the scheduler must run FROM thread 10. But if thread 10 is:
- Stuck waiting for network, or
- Being starved by threads 8, 9

...then thread 11 never gets a chance.

## Thread Layout

```
Thread 0:  bootstrap (cooperative) - main async loop
Thread 1:  network (preemptive) - network processing
Thread 2-7: system-threads (SSH sessions, etc.)
Thread 8+: user-process threads

Currently:
- Thread 8: herd (PID 10)
- Thread 9: httpd (PID 11)  
- Thread 10: first meow (PID 12)
- Thread 11: second meow (PID 13) - NEVER RUNS
```

## Solution: Global Round-Robin Index (IMPLEMENTED)

**Problem**: Starting from `current_idx + 1` causes starvation. When SSH (thread 3) gets preempted, it checks 4→5→6→7→8 and stops at 8. Every time SSH runs, the search restarts from 4.

**Fix**: Use a global `round_robin_idx` that persists across all scheduling decisions.

```rust
// Before: started from current thread (caused starvation)
let mut next_idx = (current_idx + 1) % config::MAX_THREADS;

// After: starts from global position (fair rotation)
let mut next_idx = (self.round_robin_idx + 1) % config::MAX_THREADS;
// ... find ready thread ...
self.round_robin_idx = next_idx;  // Remember position for next time
```

**Result**: Even when SSH keeps getting CPU time, the round_robin_idx advances: 8→9→10→11→0→1... All threads get scheduled fairly.

## Debug Output Added

```rust
// In schedule_indices() - logs when scheduler checks thread 11
if next_idx == 11 {
    crate::safe_print!(64, "[CHECK] tid=11 state={} from current={}\n", state, current_idx);
}

// In sgi_scheduler_handler_with_sp() - logs user thread scheduling
if new_idx >= 8 {
    crate::safe_print!(64, "[SGI-S] {} -> {} (user)\n", old_idx, new_idx);
}

// In spawn_user_thread_fn_internal() - logs slot claiming
crate::safe_print!(64, "[spawn_user_internal] claimed slot {}\n", slot_idx);
crate::safe_print!(128, "[spawn_user_internal] tid={} READY: elr={:#x} sp={:#x} x19={:#x}\n", ...);
```

## Files Modified

- `src/threading.rs` - Debug output, scheduler analysis
- `src/syscall.rs` - sys_munmap implementation
- `src/process.rs` - mmap_regions tracking, record/remove functions
- `src/mmu.rs` - remove_user_frame() to prevent double-free
- `userspace/libakuma/src/lib.rs` - realloc fix (disabled due to Issue 2)

## Next Steps

1. ~~Fix the scheduler starvation issue (Issue 4)~~ DONE - global round_robin_idx
2. Investigate why munmap from realloc causes hangs (Issue 2)
3. Consider adding thread cleanup to the main loop more frequently
4. Re-enable munmap in realloc once Issue 2 is fixed
