# INITIALIZING Slot Race Condition Fix (January 2026)

## Summary

A race condition in thread slot cleanup caused intermittent kernel crashes with `FAR=0x5` (null pointer dereference) and corrupted `TTBR0` values. The bug was introduced when attempting to clean up "stuck" INITIALIZING slots.

## Symptoms

1. **FAR=0x5 crashes** during parallel process execution:
   ```
   [Exception] Sync from EL1: EC=0x25, ISS=0x7
     ELR=0x400da428, FAR=0x5, SPSR=0x20002345
     Thread=8, TTBR0=0x1000044000000, TTBR1=0x402ef000
     WARNING: Kernel accessing user-space address!
   ```

2. **Corrupted TTBR0 values** like `0x1000044000000` (which is actually a valid user TTBR0 from the *wrong* process)

3. **Memory corruption** with garbage appearing in console output mid-print

4. **Intermittent boot failures** - system would crash "half the time"

## Root Cause Analysis

### The Race Condition

The bug was in `cleanup_terminated_internal()` which was modified to clean up INITIALIZING slots:

```rust
// BUGGY CODE - DO NOT USE
if current_state == thread_state::INITIALIZING {
    if force {
        THREAD_STATES[i].store(thread_state::FREE, Ordering::SeqCst);
        // ... print message ...
    }
    continue;
}
```

This created a race between spawn and cleanup:

```
Timeline:
─────────────────────────────────────────────────────────────────────────────

Thread A (spawn_process for hello PID 1):
    1. claim_free_slot(8) → slot 8: FREE → INITIALIZING ✓
    2. Box::new(closure) on heap
    3. with_irqs_disabled { ... }
                                            
Test Thread (calling cleanup_terminated_force):
                    4. Sees slot 8 is INITIALIZING
                    5. Sets slot 8: INITIALIZING → FREE  ← BUG!

Thread B (spawn_process for hello PID 2):
                        6. claim_free_slot(8) → slot 8: FREE → INITIALIZING ✓
                           (SAME SLOT as Thread A!)
                        7. Sets up context with Process 2's TTBR0

Thread A (continuing):
    8. Sets up context with Process 1's TTBR0 (OVERWRITES!)
    9. Sets slot 8 to READY
    
Thread B (continuing):
                        10. Sets slot 8 to READY (no-op, already READY)

Result: Slot 8 has MIXED context data from both processes!
```

### Why FAR=0x5?

The corrupted TTBR0 value `0x1000044000000` decodes to:
- ASID = 1 (bits 63:48)
- L0 page table = 0x44000000 (bits 47:0)

This is a **valid user TTBR0** from one of the hello processes - but it was saved to the wrong thread's context. When the kernel tried to access `PROCESS_INFO_ADDR` (0x1000) through these page tables, the address wasn't mapped, causing a translation fault at a near-null address.

The `FAR=0x5` specifically suggests kernel code was dereferencing a field at offset 5 from a null pointer, likely from a failed `current_process()` lookup that returned garbage.

## The Fix

Removed the INITIALIZING cleanup entirely:

```rust
// CORRECT: Never clean up INITIALIZING slots
for i in 1..config::MAX_THREADS {
    let current_state = THREAD_STATES[i].load(Ordering::SeqCst);
    
    // NOTE: We intentionally do NOT clean up INITIALIZING slots!
    // An INITIALIZING slot means spawn is in progress. Cleaning it up would create
    // a race condition where another spawn could claim the same slot, leading to
    // context corruption and crashes (FAR=0x5, TTBR0 corruption, etc.)
    //
    // If a slot is truly stuck in INITIALIZING (spawn crashed), it will be leaked.
    // This is a rare edge case and preferable to risking context corruption.
    
    // Only clean up TERMINATED threads
    if current_state != thread_state::TERMINATED {
        continue;
    }
    // ... rest of cleanup ...
}
```

## Additional Fixes Applied

### 1. TTBR0 Check in `read_current_pid()`

Added a guard to prevent reading from user-space addresses when not in a user process context:

```rust
pub fn read_current_pid() -> Option<Pid> {
    let ttbr0: u64;
    unsafe {
        core::arch::asm!("mrs {}, ttbr0_el1", out(reg) ttbr0);
    }
    
    // Boot TTBR0 is in 0x4020_0000 - 0x4400_0000 range
    let is_boot_ttbr0 = ttbr0 >= 0x4020_0000 && ttbr0 < 0x4400_0000;
    if is_boot_ttbr0 {
        return None; // Not in user process context
    }
    
    // Safe to read from PROCESS_INFO_ADDR now
    // ...
}
```

### 2. Disabled DEBUG_FRAME_TRACKING

The PMM's BTreeMap-based frame tracking was causing heap corruption under heavy allocation patterns. Disabled it as a workaround:

```rust
pub const DEBUG_FRAME_TRACKING: bool = false;
```

## Lessons Learned

1. **Never clean up state that another thread might be actively using** - INITIALIZING is a transitional state that indicates "work in progress"

2. **Lock-free state machines need careful analysis** - The atomic compare_exchange in `claim_free_slot` only protects the state transition, not the context data being written afterward

3. **Race conditions can appear as memory corruption** - The TTBR0 corruption wasn't random garbage; it was valid data from the wrong thread

4. **Intermittent bugs often indicate timing-dependent races** - "Half the time" failures are a strong signal of concurrency bugs

## Test Cases

The following test pattern triggers the race:
- Threading tests that call `cleanup_terminated_force()` between spawns
- Parallel process execution (`test_parallel_process_execution`)
- Mixed cooperative/preemptible thread tests

After the fix, all threading and process tests should pass consistently.

## Files Modified

- `src/threading.rs` - Removed INITIALIZING cleanup
- `src/process.rs` - Added TTBR0 check in `read_current_pid()`
- `src/pmm.rs` - Disabled `DEBUG_FRAME_TRACKING`
