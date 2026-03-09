# Kernel Heap and Memory Improvements

This document describes the memory improvements implemented to improve stability,
prevent OOM crashes, and free up more RAM for userspace processes.

## Motivation

The kernel previously used a fixed 16 MB talc heap for everything: page tables,
process control blocks, VFS caches, SSH buffers, networking state, and all 32
thread stacks. Under load this caused:

- Kernel panics on allocation failure (no recovery path)
- Cascading OOM: one failing process would trigger others
- SSH sessions with unbounded buffers consuming heap until exhaustion
- ~5 MB of heap permanently consumed by thread stacks that could live outside the heap

## Changes

### 1. Heap Watermark and Memory Pressure (`src/allocator.rs`)

Added `is_memory_low()` which returns `true` when free heap falls below 2 MB:

```rust
pub fn is_memory_low() -> bool {
    let free = heap_size.saturating_sub(allocated);
    free < HEAP_LOW_WATERMARK  // 2 MB
}
```

This is exposed via `ExecRuntime::is_memory_low` and used as a circuit breaker
across the kernel.

### 2. Memory Pressure Checks at Admission Points (`crates/akuma-exec/src/process.rs`, `src/ssh/protocol.rs`)

Before any expensive allocation, the kernel now checks memory pressure and
returns a clean error instead of proceeding toward OOM:

- `fork_process()` — returns `Err("Kernel memory low, cannot fork")`
- `clone_thread()` — returns `Err("Kernel memory low, cannot clone thread")`
- `spawn_process_with_channel_ext()` — returns `Err("Kernel memory low, cannot spawn new process")`
- `handle_connection()` (SSH) — drops the TCP connection before allocating session state

### 3. Fallible Process Struct Allocation (`crates/akuma-exec/src/process.rs`)

Replaced `Box::new(Process { ... })` with `Box::try_new(...)` in all three
process creation paths:

- `fork_process()`
- `clone_thread()`
- `spawn_process_with_channel_ext()`

On failure these return a `Result::Err` which propagates to the syscall layer
as `ENOMEM`, so the userspace process gets an error rather than the kernel
panicking. Requires `#![feature(allocator_api)]` in `crates/akuma-exec/src/lib.rs`.

### 4. Bounded SSH Buffers (`crates/akuma-ssh/src/session.rs`, `src/ssh/protocol.rs`)

SSH sessions previously accumulated data in unbounded `Vec<u8>` buffers.
Added explicit size limits and safe accessor methods:

```
INPUT_BUFFER_MAX       = 256 KB  (raw TCP data pending decode)
CHANNEL_DATA_BUFFER_MAX =  64 KB  (decoded terminal input)
```

New methods on `SshSession`:
- `feed_input(&[u8]) -> bool` — appends to input buffer, returns false and logs if limit exceeded
- `feed_channel_data(&[u8]) -> bool` — same for channel data buffer

All five `extend_from_slice` call sites in `src/ssh/protocol.rs` replaced with
these bounded methods.

### 5. Thread Stacks Moved from Heap to PMM (`src/pmm.rs`, `crates/akuma-exec/src/threading.rs`)

Thread stacks were allocated as `Vec<u8>` on the kernel heap, consuming ~5 MB:

| Thread type   | Count | Stack size | Total  |
|---------------|-------|------------|--------|
| System (1–7)  | 7     | 256 KB     | 1.75 MB |
| User (8–31)   | 24    | 128 KB     | 3 MB   |
| Boot exc. stack | 1   | 1 KB       | 1 KB   |
| **Total**     |       |            | **~5 MB** |

Stacks now come from the Physical Memory Manager (PMM) via contiguous page
allocation, completely bypassing the kernel heap.

#### New PMM functions (`src/pmm.rs`)

```rust
pub fn alloc_pages_contiguous_zeroed(count: usize) -> Option<PhysFrame>
pub fn free_pages_contiguous(frame: PhysFrame, count: usize)
```

The bitmap allocator gained a contiguous scan: it walks the bitmap looking for
`count` consecutive free bits, marks them all used atomically, and returns the
base frame. On deallocation it marks all `count` pages free in one pass.

Both functions disable IRQs for the duration to match the existing PMM locking
discipline.

#### Threading changes (`crates/akuma-exec/src/threading.rs`)

`allocate_stack_for_slot()` now:
1. Computes the number of 4 KB pages needed
2. Calls `(runtime().alloc_pages_contiguous_zeroed)(pages)`
3. Converts the physical frame to a kernel virtual address via `phys_to_virt`
4. Stores the virtual base in `StackInfo`

`free_stack_for_slot()` reverses the above using `virt_to_phys` then
`(runtime().free_pages_contiguous)`.

`reallocate_stack()` (called when a thread slot is reused with a different stack
size) now calls `free_stack_for_slot` then `allocate_stack_for_slot` instead of
the old Box-based path.

The boot exception stack (thread 0) follows the same pattern.

### 6. Kernel Heap Shrunk from 16 MB to 8 MB (`src/main.rs`)

With thread stacks off the heap, the heap only needs to hold metadata. The heap
constant was reduced:

```rust
const KERNEL_HEAP_SIZE: usize = 8 * 1024 * 1024;  // was 16 MB
```

## Memory Budget (256 MB QEMU)

| Component               | Before | After  | Delta    |
|-------------------------|--------|--------|----------|
| Code + boot stack       | 16 MB  | 16 MB  | —        |
| Kernel heap             | 16 MB  | 8 MB   | -8 MB    |
| Thread stacks (on heap) | ~5 MB  | 0 MB   | -5 MB    |
| Thread stacks (PMM)     | 0 MB   | ~5 MB  | +5 MB    |
| PMM bitmap + metadata   | ~1 MB  | ~1 MB  | —        |
| **Available to userspace** | **~223 MB** | **~231 MB** | **+8 MB** |

The 8 MB gain comes entirely from shrinking the heap. The stack memory moved
from heap to PMM, so it still counts against the overall PMM pool — but it is
no longer fragmented into the talc heap and no longer competes with kernel
metadata allocations.

## Runtime API Additions (`crates/akuma-exec/src/runtime.rs`)

Two new function pointers added to `ExecRuntime`:

```rust
pub alloc_pages_contiguous_zeroed: fn(usize) -> Option<PhysFrame>,
pub free_pages_contiguous: fn(PhysFrame, usize),
pub is_memory_low: fn() -> bool,
```

Registered in `src/main.rs` pointing at the corresponding PMM and allocator
functions.

## Files Changed

| File | Change |
|------|--------|
| `src/allocator.rs` | Added `is_memory_low()`, `HEAP_LOW_WATERMARK` |
| `src/pmm.rs` | Added contiguous alloc/free to bitmap and public API |
| `src/main.rs` | Heap 16→8 MB, registered new runtime functions |
| `src/ssh/protocol.rs` | Memory pressure check on accept; bounded buffer calls |
| `crates/akuma-exec/src/runtime.rs` | Three new function pointers |
| `crates/akuma-exec/src/lib.rs` | `#![feature(allocator_api)]` |
| `crates/akuma-exec/src/threading.rs` | PMM-backed stacks, new free helper |
| `crates/akuma-exec/src/process.rs` | Fallible Box, pressure checks in spawn/fork/clone |
| `crates/akuma-ssh/src/session.rs` | Buffer limits, `feed_input`, `feed_channel_data` |
