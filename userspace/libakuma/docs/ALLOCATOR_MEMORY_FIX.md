# Allocator Memory Fix (Feb 2026)

## Problem
The `HybridAllocator` used a `DeferredFreeQueue` to work around a kernel hang when calling `munmap` during `realloc`. However, this queue had two major flaws:
1. **Limited Capacity**: It only held 16 slots. If more than 16 reallocations occurred without an intervening `dealloc`, subsequent old buffers were leaked.
2. **Delayed Flushing**: The queue was only flushed during `dealloc`. In processes like `scratch clone`, which perform many allocations and reallocations to build up data structures before dropping them, the queue would fill and leak before any deallocation occurred.

## Changes
1. **Increased Capacity**: `DEFERRED_FREE_SLOTS` was increased from 16 to **128**.
2. **Proactive Flushing**: Added a call to `DEFERRED_FREES.flush()` at the start of `mmap_alloc`.

## Impact
- Memory leaked during `realloc` is now reclaimed as soon as the process attempts a new allocation.
- This prevents memory exhaustion in long-running batch operations that perform frequent reallocations (e.g., `Vec` growth, `String` concatenation).
