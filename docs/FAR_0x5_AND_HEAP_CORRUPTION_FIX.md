# FAR=0x5 Crash and Heap Corruption Fix (January 2026)

## Summary

Two related bugs were causing intermittent kernel panics and memory corruption:

1. **FAR=0x5 crash**: Kernel dereferencing address 0x5 due to unsafe TTBR0 access
2. **Heap corruption**: Timer interrupts during allocation causing race conditions

## Bug 1: FAR=0x5 Kernel Panic

### Symptoms

```
[Exception] Sync from EL1: EC=0x25, ISS=0x61
  ELR=0x40293f90, FAR=0x5, SPSR=0x80000345
  Thread=0, TTBR0=0x402e5000, TTBR1=0x402e5000
  WARNING: Kernel accessing user-space address!
```

Crashes occurred during:
- Parallel process execution tests
- VFS directory listing (subdirectory_operations)
- ELF loading (external binary pipeline)
- Other random kernel operations

### Root Cause

`read_current_pid()` in `src/process.rs` reads from `PROCESS_INFO_ADDR` (0x1000) without checking if TTBR0 points to valid user page tables.

**Memory Layout Issue:**
- With **boot TTBR0** (0x402xxxxx): Address 0x1000 is in the device memory region (0x0-0x40000000)
- With **user TTBR0** (0x44xxxxxx+): Address 0x1000 is mapped to the process info page

When kernel code calls `read_current_pid()` with boot TTBR0 active:
1. Read from 0x1000 returns garbage (device memory, undefined behavior)
2. Garbage is interpreted as PID, looked up in PROCESS_TABLE
3. Lookup returns bad pointer or null
4. Dereferencing at offset 5 causes FAR=0x5

### ISS Decoding

- **ISS=0x61**: DFSC=0x21 (sync external abort), WnR=1 (write to device memory)
- **ISS=0x07**: DFSC=0x07 (translation fault level 3), WnR=0 (read from unmapped)

Both indicate bad pointer dereference, not a simple null pointer.

### Fix

Added TTBR0 validation before reading from user address space:

```rust
pub fn read_current_pid() -> Option<Pid> {
    // CRITICAL: Check TTBR0 before reading from user address space!
    let ttbr0: u64;
    unsafe {
        core::arch::asm!("mrs {}, ttbr0_el1", out(reg) ttbr0);
    }
    
    // Mask off ASID bits (upper 16 bits) to get physical address
    let ttbr0_addr = ttbr0 & 0x0000_FFFF_FFFF_FFFF;
    
    // Boot TTBR0 is in 0x402xxxxx range
    // User TTBR0 is in 0x44xxxxxx+ range
    if ttbr0_addr >= 0x4020_0000 && ttbr0_addr < 0x4400_0000 {
        return None; // Boot TTBR0 - no user process context
    }
    
    // Safe to read from PROCESS_INFO_ADDR
    let pid = unsafe { (*(PROCESS_INFO_ADDR as *const ProcessInfo)).pid };
    if pid == 0 { None } else { Some(pid) }
}
```

**File**: `src/process.rs`

---

## Bug 2: Heap Corruption During Concurrent Execution

### Symptoms

Garbled console output during parallel process execution:

```
  TID  STATE     STACK_BASE  STACK_SIZE  STACK_USED  CANARY  TYPE         NAME
   0  running   0x41f00000    1024 KB      2 KB  0%�����������������...
```

The output starts correctly but degrades into garbage bytes mid-string.

### Root Cause

Asymmetric IRQ handling in the allocator:

```rust
// BEFORE (buggy):
unsafe fn talc_alloc(layout: Layout) -> *mut u8 {
    // with_irqs_disabled(|| {  // <-- COMMENTED OUT!
        let result = TALC.lock().malloc(layout)...
    // })
}

unsafe fn talc_dealloc(ptr: *mut u8, layout: Layout) {
    with_irqs_disabled(|| {     // <-- ENABLED
        TALC.lock().free(...)
    })
}
```

**Race Condition:**
1. Thread A calls `talc_alloc()`, acquires TALC lock
2. Timer interrupt fires during allocation
3. Scheduler switches to Thread B
4. Thread B allocates (format!, Vec, etc.)
5. Both threads now operating on heap metadata concurrently
6. Heap corruption ensues

This explains why:
- `format!` returns garbage (heap-allocated String corrupted)
- Crashes have consistent FAR=0x5 but different ELR (corrupted pointers)
- Issues are intermittent and timing-dependent

### Fix

Re-enabled IRQ protection for `talc_alloc()`:

```rust
unsafe fn talc_alloc(layout: Layout) -> *mut u8 {
    with_irqs_disabled(|| {
        let result = TALC.lock().malloc(layout)
            .map(|ptr| ptr.as_ptr())
            .unwrap_or(ptr::null_mut());
        // ... stats tracking ...
        result
    })
}
```

**File**: `src/allocator.rs`

---

## Why Both Bugs Together?

The bugs are related through heap corruption:

1. Heap corruption from Bug 2 corrupts various kernel data structures
2. Corrupted pointers have unpredictable values (like 5)
3. Dereferencing corrupted pointers causes FAR=0x5 crashes
4. Bug 1 provides a *deterministic* path to FAR=0x5 (bad TTBR0 + PROCESS_INFO read)

Fixing Bug 1 alone reduced crashes but didn't eliminate them (heap corruption still created bad pointers). Fixing Bug 2 ensures heap integrity, preventing the random corruption that made debugging difficult.

---

## Testing

After fixes, verify:

1. **Parallel process execution** - No FAR=0x5 crashes
2. **kthreads command during parallel execution** - Clean output, no garbage
3. **VFS operations** - Directory listing works reliably
4. **Shell pipeline tests** - External binary execution succeeds
5. **Thread tests** - mixed_cooperative_preemptible passes

---

## Lessons Learned

1. **Always check TTBR0 before user address access** - The kernel and user share the same virtual address space layout, but different page tables. Address 0x1000 means different things depending on TTBR0.

2. **Symmetric IRQ protection** - If deallocation disables IRQs, allocation must too. Asymmetric protection is a recipe for subtle race conditions.

3. **Heap corruption manifests randomly** - The symptom (FAR=0x5) was consistent, but the code path varied. This is a hallmark of heap corruption.

4. **Device memory reads are undefined** - Reading from device memory regions without actual hardware returns garbage, not a clean fault.
