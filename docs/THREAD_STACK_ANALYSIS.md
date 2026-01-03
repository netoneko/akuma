# Thread Stack Analysis

This document analyzes stack usage in the kernel's threading system and documents the memory protection features.

## Stack Configuration

All stack sizes are configurable via `src/config.rs`:

```rust
/// Boot/kernel stack size (1MB default)
pub const KERNEL_STACK_SIZE: usize = 1024 * 1024;

/// Default per-thread stack size (32KB)
pub const DEFAULT_THREAD_STACK_SIZE: usize = 32 * 1024;

/// Stack size for networking/async thread (256KB)
pub const ASYNC_THREAD_STACK_SIZE: usize = 256 * 1024;

/// User process stack size (64KB default)
pub const USER_STACK_SIZE: usize = 64 * 1024;

/// Maximum kernel threads
pub const MAX_THREADS: usize = 32;

/// Enable stack canary checking
pub const ENABLE_STACK_CANARIES: bool = true;
```

### Boot Thread (Thread 0)

The boot stack is configured in `src/boot.rs`:

```asm
.equ STACK_SIZE,        0x100000        // 1MB stack
.equ STACK_TOP,         0x42000000      // 32MB from kernel base

_boot:
    ldr     x0, =STACK_TOP
    mov     sp, x0
```

| Property | Value |
|----------|-------|
| Stack top | `0x42000000` (32MB from kernel base) |
| Stack size | 1MB (grows down to `0x41F00000`) |
| Usage | Async networking loop (SSH, HTTP, etc.) |
| Guard pages | ❌ None (uses 1GB block mapping) |
| Protection | ⚠️ Tracked in `StackInfo` for bounds checking |

**Status**: ✅ Fixed - Stack is positioned above the kernel binary (which is ~3.5MB).

> **Note**: The async networking loop (SSH, HTTP, network polling) currently runs on
> thread 0 with the 1MB boot stack. While `config::ASYNC_THREAD_STACK_SIZE` (256KB)
> exists for spawning dedicated network threads, the main async loop uses thread 0
> because `NetworkInit` contains non-Send types. The 1MB boot stack is more than
> sufficient for current async workloads.
>
> **Future work**: Refactor network initialization to support spawning async on a
> dedicated thread with configurable stack size.

### Spawned Threads (1-31)

Spawned threads get stacks allocated on demand with configurable sizes:

```rust
// Spawn with default 32KB stack
threading::spawn(my_thread)?;

// Spawn with custom stack size
threading::spawn_fn_with_stack(|| {
    run_network_server();
}, config::ASYNC_THREAD_STACK_SIZE, false)?;
```

| Property | Value |
|----------|-------|
| Default size | 32KB per thread (configurable) |
| Max threads | 32 (thread 0 + 31 spawned) |
| Allocation | Heap via `Vec<u8>` |
| Guard pages | ❌ None (heap allocated) |
| Protection | ✅ Stack canaries, overlap checking, bounds validation |

### User Process Stacks

User processes get stacks mapped via the ELF loader with a guard page:

```rust
const STACK_TOP: usize = 0x4000_0000;  // Top of first 1GB

// Stack layout (grows down):
// [guard page] [stack pages...] [STACK_TOP]
let guard_page = (STACK_TOP - total_size) & !(PAGE_SIZE - 1);
let stack_bottom = guard_page + PAGE_SIZE;
```

| Property | Value |
|----------|-------|
| Stack size | 64KB default (`config::USER_STACK_SIZE`) |
| Location | Top of first 1GB (`0x3FF00000-0x40000000`) |
| Allocation | Via MMU page mapping |
| Guard pages | ✅ One unmapped page at bottom |
| Protection | ✅ Hardware fault on overflow |

---

## Protection Mechanisms

### Summary Table

| Protection | Boot Thread | Kernel Threads | User Processes |
|------------|-------------|----------------|----------------|
| Guard pages | ❌ | ❌ | ✅ |
| Stack canaries | ✅ Tracked | ✅ | N/A |
| Overlap checking | N/A | ✅ | N/A |
| SP bounds validation | ✅ | ✅ | Via page faults |
| Configurable size | Fixed | ✅ | ✅ |

### Stack Canaries

Stack canaries are magic values written at the bottom of each thread stack:

```rust
const STACK_CANARY: u64 = 0xDEAD_BEEF_CAFE_BABE;
const CANARY_WORDS: usize = 8;  // 64 bytes of canaries
```

Canaries are:
- Initialized when stack is allocated
- Checked periodically in the async main loop
- Checked when threads are reclaimed
- Logged when corruption is detected

```rust
// Periodic check in async loop
let bad = threading::check_all_stack_canaries();
if !bad.is_empty() {
    console::print("[WARN] Stack overflow detected in threads: ...");
}
```

### Overlap Protection

Each thread stack is tracked via `StackInfo`:

```rust
pub struct StackInfo {
    pub base: usize,  // Stack base (lowest address)
    pub size: usize,  // Stack size in bytes
    pub top: usize,   // Stack top (highest address)
}
```

On spawn, new stacks are checked for overlap:

```rust
// Verify no overlap with existing stacks
for (i, existing) in self.stacks.iter().enumerate() {
    if i != slot_idx && new_stack.overlaps(existing) {
        return Err("Stack allocation overlaps with existing thread");
    }
}
```

Debug functions:
- `threading::check_stack_overlaps()` - Returns list of overlapping thread pairs
- `threading::get_stack_bounds(tid)` - Get base/top for a thread
- `threading::validate_current_sp()` - Check if SP is within current thread's bounds

### User Stack Guard Pages

User stacks have an unmapped guard page at the bottom:

```
0x40000000  <- STACK_TOP (unmapped, end of user space)
0x3FFF0000  <- stack_end (top of mapped stack)
   ...      <- stack pages (RW)
0x3FFE0000  <- stack_bottom (first mapped page)
0x3FFDF000  <- guard_page (UNMAPPED - causes fault on overflow)
```

On stack overflow, the user process triggers:
- **Exception**: Data Abort from EL0 (EC=0x24)
- **FAR**: Points to the guard page address
- **Result**: Process is terminated instead of silently corrupting memory

---

## Per-Thread Stack Size API

### Spawning with Custom Stack Size

```rust
use crate::config;

// Spawn extern "C" function with custom stack
threading::spawn_with_stack_size(
    my_heavy_thread,
    config::ASYNC_THREAD_STACK_SIZE,  // 256KB
    false  // not cooperative
)?;

// Spawn closure with custom stack
threading::spawn_fn_with_stack(|| {
    run_network_server();
}, 256 * 1024, false)?;
```

### Recommended Stack Sizes

| Workload | Recommended Size | Reason |
|----------|------------------|--------|
| Simple background task | 32KB | Default, sufficient for most work |
| Network/async polling | 256KB | Deep call chains in SSH/HTTP |
| Shell command execution | 64KB | Medium complexity |
| Heavy recursive work | 128KB+ | Depends on recursion depth |

---

## Stack Overflow Symptoms

### Kernel Thread Overflow (No Guard Pages)

Without guard pages, kernel thread overflow causes **silent corruption**:
- Canary corruption (detected by periodic checks)
- Random crashes or hangs
- Corrupted heap allocations
- Corrupted adjacent thread stacks

### User Process Overflow (With Guard Pages)

With guard pages, user overflow causes a **clean fault**:
```
[PROC] Process pid=1 faulted: EC=0x24 (Data Abort from EL0)
[PROC] FAR=0x3FFDF0F8 (guard page)
[PROC] Process terminated
```

---

## Async Execution and Stack Depth

The async main loop runs on thread 0 (boot thread with 1MB stack):

```rust
// In src/main.rs run_async_main()
loop {
    let _ = runner_pinned.as_mut().poll(&mut cx);  // Network
    let _ = ssh_pinned.as_mut().poll(&mut cx);     // SSH server
    let _ = web_pinned.as_mut().poll(&mut cx);     // HTTP server
    
    // Periodic canary check
    if config::ENABLE_STACK_CANARIES {
        let bad = threading::check_all_stack_canaries();
        // ...
    }
    
    threading::yield_now();
}
```

### Async Call Chain Depth

Each `poll()` creates a call chain. Complex async code = deep stacks:

| Component | async/await points | Typical call depth |
|-----------|-------------------|-------------------|
| SSH Protocol | ~100 | Deep (crypto, protocol, shell) |
| Shell Commands | ~200 | Deep (command execution, I/O) |
| Web Server | ~20 | Medium |
| Network Runner | ~40 | Medium |

### Stack Safety

**Future state**: Stored on heap - doesn't consume stack.

**Poll chains**: Use thread stack - can be deep with complex async code.

The 1MB boot stack handles current async complexity comfortably.

---

## Future Improvements

### Fine-Grained Kernel Page Tables

Currently impossible due to 1GB block mapping. Would require:

1. Switch kernel region to 4KB page mappings in `src/boot.rs`
2. Add kernel page table management in `src/mmu.rs`
3. Allocate thread stacks via PMM instead of heap
4. Leave guard pages unmapped

This would enable:
- Hardware guard pages for kernel threads
- Read-only protection for kernel code (`.text`)
- Execute-never for data sections
- Full W^X (write XOR execute) policy

---

## Related Files

- `src/config.rs` - Stack size configuration constants
- `src/boot.rs` - Boot stack setup
- `src/threading.rs` - Thread pool, per-thread stacks, canaries, overlap checking
- `src/elf_loader.rs` - User stack setup with guard page
- `src/process.rs` - Process memory management
- `src/mmu.rs` - Page table management
- `src/main.rs` - Async loop with periodic canary checks
