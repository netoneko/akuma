# Pack Parsing Crash Investigation

## Symptom

When cloning an empty or minimal GitHub repository, scratch crashes with a data abort:

```
scratch: pack contains 4 objects
[Fault] Data abort from EL0 at FAR=0x3ffef110, ELR=0x421388, ISS=0x47
[Process] PID 19 thread 8 exited (-11)
```

- **FAR**: Faulting address (~0x3ffef110, near top of userspace memory)
- **ISS=0x47**: Write access fault
- **Exit code -11**: SIGSEGV equivalent

## Root Cause Analysis

### Problem 1: Trial Decompression (Fixed)

The original `PackParser::decompress_next()` in `pack.rs` used trial decompression:

```rust
// Old code - problematic
for len in 2..remaining.len() {
    if let Ok(decompressed) = zlib::decompress(&remaining[..len]) {
        self.pos += len;
        return Ok(decompressed);
    }
}
```

**Issues:**
1. Each failed decompression attempt allocates/deallocates memory
2. The `InflateState` struct in miniz_oxide is ~32KB
3. Repeated allocations may cause stack pressure or fragmentation

### Problem 2: Incorrect Bytes Consumed Estimation (Fixed)

The original `decompress_with_consumed()` in `zlib.rs` **estimated** bytes consumed:

```rust
// Old code - wrong
let estimated = (decompressed.len() * 6 / 10) + 12;
let consumed = core::cmp::min(estimated, data.len());
```

This caused the pack parser to advance by the wrong amount, misaligning subsequent object parsing.

## Fix Applied

### 1. Updated `decompress_with_consumed()` to use streaming API

Now uses `miniz_oxide::inflate::stream::inflate()` which returns `StreamResult` with exact `bytes_consumed`:

```rust
pub fn decompress_with_consumed(data: &[u8]) -> Result<(Vec<u8>, usize)> {
    let mut state = InflateState::new_boxed(DataFormat::Zlib);
    // ... streaming loop ...
    // Returns (decompressed_data, exact_bytes_consumed)
}
```

### 2. Updated `PackParser::decompress_next()` to use the fixed function

```rust
fn decompress_next(&mut self) -> Result<Vec<u8>> {
    let remaining = &self.data[self.pos..];
    let (decompressed, consumed) = zlib::decompress_with_consumed(remaining)?;
    self.pos += consumed;
    Ok(decompressed)
}
```

## Current Status

Debug output has been added to trace the crash location:
- `parse_all()` prints which object is being parsed and at what position
- `decompress_with_consumed()` prints input length, when InflateState is created, and final consumed/output bytes

## Suspected Remaining Issues

1. **Stack overflow**: The `InflateState` is heap-allocated via `new_boxed()`, but `inflate()` may have additional stack usage

2. **Memory corruption**: The FAR address 0x3ffef110 suggests either:
   - Stack overflow (stack grows down from high address)
   - Heap corruption affecting stack guard page
   - Wild pointer write

3. **Bounds checking in delta parsing**: `parse_copy_instruction()` accesses array elements without bounds checking (though Rust should panic, not data abort)

## Files Modified

- `userspace/scratch/src/zlib.rs` - Fixed `decompress_with_consumed()` to use streaming API
- `userspace/scratch/src/pack.rs` - Updated `decompress_next()` to use the fix, added debug output
- `userspace/build.sh` - Added scratch to the build list

## Next Steps

1. Run with debug output to identify exact crash location
2. If crash is in `InflateState` creation, consider reducing stack usage
3. If crash is after decompression, check delta parsing bounds
4. Consider using the simpler non-streaming parser for small pack files
