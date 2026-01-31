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

**FIXED**: The root cause was identified as stack overflow. The FAR address 0x3ffef110 is inside the
guard page (0x3ffef000), confirming the stack exceeded its 64KB limit.

### Root Cause: Stack Overflow

The call stack when reaching `parse_all()` is quite deep:

```
_start → main → cmd_clone → clone → fetch_pack_streaming → parse_pack_from_file → parse_all
  → parse_entry → decompress_next → decompress_with_consumed → inflate (miniz_oxide internal)
```

With 64KB userspace stack, this deep call chain combined with miniz_oxide's `inflate()` internal
stack usage exceeded the limit. The `InflateState` is heap-allocated via `new_boxed()` (~32KB),
but the `inflate()` function still has significant internal stack requirements.

### Fix Applied

Increased `USER_STACK_SIZE` in `src/config.rs` from 64KB to 128KB:

```rust
pub const USER_STACK_SIZE: usize = 128 * 1024;
```

This matches `USER_THREAD_STACK_SIZE` (the kernel-side thread stack) which was already 128KB.

## Previous Issues (Already Fixed)

## Files Modified

- `userspace/scratch/src/zlib.rs` - Fixed `decompress_with_consumed()` to use streaming API
- `userspace/scratch/src/pack.rs` - Updated `decompress_next()` to use the fix, added debug output
- `userspace/build.sh` - Added scratch to the build list

## Verification

Rebuild the kernel with the increased stack size:

```bash
cargo build --release
cargo run --release
```

Then test cloning:

```bash
# Inside Akuma
scratch clone https://github.com/user/repo
```

The pack parsing should now complete without a data abort.
