# Context Switch Bug Investigation and Fixes (January 2026)

This document summarizes the investigation into context switch bugs, race conditions, and their fixes.

## Symptoms Observed

### Crash 1: Thread 3 SP Corruption
```
EC=0x25, ISS=0x10
ELR=0x4009a238, FAR=0x48000000
Thread=3, TTBR0=0x402f2000
SP=0x47fffb70, SP_EL0=0x3fffffa0
Thread 3 kernel stack: base=0x4210b038, top=0x4218b038
WARNING: Kernel SP outside thread's stack bounds!
```
- Thread 3's SP (0x47fffb70) was completely outside its allocated stack (0x4210b038-0x4218b038)
- SP was near end of physical memory (0x48000000 = 128MB boundary)

### Crash 2: Thread 1 NULL Pointer
```
EC=0x25, ISS=0x61
ELR=0x4001c958, FAR=0x25
Thread=1, TTBR0=0x402ef000
SP=0x42088ef0 (valid, within stack)
WARNING: Kernel accessing user-space address!
```
- FAR=0x25 (37 bytes) - NULL pointer with small offset
- Write operation (ISS bit 6 set)
- Async/SSH server thread

### Crash 3: Thread 2 EL0 Mode Corruption
```
EC=0x0, ISS=0x0 (Unknown exception)
Thread=2, SPSR=0x80000000
TTBR0=0x402ef000 (boot page tables)
[return_to_kernel] ERROR: no current process!
```
- System thread (2) running at EL0 (user mode) with kernel TTBR0
- SPSR=0x80000000 has bits[3:0]=0 (EL0), but system threads should be EL1h

### Crash 4: Thread 0 Format Panic
```
!!! PANIC !!!
Location: /rustc/.../core/src/fmt/num.rs:599
Message: index out of bounds: the len is 20 but the index is 23
```
- Panic during `format!` in thread 0's heartbeat
- After panic, thread 0 stuck in `halt()` loop
- Cleanup stopped running, zombies accumulated

## Root Causes Identified

### 1. Cleanup/Spawn Race Condition (CRITICAL)

**The Bug:**
```
Timeline (concurrent):
Cleanup:                          Spawn:
1. TERMINATED -> FREE             
                                  2. claim_free_slot: FREE -> INITIALIZING ✓
                                  3. Set up context in THREAD_CONTEXTS[i]
4. Zero THREAD_CONTEXTS[i] 💥     (overwrites spawn's context!)
                                  5. Set state to READY
                                  6. Scheduler switches to thread
                                  7. Thread runs with zeroed context -> CRASH
```

After cleanup set state to FREE (step 1), spawn could immediately claim the slot (step 2-3). But cleanup was still running and would zero the context (step 4), overwriting the freshly initialized context.

**Result:** Newly spawned threads had zeroed contexts:
- `sp = 0` → NULL stack pointer → crash
- `x30 = 0` → NULL return address → crash
- `spsr = 0` → EL0 mode → kernel code runs at user privilege → crash

### 2. Zeroed Context Detection Failure

**The Bug:**
```rust
// Old code - WRONG
let is_new_thread = new_saved_elr == 0 && new_saved_spsr == 0;

// A zeroed context has spsr=0, elr=0
// This was incorrectly considered a "new thread" and skipped corruption checks!
```

Properly initialized new threads have `spsr = 0x00000005` (EL1h), NOT `spsr = 0`.

When the corruption check saw a zeroed context (spsr=0, elr=0), it thought it was a valid new thread and didn't recover, leading to EL0 mode crashes.

### 3. Heap Allocation in IRQ Context

**The Bug:**
```rust
// In sgi_scheduler_handler (IRQ context)
crate::console::print(&alloc::format!("[SGI] switching {} -> {}\n", ...));
```

`alloc::format!` allocates on the heap. If:
1. Main thread was allocating when timer fired
2. IRQ handler tried to allocate
3. Allocator lock was held → deadlock or corruption

This caused earlier crashes and made debugging difficult.

### 4. Thread 0 Panic Halts System

**The Bug:**
```rust
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // ... print panic info ...
    halt()  // Loops forever
}
```

When thread 0 panicked (e.g., during format!), it entered `halt()` and stopped:
- Cleanup never ran
- Zombies accumulated  
- System degraded over time

## Fixes Applied

### Fix 1: Cleanup Race Condition
**File:** `src/threading.rs`

```rust
// Before: TERMINATED -> FREE (race window)
// After: TERMINATED -> INITIALIZING -> FREE (no race)

if THREAD_STATES[i].compare_exchange(
    thread_state::TERMINATED,
    thread_state::INITIALIZING,  // Block spawns during cleanup
    Ordering::SeqCst,
    Ordering::SeqCst,
).is_ok() {
    // ... zero context, clear metadata ...
    
    // NOW set to FREE - cleanup complete
    THREAD_STATES[i].store(thread_state::FREE, Ordering::SeqCst);
}
```

Spawn's `claim_free_slot` uses `compare_exchange(FREE, INITIALIZING)`, so it fails while cleanup is running.

### Fix 2: Zeroed Context Detection
**File:** `src/threading.rs`

```rust
// Only consider a thread "new" if SPSR is the expected kernel mode
let is_valid_new_thread = new_saved_elr == 0 && new_saved_spsr == 0x00000005;

// Detect zeroed contexts and terminate them
if new_idx < 8 && !is_valid_new_thread && is_user_spsr {
    let is_zeroed_context = new_saved_spsr == 0 && new_saved_x30 == 0;
    
    if is_zeroed_context {
        // Context was zeroed - thread is unusable
        crate::console::print("[SGI CORRUPT] system thread has zeroed context - terminating\n");
        THREAD_STATES[new_idx].store(thread_state::TERMINATED, Ordering::SeqCst);
        return; // Don't switch to this thread
    } else {
        // Just SPSR corruption, try to recover
        (*get_context_mut(new_idx)).spsr = 0x00000345; // EL1h
    }
}
```

### Fix 3: No Heap Allocation in IRQ
**File:** `src/threading.rs`

Replaced all `alloc::format!` in `sgi_scheduler_handler` with static strings:
```rust
// Before
crate::console::print(&alloc::format!("[CANARY CORRUPT] Thread {} ...", idx));

// After
crate::console::print("[CANARY] old thread stack corrupt\n");
```

### Fix 4: Thread 0 Panic Resilience
**File:** `src/main.rs`, `src/console.rs`

Added non-allocating number printing:
```rust
// console.rs
pub fn print_u64(n: u64) {
    let mut buf = [0u8; 20];
    // ... safe, non-panicking implementation ...
}

// main.rs - Thread 0's heartbeat
console::print("[Thread0] loop=");
console::print_u64(loop_counter);
console::print(" | zombies=");
console::print_u64(threading::thread_stats().2 as u64);
console::print("\n");
```

No `format!` macro = no panic possible in the critical path.

## Architecture: THREAD_CONTEXTS

The THREAD_CONTEXTS static array was introduced to enable lock-free context access:

```rust
static THREAD_CONTEXTS: [SyncContext; config::MAX_THREADS] = { ... };

fn get_context_mut(idx: usize) -> *mut Context {
    THREAD_CONTEXTS[idx].get()
}
```

**Benefits:**
1. No lock needed during `switch_context` - prevents deadlock
2. Contexts accessible without holding POOL lock
3. Scheduler can safely access contexts from IRQ context

**Safety invariants:**
1. Only scheduler (with IRQs masked) modifies contexts during switch
2. A thread's context is only accessed when that thread is NOT running
3. Context must be fully initialized before state becomes READY
4. Context is zeroed when state becomes FREE (via INITIALIZING)

## Remaining Issues

### Memory Leak (Under Investigation)
```
Potentially leaked frames: 509
ELF Loader: 400 pages
User Page Table: 72 pages
User Data: 37 pages
```

When processes terminate, their ELF and page table memory may not be properly freed. This needs investigation in the process cleanup path.

### Potential Causes:
1. Thread cleanup doesn't trigger process memory cleanup
2. Process exit doesn't free all allocated pages
3. Zombie threads hold references preventing cleanup

## Testing Results

After fixes:
- System runs for 4+ million iterations (was crashing within thousands)
- Thread 0 continues running after format errors
- Corrupted contexts are detected and terminated safely
- Cleanup race condition eliminated

## Files Modified

- `src/threading.rs`: Cleanup race fix, zeroed context detection, no-alloc prints
- `src/main.rs`: Thread 0 panic resilience
- `src/console.rs`: Added `print_u64()` for safe number printing

## Lessons Learned

1. **State machine transitions must be atomic** - The cleanup race showed that multi-step cleanup with state changes in between allows race conditions.

2. **Never allocate in IRQ handlers** - Heap allocation can deadlock or corrupt state when the main thread was also allocating.

3. **Validate context before switching** - Corrupted contexts should be detected and handled, not blindly switched to.

4. **Critical threads need resilience** - Thread 0 running cleanup should not halt the system on recoverable errors.

5. **Zero is not a valid new-thread marker** - A zeroed context (SPSR=0) is corrupted, not initialized. Valid new threads have SPSR=0x00000005.

6. **NEVER return early from sgi_scheduler_handler after schedule_indices** - This is CRITICAL:
   ```
   schedule_indices() updates CURRENT_THREAD BEFORE returning.
   
   If you detect corruption and call `return`:
   - CURRENT_THREAD points to new_idx
   - But you're still running on old_idx's stack!
   - Next timer save goes to WRONG slot
   - Everything corrupts
   ```
   Always continue to switch_context. Fix the context as best you can, but never abort mid-switch.
