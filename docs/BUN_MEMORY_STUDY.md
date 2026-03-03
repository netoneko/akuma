# Bun Memory Study

Chronological record of every memory-related crash encountered while bringing
up Bun (the JavaScript runtime) on Akuma, and the fixes applied. Bun is a
93MB statically-linked AArch64 binary whose allocator (mimalloc) and JIT
engine (JavaScriptCore) push the limits of Akuma's user virtual address space.

---

## 1. GIC/UART Collision (heap vs device MMIO)

### Symptom

Bun's code occupies VA 0x0020_0000-0x05AF_D950, placing brk at
0x05C6_E000. With a 64MB heap lazy region, the heap extends to
0x09C6_E000 -- directly through the device MMIO range (GIC at
0x0800_0000, UART at 0x0900_0000).

**First manifestation: silent death.** Bun exited with -11 (SIGSEGV) but
no `[Fault]` messages appeared on the serial console. Demand paging for a
heap address in the UART's L2 range replaced the UART's 2MB L2 block
descriptor with an L3 page table pointer. This destroyed the kernel's UART
mapping -- `safe_print!()` calls wrote to a zeroed page instead of the
UART register.

**Second manifestation: permission fault at GIC.**

```
[DA] pid=29 far=0x7fff000 elr=0x4141d48 iss=0x7   <- demand paged OK
[DA] pid=29 far=0x8000000 elr=0x4141d48 iss=0xe   <- GIC page, PERMISSION FAULT
[Fault] Data abort from EL0 at FAR=0x8000000, ELR=0x4141d48, ISS=0xe
```

ISS=0x0e is a permission fault (DFSC level 2). The GIC page is mapped with
EL1-only permissions. ELR is the same across consecutive pages -- a memset
walking through the heap.

### Root Cause

The original design identity-mapped device MMIO (GIC, UART, fw_cfg,
VirtIO) in every user page table. Devices occupied VA 0x0800_0000-
0x0C00_0000. Small binaries never reached these addresses, but bun's 93MB
image placed brk high enough that the heap grew straight through them.

### Fix

Remapped GIC, UART, and fw_cfg to virtual addresses under L0[1]
(`0x80_0000_0000`+) using a shared page table chain (L1/L2/L3). With
T0SZ=16 (48-bit VA space), L0[1] is at VA 0x80_0000_0000 -- far from any
user memory region. VirtIO at 0x0A00_0000 was initially left
identity-mapped (see issue 3 below).

An additional bug was found in `exceptions.rs`: permission faults
(DFSC=0x0C) were incorrectly treated as translation faults eligible for
demand paging, causing infinite retry loops on device pages.

See `docs/DEVICE_MMIO_VA_CONFLICT.md` for full details.

---

## 2. Heap Exhaustion (64MB fixed limit)

### Symptom

After fixing the GIC/UART collision, bun crashed at a higher heap address:

```
[DA] pid=29 far=0x9c6d000 elr=0x4141d48 iss=0x7
[DA] pid=29 far=0x9c6e000 elr=0x4141d48 iss=0x7
[DP] no lazy region for FAR=0x9c6e000 pid=29
[Fault] Data abort from EL0 at FAR=0x9c6e000, ELR=0x4141d48, ISS=0x7
```

The demand paging handler found no lazy region covering FAR=0x9c6e000.
The heap lazy region extended only 64MB from brk (0x05C6_E000 + 64MB =
0x09C6_E000). bun's mimalloc allocator exhausted this space.

### Root Cause

`HEAP_LAZY_SIZE` was a hardcoded 64MB constant. This is ample for small
binaries but insufficient for mimalloc's arena-based allocator in a 93MB
binary.

### Fix

Replaced the fixed 64MB constant with a `compute_heap_lazy_size()` helper
in `src/process.rs`. The dynamic size is computed as:

1. Query free physical pages via `pmm::stats()`
2. Convert to bytes, subtract a reserve (8MB)
3. Cap at the available VA gap (brk to mmap_start)
4. Enforce a 16MB minimum

This allows bun to claim more heap as long as physical memory is
available, while preventing the heap from colliding with the mmap region.

---

## 3. VirtIO Collision (heap reaches 0x0A00_0000)

### Symptom

With dynamic heap sizing and JIT disabled (`JSC_useDFGJIT=0
JSC_useFTLJIT=0 JSC_useBBQJIT=0 JSC_useOMGJIT=0`), the heap grew further
and hit the VirtIO MMIO page:

```
[DA] pid=30 far=0x9ffe000 elr=0x4141d48 iss=0x7
[DA] pid=30 far=0x9fff000 elr=0x4141d48 iss=0x7
[DA] pid=30 far=0xa000000 elr=0x4141d48 iss=0xf
[Fault] Data abort from EL0 at FAR=0xa000000, ELR=0x4141d48, ISS=0xf
```

ISS=0xf is DFSC=0b01111, a permission fault at level 3. The VirtIO page
at 0x0A00_0000 was mapped with EL1-only device memory attributes. The
pattern is identical to the GIC collision: a memset walks consecutive
pages and faults on the device page.

### Root Cause

When GIC/UART/fw_cfg were remapped to L0[1], VirtIO was left
identity-mapped at 0x0A00_0000 in user page tables under the assumption
that "the 64MB heap won't reach it." After switching to dynamic heap
sizing, the heap could grow well past 0x0A00_0000.

The DMA concern that motivated keeping VirtIO identity-mapped turned out
to be unfounded: `virt_to_phys()` in `virtio_hal.rs` is only used for DMA
*buffer* addresses (allocated from kernel heap at 0x4000_0000+, which are
identity-mapped kernel RAM). The VirtIO MMIO *registers* (the control
plane) don't need identity mapping for DMA to work.

### Fix

Remapped VirtIO to L0[1] at VA `0x80_0000_4000` (L3 slot 4), using the
same shared device page table chain as GIC/UART/fw_cfg. Removed the
L2[80]/L3 identity mapping from `add_kernel_mappings()`. Updated
`VIRTIO_MMIO_ADDRS` in `smoltcp_net.rs`, `block.rs`, and `rng.rs` to use
`DEV_VIRTIO_VA`-based offsets.

After this fix, L1[0]'s L2 table in user page tables has no device entries
at all. The entire first 1GB of VA space (0x0-0x3FFF_FFFF) is free for
user code, heap, and data.

---

## 4. 128GB mmap Rejection (JIT gigacage)

### Symptom

With JIT enabled (default), bun's JavaScriptCore engine attempted to mmap
128GB of virtual address space for its "gigacage" JIT region:

```
[mmap] REJECT: pid=30 size=0x2000000000 next=0x90000000 limit=0xffd00000
```

The mmap was rejected because the user VA space was capped at ~4GB
(`MAX_STACK_TOP = 0x1_0000_0000`). After the mmap failure, a subsequent
access faulted:

```
[DA] pid=30 far=0x5000000e8 elr=0x300184a8 iss=0x45
[DP] no lazy region for FAR=0x5000000e8 pid=30
[Fault] Data abort from EL0 at FAR=0x5000000e8, ELR=0x300184a8, ISS=0x45
```

### Root Cause

`compute_stack_top()` in `src/elf_loader.rs` limited the user VA space to
4GB (`MAX_STACK_TOP`) with 3GB reserved for mmap (`MIN_MMAP_SPACE`). This
was far too small for JSC's gigacage, which requires 128GB of contiguous
VA space for its JIT code regions.

### Fix

Raised `MAX_STACK_TOP` to 256GB (`0x40_0000_0000`) and `MIN_MMAP_SPACE`
to 128GB (`0x20_0000_0000`). These only affect large/dynamic binaries --
small static binaries still use the default 1GB address space. The
AArch64 MMU with T0SZ=16 supports 48-bit VA (256TB), so 256GB is well
within range. Page tables are demand-allocated, so the large VA
reservation has no physical memory cost.

---

## 5. Fork stack_top Bug

### Symptom

Not directly observed as a crash (masked by earlier issues), but would
have caused incorrect memory copying during `clone()`/fork.

### Root Cause

`src/process.rs` line ~2486 hardcoded `stack_top = 0x40000000` (1GB)
in the fork path. When the parent process had an expanded VA space (e.g.,
256GB for bun), the forked child would copy memory using the wrong stack
address, missing the actual stack entirely.

### Fix

Changed `let stack_top = 0x40000000` to `let stack_top =
parent.memory.stack_top`, so the child inherits the parent's actual stack
layout.

---

## Device VA Map (final state)

All device MMIO is mapped via L0[1] (`0x80_0000_0000`+), shared across
all user address spaces:

| L3 slot | Virtual Address   | Physical Address | Device            |
|---------|-------------------|------------------|-------------------|
| 0       | `0x80_0000_0000`  | `0x0800_0000`    | GIC distributor   |
| 1       | `0x80_0000_1000`  | `0x0801_0000`    | GIC CPU interface |
| 2       | `0x80_0000_2000`  | `0x0900_0000`    | UART PL011        |
| 3       | `0x80_0000_3000`  | `0x0902_0000`    | fw_cfg            |
| 4       | `0x80_0000_4000`  | `0x0A00_0000`    | VirtIO MMIO       |

## User VA Layout (large/dynamic binaries)

```
0x0000_0000          +---------------------------+
                     | (unmapped)                |
0x0020_0000          +---------------------------+
                     | ELF code + data           |
brk (~0x05C6_E000)   +---------------------------+
                     | Heap (lazy, grows up)     |
                     |           |               |
                     |           v               |
                     |   (free VA space)         |
mmap_start           +---------------------------+
                     | mmap region (grows up)    |  128GB reserved
                     |                           |
stack_bottom         +---------------------------+
                     | User stack (2MB)          |
stack_top (256GB)    +---------------------------+
```

## Related Documentation

- `docs/DEVICE_MMIO_VA_CONFLICT.md` -- Device MMIO remapping details
- `docs/BUN_MISSING_SYSCALLS.md` -- Syscalls added for bun
- `docs/MEMORY_LAYOUT.md` -- Physical and virtual memory layout
- `docs/USERSPACE_MEMORY_MODEL.md` -- User address space layout
