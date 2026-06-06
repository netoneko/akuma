# Memory Leak Fix (Feb 2026)

## Overview
The "OUT OF MEMORY" error during `scratch clone` was caused by a combination of allocator leaks in `libakuma` and inefficient memory management in the `PackParser`.

## Root Causes
1. **Allocator Leak**: The userspace allocator was leaking old memory blocks during `realloc` because its internal deferred free queue was too small (16 slots) and only flushed during `dealloc`. (Fixed in `libakuma`).
2. **Redundant Cloning**: `PackParser::parse_all` was cloning object data multiple times:
   - When moving from `PackEntry` to the `resolved` map.
   - When writing to the object store.
   - When resolving deltas.

## Optimizations
1. **Move Semantics**: Modified `PackEntry` to use `Option<Vec<u8>>`. This allows the parser to `.take()` the data and move it into the next stage instead of cloning it.
2. **In-Place Resolution**: Objects are now moved into the `resolved` map immediately after being written to the store, avoiding a clone.
3. **Early Data Clearing**: Raw delta instructions are explicitly cleared (`entry.data = None`) immediately after the delta is applied to a base object, freeing memory as soon as possible.

## Results
- Significantly reduced peak heap usage during `git clone` operations.
- `scratch` can now handle larger repositories without hitting the 64MB userspace limit.
