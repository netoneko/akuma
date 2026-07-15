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

---

## Bug 3: PMM Spinlock without IRQ Protection (January 2026)

### Symptoms

Potential deadlock during parallel process execution. If a timer fires while a thread holds the PMM lock, the scheduler switches to another thread. If that thread tries to allocate/free pages, it spins forever waiting for the lock held by the preempted thread.

While primarily a deadlock issue, the unpredictable timing could contribute to corruption symptoms when combined with other issues.

### Root Cause

PMM functions (`alloc_page`, `free_page`, etc.) used `Spinlock` without disabling IRQs:

```rust
// BEFORE (buggy):
pub fn alloc_page() -> Option<PhysFrame> {
    let mut pmm = PMM.lock();  // Timer can fire here!
    // ...
}
```

### Fix

Wrapped all PMM operations in `with_irqs_disabled()`:

```rust
pub fn alloc_page() -> Option<PhysFrame> {
    crate::irq::with_irqs_disabled(|| {
        let mut pmm = PMM.lock();
        // ...
    })
}
```

**File**: `src/pmm.rs`

---

## Bug 4: talc_realloc Gap in IRQ Protection (January 2026)

### Symptoms

Heap corruption during parallel execution with many reallocations (Vec growth, String push_str, etc.).

### Root Cause

`talc_realloc` called `talc_alloc` and `talc_dealloc` which were individually IRQ-protected, but the memory copy between them was not:

```rust
// BEFORE (buggy):
unsafe fn talc_realloc(...) {
    let new_ptr = talc_alloc(new_layout);     // IRQs disabled during this
    ptr::copy_nonoverlapping(ptr, new_ptr, copy_size);  // IRQs ENABLED here!
    talc_dealloc(ptr, layout);                 // IRQs disabled during this
}
```

While the heap metadata stayed consistent (alloc/dealloc are atomic), the timing window during the copy could cause subtle issues when combined with context switches.

### Fix

Wrapped the entire realloc operation in a single `with_irqs_disabled()` block:

```rust
unsafe fn talc_realloc(...) {
    with_irqs_disabled(|| {
        // Inline alloc, copy, and dealloc - all atomic
    })
}
```

**File**: `src/allocator.rs`

---

## Bug 5: PROCESS_TABLE Lock without IRQ Protection (January 2026)

### Symptoms

Potential deadlock when `list_processes()` (used by `ps` command) is called during parallel execution.

### Fix

Added `with_irqs_disabled()` wrapper to `list_processes()`:

```rust
pub fn list_processes() -> Vec<ProcessInfo2> {
    crate::irq::with_irqs_disabled(|| {
        let table = PROCESS_TABLE.lock();
        // ... collect process info ...
    })
}
```

**File**: `src/process.rs`

---

## Summary of All Fixes

| Bug | Issue | File | Fix |
|-----|-------|------|-----|
| 1 | TTBR0 check before user address access | `process.rs` | Check TTBR0 in `read_current_pid()` |
| 2 | Asymmetric IRQ protection in allocator | `allocator.rs` | Wrap `talc_alloc` in `with_irqs_disabled()` |
| 3 | PMM lock without IRQ protection | `pmm.rs` | Wrap all PMM functions in `with_irqs_disabled()` |
| 4 | Realloc gap in IRQ protection | `allocator.rs` | Wrap entire realloc in single `with_irqs_disabled()` |
| 5 | PROCESS_TABLE lock without IRQ protection | `process.rs` | Wrap `list_processes()` in `with_irqs_disabled()` |

The key principle: **Any Spinlock acquisition must happen with IRQs disabled** to prevent deadlock when a timer fires and the scheduler tries to switch threads.

---

## Bug 6: Missing TLB Flush in activate() (January 2026)

### Symptoms

External abort on translation table walk (DFSC=0x21) during memory access after switching address spaces.
FAR=0x5 with ISS=0x61 indicates the MMU failed to read the L1 page table during translation.

### Root Cause

When switching TTBR0 from boot page tables to user address space (in `activate()`), the TLB was not flushed. The `deactivate()` function correctly flushed TLB, but `activate()` did not:

```rust
// BEFORE (buggy):
pub fn activate(&self) {
    let ttbr0 = self.ttbr0();
    unsafe {
        core::arch::asm!(
            "msr ttbr0_el1, {ttbr0}",
            "isb",
            // No TLB flush!
        );
    }
}
```

Stale TLB entries from the previous address space could cause:
- Wrong physical addresses being accessed
- External aborts during translation table walk
- Permission faults

### Fix

Added `dsb ishst` before the switch and `flush_tlb_all()` after:

```rust
pub fn activate(&self) {
    let ttbr0 = self.ttbr0();
    unsafe {
        core::arch::asm!(
            "dsb ishst",           // Ensure previous writes complete
            "msr ttbr0_el1, {ttbr0}",
            "isb",
        );
    }
    flush_tlb_all();               // Remove stale entries
}
```

**File**: `src/mmu.rs`

---

## Updated Summary of All Fixes

| Bug | Issue | File | Fix |
|-----|-------|------|-----|
| 1 | TTBR0 check before user address access | `process.rs` | Check TTBR0 in `read_current_pid()` |
| 2 | Asymmetric IRQ protection in allocator | `allocator.rs` | Wrap `talc_alloc` in `with_irqs_disabled()` |
| 3 | PMM lock without IRQ protection | `pmm.rs` | Wrap all PMM functions in `with_irqs_disabled()` |
| 4 | Realloc gap in IRQ protection | `allocator.rs` | Wrap entire realloc in single `with_irqs_disabled()` |
| 5 | PROCESS_TABLE lock without IRQ protection | `process.rs` | Wrap `list_processes()` in `with_irqs_disabled()` |
| 6 | Missing TLB flush in activate() | `mmu.rs` | Add `flush_tlb_all()` after TTBR0 switch |
| 7 | Missing TLB flush in switch_context | `threading.rs` | Add TLBI after TTBR0 switch in asm |
| 8 | Incomplete TLB flush in activate/deactivate | `mmu.rs` | Flush TLB both BEFORE and AFTER switch |
| 9 | ProcessChannel lock without IRQ protection | `process.rs` | Wrap write/try_read/read_all |
| 10 | PROCESS_TABLE ops without IRQ protection | `process.rs` | Wrap register/unregister/lookup |
| 11 | PROCESS_CHANNELS ops without IRQ protection | `process.rs` | Wrap register/get/remove_channel |

---

## Bug 7: Missing TLB Flush in switch_context (January 2026)

### Symptoms

External abort on translation table walk (DFSC=0x21) occurring intermittently after context switches.
Page tables appear valid in crash handler, but fault occurs during access.

### Root Cause

The `switch_context` assembly was switching TTBR0 without TLB flush:

```asm
// BEFORE:
ldr x9, [x1, #128]
msr ttbr0_el1, x9
isb
ret  // No TLB flush!
```

### Fix

Added full TLB flush sequence:

```asm
ldr x9, [x1, #128]
dsb ish
msr ttbr0_el1, x9
isb
tlbi vmalle1
dsb ish
isb
ret
```

**File**: `src/threading.rs`

---

## Bug 8: Incomplete TLB Flush in activate/deactivate

### Fix

Modified both functions to flush TLB BEFORE and AFTER the TTBR0 switch for maximum safety.

**File**: `src/mmu.rs`

---

## Bugs 9-11: ProcessChannel and Tables Without IRQ Protection

### Symptoms

Garbled output (0xFF bytes in output stream), heap corruption.

### Fix

Wrapped all Spinlock operations in `with_irqs_disabled()`:
- `ProcessChannel::write`, `try_read`, `read_all`
- `register_process`, `unregister_process`, `lookup_process`
- `register_channel`, `get_channel`, `remove_channel`

**File**: `src/process.rs`
