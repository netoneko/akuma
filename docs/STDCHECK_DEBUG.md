# stdcheck Debugging Notes

## Current Status (Jan 3, 2026)

### Problem
The `stdcheck` binary fails its `String::push_str` test even though the data appears correct.

### Symptoms
1. `Layout` struct passed to `GlobalAlloc::realloc` has corrupted alignment:
   - Expected: `align = 1` (for u8/String)
   - Actual: `align = 7` (invalid, not power of 2)
   - Size is correct: `size = 5` (for "Hello")

2. The Layout corruption causes `Layout::from_size_align(new_size, align)` to fail.

3. With workaround (default to align=1 if invalid), realloc succeeds and data is copied correctly, but test still fails.

### Memory Layout (per-process)
- Code: 0x400000
- Heap (brk): ~0x403000
- Mmap: 0x10000000 - 0x3FEF0000
- Stack: 0x3FFF0000 - 0x40000000 (64KB)
- SP at realloc: 0x3FFFFEB0 (valid, within stack)

### Key Observations
1. Stack location and SP are valid
2. Kernel and user have separate stacks (SP_EL1 vs SP_EL0)
3. Per-process mmap tracking is working
4. The corruption affects only the `align` field of Layout, not `size`
5. align=7 is suspicious: 5 + 2 = 7 (could be offset error?)

### AArch64 Calling Convention for realloc
```
fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8
```
- x0 = self (allocator pointer)
- x1 = ptr 
- x2, x3 = layout (size, align) as two u64s
- x4 = new_size

### Hypothesis
The Layout struct might be getting corrupted during:
1. Register passing (unlikely if size is correct)
2. Compiler optimization issue
3. Something in the call chain before realloc

### Next Steps
1. Capture raw register values at realloc entry
2. Check if kernel code is interfering with user registers during syscall
3. Verify the issue is in userspace, not kernel interference

