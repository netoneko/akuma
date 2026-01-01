# Userspace Heap Corruption Bug

## Status: OPEN

## Summary

A memory corruption bug affects userspace programs when performing heap reallocation operations (e.g., `String::push_str`, `Vec` growth). The allocator's internal state gets corrupted, causing Data aborts when accessing invalid memory addresses.

## Symptoms

1. **Vec and String::from work** - Simple allocations succeed
2. **push_str crashes** - Operations requiring reallocation cause Data abort
3. **Corrupted pointer value** - Allocator head jumps from ~0x402000 to ~0x814000
4. **Data abort** - `FAR=0x814003` or similar, with `ISS=0x46` (translation fault)

Example output:
```
[TEST] Vec... PASS
[TEST] String... [Fault] Data abort from EL0 at FAR=0x816003, ISS=0x46
[exit code: -11]
```

## Technical Details

### Memory Layout (failing case)

```
0x400000-0x400A83: .text (code)
0x400A84-0x400B63: .rodata
0x401000-0x40100F: .bss (16 bytes - allocator static)
0x402000-0x411FFF: heap (64KB pre-allocated)
0x3FFF_0000-0x3FFF_F000: stack
```

### Allocator Implementation

The userspace allocator (`libakuma/src/lib.rs`) uses a simple bump allocator:

```rust
pub struct BrkAllocator {
    head: UnsafeCell<usize>,  // Next allocation address
    end: UnsafeCell<usize>,   // End of heap
}
```

- Located in `.bss` at ~0x401000 or 0x402000 (depends on binary size)
- Uses `brk` syscall to extend heap
- No deallocation (memory freed on process exit)

### Corruption Pattern

1. Vec allocates at 0x402000, head becomes ~0x402010
2. String::from allocates, head becomes ~0x402020
3. push_str triggers realloc, allocator is called
4. **head is now 0x814000** - corruption happened!
5. alloc returns 0x814000, which is unmapped
6. memcpy writes to 0x814000 → Data abort

### What We Verified

| Check | Result |
|-------|--------|
| Physical addresses unique | ✓ No PMM collision |
| Pages zeroed correctly | ✓ BSS is zero |
| ELF segments loaded correctly | ✓ Correct VA→PA mappings |
| Heap pages pre-allocated | ✓ 16 pages at 0x402000+ |
| brk syscall working | ✓ Returns correct values |

### Binary Size Sensitivity

The bug is sensitive to ELF binary layout:

| Binary Size | BSS Location | Bug Manifests |
|-------------|--------------|---------------|
| ~72KB | 0x401000 | Yes - crashes on push_str |
| ~77KB | 0x402000 | Sometimes works |

Adding debug prints shifts the layout and can hide/reveal the bug.

## Hypotheses

### 1. Wild Pointer from Vec/String Internals (Most Likely)
- Vec's internal RawVec or String's buffer management may write to wrong address
- Could be triggered by specific alignment or size combinations

### 2. Memory Ordering Issue
- `UnsafeCell` used without memory barriers
- Compiler reordering could cause stale reads

### 3. Stack Corruption
- Unlikely - stack is at 0x3FFF_F000, far from BSS

### 4. MMU Page Table Issue
- Page tables checked - all unique PAs
- No evidence of shared mappings

## Workarounds

1. **Avoid reallocation**: Use `String::with_capacity()` or `Vec::with_capacity()`
2. **Increase binary size**: Adding static data shifts layout (not reliable)

## Proposed Fix: Implement mmap

Replace brk-based allocation with mmap:

1. Each allocation gets its own page(s)
2. Unmapped pages fault immediately (easier debugging)
3. Cleaner memory model
4. Standard Unix API

## Related Files

- `userspace/libakuma/src/lib.rs` - Allocator implementation
- `src/elf_loader.rs` - ELF loading and heap setup
- `src/process.rs` - brk syscall handling
- `src/syscall.rs` - Syscall dispatch

## Kernel EC=0x0 Crashes

During debugging, kernel crashes were observed with `EC=0x0` (Unknown exception). These were caused by:

1. Using `for` loops instead of unrolled code in ELF loader
2. Adding too many debug print statements

**Root cause**: Unknown, but binary size/layout affects kernel stability too.

**Workaround**: Keep ELF loader code simple and unrolled.

## Reproduction

```bash
# Build stdcheck with push_str test
cd userspace
cargo build --release

# Run kernel
cd ..
cargo run --release

# Via SSH
ssh user@localhost -p 2222 "pkg install stdcheck && stdcheck"
```

## Timeline

- **Initial discovery**: String::push_str causes crash
- **Investigation**: ~3 hours of debugging
- **Physical memory**: Verified unique, no collision
- **Page tables**: Verified correct mappings
- **Kernel crashes**: Discovered EC=0x0 sensitivity to code size
- **Current status**: Root cause unknown, mmap recommended

## See Also

- [AI_DEBUGGING.md](AI_DEBUGGING.md) - Debugging flow documentation
- [PACKAGES.md](PACKAGES.md) - Package system documentation

