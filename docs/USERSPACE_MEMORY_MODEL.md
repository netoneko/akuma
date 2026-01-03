# Userspace Memory Model

This document explains how memory addressing works in userspace (libakuma) and why userspace code does **not** need physical/virtual address translation.

## Overview

Userspace programs operate entirely in virtual address space. The MMU hardware transparently translates virtual addresses to physical addresses. Userspace code never sees or needs to handle physical addresses.

## Memory Flow: mmap Example

```
┌─────────────────────────────────────────────────────────────────┐
│                      USERSPACE (libakuma)                       │
├─────────────────────────────────────────────────────────────────┤
│ 1. Call mmap(0, 4096, PROT_READ|PROT_WRITE, MAP_ANONYMOUS)      │
│ 2. Receive VA 0x10000000                                        │
│ 3. Use pointer directly: *ptr = value                           │
│    └─► MMU translates VA → PA automatically                     │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼ syscall (svc #0)
┌─────────────────────────────────────────────────────────────────┐
│                         KERNEL                                   │
├─────────────────────────────────────────────────────────────────┤
│ sys_mmap():                                                      │
│   1. Choose VA: 0x10000000 (from NEXT_MMAP_ADDR)                │
│   2. Allocate PA: pmm::alloc_page_zeroed() → 0x47001000         │
│   3. Map in page tables: map_user_page(VA, PA)                  │
│   4. Return VA to userspace                                      │
│                                                                  │
│ Page Table Entry:                                                │
│   VA 0x10000000 → PA 0x47001000 | VALID | USER | RW              │
└─────────────────────────────────────────────────────────────────┘
```

## Why Userspace Doesn't Need Translation

### 1. Userspace Only Sees Virtual Addresses

All syscalls that return memory addresses return **virtual addresses**:

| Syscall | Returns |
|---------|---------|
| `mmap()` | Virtual address of mapped region |
| `brk()` | Virtual address of heap end |

Userspace code uses these VAs directly as pointers.

### 2. MMU Translation is Transparent

When userspace code dereferences a pointer:

```rust
let ptr = mmap(0, 4096, ...) as *mut u8;
unsafe { *ptr = 42; }  // MMU translates VA → PA automatically
```

The CPU's MMU uses the page tables (set up by the kernel) to translate the VA to a PA. This happens in hardware, invisible to software.

### 3. Kernel Handles All PA↔VA Translation

The kernel is responsible for:
- Allocating physical memory (PMM)
- Setting up page table mappings (VA → PA)
- Using `phys_to_virt()` when accessing physical memory
- Returning only VAs to userspace

## Userspace Address Space Layout

```
0x00000000 ┌─────────────────────────┐
           │ (unmapped)              │
0x00400000 ├─────────────────────────┤
           │ Code (.text)            │  ← ELF loaded here
           │ Data (.data, .bss)      │
           ├─────────────────────────┤
           │ Heap (brk-based)        │  ← Grows upward
           │         ↓               │
0x10000000 ├─────────────────────────┤
           │ mmap region             │  ← mmap allocations
           │         ↓               │
           │                         │
           │         ↑               │
0x3FFF0000 ├─────────────────────────┤
           │ Stack                   │  ← Grows downward
0x3FFFF000 └─────────────────────────┘
0x40000000 ┌─────────────────────────┐
           │ (kernel memory - not    │  ← User cannot access
           │  accessible from EL0)   │
           └─────────────────────────┘
```

## Cache Coherency

When the kernel allocates and zeros a page, it writes via the kernel's identity mapping (PA used as VA). The user then accesses the same physical page via a different VA.

To ensure coherency between these two VA mappings:

```rust
// In pmm::alloc_page_zeroed()
let virt_addr = phys_to_virt(frame.addr);
core::ptr::write_bytes(virt_addr, 0, PAGE_SIZE);

// Clean cache to ensure zeros are visible via user's VA
while addr < end {
    core::arch::asm!("dc cvac, {addr}", addr = in(reg) addr);
    addr += 64;  // Cache line size
}
core::arch::asm!("dsb ish");
```

This ensures that when userspace reads the newly mapped page, it sees the zeros written by the kernel.

## libakuma Allocator

The userspace allocator in libakuma uses mmap for page-based allocation:

```rust
// libakuma/src/lib.rs
unsafe fn mmap_alloc(&self, layout: Layout) -> *mut u8 {
    let addr = mmap(0, alloc_size, PROT_READ | PROT_WRITE, MAP_ANONYMOUS);
    if addr == MAP_FAILED { null_mut() } else { addr as *mut u8 }
}
```

Key points:
- `mmap()` returns a VA
- The allocator uses this VA directly as a pointer
- No translation needed - MMU handles it

## Common Misconceptions

### "Userspace needs virt_to_phys for DMA"

**False** - Userspace cannot do DMA. Only the kernel can set up DMA operations, and it handles all address translation internally.

### "mmap returns a physical address"

**False** - `mmap()` always returns a virtual address. The kernel translates it to physical when setting up page tables.

### "Userspace pointers are physical addresses"

**False** - All userspace pointers are virtual addresses. The MMU makes this transparent to the program.

## Debugging Tips

If you see unexpected behavior in userspace memory:

1. **Check if pages are mapped** - Use kernel debug output to verify mmap created page table entries
2. **Verify cache coherency** - Ensure kernel flushes cache after writing to pages
3. **Check VA range** - Ensure allocations don't overlap with code, stack, or kernel space
4. **Verify ELF loading** - Check that code/data segments are properly mapped before execution

## Related Documentation

- `docs/IDENTITY_MAPPING_DEPENDENCIES.md` - Kernel address translation
- `docs/MEMORY_LAYOUT.md` - Overall memory layout
- `docs/HEAP_CORRUPTION_ANALYSIS.md` - Allocator debugging

