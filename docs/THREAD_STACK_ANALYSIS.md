# Thread Stack Analysis

This document analyzes stack usage in the kernel's threading system and provides guidance on implementing stack guard pages.

## Current Stack Configuration

### Boot Thread (Thread 0)

The boot stack is configured in `src/boot.rs`:

```asm
.equ STACK_SIZE,        0x100000        // 1MB stack
.equ STACK_TOP,         0x42000000      // 32MB from kernel base

_boot:
    ldr     x0, =STACK_TOP
    mov     sp, x0
```

| Property | Value |
|----------|-------|
| Stack top | `0x42000000` (32MB from kernel base) |
| Stack size | 1MB (grows down to `0x41F00000`) |
| Location | Above kernel binary (~3MB), below heap |

**Status**: ✅ Fixed - Stack is now placed above the kernel binary.

### Spawned Threads (1-31)

Spawned threads get stacks from the heap in `src/threading.rs`:

```rust
const STACK_SIZE: usize = 32 * 1024;  // 32KB
const MAX_THREADS: usize = 32;

pub fn init(&mut self) {
    for i in 1..MAX_THREADS {
        let stack_vec: Vec<u8> = alloc::vec![0u8; STACK_SIZE];
        let stack_box = stack_vec.into_boxed_slice();
        let stack_ptr = Box::into_raw(stack_box) as *mut u8;
        self.stacks[i] = stack_ptr as usize;
    }
}
```

| Property | Value |
|----------|-------|
| Stack size | 32KB per thread |
| Max threads | 32 (thread 0 + 31 spawned) |
| Total memory | ~1MB for all thread stacks |
| Allocation | Heap via `Vec<u8>` |
| Guard pages | ❌ None |

### User Process Stacks

User processes get stacks mapped via the ELF loader in `src/elf_loader.rs`:

```rust
const STACK_TOP: usize = 0x4000_0000;  // Top of first 1GB
let stack_bottom = STACK_TOP - stack_size;  // Default: 64KB
```

| Property | Value |
|----------|-------|
| Stack size | 64KB default |
| Location | Top of first 1GB (`0x3FF00000-0x40000000`) |
| Allocation | Via MMU page mapping |
| Guard pages | ❌ None (could be added) |

## Stack Overflow Risks

### Current Situation

| Stack | Size | Guard Page | Risk |
|-------|------|------------|------|
| Boot stack | 1MB | ❌ | Low - large size, well-positioned |
| Thread stacks | 32KB | ❌ | Medium - heap allocated, no isolation |
| User stacks | 64KB | ❌ | Medium - could add guard pages |

### Why No Guard Pages?

**Boot stack**: Uses kernel 1GB block mapping (no fine-grained page control).

**Thread stacks**: Allocated via heap (`Vec<u8>`), not via page allocator. All heap memory uses the same 1GB block mapping.

**User stacks**: Could have guard pages since they use 4KB page mappings, but currently don't.

### Overflow Consequences

Without guard pages, stack overflow causes **silent memory corruption**:
- Boot stack overflow → corrupts heap
- Thread stack overflow → corrupts adjacent heap allocations
- User stack overflow → corrupts adjacent user memory regions

## Async Execution and Stack Depth

The main async polling loop runs on thread 0 (boot thread):

```rust
// In src/main.rs run_async_main()
loop {
    let _ = runner_pinned.as_mut().poll(&mut cx);        // Network
    let _ = ssh_pinned.as_mut().poll(&mut cx);           // SSH server
    let _ = web_pinned.as_mut().poll(&mut cx);           // HTTP server
    // ...
}
```

### Async Call Chain Depth

Each `poll()` creates a call chain. Complex async code = deep stacks:

| Component | async/await points | Typical call depth |
|-----------|-------------------|-------------------|
| SSH Protocol | ~100 | Deep (crypto, protocol, shell) |
| Shell Commands | ~200 | Deep (command execution, I/O) |
| Web Server | ~20 | Medium |
| Network Runner | ~40 | Medium |

### Stack Safety

**Future state**: Stored on heap (via `Box::pin()` or `pin!` macro) - doesn't consume stack.

**Poll chains**: Use thread stack - can be deep with complex async code.

The 1MB boot stack should handle current async complexity, but has no safety margin against overflow.

---

## Implementing Guard Pages

### Strategy Overview

Guard pages are unmapped pages placed at stack boundaries. Accessing them triggers a page fault instead of silent corruption.

```
┌───────────────────┐ High address
│   Stack data      │
│        ↓          │ (grows down)
│   (used stack)    │
├───────────────────┤
│   (unused stack)  │
├───────────────────┤
│   GUARD PAGE      │ ← Unmapped - triggers fault on access
├───────────────────┤
│   Other memory    │ Low address
└───────────────────┘
```

### Option 1: Guard Pages for User Stacks (Easiest)

User stacks already use 4KB page mappings via `UserAddressSpace`. Adding a guard page is straightforward.

**In `src/elf_loader.rs`:**

```rust
pub fn load_elf_with_stack(
    elf_data: &[u8],
    stack_size: usize,
) -> Result<(usize, UserAddressSpace, usize, usize, usize, usize), ElfError> {
    let mut loaded = load_elf(elf_data)?;

    const STACK_TOP: usize = 0x4000_0000;
    const PAGE_SIZE: usize = 4096;
    
    // Reserve one page for guard
    let stack_with_guard = stack_size + PAGE_SIZE;
    let stack_bottom = STACK_TOP - stack_with_guard;
    let stack_bottom_aligned = stack_bottom & !(PAGE_SIZE - 1);
    
    // Guard page is at stack_bottom_aligned (DON'T map it)
    let guard_page = stack_bottom_aligned;
    let actual_stack_bottom = guard_page + PAGE_SIZE;
    
    // Only map pages ABOVE the guard page
    let stack_pages = stack_size / PAGE_SIZE;
    for i in 0..stack_pages {
        let page_va = actual_stack_bottom + i * PAGE_SIZE;
        loaded.address_space
            .alloc_and_map(page_va, user_flags::RW_NO_EXEC)
            .map_err(|e| ElfError::MappingFailed(e))?;
    }
    
    // Stack usable region: actual_stack_bottom to STACK_TOP
    let stack_end = actual_stack_bottom + stack_pages * PAGE_SIZE;
    let initial_sp = (stack_end - 16) & !0xF;
    
    Ok((loaded.entry_point, loaded.address_space, initial_sp, 
        loaded.brk, actual_stack_bottom, stack_end))
}
```

**Result**: User stack overflow triggers `EC=0x24` (Data Abort from EL0) instead of corruption.

### Option 2: Guard Pages for Thread Stacks (Medium Difficulty)

Thread stacks are currently heap-allocated. To add guard pages:

1. Allocate via PMM (page allocator) instead of heap
2. Leave one page unmapped at the bottom

**Modify `src/threading.rs`:**

```rust
use crate::pmm;
use crate::mmu::PAGE_SIZE;

const STACK_SIZE: usize = 32 * 1024;  // 32KB = 8 pages
const STACK_PAGES: usize = STACK_SIZE / PAGE_SIZE;
const GUARD_PAGES: usize = 1;
const TOTAL_PAGES: usize = STACK_PAGES + GUARD_PAGES;

pub fn init(&mut self) {
    self.slots[IDLE_THREAD_IDX].state = ThreadState::Running;
    self.stacks[IDLE_THREAD_IDX] = 0;  // Boot stack

    for i in 1..MAX_THREADS {
        // Allocate contiguous pages for stack + guard
        if let Some(frame) = pmm::alloc_pages_zeroed(TOTAL_PAGES) {
            // Guard page at bottom (frame.addr)
            // Stack pages above it (frame.addr + PAGE_SIZE)
            let stack_base = frame.addr + (GUARD_PAGES * PAGE_SIZE);
            self.stacks[i] = stack_base;
        } else {
            // Fallback to heap allocation without guard
            let stack_vec: Vec<u8> = alloc::vec![0u8; STACK_SIZE];
            let stack_ptr = Box::into_raw(stack_vec.into_boxed_slice()) as *mut u8;
            self.stacks[i] = stack_ptr as usize;
        }
    }
    self.initialized = true;
}
```

**Problem**: The kernel uses 1GB block mapping - ALL physical memory in the kernel region is mapped. The guard page is still accessible!

**Solution**: Need to switch kernel region to 4KB page mappings (see Option 3).

### Option 3: Fine-Grained Kernel Page Tables (Full Solution)

To have working guard pages in kernel space, the kernel needs to use 4KB page mappings instead of 1GB blocks.

**Changes required:**

1. **Modify boot page tables** (`src/boot.rs`):
   - Instead of 1GB block for L1[1], create L2 table
   - Map kernel code/data/heap as 4KB pages with proper permissions
   - Leave guard pages unmapped

2. **Modify MMU init** (`src/mmu.rs`):
   - Add functions to manage kernel page mappings
   - `map_kernel_page(va, pa, flags)` 
   - `unmap_kernel_page(va)`

3. **Modify threading** (`src/threading.rs`):
   - Allocate stack pages via PMM
   - Call `unmap_kernel_page()` for guard page addresses

**Example kernel page table setup:**

```rust
// In src/mmu.rs - new function
pub fn setup_kernel_page_tables() -> Result<(), &'static str> {
    // Get current L1 table address from boot code
    let boot_l1_addr: usize = unsafe {
        // Read from boot_ttbr0_addr
        // ...
    };
    
    // Allocate L2 table for kernel 1GB region (0x40000000-0x7FFFFFFF)
    let l2_frame = pmm::alloc_page_zeroed()
        .ok_or("Failed to allocate L2 for kernel")?;
    
    // Each L2 entry covers 2MB (512 entries × 2MB = 1GB)
    // For fine-grained control, we need L3 tables too
    
    // Map kernel sections with appropriate permissions:
    // .text:   Read-only, Executable
    // .rodata: Read-only, No-execute  
    // .data/.bss: Read-write, No-execute
    // Stack:   Read-write, No-execute (with guard page unmapped)
    // Heap:    Read-write, No-execute
    
    // ... implementation ...
    
    Ok(())
}

pub unsafe fn unmap_kernel_page(va: usize) {
    // Walk kernel page tables
    // Clear the L3 entry for this VA
    // Flush TLB
    flush_tlb_page(va);
}
```

**Trade-offs**:
- More complex boot code
- Slight TLB pressure (more entries needed)
- But: Stack overflow detection, code protection, better security

### Option 4: Stack Canaries (Software Detection)

If hardware guard pages are too complex, use software detection:

```rust
const STACK_CANARY: u64 = 0xDEAD_BEEF_CAFE_BABE;

pub fn init(&mut self) {
    for i in 1..MAX_THREADS {
        let stack_vec: Vec<u8> = alloc::vec![0u8; STACK_SIZE];
        let stack_ptr = Box::into_raw(stack_vec.into_boxed_slice()) as *mut u8;
        
        // Write canary at bottom of stack
        unsafe {
            let canary_ptr = stack_ptr as *mut u64;
            for j in 0..8 {  // 64 bytes of canaries
                canary_ptr.add(j).write_volatile(STACK_CANARY);
            }
        }
        
        self.stacks[i] = stack_ptr as usize;
    }
}

pub fn check_stack_overflow(&self, thread_idx: usize) -> bool {
    if thread_idx == 0 || thread_idx >= MAX_THREADS {
        return false;
    }
    
    let stack_base = self.stacks[thread_idx];
    if stack_base == 0 {
        return false;
    }
    
    unsafe {
        let canary_ptr = stack_base as *const u64;
        for j in 0..8 {
            if canary_ptr.add(j).read_volatile() != STACK_CANARY {
                return true;  // Overflow detected!
            }
        }
    }
    false
}
```

Call `check_stack_overflow()` periodically or on context switch.

**Limitation**: Detects overflow after it happens, not at the moment of overflow.

---

## Recommendations

### Immediate (Low Effort)

1. **Add user stack guard pages** (Option 1)
   - Simple change to `elf_loader.rs`
   - Catches user process stack overflows

2. **Add stack canaries** (Option 4)
   - Software detection for thread stacks
   - Check on context switch or periodically

### Medium Term

3. **Increase thread stack size**
   ```rust
   const STACK_SIZE: usize = 64 * 1024;  // 64KB
   ```
   - Reduces overflow risk
   - Trade-off: MAX_THREADS or heap usage

### Long Term

4. **Implement fine-grained kernel page tables** (Option 3)
   - Full hardware protection for kernel code and stacks
   - Significant refactoring of boot and MMU code
   - Enables proper W^X (write XOR execute) policy

---

## Related Files

- `src/boot.rs` - Boot stack setup
- `src/threading.rs` - Thread pool and stack allocation
- `src/elf_loader.rs` - User stack setup
- `src/mmu.rs` - Page table management
- `src/pmm.rs` - Physical page allocation
- `linker.ld` - Kernel section symbols (`_text_start`, `_kernel_phys_end`, etc.)
