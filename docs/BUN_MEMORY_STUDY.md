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

## 6. Eager ELF Loading Exhausts Physical Memory

### Symptom

After all prior fixes, bun ran significantly longer during initialization
but eventually crashed when the heap exhausted its lazy region:

```
[DA] pid=29 far=0xa350000 elr=0x4141d48 iss=0x7
[DP] no lazy region for FAR=0xa350000 pid=29
[Fault] Data abort from EL0 at FAR=0xa350000, ELR=0x4141d48, ISS=0x7
```

The heap lazy region was only ~71MB despite dynamic sizing. On a 256MB
system, far more should have been available.

### Root Cause

`load_elf_from_path` in `src/elf_loader.rs` eagerly allocated and loaded
every page of every PT_LOAD segment. For bun's 93MB binary, this consumed
~23,800 physical pages (~93MB) upfront before the process even started
running. With kernel overhead, only ~71MB of free physical pages remained
for the dynamically-computed heap lazy region.

The function was ironically named "on-demand" but loaded all data eagerly
into pre-allocated physical pages.

### Fix

Converted `load_elf_from_path` to a truly demand-paged loader:

1. **Extended lazy region model** (`src/process.rs`): replaced the
   tuple-based `(start_va, size, flags)` with a `LazyRegion` struct
   containing a `LazySource` enum (`Zero` for anonymous, `File` for
   file-backed regions with path/offset/filesz/segment_va metadata).

2. **Deferred segment loading** (`src/elf_loader.rs`): instead of
   allocating and mapping pages for each PT_LOAD segment, the loader now
   collects `DeferredLazySegment` descriptors and returns them to the
   caller. After PID allocation, the segments are registered as
   file-backed lazy regions. Gap regions between segments are registered
   as zero-fill lazy regions. The 16-page heap pre-allocation and eager
   interpreter loading (~1MB) are preserved.

3. **File-backed page faults** (`src/exceptions.rs`): both
   `EC_DATA_ABORT_LOWER` and `EC_INST_ABORT_LOWER` handlers now check the
   `LazySource` on fault. For `LazySource::File`, the handler reads the
   page data from disk via `vfs::read_at()` before mapping. For
   instruction faults on code pages, cache maintenance (`DC CVAU` + `IC
   IVAU` + `DSB ISH` + `ISB`) is performed after loading to ensure
   instruction cache coherency.

### Memory Savings

For bun's 93MB binary on a 256MB system:
- **Before**: ~93MB consumed at load, ~71MB left for heap
- **After**: ~1MB consumed at load (interpreter only + 16 heap pages),
  ~163MB available for heap, pages loaded from disk on demand

---

## 7. Unaligned Segment Page Placement

### Symptom

After implementing the demand-paged ELF loader, bun crashed immediately
during interpreter startup:

```
[IA-DP] pid=29 va=0x2a3e000 foff=0x282e500 seg_va=0x2a3e500 first=0xd280001d
[DA] pid=29 far=0xfffffffffffffff8 elr=0x2a3e500 iss=0x44
[Fault] Data abort from EL0 at FAR=0xfffffffffffffff8, ELR=0x2a3e500, ISS=0x44
```

FAR=0xfffffffffffffff8 is address -8 -- a null pointer dereference with
a negative offset. The code at `ELR=0x2a3e500` was executing garbage
instructions.

### Root Cause

Bun's second PT_LOAD segment starts at `p_vaddr=0x2a3e500`, which is
**not page-aligned** (0x500 bytes into a 4KB page). The demand paging
handler registered a lazy region starting at the page-aligned address
`0x2a3e000`, with `segment_va=0x2a3e500`.

When the page at `0x2a3e000` was faulted in, the handler computed:

```
offset_in_seg = page_va.saturating_sub(segment_va)
              = 0x2a3e000 - 0x2a3e500
              = 0                          (saturating)
file_pos      = file_offset + 0
```

It then read 4096 bytes of file data and placed them at **byte 0** of
the page. But the segment data should start at byte `0x500` within the
page (the first `0x500` bytes belong to the gap/previous segment's tail
and should be zero). The result: every byte in the page was shifted
`0x500` bytes earlier than its correct position. The instruction at VA
`0x2a3e500` (byte `0x500` of the page) was actually executing data from
`0x500` bytes into the segment -- completely wrong code.

```
Page at 0x2a3e000:
  WRONG:  [seg_data[0..0x1000] ................]  <- data starts at byte 0
  RIGHT:  [00 00 ... 00 | seg_data[0..0xB00] ..]  <- data starts at byte 0x500
                         ^
                    0x2a3e500 (segment_va)
```

The old eager loader handled this correctly with a `copy_start`
variable that offset the destination within the page.

### Fix

Added `copy_start` calculation to both the `EC_DATA_ABORT_LOWER` and
`EC_INST_ABORT_LOWER` demand paging handlers in `src/exceptions.rs`:

```rust
let copy_start = if page_va < segment_va { segment_va - page_va } else { 0 };
// ...
let dst = (page_ptr as *mut u8).add(copy_start);
let buf = core::slice::from_raw_parts_mut(dst, read_len);
let read_len = min(PAGE_SIZE - copy_start, filesz - offset_in_seg);
```

When `page_va < segment_va`, file data is placed at offset `copy_start`
within the page. The leading bytes remain zero (from `alloc_page_zeroed`).
For pages entirely within the segment (`page_va >= segment_va`),
`copy_start = 0` and behavior is unchanged.

A companion fix ensures cache maintenance (`DC CVAU` + `IC IVAU`) is
performed in the data abort handler when demand-paging executable
(non-UXN) file-backed pages, since a code page may be first accessed
via a data read by the dynamic linker before the CPU fetches
instructions from it.

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

## 8. Resolution Phase Memory Threshold (~1050MB)

### Symptom

`bun install @google/gemini-cli` hangs indefinitely during the "resolving"
phase at 1024MB RAM but works correctly at 1050MB+:

```
# At 1024MB - hangs after DNS resolution
akuma:/> bun install @google/gemini-cli
bun add v1.2.21 (7c45ed97)
  🔍 @google/gemini-cli
  <hangs forever>

# At 1050MB+ - proceeds to download
akuma:/> bun install @google/gemini-cli
bun add v1.2.21 (7c45ed97)
  🔍 kleur [132/263]
  <downloads proceed>
```

### Investigation

Kernel logs show DNS resolution succeeds in both cases:

```
[syscall] sendto(fd=14, len=36, dest=10.0.2.3:53)
[DNS] query sent OK: 36 bytes
[UDP] recvmsg OK: 228 bytes from 10.0.2.3:53  <- DNS response received
```

At 1050MB+, bun immediately creates TCP sockets to npm registry:

```
[syscall] socket(type=TCP) = fd 15
[syscall] connect(fd=15, ip=104.16.2.34:443)
[epoll] ctl ADD epfd=11 fd=15 events=0x4
```

At 1024MB, no TCP sockets are created. Instead:
1. A worker thread (TID=48) is spawned after DNS response
2. Both main and worker threads enter `epoll_pwait` with infinite timeout
3. Worker thread exits after ~11 seconds (bun's internal timeout)
4. Main thread remains stuck

### Root Cause (Hypothesis)

Bun's internal memory allocator (mimalloc or JSC) fails silently when
physical memory is below ~1050MB. The exact failure point is within bun's
userspace code path between receiving the DNS response and creating HTTP
connections. No kernel errors (ENOMEM, etc.) are returned -- bun simply
doesn't proceed.

The ~26MB difference (1024MB vs 1050MB) suggests a specific allocation
size threshold that triggers fallback behavior leading to the internal
timeout.

### Workaround

Run QEMU with at least 1.1GB RAM for `bun install`:

```bash
MEMORY=1100M cargo run --release
```

### Related Crash at Higher Memory

Even at 2GB, bun can crash during resolution with a kernel data abort:

```
[Exception] Sync from EL1: EC=0x25, ISS=0x47
  ELR=0x403d22c8, FAR=0x50005000
  Process PID=45 'HTTP Client'
```

This is a separate issue where kernel code accesses a lazy mmap page
that hasn't been demand-paged yet. See issue #9 below.

---

## 9. Kernel Data Abort on Lazy mmap Pages (ONGOING)

### Symptom

During `bun install` at 2GB, the kernel crashes with a data abort from EL1:

```
[Exception] Sync from EL1: EC=0x25, ISS=0x47
  ELR=0x403d22c8, FAR=0x50005000, SPSR=0x80002345
  Thread=9, TTBR0=0x2d0000494cb000, TTBR1=0x40425000
  SP=0x49202060, SP_EL0=0x903feee0
  Instruction at ELR: 0xf800852b
  Likely: Rn(base)=x9, Rt(dest)=x11
  Process PID=45 'HTTP Client'
```

Key details:
- **EC=0x25**: Data abort from EL1 (kernel mode)
- **FAR=0x50005000**: Within bun's 1GB lazy mmap region (0x50000000-0x90000000)
- **ELR=0x403d22c8**: In kernel space (above 0x40000000)
- **ISS=0x47**: Translation fault at level 3 (page not mapped)

### Root Cause

The kernel is attempting to access a userspace lazy mmap page directly
without first ensuring it's mapped. This happens when:

1. Bun allocates a large lazy mmap region (e.g., 1GB at 0x50000000)
2. A syscall handler or kernel function dereferences a userspace pointer
   within this region
3. The page at FAR hasn't been demand-paged yet (no physical backing)
4. The kernel MMU faults because the page table entry is empty

The instruction `0xf800852b` is `str x11, [x9], #8` -- a memcpy-like
store operation. The kernel is likely copying data to/from userspace
without validating that the destination pages exist.

### Misleading Diagnostic

The crash message includes:
```
WARNING: User SP below stack bottom - STACK OVERFLOW!
```

This is misleading for `CLONE_VM` threads. Worker threads created with
`clone(CLONE_VM|CLONE_THREAD)` use mmap'd stacks (e.g., at 0x903feee0)
while `ProcessMemory` still reports the *process's* high stack region
(0x203fe00000). The comparison is invalid for threads.

### Root Cause Analysis

After investigation, the crash mechanism is:

1. **Syscall entry**: Thread A validates a user buffer via `validate_user_ptr()`
2. **Demand paging**: `ensure_user_pages_mapped()` maps any lazy pages
3. **Race condition**: Thread B calls `munmap()` on the region
4. **Fault**: Thread A's memset/memcpy faults on the now-unmapped page
5. **Kernel panic**: EL1 data abort handler halts (no recovery path)

The `validate_user_ptr` function already calls `ensure_user_pages_mapped`,
but there's a TOCTTOU (time-of-check-to-time-of-use) race when another
thread unmaps the region between validation and use.

### Status

**NOT YET FIXED.** Potential fixes:
1. Handle EL1 data aborts on user addresses by demand-paging from kernel mode
2. Pin pages during syscall operations (complex, requires reference counting)
3. Add per-region locks to prevent concurrent munmap during syscall
4. Catch EL1 faults and return EFAULT instead of panicking (requires stack unwind)

---

## Related Documentation

- `docs/DEVICE_MMIO_VA_CONFLICT.md` -- Device MMIO remapping details
- `docs/BUN_MISSING_SYSCALLS.md` -- Syscalls added for bun
- `docs/MEMORY_LAYOUT.md` -- Physical and virtual memory layout
- `docs/USERSPACE_MEMORY_MODEL.md` -- User address space layout
