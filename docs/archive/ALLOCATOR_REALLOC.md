# Allocator Realloc Issue and Deferred Free Queue

## Problem

The userspace mmap-based allocator in `libakuma` experienced hangs when calling `munmap` directly from within the `realloc` implementation.

### Symptoms

When `realloc` tried to free the old buffer after allocating a new one and copying data:

```rust
// This caused system hangs:
ptr::copy_nonoverlapping(ptr, new_ptr, copy_size);
munmap(ptr, old_size);  // HANG!
```

### Observations

1. **`dealloc` works fine** - Calling `munmap` from the `dealloc` trait method (invoked by Rust's Drop) works correctly
2. **`realloc` hangs** - The same `munmap` syscall hangs when called from within `realloc`
3. **Context matters** - The difference is the call context, not the syscall itself

### Suspected Causes

1. **Scheduler interaction** - `realloc` may be called during sensitive operations where the scheduler state is in flux
2. **Nested allocation context** - `realloc` is often called from within other allocations (e.g., Vec::push triggering grow)
3. **Page table/TLB state** - Immediately unmapping after mapping might hit a race in the kernel's memory management
4. **Exception handler path** - Some reallocs happen during exception handling where syscalls may behave differently

## Solution: Deferred Free Queue

Instead of freeing immediately during `realloc`, we queue the old buffer and free it during the next `dealloc` call.

### Implementation

```rust
// In allocator module:
struct DeferredFree {
    ptr: usize,
    size: usize,
}

struct DeferredFreeQueue {
    entries: UnsafeCell<[DeferredFree; DEFERRED_FREE_SLOTS]>,
    count: AtomicUsize,
}

static DEFERRED_FREES: DeferredFreeQueue = DeferredFreeQueue::new();
```

### Flow

1. **During `realloc`:**
   - Allocate new buffer with `mmap`
   - Copy data from old to new
   - Queue old buffer in `DEFERRED_FREES` (instead of calling `munmap`)
   - Return new buffer

2. **During `dealloc`:**
   - Flush the deferred free queue (call `munmap` on all queued buffers)
   - Then free the current buffer normally

### Why This Works

- `dealloc` is called from Rust's Drop implementations
- Drop runs in a "clean" context after the main operation is complete
- The kernel's `munmap` works correctly in this context

### Limitations

- **Queue size**: Limited to 16 entries (`DEFERRED_FREE_SLOTS`)
- **Overflow behavior**: If queue overflows, old entries leak (better than hanging)
- **Memory tracking**: `FREED_BYTES` is updated when items are queued, not when actually freed

## Memory Usage Implications

### Before Fix

Every `String::push_str` or `Vec::extend` that triggered a realloc would leak the old buffer. Memory grew unbounded during normal operation.

### After Fix

Old buffers are freed during subsequent `dealloc` calls. Memory usage should stabilize after initial growth.

### Monitoring

The prompt in `meow` shows current memory usage:
```
[2k/32k|204K] (=^･ω･^=) >
         ^^^^^ memory usage
```

If memory exceeds 2MB, a warning is shown:
```
[!] Memory high - consider /clear to reset
```

## Future Investigation

To properly fix this, we need to understand why `munmap` hangs when called from `realloc`. Potential debugging approaches:

1. Add kernel-side logging to the `munmap` syscall handler
2. Check if the hang is in the syscall itself or in returning from it
3. Test if adding a memory barrier before `munmap` helps
4. Check scheduler state during the hang

## Related Files

- `userspace/libakuma/src/lib.rs` - Allocator implementation
- `docs/HEAP_CORRUPTION_ANALYSIS.md` - Related heap issues
- `docs/STDCHECK_DEBUG.md` - ABI issues that affected realloc
