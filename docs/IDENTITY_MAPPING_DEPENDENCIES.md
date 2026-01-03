# Identity Mapping Dependencies

This document catalogs places in the kernel that rely on **physical == virtual address identity mapping** and would break if that assumption is removed.

## Current Memory Model

The kernel uses identity mapping set up at boot time:

| L1 Entry | Address Range | Description |
|----------|--------------|-------------|
| L1[0] | 0x00000000 - 0x3FFFFFFF | Device memory (GIC, UART, VirtIO) |
| L1[1] | 0x40000000 - 0x7FFFFFFF | RAM block 1 |
| L1[2] | 0x80000000 - 0xBFFFFFFF | RAM block 2 |

This allows the kernel to use physical addresses directly as pointers.

## Why This Matters

If you ever:
- Move the kernel to the high half (TTBR1 with `0xFFFF_xxxx` addresses)
- Remove identity mapping for certain regions
- Allocate memory outside the identity-mapped range
- Enable KASLR (kernel address space layout randomization)

You will need to introduce proper `phys_to_virt()` and `virt_to_phys()` translation functions.

---

## Issue Catalog

### 1. PMM `alloc_page_zeroed()` - High Severity

**File:** `src/pmm.rs:290-296`

```rust
pub fn alloc_page_zeroed() -> Option<PhysFrame> {
    let frame = alloc_page()?;
    unsafe {
        core::ptr::write_bytes(frame.addr as *mut u8, 0, PAGE_SIZE);  // ⚠️
    }
    Some(frame)
}
```

**Problem:** Dereferences a physical address (`frame.addr`) directly as a pointer.

**Fix Required:** Use `phys_to_virt(frame.addr)` before dereferencing.

---

### 2. Kernel Page-Based Allocator - High Severity

**File:** `src/allocator.rs:168-197`

```rust
for i in 0..pages {
    if let Some(frame) = crate::pmm::alloc_page_zeroed() {
        if i == 0 {
            first_addr = Some(frame.addr);  // ⚠️ Physical address stored
        }
    }
}
first_addr.map(|a| a as *mut u8).unwrap_or(ptr::null_mut())  // ⚠️ Returned as VA
```

**Problem:** Returns a physical address from PMM directly to Rust's global allocator as a usable pointer.

**Fix Required:** Either:
- Map the physical pages into kernel VA space and return the VA
- Use `phys_to_virt()` translation

---

### 3. ELF Loader Segment Copy - High Severity

**File:** `src/elf_loader.rs:156-159`

```rust
unsafe {
    let dst = (frame_addr + copy_start) as *mut u8;  // ⚠️
    let src = elf_data.as_ptr().add(file_offset);
    core::ptr::copy_nonoverlapping(src, dst, copy_len);
}
```

**Problem:** Copies ELF segment data to a physical frame address directly as a pointer.

**Fix Required:** Use `phys_to_virt(frame_addr)` for the destination pointer.

---

### 4. MMU Page Table Manipulation - High Severity

**File:** `src/mmu.rs` (multiple locations)

```rust
// In add_kernel_mappings()
let l0_ptr = self.l0_frame.addr as *mut u64;  // ⚠️
unsafe {
    let l1_entry = (l1_frame.addr as u64) | flags::VALID | flags::TABLE;
    core::ptr::write_volatile(l0_ptr, l1_entry);
}
```

```rust
// In get_or_create_table() - standalone function
unsafe fn get_or_create_table(table_ptr: *mut u64, idx: usize) -> usize {
    if let Some(frame) = crate::pmm::alloc_page_zeroed() {
        let new_entry = (frame.addr as u64) | flags::VALID | flags::TABLE;
        table_ptr.add(idx).write_volatile(new_entry);
        frame.addr  // ⚠️ Returns PA, caller uses as VA
    }
}
```

**Problem:** All page table operations use `PhysFrame.addr` directly as pointers.

**Affected functions:**
- `UserAddressSpace::add_kernel_mappings()`
- `UserAddressSpace::map_page()`
- `UserAddressSpace::get_or_create_table()`
- `UserAddressSpace::unmap_page()`
- Standalone `get_or_create_table()`
- Standalone `map_user_page()`

**Fix Required:** All page table accesses need `phys_to_virt()` translation.

---

### 5. VirtIO HAL DMA Allocation - Critical Severity

**File:** `src/virtio_hal.rs:17-30`

```rust
fn dma_alloc(...) -> (PhysAddr, NonNull<u8>) {
    let virt = unsafe { alloc_zeroed(layout) };
    // On QEMU ARM64 virt machine, physical == virtual for RAM
    let phys = virt as usize;  // ⚠️ Assumes VA == PA
    (phys, ptr)
}
```

**Problem:** Assumes allocator pointers are physical addresses. VirtIO devices need true physical addresses for DMA.

**Fix Required:** Use `virt_to_phys()` to get the actual physical address:
```rust
let phys = virt_to_phys(virt as usize);
```

---

### 6. VirtIO RNG Queue PFN - Critical Severity

**File:** `src/rng.rs:284-288`

```rust
// Set queue PFN (page frame number) - legacy mode
let queue_pfn = (queue_mem as usize) / PAGE_SIZE;  // ⚠️
unsafe {
    write_volatile((base_addr + VIRTIO_MMIO_QUEUE_PFN) as *mut u32, queue_pfn as u32);
}
```

**Problem:** VirtIO legacy mode expects a physical page frame number, but this code passes a virtual pointer divided by page size.

**Fix Required:**
```rust
let queue_phys = virt_to_phys(queue_mem as usize);
let queue_pfn = queue_phys / PAGE_SIZE;
```

---

### 7. VirtIO RNG Buffer Descriptor - Critical Severity

**File:** `src/rng.rs:340-346`

```rust
unsafe {
    let d = &mut *self.desc.add(desc_idx as usize);
    d.addr = self.buffer as u64;  // ⚠️ VA passed as PA to device
    d.len = to_read as u32;
    d.flags = VIRTQ_DESC_F_WRITE;
    d.next = 0;
}
```

**Problem:** VirtIO descriptor `addr` field needs a physical address for DMA, but a virtual pointer is passed.

**Fix Required:**
```rust
d.addr = virt_to_phys(self.buffer as usize) as u64;
```

---

## Summary Table

| Location | Issue | Severity | DMA? |
|----------|-------|----------|------|
| `pmm.rs:290-296` | `alloc_page_zeroed()` uses PA directly | High | No |
| `allocator.rs:168-197` | Page allocator returns PA as pointer | High | No |
| `elf_loader.rs:156-159` | ELF copy uses PA as pointer | High | No |
| `mmu.rs` (multiple) | Page table walks use PA as pointer | High | No |
| `virtio_hal.rs:27` | DMA assumes VA == PA | **Critical** | Yes |
| `rng.rs:285` | Queue PFN from VA pointer | **Critical** | Yes |
| `rng.rs:342` | DMA buffer VA passed as PA | **Critical** | Yes |

---

## Recommended Fix Strategy

### 1. Add Translation Functions

Create `src/mm.rs` or add to `src/mmu.rs`:

```rust
/// Physical to virtual address translation
/// Only valid for identity-mapped kernel memory
#[inline]
pub fn phys_to_virt(paddr: usize) -> usize {
    // For now: identity mapping
    paddr
    // Future: paddr + KERNEL_VIRT_OFFSET
}

/// Virtual to physical address translation
/// Only valid for identity-mapped kernel memory  
#[inline]
pub fn virt_to_phys(vaddr: usize) -> usize {
    // For now: identity mapping
    vaddr
    // Future: vaddr - KERNEL_VIRT_OFFSET (or walk page tables)
}
```

### 2. Update Code Incrementally

Replace all direct `frame.addr as *mut T` with `phys_to_virt(frame.addr) as *mut T`.

### 3. Priority Order

1. **VirtIO/DMA code** (Critical) - broken DMA causes device malfunction
2. **PMM zeroing** (High) - required for correct page initialization
3. **ELF loader** (High) - required for process loading
4. **MMU code** (High) - required for page table management
5. **Allocator** (High) - required for kernel heap

---

## Testing After Changes

After introducing translation functions, verify:

1. Kernel boots successfully
2. Block device reads/writes work (VirtIO-blk)
3. Network works (VirtIO-net)  
4. Random number generation works (VirtIO-rng)
5. User processes load and execute correctly
6. Memory allocation stress tests pass

---

## Related Documentation

- `docs/MEMORY_LAYOUT.md` - Kernel memory regions
- `docs/HEAP_CORRUPTION_ANALYSIS.md` - Previous memory bugs
- `src/boot.rs` - Boot page table setup

