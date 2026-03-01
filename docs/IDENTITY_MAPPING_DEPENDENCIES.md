# Identity Mapping Dependencies

This document catalogs places in the kernel that rely on **physical == virtual address identity mapping** and documents the translation functions used to make this explicit.

## Current Memory Model

The kernel uses identity mapping set up at boot time:

| L1 Entry | Address Range | Description |
|----------|--------------|-------------|
| L1[0] | 0x00000000 - 0x3FFFFFFF | Device memory (GIC, UART, VirtIO) |
| L1[1] | 0x40000000 - 0x7FFFFFFF | RAM block 1 |
| L1[2] | 0x80000000 - 0xBFFFFFFF | RAM block 2 |

This allows the kernel to use physical addresses directly as pointers via translation functions.

## Translation Functions

The kernel provides explicit translation functions in `src/mmu.rs`:

```rust
/// Physical to virtual address translation
pub fn phys_to_virt(paddr: usize) -> *mut u8 {
    // Identity mapping: VA == PA for kernel memory (0x40000000+)
    paddr as *mut u8
}

/// Virtual to physical address translation
pub fn virt_to_phys(vaddr: usize) -> usize {
    // Identity mapping: PA == VA for kernel memory
    vaddr
}
```

These functions currently implement identity mapping but provide a single point of change if the memory model is updated in the future.

---

## Status: ✅ All Issues Fixed

All identified physical/virtual address translation issues have been fixed in the codebase. The following sections document where translation functions are used.

### 1. PMM `alloc_page_zeroed()` - ✅ Fixed

**File:** `src/pmm.rs:290-316`

```rust
pub fn alloc_page_zeroed() -> Option<PhysFrame> {
    use crate::mmu::phys_to_virt;
    
    let frame = alloc_page()?;
    unsafe {
        let virt_addr = phys_to_virt(frame.addr);  // ✅ Uses translation
        core::ptr::write_bytes(virt_addr, 0, PAGE_SIZE);
        
        // Clean data cache for coherency with other VA mappings
        // ...
    }
    Some(frame)
}
```

### 2. Kernel Page-Based Allocator - ✅ Fixed

**File:** `src/allocator.rs:168-171`

```rust
if let Some(frame) = crate::pmm::alloc_page_zeroed() {
    if i == 0 {
        first_addr = Some(crate::mmu::phys_to_virt(frame.addr));  // ✅
    }
}
```

### 3. ELF Loader Segment Copy - ✅ Fixed

**File:** `src/elf_loader.rs:157-160`

```rust
unsafe {
    let dst = crate::mmu::phys_to_virt(frame_addr + copy_start);  // ✅
    let src = elf_data.as_ptr().add(file_offset);
    core::ptr::copy_nonoverlapping(src, dst, copy_len);
}
```

### 4. MMU Page Table Manipulation - ✅ Fixed

**File:** `src/mmu.rs` (multiple locations)

All page table operations use `phys_to_virt()`:

```rust
// In add_kernel_mappings()
let l0_ptr = phys_to_virt(self.l0_frame.addr) as *mut u64;  // ✅
let l1_ptr = phys_to_virt(l1_frame.addr) as *mut u64;       // ✅
let l2_ptr = phys_to_virt(l2_frame.addr) as *mut u64;       // ✅

// In map_page()
let l0_ptr = phys_to_virt(self.l0_frame.addr) as *mut u64;  // ✅
let l1_ptr = phys_to_virt(l1_frame.addr) as *mut u64;       // ✅
// ... etc

// In unmap_page()
let l0_ptr = phys_to_virt(self.l0_frame.addr) as *mut u64;  // ✅
let l1_ptr = phys_to_virt(l1_addr) as *mut u64;             // ✅
// ... etc
```

### 5. VirtIO HAL DMA Allocation - ✅ Fixed

**File:** `src/virtio_hal.rs:19-37`

```rust
fn dma_alloc(...) -> (PhysAddr, NonNull<u8>) {
    let virt = unsafe { alloc_zeroed(layout) };
    let phys = virt_to_phys(virt as usize);  // ✅ Uses translation
    (phys, ptr)
}

unsafe fn mmio_phys_to_virt(paddr: PhysAddr, _size: usize) -> NonNull<u8> {
    unsafe { NonNull::new_unchecked(phys_to_virt(paddr)) }  // ✅
}

unsafe fn share(buffer: NonNull<[u8]>, ...) -> PhysAddr {
    virt_to_phys(buffer.as_ptr() as *mut u8 as usize)  // ✅
}
```

### 6. VirtIO RNG Queue PFN - ✅ Fixed

**File:** `src/rng.rs:286-287`

```rust
let queue_phys = crate::mmu::virt_to_phys(queue_mem as usize);  // ✅
let queue_pfn = queue_phys / PAGE_SIZE;
```

### 7. VirtIO RNG Buffer Descriptor - ✅ Fixed

**File:** `src/rng.rs:345`

```rust
d.addr = crate::mmu::virt_to_phys(self.buffer as usize) as u64;  // ✅
```

---

## Device MMIO and User Page Tables

The boot page tables identity-map all device MMIO via a 1GB L1 block (L1[0]).
User page tables do NOT map most device MMIO — only VirtIO at 0x0a00_0000
is retained because it does not conflict with user heap addresses.

The kernel accesses GIC (0x0800_0000), UART (0x0900_0000), and fw_cfg
(0x0902_0000) by temporarily swapping TTBR0 to boot page tables via
`mmu::with_boot_ttbr0()`. This was necessary because large binaries like bun
(93MB, brk at 0x05C6_E000) have heap regions that overlap with these device
addresses. See `docs/DEVICE_MMIO_VA_CONFLICT.md` for the full analysis.

## Future Changes

If the kernel is ever moved to a non-identity-mapped configuration:

1. Update `phys_to_virt()` to add the kernel virtual offset
2. Update `virt_to_phys()` to subtract the kernel virtual offset (or walk page tables)
3. All existing code should work without changes

---

## Related Documentation

- `docs/MEMORY_LAYOUT.md` - Kernel memory regions
- `docs/DEVICE_MMIO_VA_CONFLICT.md` - Device MMIO VA conflict and fix
- `docs/HEAP_CORRUPTION_ANALYSIS.md` - Previous memory bugs
- `docs/USERSPACE_MEMORY_MODEL.md` - Userspace address handling
- `src/boot.rs` - Boot page table setup
