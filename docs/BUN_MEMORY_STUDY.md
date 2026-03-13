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
0x4000_0000          + - - - - - - - - - - - - - +
                     | Kernel RAM (identity      |  512 × 2MB blocks
                     | mapped, EL1 only)         |  mmap allocator skips
0x8000_0000          + - - - - - - - - - - - - - +
mmap_start           +---------------------------+
                     | mmap region (grows up)    |  128GB reserved
                     |                           |
stack_bottom         +---------------------------+
                     | User stack (auto-sized)   |
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

This was caused by the kernel identity mapping gap. FAR=0x50005000 is
a physical page address outside the 256MB identity-mapped range in user
page tables. Fixed by issue #14.

---

## 9. Kernel Data Abort on Lazy mmap Pages

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

### Misleading Diagnostic

The crash message includes:
```
WARNING: User SP below stack bottom - STACK OVERFLOW!
```

This is misleading for `CLONE_VM` threads. Worker threads created with
`clone(CLONE_VM|CLONE_THREAD)` use mmap'd stacks (e.g., at 0x903feee0)
while `ProcessMemory` still reports the *process's* high stack region
(0x203fe00000). The comparison is invalid for threads.

### Root Cause

Initially suspected as a TOCTTOU race on user pages, this was actually
caused by the **kernel identity mapping gap** (see issue #14 below).
FAR=0x50005000 is NOT a user lazy mmap page — it's a physical address
in the kernel's RAM range that the kernel tried to access via
`phys_to_virt()` (identity mapping: VA == PA). User page tables only
identity-mapped 256MB (0x40000000-0x4FFFFFFF), so physical addresses
at 0x50000000+ had no mapping in TTBR0 during syscalls.

The instruction `0xf800852b` (`str x11, [x9], #8`) was the kernel
writing to a newly-allocated physical page frame during demand paging
or `alloc_page_zeroed()`, not accessing user memory.

### Fix (2026-03-13)

Resolved by issue #14 (Kernel Identity Mapping Gap). See below.

---

## 10. Automatic User Stack Sizing

### Symptom

`bun install @google/gemini-cli` crashes with SIGSEGV during package resolution:

```
[WILD-DA] pid=54 FAR=0x74 ELR=0x5410618 last_sc=56 (openat)
  x2=0x725c33312e302e33 ("3.0.1\r3")
  x3=0x6e656d6d6f436e5c ("\nCommon")
[signal] Delivering sig 11 to handler 0x2ceca60
```

FAR=0x74 is address 116 -- a null pointer dereference with a small offset.
This occurs during JSON parsing of the package metadata for gemini-cli's
263 dependencies. The same binary successfully installs `express` (30 deps).

### Root Cause (Hypothesis)

Bun uses deep recursion for JSON parsing. With a complex dependency tree
like gemini-cli (263 packages with nested dependencies), the stack depth
exceeds the allocated user stack size. The previous 2MB fixed stack was
sufficient for simple packages but may be exhausted by complex ones.

The null pointer dereference is likely a consequence of stack corruption
from buffer overflow -- the actual data structures are corrupted when
the stack grows into adjacent memory.

### Fix

Implemented automatic user stack sizing based on available RAM:

```rust
// src/config.rs
pub const USER_STACK_SIZE_OVERRIDE: usize = 0;  // 0 = auto

pub const fn compute_user_stack_size(ram_size_bytes: usize) -> usize {
    if USER_STACK_SIZE_OVERRIDE != 0 {
        return USER_STACK_SIZE_OVERRIDE;
    }
    // Stack = RAM / 2048, clamped to [128KB, 8MB]
    let computed = ram_size_bytes / 2048;
    clamp(computed, 128*1024, 8*1024*1024)
}
```

Scaling table:

| RAM    | Stack Size |
|--------|------------|
| 256 MB | 128 KB     |
| 512 MB | 256 KB     |
| 1 GB   | 512 KB     |
| 2 GB   | 1 MB       |
| 4 GB   | 2 MB       |
| 8 GB   | 4 MB       |
| 16 GB+ | 8 MB (max) |

The kernel logs the computed stack size at boot:

```
User stack: 1024 KB (auto-scaled from RAM)
```

Set `USER_STACK_SIZE_OVERRIDE` to a non-zero value in `config.rs` to
override automatic scaling.

---

## 11. UDP Buffer Size for DNS

### Symptom

DNS responses larger than 512 bytes may be truncated, causing resolution
failures for packages with many CNAME chains, A/AAAA records, or TXT
records in the additional section.

### Fix

Increased `UDP_PAYLOAD_SIZE` from 512 to 1500 bytes in
`crates/akuma-net/src/smoltcp_net.rs`:

```rust
const UDP_PAYLOAD_SIZE: usize = 1500;
```

1500 bytes is the standard Ethernet MTU and handles most DNS responses
without fragmentation.

---

## 12. gemini-cli Resolution Freeze (ONGOING)

### Symptom

`bun install @google/gemini-cli` freezes during the "Resolving" phase, even
with sufficient RAM (2GB) and stack (1MB). The process times out after ~11
seconds without making progress. Meanwhile, `bun install express` works
correctly under identical conditions.

### Observed Behavior

**express (works):**
```
[syscall] sendto(fd=14, len=36, dest=10.0.2.3:53)  <- DNS query
[syscall] socket(type=TCP) = fd 15                  <- TCP created
[syscall] connect(fd=15, ip=104.16.2.34:443)       <- HTTPS to registry
🔍 Resolving [1/131]
📦 Installing [15/65]
installed express@5.2.1 [15.99s]
```

**gemini-cli (freezes):**
```
[syscall] sendto(fd=14, len=36, dest=10.0.2.3:53)  <- DNS query sent
<no TCP connections>
<no resolution progress>
[exception] Process 49 (/bin/bun) exited (code 0) [10.99s]  <- worker timeout
```

### Key Differences

1. **DNS query is sent** for both packages
2. **No TCP connections** are created for gemini-cli after DNS
3. **Worker thread exits** after ~11 seconds (bun's internal timeout)
4. **No SIGSEGV or crash** - bun silently fails to proceed

### Investigation

The DNS query is sent to 10.0.2.3:53 (QEMU's user-mode networking DNS).
No `recvmsg` or `recvfrom` syscall appears in logs after the DNS query,
suggesting bun either:
- Doesn't receive the DNS response
- Receives it but fails to parse it
- Has an internal error before attempting TCP connections

Kernel logs show no errors - bun simply doesn't call the network syscalls
needed to establish HTTP connections.

### Ruling Out

- **Stack size**: Tested with 1MB (2GB RAM) and 2MB (4GB RAM) - same behavior
- **UDP buffer size**: Increased to 1500 bytes - same behavior  
- **DNS truncation**: DNS response for npm registry is typically ~200 bytes
- **IPv6**: IPv6 socket creation fails with ENOSYS but bun falls back to IPv4

### Status

**NOT YET FIXED.** This appears to be a bun-specific issue triggered by
the `@google/gemini-cli` package metadata. Possible causes:

1. **Package-specific parsing bug**: Something in gemini-cli's metadata
   triggers a code path in bun that hangs
2. **Memory allocation failure**: bun's mimalloc may fail silently for
   the 263-package dependency tree
3. **DNS response parsing**: The DNS response may have characteristics
   that trigger a parsing bug

This issue is separate from the kernel - the kernel correctly handles
all syscalls, but bun never issues the syscalls to proceed.

---

## Memory Requirements Summary

| Package Complexity | Minimum RAM | Notes                           |
|--------------------|-------------|----------------------------------|
| Simple (express)   | 256 MB      | 30 packages, basic resolution   |
| Medium (typescript)| 512 MB-1 GB | More complex dep tree           |
| Large (gemini-cli) | 1.5-2+ GB   | 263 packages, deep recursion    |

### OOM Crash Signature

When bun runs out of memory during operation, the crash pattern is:

```
[DA-DP] ... anon alloc failed, 0 free pages
[signal] sig 11 frame page ... not mappable
[Fault] Process N (name) SIGSEGV after Xs
```

This indicates the demand paging handler couldn't allocate a physical
page for a heap/stack expansion.

---

## 13. Safe User Memory Access (Principled Fix)

### Symptom

Bun's JIT and allocator (mimalloc) frequently trigger Data Aborts from EL1
(`EC=0x25`) during syscalls. This happens when the kernel attempts to
dereference a userspace pointer that:
1.  Is valid in software (`validate_user_ptr` passes) but not yet in hardware
    (TLB/coherency lag).
2.  Is part of a lazy mmap region that hasn't been demand-paged yet.
3.  Is concurrently unmapped by another thread (race condition).

**Manifestation: Kernel-level process kill.** The previous exception handler
treated EL1 faults on user addresses as fatal kernel errors and killed the
process to preserve integrity.

```
[Exception] Sync from EL1: EC=0x25, ISS=0x47
  ELR=0x403d7f30, FAR=0x50004000
  EC=0x25 in kernel code — killing current process (EFAULT)
```

### Root Cause

The kernel relied on "check-then-use" (TOCTTOU) validation. If the check passed
but the use failed (due to the reasons above), the kernel had no recovery path
other than killing the process. Standard Linux behavior requires returning
`EFAULT` from the syscall instead.

### Fix (2026-03-13)

Implemented a comprehensive "Safe User Access" mechanism:

1.  **Thread-Local Recovery:** Added `user_copy_fault_handler` to `ThreadSlot`.
    This stores a recovery address during sensitive copy operations.
2.  **Fault Redirection:** Updated `rust_sync_el1_handler` (`src/exceptions.rs`)
    to check this handler. If set, it redirects `ELR_EL1` to the recovery path
    instead of killing the process.
3.  **Assembly Primitives:** Implemented `copy_from_user_safe` and
    `copy_to_user_safe` in `crates/akuma-exec/src/mmu/user_access.rs` using
    AArch64 assembly. These functions wrap a byte-copy loop with the
    registration/clearing of the fault handler.
4.  **Comprehensive Hardening:** Refactored over 100 instances of raw pointer
    dereferences across the entire syscall layer to use safe primitives:
    -   **Networking:** `sys_sendto`, `sys_recvfrom`, `sys_sendmsg`, `sys_recvmsg` now
        use intermediate kernel buffers.
    -   **Filesystem:** `sys_read`, `sys_write`, `sys_getdents64`, `sys_getcwd` now
        perform chunked safe copies.
    -   **Synchronization:** `sys_futex` safely reads user-space atomic values.
    -   **Metadata:** `sys_fstat`, `sys_newfstatat`, `sys_uname`, `sys_sysinfo` use safe copies.
5.  **Poll Efficiency:** Improved `epoll_pwait`, `ppoll`, and `pselect6` to use
    `schedule_blocking` instead of busy-yielding, allowing threads to sleep
    until network events or timeouts occur.

### Impact

- **Stability:** `opencode` and `bun install` no longer trigger kernel-level
  process kills for valid (but unmapped) memory access. The kernel correctly
  returns `EFAULT` (or demand-pages via the safe access path) and remains stable.
- **Performance:** Efficient polling reduces CPU usage during "Resolving" phases.
- **Correctness:** Resolved the `bun install` hang by ensuring DNS responses
  and socket events correctly wake the event loop.

---

## 14. Kernel Identity Mapping Gap (opencode crash)

### Symptom

`opencode` (a Bun-based application) crashes during startup:

```
[mmap] pid=53 len=0x81000 prot=0x3 flags=0x4022 = 0x2004f6000 (lazy, 45 regions)
[Exception] Sync from EL1: EC=0x25, ISS=0x47
  ELR=0x403dd844, FAR=0x50004000, SPSR=0x80002345
  Thread=8, TTBR0=0x350000454cb000, TTBR1=0x40430000
  Instruction at ELR: 0xf800852b
  Likely: Rn(base)=x9, Rt(dest)=x11
  EC=0x25 in kernel code — killing current process (EFAULT)
  Killing PID 53 (/usr/bin/opencode)
```

The Bun runtime reports `error: EEXIST: file already exists, epoll_ctl`
followed by `[exit code: -14]`. `bun install express` works fine because
it allocates fewer pages (the PMM doesn't cross the 256MB threshold).

### Root Cause

**The kernel identity-mapped only 256MB of RAM in user page tables,
but the PMM allocates physical pages from the entire 1GB of RAM.**

The boot page tables use a 1GB L1 block to identity-map all of RAM
(0x40000000-0x7FFFFFFF). User page tables, however, used 2MB L2 block
entries for only 128 blocks (256MB: 0x40000000-0x4FFFFFFF), leaving
L2[128..511] (0x50000000-0x7FFFFFFF) zeroed:

```
add_kernel_mappings() — BEFORE FIX:
  L2[0..127]:   2MB blocks → PA 0x40000000-0x4FFFFFFF ✅ (256MB)
  L2[128..511]: zeroed → VA 0x50000000-0x7FFFFFFF    ❌ (unmapped!)
```

When system memory usage exceeded ~256MB of physical pages, the PMM
returned pages at physical addresses ≥ 0x50000000. The kernel wrote
to these pages via `phys_to_virt(paddr) = paddr as *mut u8` (identity
mapping), but user TTBR0 had no mapping at those VAs, causing an EL1
data abort.

The crash typically happened during:
- `alloc_page_zeroed()` zeroing a newly-allocated page frame
- Demand paging file reads into a page frame
- Page table manipulation when table frames were above 0x50000000

### Why It Didn't Manifest Earlier

The first 256MB of physical RAM (0x40000000-0x4FFFFFFF) includes the
kernel image (~3MB), boot page tables, kernel heap (16MB), and early
allocations. Small binaries like express only need ~30 packages and
never push PMM allocations past the 256MB boundary. Large binaries
like opencode (Bun, 93MB binary with 45+ lazy mmap regions) consume
enough memory to trigger allocations above 0x50000000.

### Fix (2026-03-13)

Three changes:

1. **Extended kernel RAM mapping to 1GB.**
   `add_kernel_mappings()` now creates 512 L2 block entries
   (0x40000000-0x7FFFFFFF), matching the boot page tables' 1GB coverage.
   All PMM-allocated pages are now accessible via identity mapping
   regardless of which TTBR0 is active.

   ```
   add_kernel_mappings() — AFTER FIX:
     L2[0..511]: 2MB blocks → PA 0x40000000-0x7FFFFFFF ✅ (1GB)
   ```

2. **Updated mmap VA allocator to skip the full kernel range.**
   `KERNEL_VA_END` in `ProcessMemory` changed from 0x50000000 to
   0x80000000. The mmap allocator already skipped 0x40000000-0x4FFFFFFF;
   now it skips the full 1GB identity-mapped range. Normal mmap
   allocations are placed well above 0x80000000 (e.g., 0xbca52000)
   so this has no practical impact.

3. **Fixed block shattering to preserve identity mapping.**
   When `map_user_page` encounters a 2MB block descriptor (e.g., from
   a user `MAP_FIXED` at 0x50000000 for Bun's JSC Gigacage), the block
   must be "shattered" into 512 L3 page entries. Previously,
   `get_or_create_table_atomic` replaced the block with a zeroed L3
   table, destroying the identity mapping for the entire 2MB range. Now,
   `shatter_block_to_pages()` populates the L3 table with page entries
   that reproduce the block's identity mapping. Only the specific 4KB
   page targeted by the user mmap is overwritten.

### Diagnostic Improvement

The EL1 fault handler now prints a hint when FAR falls in the kernel
identity-mapped range (0x40000000-0x7FFFFFFF), making this class of
bug easier to identify in the future.

### Kernel Tests

- `test_kernel_identity_map_covers_full_ram` — walks user page tables
  to verify VA 0x40000000, 0x50000000, 0x50004000 (the crash address),
  0x60000000, 0x7FFE0000 are all mapped
- `test_mmap_allocator_skips_full_kernel_range` — verifies `alloc_mmap`
  never returns addresses in 0x40000000-0x7FFFFFFF
- `test_block_shatter_preserves_identity` — verifies shattered 2MB
  block produces correct 4KB L3 page entries

### Known Limitation

When a user `MAP_FIXED` overlaps the kernel RAM range (e.g., Bun's JSC
Gigacage at 0x50000000), block shattering preserves the identity mapping
for 511 of the 512 pages in the 2MB block. The one page overwritten by
the user mapping is no longer identity-accessible via user TTBR0. If
the PMM later allocates a physical page at that exact address, the
kernel's `phys_to_virt()` write would go to the user's page instead.

This is extremely unlikely in practice (the PMM would need to allocate
a page at the exact same PA as a user-mapped VA) and could be fully
resolved by routing `phys_to_virt()` through TTBR1 (the kernel
high-half page tables, which always have the boot identity mapping).
This is tracked as a future improvement.

---

## Related Documentation

- `docs/EPOLL_EL1_CRASH_FIX.md` -- Detailed design of the principled fix
- `docs/DEVICE_MMIO_VA_CONFLICT.md` -- Device MMIO remapping details
- `docs/BUN_MISSING_SYSCALLS.md` -- Syscalls added for bun
- `docs/MEMORY_LAYOUT.md` -- Physical and virtual memory layout
- `docs/USERSPACE_MEMORY_MODEL.md` -- User address space layout
- `docs/IDENTITY_MAPPING_DEPENDENCIES.md` -- Kernel identity mapping catalog
