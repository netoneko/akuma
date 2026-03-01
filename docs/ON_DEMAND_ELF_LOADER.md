# On-Demand ELF Loader & Large Binary Support

## Problem

The kernel's `read_file()` path buffers the entire ELF binary into kernel heap
before parsing. The ext2 filesystem enforces a 16MB safety limit in
`read_inode_data()` to prevent OOM, and the kernel heap itself is only 32MB.

This means any binary larger than 16MB fails with:
```
Error: Failed to read /bin/bun: Internal error
```

The bun runtime binary is ~93MB, making it impossible to load through the
existing path.

## Solution

Added an on-demand ELF loader that reads segment data page-by-page from disk
using `vfs::read_at()`, never buffering more than one 4KB page at a time.

### New functions

**`src/elf_loader.rs`:**

- `load_elf_from_path(path, file_size)` — Manually parses the 64-byte ELF64
  header and program header table from a small buffer read via `read_at()`.
  Then for each PT_LOAD segment, reads file data one page at a time and copies
  it directly into the user address space. Peak kernel heap usage is ~4KB
  regardless of binary size.

- `load_elf_with_stack_from_path(path, file_size, args, env, stack_size)` —
  Wrapper that calls `load_elf_from_path` and sets up the user stack, auxiliary
  vector, and pre-allocated heap pages (same as `load_elf_with_stack`).

**`src/process.rs`:**

- `Process::from_elf_path(name, path, file_size, args, env)` — Creates a new
  process using the on-demand loader.

- `Process::replace_image_from_path(path, file_size, args, env)` — Replaces the
  current process image using on-demand loading (for execve of large binaries).

### Fallback logic

Both `spawn_process_with_channel_ext()` and `do_execve()` now try `read_file()`
first. If it fails (file exceeds 16MB limit), they fall back to the on-demand
path:

```
read_file(path) → success → from_elf(elf_data)
               → FsError  → file_size(path) → from_elf_path(path, size)
```

Small binaries (<16MB) still use the original buffered path with the `elf` crate
parser. Large binaries use the manual header parser + `read_at()`.

### Manual ELF parsing

The on-demand loader parses ELF64 headers directly instead of using the `elf`
crate's `ElfBytes::minimal_parse()`, which requires the full file buffer. The
manual parser reads:

- ELF64 header (64 bytes): magic, class, endianness, type, machine, entry point,
  program header offset/count/size
- Program headers (56 bytes each): type, flags, offset, vaddr, filesz, memsz

### Inter-segment gap filling

Large ELF binaries typically have gaps between PT_LOAD segments (e.g., between
read-only data and executable code). On Linux, the kernel creates one contiguous
mapping spanning all segments, so gap pages are anonymous zero-filled memory.

The on-demand loader now fills these gaps after loading all segments. It sorts
PT_LOAD segments by virtual address and maps zero-filled pages for any unmapped
ranges between consecutive segments. For bun, this fills a 15-page (60KB) gap
between the read-only data segment (ends at 0x2A2E4D4) and the code segment
(starts at 0x2A3E500).

Without gap filling, bun crashes with a translation fault when it accesses
addresses in the gap (e.g., FAR=0x2a2f000).

### Limitations

- **No kernel-side relocations for large ET_EXEC binaries.** The `elf` crate is
  needed to parse SHT_RELA sections (at the end of the file), which the on-demand
  loader skips. This only affects non-PIE ET_EXEC binaries. All modern large
  binaries (bun, node, etc.) are ET_DYN (PIE) and self-relocate at startup.

- **Interpreter loading still uses `read_file()`.** The dynamic linker
  (ld-musl-aarch64.so.1) is typically <1MB, well within the 16MB limit.

## Memory impact

| Binary size | Old path (buffered) | New path (on-demand) |
|-------------|--------------------|--------------------|
| 1 MB        | 1 MB heap          | 1 MB heap (uses old path) |
| 93 MB       | FAILS (>16MB)      | ~4 KB heap + user pages |

The on-demand loader allocates user-space pages through the PMM (physical memory
manager), which draws from the user pages pool — separate from the kernel heap.

---

## Dynamic Virtual Address Space

### Problem

The original kernel used a fixed 1GB user address space (STACK_TOP = 0x40000000).
Large binaries like bun (93MB code) with a dynamic linker need significantly more
VA space:

- ELF segments span 0x200000–0x5C6D418 (~92MB)
- Interpreter loaded at 0x30000000
- bun's arena allocator reserves 1GB+ contiguous VA regions via mmap

### Solution: `compute_stack_top()`

`src/elf_loader.rs` dynamically computes `STACK_TOP` based on binary layout:

- **Small static binaries** (code < 64MB, no interpreter): keep default 1GB
  layout (STACK_TOP = 0x40000000)
- **Large or dynamic binaries**: expand to provide ~3GB mmap space, with
  `STACK_TOP` up to 4GB (`MAX_STACK_TOP = 0x1_0000_0000`), and
  `MIN_MMAP_SPACE` = 3GB to ensure large JIT allocators have room

The mmap region sits between the code end and the stack bottom. For bun, the
layout is:

```
0x00200000–0x05C6D418  ELF segments (code + data)
0x05C6E000             brk (heap start)
0x30000000–0x30100000  musl dynamic linker
0x30100000–0x303F6000  libstdc++.so.6 + libgcc_s.so.1
0x50000000–0xFFE00000  mmap region (~2.75GB usable)
0xFFE00000–0x100000000 stack (2MB)
```

Note: VA 0x40000000–0x4FFFFFFF is reserved for kernel identity-mapped RAM
(see "Kernel RAM Mapping" below).

---

## User Page Table Fixes

### L1 BLOCK descriptor bug

`add_kernel_mappings()` in `src/mmu.rs` originally created a 1GB BLOCK
descriptor at L1[2] mapping VA 2–3GB → PA 0x80000000. With only 256MB RAM
(PA 0x40000000–0x4FFFFFFF), PA 0x80000000 does not exist.

When `map_page` tried to map the user stack at VA ~0xBFFC0000 (in the 2–3GB
range), `get_or_create_table` saw L1[2] had `VALID` set and treated the BLOCK
descriptor as a TABLE descriptor — extracting PA 0x80000000 as a page table
pointer. Accessing `phys_to_virt(0x80000000)` = 0x80000000 (identity-mapped
but no RAM) caused a synchronous external abort (EC=0x25, ISS=0x10,
FAR=0x80000FF8).

**Fix:** Removed the bogus L1[2] block mapping and hardened
`get_or_create_table` and `get_or_create_table_raw` to distinguish BLOCK
(bit[1]=0) from TABLE (bit[1]=1) descriptors. If a BLOCK is encountered where
a TABLE is needed, it is replaced with a fresh zeroed page table.

### Kernel RAM mapping (L1[1] optimization)

The original 1GB L1 BLOCK at L1[1] mapped VA 0x40000000–0x7FFFFFFF → PA
0x40000000–0x7FFFFFFF. This wasted 768MB of user VA space since only 256MB
of physical RAM exists (PA 0x40000000–0x4FFFFFFF).

**Fix:** Replaced the 1GB L1 BLOCK with an L2 TABLE containing 128 × 2MB
blocks, mapping only the actual 256MB of RAM. L2 entries [128..511] are left
zeroed, making VA 0x50000000–0x7FFFFFFF available for user mmap. This creates
a large contiguous mmap region that bun's 1GB arena can fit into.

Constants in `src/process.rs`:
```rust
const KERNEL_VA_START: usize = 0x4000_0000;
const KERNEL_VA_END: usize   = 0x5000_0000;
```

`alloc_mmap()` skips this range: if `next_mmap` would enter the kernel VA
hole, it jumps to `KERNEL_VA_END` (0x50000000).

---

## Demand Paging (Lazy mmap)

### Problem

bun's mimalloc allocator reserves a 1GB contiguous VA region via mmap
(`MAP_ANONYMOUS`, `PROT_READ|PROT_WRITE`). Eagerly allocating 262,144 physical
pages for this exhausts the PMM (only ~65K pages total, ~48K free).

### Solution

`sys_mmap` in `src/syscall.rs` uses lazy (demand-paged) allocation when any of
these conditions hold:

1. `prot == PROT_NONE` (classic lazy reservation)
2. `flags & MAP_NORESERVE` (caller doesn't expect physical backing)
3. Anonymous region > 1MB (256 pages) — too expensive to eagerly back

Lazy regions are stored in a global `LAZY_REGION_TABLE` in `src/process.rs` —
a `Spinlock<BTreeMap<Pid, Vec<(usize, usize, u64)>>>` mapping PID to a list of
`(start_va, size, page_flags)` tuples. No physical pages are allocated at mmap
time.

#### Why a global table instead of per-Process fields?

Early implementations stored `lazy_regions` as a `Vec` directly on the `Process`
struct. This caused silent data loss due to aliasing `&mut Process` references:
multiple `current_process()` calls within `sys_mmap` (via `alloc_mmap` and the
outer handler) could produce references that the compiler assumed didn't alias,
leading to optimized-out writes. Moving to a separate Spinlock-protected table
eliminates all aliasing issues.

#### CLONE_VM PID keying

With `CLONE_THREAD | CLONE_VM`, multiple PIDs share the same address space.
Lazy regions must be keyed by the address-space owner PID (read from the
process info page via `read_current_pid()`) — NOT the per-thread PID from
`THREAD_PID_MAP`. This ensures all threads in a thread group see the same lazy
regions for both push and lookup.

### Page fault handler

When user code accesses a lazy region, a translation fault fires (EL0 data
abort). The handler in `src/exceptions.rs`:

1. Checks DFSC (bits [5:2] of ISS) for translation fault codes (0x04, 0x08,
   0x0C for L1/L2/L3 faults)
2. Calls `lazy_region_flags(far)` to check if the faulting address is in a
   lazy region
3. If matched: allocates a zeroed physical page, maps it with the stored
   permissions (or `RW_NO_EXEC` default for `PROT_NONE` regions), tracks the
   frame in the process address space, and returns to retry the instruction
4. If not matched or OOM: falls through to the fault reporter (SIGSEGV)

### Partial munmap

JIT allocators (bun/JSC) use a pattern where they mmap a large region, then
munmap the prefix and suffix to produce an aligned sub-range:

```
mmap(NULL, 1GB)      → addr
munmap(addr, prefix)  → trim start to alignment boundary
munmap(aligned+needed, suffix) → trim end
```

`munmap_lazy_region()` in `src/process.rs` handles four cases:
- **Full removal** — unmap covers entire region
- **Prefix removal** — advance region start, shrink size
- **Suffix removal** — shrink size from the end
- **Middle split** — split into two regions

Without this, `sys_munmap` would delete the entire 1GB region when only the
prefix was unmapped, causing the JIT to fault on the aligned sub-range.

### mprotect integration

`sys_mprotect` handles lazy regions that transition from `PROT_NONE` to a real
protection. When `prot != 0` and the page isn't mapped yet, it allocates a
physical page and maps it with the new permissions — effectively materializing
the lazy allocation on demand.

---

## Pointer Validation Fix

### Problem

`validate_user_ptr()` and `copy_from_user_str()` in `src/syscall.rs` rejected
any user pointer above 0x40000000 (the old 1GB limit). With the dynamic address
space, bun's stack lives at 0xBFFC0000–0xC0000000. Every syscall the dynamic
linker made — `openat` with a filename on the stack, `writev` for error
messages — was silently rejected with EFAULT. The linker couldn't search for
libraries or print diagnostics, so it called `_exit(127)`.

### Fix

`user_va_limit()` reads `stack_top` from the current process's `ProcessMemory`,
falling back to 0x40000000 when no process is active. Both `validate_user_ptr`
and `copy_from_user_str` use this dynamic limit instead of the hardcoded 1GB.

---

## New Syscalls and Stubs

Syscalls added or stubbed to support bun. See also `docs/BUN_MISSING_SYSCALLS.md`
for the full list with implementation details.

| Syscall | Number | Implementation |
|---------|--------|----------------|
| `getrusage` | 165 | Zero-fills a 144-byte `rusage` struct (no real tracking) |
| `msync` | 227 | Returns 0 (no-op; no swap or persistent mmap) |
| `process_vm_readv` | 270 | Returns `ENOSYS` (cross-process memory read not supported) |
| `eventfd2` | 19 | Returns a virtual `EventFd` file descriptor |
| `epoll_create1` | 20 | Returns a virtual `EpollFd` file descriptor |
| `epoll_ctl` | 21 | No-op stub (returns 0) |
| `epoll_pwait` | 22 | Returns 0 events immediately |
| `timerfd_create` | 85 | Returns a virtual `TimerFd` file descriptor |
| `timerfd_settime` | 86 | No-op stub (returns 0) |
| `timerfd_gettime` | 87 | Returns zeroed `itimerspec` |
| `clock_getres` | 114 | Returns 1ns resolution |
| `sched_setparam` | 118 | No-op stub (returns 0) |
| `sched_getparam` | 119 | Returns zeroed `sched_param` |
| `sched_setaffinity` | 122 | No-op stub (returns 0) |
| `sched_getaffinity` | 123 | Returns single-CPU affinity mask |
| `sched_yield` | 124 | Calls `threading::yield_now()` |
| `tkill` | 130 | No-op stub (returns 0) |
| `uname` | 160 | Returns "Akuma" sysname, "aarch64" machine |
| `sysinfo` | 179 | Returns real free/total memory from PMM |
| `membarrier` | 319 | Returns supported commands bitmask |
| `close_range` | 436 | Closes file descriptors in the given range |

### `/dev/urandom` and `/dev/random`

bun requires `/dev/urandom` for cryptographic randomization and intentionally
crashes (`FAR=0xBBADBEEF`) if it cannot be opened. Implemented as a virtual
device file descriptor (`DevUrandom`). See `docs/DEV_RANDOM.md` for details.

### `/proc/self/exe`

bun calls `readlinkat(AT_FDCWD, "/proc/self/exe", ...)` and
`openat(AT_FDCWD, "/proc/self/exe", ...)` to find its own executable path.
Both `sys_readlinkat` and `sys_openat` intercept this path and redirect to the
current process's binary name (e.g., `/bin/bun`).

### `mremap` hardening

`sys_mremap` now validates `old_addr` against `user_va_limit()` and checks
the source buffer with `validate_user_ptr` before copying, preventing kernel
crashes from invalid addresses.

### `madvise` no-op

`sys_madvise` is a no-op (returns 0). Previously it attempted to honor
`MADV_DONTNEED` by unmapping pages, which crashed the kernel when applied to
lazy-mapped pages that had no backing physical page.

---

## Diagnostic Improvements

### Verbose mmap logging

When `SYSCALL_DEBUG_IO_ENABLED = true` in `src/config.rs`, all mmap variants
log with PID:

```
[mmap] pid=29 len=0x40000000 prot=0x3 flags=0x22 = 0x50000000 (lazy)
[mmap] pid=29 len=0x1000 prot=0x3 flags=0x22 = 0x90000000 (eager)
[mmap] pid=29 fd=3 file=/usr/lib/libstdc++.so.6.0.34 off=0 len=2904064 = 0x30100000 (read 2820600 bytes)
[mmap] pid=29 len=0x1000 FAIL OOM at page 0/1
[mmap] pid=29 len=0x1000 alloc_mmap FAILED
```

Previously, eager anonymous mmaps were completely silent (excluded from both
the syscall dispatcher filter and the handler-specific logs). This made it
impossible to diagnose failed small allocations.

### TPIDR_EL0 in fault dump

The EL0 data abort fault handler now prints `TPIDR_EL0` alongside the existing
register dump:

```
[Fault] Data abort from EL0 at FAR=0x10, ELR=0x3f21f94, ISS=0x7
[Fault]  x0=0x0 x1=0x20 x2=0x80 x3=0x5afe860
[Fault]  x19=0x5aff410 x20=0x5afeec0 x29=0xbffffbc0 x30=0x3f22184
[Fault]  SP_EL0=0xbffffbc0 SPSR=0x20000000 TPIDR_EL0=0x...
```

This helps diagnose TLS-related crashes: if `TPIDR_EL0 = 0`, the crash is
likely a thread-local storage access before musl's `__init_tls()` ran, since
AArch64 TLS accesses use `mrs x0, tpidr_el0; ldr reg, [x0, #offset]`.

---

## Demand Paging x0 Clobber Bug

### Problem

The `sync_el0_handler` assembly epilogue always loads x0 from a "return value"
slot (offset 280 in the trap frame), designed for syscalls where x0 should
contain the syscall result. But when a data abort triggers successful demand
paging, the handler returned 0, and the epilogue placed 0 into x0 — clobbering
the user's original x0.

The faulting instruction would be retried with x0 = 0 instead of the original
value. If the faulting instruction was inside a loop (e.g., mimalloc's free-list
initialization), x0 would be silently zeroed. After the loop, a subsequent
`ldr reg, [x0, #offset]` would crash with FAR = offset, appearing as a NULL
pointer dereference.

### Diagnosis

Disassembly of the crash site (bun at ELR=0x3f21f94) showed:

```asm
ldrh w11, [x0, #0xa]     ; succeeds — x0 valid at function entry
ldr  x10, [x0, #0x30]    ; succeeds
; ... loop writes to demand-paged memory, triggers fault ...
; ... x0 clobbered to 0 by exception return ...
ldr  x10, [x0, #0x10]    ; CRASH: x0 = 0, FAR = 0x10
```

TPIDR_EL0 was valid (0x303f60e8), ruling out TLS. The x0 clobber was confirmed
by tracing the exception handler's register restore path.

### Fix

Changed the demand paging success return from `return 0` to
`return (*frame).x0`, preserving the user's original x0 through the epilogue.

This bug only affected demand-paged data aborts. Syscalls (EC_SVC64) correctly
use the return value as x0. Fatal faults call `return_to_kernel()` which never
reaches the epilogue.

---

## Exception Handling Additions

### MSR/MRS trap (EC=0x18)

bun reads `CTR_EL0` (Cache Type Register) to determine cache line sizes for
JIT code generation. The handler in `src/exceptions.rs` emulates system
register reads and cache maintenance instructions:

- **MRS (read):** Returns the actual `CTR_EL0` hardware value; returns 0 for
  other unrecognized system registers.
- **DC CVAU / IC IVAU (write):** Performs cache maintenance on behalf of user
  code. This is a fallback — with `SCTLR_EL1.UCI = 1` (set in `src/boot.rs`),
  these instructions normally execute in EL0 without trapping.

### BRK instruction (EC=0x3C)

bun's JSC and libc use `BRK` instructions for assertion failures and
deliberate panics. The handler terminates the process cleanly with `SIGTRAP`
instead of printing an unhandled exception error.

---

## JIT Cache Coherency

### Problem

bun's JavaScriptCore JIT writes executable code into demand-paged mmap
regions. On AArch64, instruction and data caches are not coherent — writing
code through the data cache and then executing it requires explicit cache
maintenance:

1. `DC CVAU` — clean data cache by VA to point of unification
2. `DSB ISH` — ensure clean completes
3. `IC IVAU` — invalidate instruction cache by VA
4. `DSB ISH` + `ISB` — synchronize

Without `SCTLR_EL1.UCI = 1`, `DC CVAU` and `IC IVAU` from EL0 trap to EL1.
The original kernel did not set UCI, so these instructions trapped. The trap
handler (EC=0x18) only handled `MRS CTR_EL0` reads and silently skipped all
other system instructions — advancing PC without performing the cache
operation. The CPU then executed stale instruction cache contents, producing
garbage instructions (e.g., x8 loaded with an mmap address instead of a
syscall number, causing "Unknown syscall: 1349652480").

### Fix

1. **`src/boot.rs`:** Set `SCTLR_EL1.UCI = 1` and `SCTLR_EL1.UCT = 1` so
   `DC CVAU`, `IC IVAU`, and `MRS CTR_EL0` execute directly from EL0 without
   trapping.
2. **`src/exceptions.rs`:** EC_MSR_MRS_TRAP handler now emulates `DC CVAU`
   and `IC IVAU` for robustness (fallback if UCI alone isn't sufficient).
3. **`src/syscall.rs`:** `sys_mprotect` flushes data cache and invalidates
   instruction cache when adding `PROT_EXEC` permission, ensuring the kernel-
   side permission change also guarantees cache coherency.

---

## CLONE_VM Thread Group Cleanup

### Problem

When a CLONE_VM thread group's main thread exits (e.g., SIGSEGV), it drops
its `UserAddressSpace` which has `shared == false`, freeing ALL page tables
and user pages. But sibling threads (created via `clone_thread`) have
`shared == true` address spaces pointing to the same L0 page table. When
the scheduler gives them CPU time, TTBR0 points to freed memory, causing
EL1 data aborts. The EL1 handler enters a `wfe` loop with IRQs disabled,
halting the entire system.

### Fix

1. **`src/process.rs`:** Added `kill_thread_group(my_pid, l0_phys)` which
   finds all processes sharing the same L0 page table address (via
   `UserAddressSpace::l0_phys()`), cleans up their file descriptors, lazy
   regions, channels, and thread state, then unregisters them. Called from
   `return_to_kernel` before dropping the address-space owner.

2. **`src/syscall.rs`:** `exit_group` (NR 94) now calls `sys_exit_group`
   which invokes `kill_thread_group` to terminate all sibling threads, not
   just the calling thread.

3. **`src/mmu.rs`:** Added `l0_phys()` and `is_shared()` accessors to
   `UserAddressSpace`.

### tkill fix

`tkill` (NR 130) previously called `return_to_kernel(-sig)` on the calling
thread, ignoring the target TID entirely. Any `tkill` call would kill the
caller. Changed to a no-op stub since signal delivery is not implemented.

---

## Files Modified

| File | Changes |
|------|---------|
| `src/boot.rs` | Set SCTLR_EL1.UCI and UCT for EL0 cache maintenance |
| `src/elf_loader.rs` | On-demand ELF loader, `compute_stack_top()`, 4GB VA space |
| `src/mmu.rs` | L2 kernel RAM table, BLOCK-vs-TABLE hardening, `track_page_table_frame`, `l0_phys()`, `is_shared()` |
| `src/process.rs` | `LAZY_REGION_TABLE`, `munmap_lazy_region`, `clone_lazy_regions`, `kill_thread_group`, `EpollFd`/`TimerFd`/`EventFd` variants, `alloc_mmap` kernel VA skip |
| `src/syscall.rs` | `user_va_limit()`, demand-paged mmap, mmap logging, `/dev/urandom`, `/proc/self/exe`, `mremap` hardening, ~20 new syscall stubs, partial munmap, CLONE_VM PID keying, mprotect cache flush, `sys_exit_group`, nanosleep ABI fix |
| `src/exceptions.rs` | Demand paging fault handler, x0 preservation fix, MSR/MRS+DC/IC trap handler, BRK handler, TPIDR_EL0 in fault dump |
| `src/config.rs` | `USER_STACK_SIZE = 2MB` |
| `bootstrap/lib/` | `libc.musl-aarch64.so.1 → ld-musl-aarch64.so.1` symlink |
