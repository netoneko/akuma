# OOM Recovery Options

When the kernel heap is exhausted, any allocation triggers Rust's default
`handle_alloc_error` (in `alloc/src/alloc.rs`), which panics. The kernel's
`#[panic_handler]` in `src/main.rs` calls `halt()`, killing the entire system.

The kernel currently has **no custom `#[alloc_error_handler]`**. Userspace
(`libakuma`) does have one, but it only covers userspace-side allocations.
Kernel-side allocations made on behalf of a user session (SSH channel buffers,
network I/O, VFS reads) go through the kernel's global allocator and hit the
default panic path.

## Current crash flow

```
kernel code calls Vec::push / Box::new / etc.
  → GlobalAlloc::alloc returns null (talc heap full)
    → alloc::alloc::handle_alloc_error (default: panic!)
      → #[panic_handler] in src/main.rs
        → halt()  ← entire system dead
```

## Option 1: Custom `#[alloc_error_handler]` — kill thread, not system

Add `#![feature(alloc_error_handler)]` to `src/main.rs` and define a handler
that terminates only the current thread instead of halting:

```rust
#[alloc_error_handler]
fn alloc_error(layout: core::alloc::Layout) -> ! {
    console::print("[KERNEL OOM] size=");
    console::print_dec(layout.size());
    console::print(" align=");
    console::print_dec(layout.align());
    console::print("\n");

    let tid = threading::current_thread_id();
    if tid >= config::RESERVED_THREADS {
        // User process thread — tear down just this process
        process::return_to_kernel(-12); // ENOMEM
    } else {
        // System thread — can't easily recover, fall back to panic
        panic!("OOM on system thread {}", tid);
    }
}
```

**Pros:**
- Single function, minimal code change
- User process OOM no longer kills the system
- `return_to_kernel` already cleans up address space, page tables, FDs, and
  frees all process memory — other processes can continue

**Cons:**
- System threads (0..RESERVED_THREADS) still panic on OOM
- Doesn't help if the OOM happens in shared kernel paths (e.g. the network
  polling loop on thread 0)
- The handler must diverge (`-> !`), so it can't "retry" — the thread is gone

**Complexity:** Low

## Option 2: Fallible allocations in hot paths

Replace infallible allocations with `try_*` variants in code that handles
variable-size or untrusted data:

```rust
// Before (panics on OOM):
let mut buf = vec![0u8; channel_data.len()];

// After (returns error):
let mut buf = Vec::new();
buf.try_reserve(channel_data.len())
    .map_err(|_| "out of memory")?;
buf.resize(channel_data.len(), 0);
```

Key paths to audit:
- `src/ssh/` — channel data receive buffers, key exchange
- `src/smoltcp_net.rs` — packet buffers
- `src/shell/` — command output collection
- `src/vfs/ext2.rs` — file read buffers
- `src/process.rs` — process spawn / ELF loading (already partly fallible)
- `src/syscall.rs` — `sys_read`, `sys_write` buffer allocation

**Pros:**
- Most robust — OOM is handled at the point of allocation, caller decides
  what to do (close connection, return error code, etc.)
- Works for system threads too
- No unstable features needed for the core pattern (`try_reserve` is stable)

**Cons:**
- Requires auditing and changing many call sites
- Easy to miss a path — one missed `Vec::push` still panics
- `Box::try_new` is nightly-only (but we're already on nightly)

**Complexity:** High (many call sites)

## Option 3: Emergency memory reserve

Pre-allocate a reserve pool at boot. When OOM hits, free the reserve to give
the system breathing room for graceful shutdown of the offending task:

```rust
static EMERGENCY_RESERVE: Spinlock<Option<alloc::vec::Vec<u8>>> =
    Spinlock::new(None);

// At boot:
*EMERGENCY_RESERVE.lock() = Some(alloc::vec![0u8; 256 * 1024]);

// In alloc_error_handler:
fn alloc_error(layout: core::alloc::Layout) -> ! {
    // Release reserve back to heap
    let _ = EMERGENCY_RESERVE.lock().take();
    console::print("[OOM] Released 256KB emergency reserve\n");

    let tid = threading::current_thread_id();
    if tid >= config::RESERVED_THREADS {
        process::return_to_kernel(-12);
    } else {
        panic!("OOM on system thread");
    }
}
```

**Pros:**
- Combines with Option 1 to make thread teardown itself less likely to OOM
- The freed memory lets `return_to_kernel` / `Drop` impls run without
  hitting a second OOM

**Cons:**
- One-shot: once the reserve is freed, the next OOM has no safety net
- 256KB of permanently "wasted" memory during normal operation
- Need a mechanism to refill the reserve after the crisis passes

**Complexity:** Low (builds on Option 1)

## Option 4: Per-session / per-process memory limits

Track kernel-heap bytes consumed on behalf of each process or SSH session.
Reject allocations that would exceed a quota before they hit the global OOM:

```rust
// In process.rs or a new module:
pub struct MemoryQuota {
    used: AtomicUsize,
    limit: usize,
}

impl MemoryQuota {
    pub fn try_charge(&self, bytes: usize) -> bool {
        // atomic CAS loop to check limit
    }
    pub fn release(&self, bytes: usize) { ... }
}
```

Then wrap allocations in quota-aware helpers in SSH/network code.

**Pros:**
- One greedy session can't starve the rest of the system
- Provides back-pressure before global OOM is reached
- Can expose limits via procfs for observability

**Cons:**
- Significant plumbing — need to associate every allocation with a session
- Hard to account for allocations in shared subsystems (smoltcp buffers,
  VFS caches)
- Doesn't protect against kernel-internal OOM (thread stacks, page tables)

**Complexity:** High

## Option 5: Increase heap / RAM

Raise QEMU RAM from 128MB to 256MB+ in `scripts/run.sh` (`-m 256M`) and
update `DEFAULT_RAM_SIZE` in `src/main.rs`. The kernel heap scales with
available RAM.

**Pros:**
- Zero code changes
- Immediately raises the ceiling

**Cons:**
- Doesn't fix the fundamental issue — just delays it
- Downloads and other unbounded operations can still exhaust any amount

**Complexity:** Trivial

## Recommended approach

Start with **Option 1 + Option 3** (custom alloc error handler with emergency
reserve). This is a small change that immediately stops user-process OOM from
being a system-wide kill, and the reserve ensures the teardown path has memory
to work with.

Then incrementally apply **Option 2** to the SSH and network data paths —
these are the most likely to handle large, variable-size data that triggers OOM.

Option 5 (more RAM) is a good stopgap while implementing the above.
