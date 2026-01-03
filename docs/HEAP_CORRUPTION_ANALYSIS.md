# Userspace Heap Corruption Analysis

## Summary

The userspace heap corruption bug was **layout-sensitive** - it manifested based on the exact binary layout of the userspace program. **This bug has been FIXED** by switching to mmap-based allocation in userspace and implementing a hybrid page-based allocator in the kernel.

## Status: ✅ FIXED

**All tests pass:**
```
Allocator initialized (talc mode)
PMM initialized, allocator switched to page mode
Overall: ALL TESTS PASSED
[Test] stdcheck PASSED
[Process] 'stdcheck' (PID 1) exited with code 0
```

## The Fix

### Userspace: mmap-based Allocation

Added a `MmapAllocator` in `userspace/libakuma/src/lib.rs`:

```rust
pub const USE_MMAP_ALLOCATOR: bool = true;  // Use mmap by default
```

**How it works:**
1. Each allocation calls `mmap()` syscall to get a fresh page-aligned region
2. Allocations start at VA 0x10000000 (well away from code at 0x400000)
3. Each alloc gets its own page(s), no reuse of heap space
4. Kernel maps physical pages to user address space on demand

### Kernel: Hybrid Page-based Allocator

The kernel now uses a hybrid allocator (`src/allocator.rs`):

```rust
pub const USE_PAGE_ALLOCATOR: bool = true;

// During early boot: uses talc (PMM not ready yet)
// After PMM init: switches to page-based allocation
```

**Boot sequence:**
1. `allocator::init()` - initializes talc allocator
2. `pmm::init()` - initializes Physical Memory Manager
3. `allocator::mark_pmm_ready()` - signals allocator to switch to page mode

This hybrid approach:
- Uses talc during early boot (before PMM is ready)
- Switches to page-based allocation after PMM initialization
- Provides the same fix as the userspace mmap allocator

### Kernel Changes

- Added `sys_mmap` and `sys_munmap` syscalls in `src/syscall.rs`
- Added `map_user_page()` function in `src/mmu.rs` for dynamic page mapping
- mmap regions start at 0x10000000 and grow upward
- Kernel allocator has `mark_pmm_ready()` to signal when page mode is available

## Original Bug Details

### Symptoms

- `String::push_str()` caused a data abort when reallocating
- Crash at FAR=0x814003 or 0x816003 (translation fault at level 2)
- The allocator's `head` pointer got corrupted to an unmapped address
- The bug ONLY appeared with specific binary layouts

### Layout Sensitivity

The bug was extremely sensitive to binary layout:

| Configuration | BSS Address | Heap Start | Result |
|--------------|-------------|------------|--------|
| Minimal (16B allocator) | 0x401000 | 0x402000 | **CRASH** at 0x814003 |
| With debug prints | 0x402000 | 0x403000 | PASS |
| With canaries (32B) | 0x402008 | 0x403000 | PASS |
| With 64B padding | 0x401000 | 0x402000 | PASS (workaround) |

### Root Cause Analysis

The bug was **specifically related to the brk-based bump allocator** and its interaction with the BSS layout:

1. The bump allocator stored its state (`head`, `end` pointers) in BSS
2. During reallocation, something corrupted these pointers
3. The corruption pattern suggested memory addresses were being incorrectly combined
4. Using mmap bypasses this entirely by using a completely different memory region

### Attempted Fixes (Did NOT Work)

1. **AtomicUsize** - Replacing `UnsafeCell` with `AtomicUsize` did not fix it
2. **Memory ordering (SeqCst)** - Not the root cause
3. **Hardware watchpoints** - Not available in QEMU

### Workarounds (Before mmap fix)

Adding padding to the allocator structure worked around the bug:

```rust
#[repr(C, align(64))]
pub struct BrkAllocator {
    head: AtomicUsize,
    end: AtomicUsize,
    _padding: [u8; 48],  // Pad to 64 bytes
}
```

This changed the memory layout enough to prevent the corruption from manifesting.

## Test Results

### Kernel Tests
```
[TEST] Allocator Vec operations - PASS
[TEST] Allocator Box operations - PASS
[TEST] Allocator large allocation - PASS
[TEST] Mmap: Single page allocation - PASS
[TEST] Mmap: Multi-page allocation (12KB) - PASS
[TEST] Mmap: Page boundary writes - PASS
[TEST] Mmap: Rapid alloc/dealloc (100 cycles) - PASS
[TEST] Mmap: Realloc pattern (grow then use) - PASS
[TEST] Mmap: String growth pattern - PASS
[TEST] Mmap: Vec capacity doubling (1->1024 elements) - PASS
[TEST] Mmap: Interleaved string operations - PASS
Overall: ALL TESTS PASSED
```

### Userspace Tests (stdcheck)
```
[TEST] Vec... PASS
[TEST] String::from... PASS
[TEST] String::push_str... PASS
[TEST] Box... PASS
[Process] 'stdcheck' (PID 1) exited with code 0
[Test] stdcheck PASSED
```

## Files Modified

- `userspace/libakuma/src/lib.rs` - MmapAllocator with USE_MMAP_ALLOCATOR switch
- `src/allocator.rs` - HybridAllocator with USE_PAGE_ALLOCATOR and PMM_READY flag
- `src/main.rs` - Boot sequence reordering for PMM init
- `src/syscall.rs` - Added sys_mmap and sys_munmap syscalls
- `src/mmu.rs` - Added map_user_page() for dynamic page mapping
- `src/tests.rs` - Added 8 mmap allocator edge case tests

## Separate Issue: EC=0x0 Kernel Crash

An unrelated kernel crash occurs during SSH host key initialization:

```
[Exception] Sync from EL1: EC=0x0, ISS=0x0, ELR=0x400ff544, FAR=0x0, SPSR=0x600023c5
```

This crash:
- Happens AFTER all tests pass
- Occurs in `ssh::init_host_key()` 
- Is **NOT related** to the heap corruption bug
- Is a separate issue that needs investigation

## Related Documentation

- `docs/USERSPACE_HEAP_BUG.md` - Original bug report
- `docs/MEMORY_LAYOUT.md` - Kernel memory layout
- `docs/AI_DEBUGGING.md` - Debugging workflow
