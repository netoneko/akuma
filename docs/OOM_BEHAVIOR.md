# Out-of-Memory (OOM) Behavior

This document describes the kernel's behavior when physical memory is exhausted,
common failure scenarios, and how to diagnose and address them.

## Overview

Akuma uses a bitmap-based physical memory manager (PMM) that allocates 4KB pages.
When memory is exhausted, various subsystems fail in different ways:

| Component | Failure Mode | Symptom |
|-----------|--------------|---------|
| ELF Loader | `AddressSpaceFailed` | "Failed to load ELF: Failed to create address space" |
| Userspace Allocator | Panic | "PANIC!" followed by process exit |
| Page Fault Handler | SIGSEGV | "[Fault] Data abort from EL0" |

## Memory Budget (128 MB Configuration)

With the default 128 MB RAM configuration:

```
Total RAM:           128 MB = 32,768 pages (4KB each)
Kernel code/stack:   ~16 MB = ~4,096 pages
Kernel heap:         ~32 MB = ~8,192 pages
Available for users: ~80 MB = ~20,480 pages
```

### Per-Process Memory Usage

| Process | Code Pages | Stack | Heap Pre-alloc | Page Tables | Total |
|---------|------------|-------|----------------|-------------|-------|
| hello   | 1          | 16    | 16             | ~5          | ~38 pages (~152 KB) |
| httpd   | 7          | 16    | 16             | ~5          | ~44 pages (~176 KB) |
| herd    | 9          | 16    | 16             | ~5          | ~46 pages (~184 KB) |
| qjs     | 132        | 16    | 16             | ~5          | ~169 pages (~676 KB) |

**Note:** qjs (QuickJS) is a JavaScript engine and uses significantly more memory
than simple binaries. It also allocates additional heap dynamically during execution.

## Common Failure Scenario: Cascading OOM

When running httpd with CGI support, each JavaScript request spawns a qjs process:

1. **Initial state**: herd (PID 8), httpd (PID 9) running, ~20,000 free pages
2. **CGI request arrives**: httpd spawns qjs (PID 10), uses ~170 pages
3. **Multiple requests**: Each concurrent request spawns another qjs
4. **Memory exhaustion**: After ~100 concurrent qjs processes, memory exhausted

When memory is exhausted, **all processes may fail simultaneously**:

```
[spawn_process] path=/bin/qjs user_threads_available=22
[ELF] Loaded: entry=0x400000 brk=0x48326c pages=132
PANIC!                                          <- qjs OOM
[exception] Process 19 exited, calling return_to_kernel(1)
PANIC!                                          <- httpd OOM (allocating response buffer)
[exception] Process 9 exited, calling return_to_kernel(1)
PANIC!                                          <- herd OOM (allocating log buffer)
[exception] Process 8 exited, calling return_to_kernel(1)
[Herd] Process exited with code 1
```

This is a **cascading failure**: when system memory is exhausted, any process
attempting allocation will fail. Since Rust's default allocation failure handler
panics, multiple processes crash around the same time.

## Failure Points in Detail

### 1. ELF Loading Failure

When spawning a new process, `UserAddressSpace::new()` allocates:
- 1 page for L0 page table
- 1 page for L1 page table  
- 1 page for L2 page table
- N pages for ELF segments (code, data)
- 16 pages for user stack
- 16 pages for pre-allocated heap

If any allocation fails, the error propagates:

```rust
// src/elf_loader.rs
let l0_frame = pmm::alloc_page_zeroed()?;  // Returns None if OOM
// ...
Err(ElfError::AddressSpaceFailed)  // Becomes "Failed to create address space"
```

### 2. Userspace Allocation Failure

The userspace allocator uses `brk()` to expand the heap:

```rust
// userspace/libakuma/src/lib.rs
fn brk_expand(&self, needed: usize) -> bool {
    let result = super::brk(new_end);
    if result >= new_end {
        true   // Success
    } else {
        false  // Kernel couldn't allocate pages
    }
}

unsafe fn brk_alloc(&self, layout: Layout) -> *mut u8 {
    if new_head > current_end {
        if !self.brk_expand(needed) {
            return ptr::null_mut();  // OOM
        }
    }
    // ...
}
```

When `GlobalAlloc::alloc` returns null, Rust calls `handle_alloc_error` which
panics by default. The panic handler in libakuma prints "PANIC!" and exits:

```rust
// userspace/libakuma/src/lib.rs
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    eprint("PANIC!\n");
    exit(1);
}
```

### 3. Page Fault on Unmapped Memory

If a process accesses memory beyond its mapped pages (e.g., stack overflow,
heap access before brk expansion), a data abort occurs:

```rust
// src/exceptions.rs
esr::EC_DATA_ABORT_LOWER => {
    crate::safe_print!(128, "[Fault] Data abort from EL0 at FAR={:#x}...\n", far);
    crate::process::return_to_kernel(-11)  // SIGSEGV
}
```

## Diagnosis

### Check PMM Stats

Use the shell command to check physical memory:

```
akuma:/> pmm
Physical Memory Manager:

             pages       MB
Total:      32768      128
Allocated:  12000       46
Free:       20768       82
```

### Enable Frame Tracking

For detailed allocation tracking, enable debug mode in `src/pmm.rs`:

```rust
pub const DEBUG_FRAME_TRACKING: bool = true;
```

Then use:

```
akuma:/> pmm leaks
Current allocations:

Kernel:               150
User Page Table:       45
User Data:            200
ELF Loader:          1320
Unknown:                0
```

### Monitor Thread Stats

The main loop logs thread statistics:

```
[Thread0] loop=100000 | run=1 rdy=3 wait=1 term=0 init=0 free=27
```

- `free=27` means 27 thread slots available (out of 32)
- Low `free` count with multiple qjs processes indicates high load

## Solutions

### 1. Increase RAM (Quick Fix)

Edit `scripts/run.sh`:

```bash
qemu-system-aarch64 \
  ...
  -m 256M \   # Increase from 128M
  ...
```

Update the matching constant in `src/main.rs`:

```rust
const DEFAULT_RAM_SIZE: usize = 256 * 1024 * 1024;
```

### 2. Limit Concurrent CGI Processes

In httpd, check available memory before spawning:

```rust
// Check if we have enough pages for qjs (~200 pages = 800KB)
let (_, _, free) = pmm::stats();
if free < 250 {
    send_error(stream, 503, "Service Unavailable");
    return;
}
```

### 3. Graceful OOM Handling in Userspace

Instead of panicking on allocation failure, handle it gracefully:

```rust
// In user code, check allocations
let vec = Vec::try_reserve(size);
if vec.is_err() {
    // Handle OOM gracefully
    return Err("Out of memory");
}
```

### 4. Request Queuing

For high-traffic CGI, implement a request queue instead of spawning
unlimited concurrent processes:

```rust
const MAX_CGI_PROCESSES: usize = 3;
static CGI_SEMAPHORE: AtomicUsize = AtomicUsize::new(MAX_CGI_PROCESSES);

fn handle_cgi() {
    if CGI_SEMAPHORE.fetch_sub(1, Ordering::SeqCst) == 0 {
        CGI_SEMAPHORE.fetch_add(1, Ordering::SeqCst);
        send_error(stream, 503, "Too many requests");
        return;
    }
    // ... handle CGI ...
    CGI_SEMAPHORE.fetch_add(1, Ordering::SeqCst);
}
```

## Memory Reclamation

When a process exits, its memory is freed:

```rust
// src/mmu.rs
impl Drop for UserAddressSpace {
    fn drop(&mut self) {
        // Free all user pages
        for frame in &self.user_frames {
            pmm::free_page(*frame);
        }
        // Free all page table frames
        for frame in &self.page_table_frames {
            pmm::free_page(*frame);
        }
        // Free L0 table
        pmm::free_page(self.l0_frame);
        // Free ASID
        ASID_ALLOCATOR.lock().free(self.asid);
    }
}
```

Memory should be reclaimed promptly when processes exit. If memory isn't being
freed, enable `DEBUG_FRAME_TRACKING` to identify leaks.

## Related Documentation

- [MEMORY_LAYOUT.md](MEMORY_LAYOUT.md) - Physical and virtual memory layout
- [USERSPACE_MEMORY_MODEL.md](USERSPACE_MEMORY_MODEL.md) - User address space design
- [THREAD_STACK_ANALYSIS.md](THREAD_STACK_ANALYSIS.md) - Stack sizing and overflow detection
