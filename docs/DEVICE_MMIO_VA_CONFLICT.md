# Device MMIO Virtual Address Conflict

This document explains how user process heap memory collides with kernel device
MMIO mappings in the shared TTBR0 address space, and the fix applied to resolve
it.

## Background

On the QEMU virt machine, device peripherals are at physical addresses in the
first 1GB:

| Device | Physical Address | Purpose |
|--------|-----------------|---------|
| GIC distributor | 0x0800_0000 | Interrupt controller |
| GIC CPU interface | 0x0801_0000 | Per-CPU interrupt handling |
| UART (PL011) | 0x0900_0000 | Serial console |
| fw_cfg | 0x0902_0000 | QEMU firmware config |
| VirtIO MMIO | 0x0a00_0000 | Network, block, RNG devices |

The kernel runs via TTBR0 identity mapping (boot page tables use a 1GB L1 block
for device memory at L1[0]). `phys_to_virt()` is identity — VA equals PA. TTBR1
currently points to the same tables as TTBR0 and is unused.

## The Conflict

When a user process runs, the kernel switches TTBR0 to the process's page
tables. These user page tables must also map:

1. **Kernel RAM** (0x4000_0000+) — so the kernel can execute during
   exceptions/syscalls
2. **Device MMIO** (0x0800_0000-0x0C00_0000) — so the kernel can access
   UART, GIC, VirtIO while handling exceptions with user TTBR0 active

The original implementation mapped device MMIO as 32 L2 block descriptors
(2MB each) at L2 indices 64-96, reserving the entire VA range
0x0800_0000-0x0C00_0000 in every user address space.

For small binaries this worked fine — code loads at 0x0040_0000, brk (heap
start) is around 0x0041_0000, and the 64MB heap lazy region stays well below
0x0800_0000.

**Bun is 93MB.** Its code occupies VA 0x0020_0000-0x05AF_D950, placing brk at
0x05C6_E000. With a 64MB heap lazy region, the heap extends to 0x09C6_E000 —
directly through the device MMIO range:

```
0x0000_0000  ┌──────────────────────┐
             │ (unmapped)           │
0x0020_0000  ├──────────────────────┤
             │ Bun code + data      │  93MB
0x05C6_E000  ├──────────────────────┤
             │ Heap (brk)           │  grows upward
             │         ↓            │
0x0800_0000  ├──── GIC ────────────┤ ← COLLISION
0x0900_0000  ├──── UART ───────────┤ ← COLLISION
0x0A00_0000  ├──── VirtIO ─────────┤
0x09C6_E000  ├──────────────────────┤  heap end (64MB)
             │                      │
```

## Symptoms

### Symptom 1: Silent process death (exit code -11, no fault messages)

The first manifestation was bun exiting with -11 (SIGSEGV) but no `[Fault]`
messages appearing on the serial console. This happened because demand paging
for a heap address in the UART's L2 range (0x0900_0000) replaced the UART's
2MB L2 block descriptor with an L3 page table pointer. This **destroyed the
kernel's UART mapping** — subsequent `safe_print!()` calls wrote to a zeroed
page instead of the UART register. The kernel could only print again after
`return_to_kernel()` restored the boot TTBR0.

**Fix:** Replaced L2 2MB block descriptors with L3 page-level entries for
device pages. This prevents demand paging from clobbering device mappings since
`get_or_create_table_raw` finds the existing L3 table and reuses it.

### Symptom 2: Permission fault at 0x0800_0000 (ISS=0x0E)

With L3 device entries, the UART clobbering was fixed, but bun's heap still
collided with the GIC device page at 0x0800_0000. The GIC page is mapped with
EL1-only permissions, so EL0 access triggers a permission fault:

```
[DA] pid=29 far=0x7ffd000 elr=0x4141d48 iss=0x7   ← demand paged OK
[DA] pid=29 far=0x7ffe000 elr=0x4141d48 iss=0x7   ← demand paged OK
[DA] pid=29 far=0x7fff000 elr=0x4141d48 iss=0x7   ← demand paged OK
[DA] pid=29 far=0x8000000 elr=0x4141d48 iss=0xe   ← GIC page, PERMISSION FAULT
[Fault] Data abort from EL0 at FAR=0x8000000, ELR=0x4141d48, ISS=0xe
```

The same ELR hits consecutive pages — this is a memset/memcpy walking through
the heap. Register x0=0x7fffffd shows a write near the end of a buffer that
spans across the page boundary into 0x8000000.

On Linux this would not crash because brk is not capped at 0x8000000 and the
page at that address would be normal heap memory.

## Solution: Remap device MMIO to non-conflicting VAs via L0[1]

The fix remaps conflicting device pages (GIC, UART, fw_cfg) to virtual
addresses under L0[1] (`0x80_0000_0000`+). With T0SZ=16 (48-bit VA space),
L0 has 512 entries. User processes only use L0[0] (first 512GB), so devices
in L0[1] can never conflict with user heap, mmap, or stack.

### Remapped device virtual addresses

All 4 device pages fit in one L3 table under L0[1] -> L1[0] -> L2[0]:

| L3 slot | Virtual Address      | Physical Address | Device           |
|---------|---------------------|-----------------|------------------|
| 0       | `0x80_0000_0000`    | `0x0800_0000`   | GIC distributor  |
| 1       | `0x80_0000_1000`    | `0x0801_0000`   | GIC CPU interface|
| 2       | `0x80_0000_2000`    | `0x0900_0000`   | UART PL011       |
| 3       | `0x80_0000_3000`    | `0x0902_0000`   | fw_cfg           |

VirtIO at 0x0a00_0000 stays identity-mapped (no heap conflict, and DMA
relies on `virt_to_phys()` identity for buffer addresses).

### Boot page table changes

`src/boot.rs` expands the BSS page table area from 3 to 6 pages. The
additional 3 pages (L1, L2, L3) form the L0[1] device mapping chain,
set up in assembly before MMU enable. This means the remapped VAs work
from the very first Rust instruction.

### Shared user page tables

During kernel init, `mmu::init_shared_device_tables()` allocates 3 pages
(L1, L2, L3) with the same device entries. Every user address space's
`add_kernel_mappings()` sets L0[1] to point to this shared L1 page.
The shared pages are never tracked in `page_table_frames` and are never
freed, so per-process overhead is zero.

### User page table changes

`add_kernel_mappings()` in `src/mmu.rs` no longer creates L3 device page
entries for L2[64] (GIC) or L2[72] (UART/fw_cfg) in L1[0]'s L2 table.
Only L2[80] (VirtIO) retains its identity-mapped device L3 table.

### Driver constant changes

Device drivers reference the remapped VAs via constants in `src/mmu.rs`
(`DEV_GIC_DIST_VA`, `DEV_GIC_CPU_VA`, `DEV_UART_VA`, `DEV_FW_CFG_VA`).
No runtime TTBR0 swaps or function wrapping is needed.

### Heap uncapped

With conflicting devices removed from L0[0], the heap lazy region uses
the full 64MB `HEAP_LAZY_SIZE` without any cap.
`set_brk()` no longer rejects addresses past 0x0800_0000.

## Additional bug: `is_translation_fault` classification

The data abort handler in `src/exceptions.rs` incorrectly treated permission
faults (DFSC bits [5:2] = 0x0C) as translation faults eligible for demand
paging. With L3 device entries guarded by `map_user_page`'s existing-entry
check, a permission fault on a device page would enter demand paging, skip the
existing entry, return "success", and the CPU would retry the same faulting
instruction — an infinite loop.

Fix: only DFSC 0x04 (translation fault) and 0x08 (access flag fault) trigger
demand paging. Permission faults (0x0C) fall through to the SIGSEGV path.

## Related documentation

- `docs/MEMORY_LAYOUT.md` — Physical and virtual memory layout
- `docs/IDENTITY_MAPPING_DEPENDENCIES.md` — Kernel identity mapping
- `docs/USERSPACE_MEMORY_MODEL.md` — User address space layout
- `docs/ON_DEMAND_ELF_LOADER.md` — On-demand loading for large binaries
- `docs/BUN_MISSING_SYSCALLS.md` — Bun syscall support
