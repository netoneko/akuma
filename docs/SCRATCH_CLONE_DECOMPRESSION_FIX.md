# scratch clone: zlib decompression fix

## Symptom

`scratch clone` fails during checkout with "zlib decompression failed":

```
scratch: stored 908 objects
scratch: HEAD set to main
scratch: checking out files
scratch: clone failed: zlib decompression failed
```

Or, with larger repos, the TLS connection drops mid-download but the clone
continues anyway with a truncated pack:

```
scratch: received 194 KB (58 kbps)
scratch: TLS read error, stopping
scratch: download finished, total 255 KB (80 kbps)
scratch: downloaded 245771 bytes
scratch: parsing pack file...
scratch: pack contains 3763 objects
```

## Root Cause

Three interacting bugs:

### 1. `sys_openat` ignores `O_TRUNC`

`sys_openat` in `src/syscall.rs` created a file descriptor but never truncated the
file when `O_TRUNC` was set. Because `sys_write` uses a read-modify-write pattern
(reads the entire existing file, overlays new bytes at the current position, writes
the full buffer back), opening an existing file with `O_TRUNC` and writing shorter
data left trailing garbage from the old content:

```
File before:  [AAAAAAAAAA]  (10 bytes, old compressed data)
Write 6 bytes: [BBBBBBAAAA]  (6 new + 4 old = 10 bytes on disk)
Expected:      [BBBBBB]      (6 bytes, cleanly truncated)
```

`zlib::decompress` then fails on the trailing garbage after the valid zlib stream.

### 2. `store.write_raw()` skips writing if file exists

`ObjectStore::write_raw()` in `userspace/scratch/src/store.rs` had an early return:

```rust
if self.exists(&sha) {
    return Ok(sha);
}
```

If a previous (failed) clone left object files on disk — potentially corrupted by
the ext2 `first_data_block` off-by-one bug (see `docs/EXT2_FIRST_DATA_BLOCK_FIX.md`)
— the current clone would reuse those files without verification. During checkout,
reading a corrupt file would fail decompression.

## Fix

### `src/syscall.rs` — handle `O_TRUNC` on open

```rust
fn sys_openat(...) -> u64 {
    ...
    if flags & O_TRUNC != 0 {
        let _ = crate::fs::write_file(path, &[]);
    }
    ...
}
```

When `O_TRUNC` is set, the file is truncated to zero length before the file
descriptor is created. Subsequent `sys_write` calls start from an empty file,
producing correct output.

### `userspace/scratch/src/store.rs` — always write objects

Removed the `exists()` early return from `write_raw()`. Objects are always freshly
compressed and written with `O_TRUNC`, ensuring the data on disk matches what was
just decompressed from the pack. The slight extra cost of recompression is
negligible for scratch's use case.

### `userspace/scratch/src/stream.rs` — TLS errors must not be swallowed

`process_pack_streaming` had:

```rust
Err(_) => {
    print("\nscratch: TLS read error, stopping\n");
    break;  // returns Ok(()) — caller parses truncated pack!
}
```

A TLS read error broke out of the loop and returned `Ok(())`. The caller then
parsed the truncated pack file, which either failed during decompression or
produced corrupt objects. This was the primary trigger for the error.

Fixed to return `Err(...)` when:

- No body data has been received at all.
- Chunked transfer has not seen the final zero-length chunk.
- Content-Length is known and total received bytes are less than expected.

A TLS error is only tolerated for non-chunked responses without Content-Length
(pure `Connection: close`), where some servers drop TCP without a clean TLS
`close_notify`.

## Related

- `docs/EXT2_FIRST_DATA_BLOCK_FIX.md` — the ext2 block allocation off-by-one that
  can corrupt files on disk images used before the fix. Disk images affected by that
  bug should be recreated with `./scripts/create_disk.sh`.
