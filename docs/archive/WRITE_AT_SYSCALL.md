# `write_at` syscall optimization

## Problem

Every `sys_write` to a file did a full read-modify-write cycle:

```
read_file(path)          → read ALL blocks from ext2 via VirtIO
buf[position..] = data   → modify in-memory
write_file(path, buf)    → truncate ALL blocks, re-allocate, re-write ALL
```

For a 250KB pack file downloaded in 4KB TLS chunks (~62 writes), total VirtIO I/O
was approximately:

```
Write  1:  read 0KB   + write 4KB   =    4KB
Write  2:  read 4KB   + write 8KB   =   12KB
Write  3:  read 8KB   + write 12KB  =   20KB
...
Write 62:  read 244KB + write 248KB =  492KB
                                     --------
Total I/O:                            ~7.8MB
```

Each write got slower as the file grew. By ~200KB the per-write latency exceeded
the TLS keepalive/read timeout, causing the connection to drop mid-transfer:

```
scratch: received 194 KB (58 kbps)
scratch: TLS read error, stopping
```

The reported 58 kbps was not the network speed — it was the ext2 write speed
being measured as if it were download throughput.

## Fix

Added `write_at(path, offset, data)` to the VFS layer. `sys_write` now calls it
directly instead of the read-modify-write pattern.

### VFS trait (`src/vfs/mod.rs`)

```rust
fn write_at(&self, path: &str, offset: usize, data: &[u8]) -> Result<usize, FsError>;
```

Default implementation falls back to read-modify-write for filesystems that don't
override it (e.g. procfs).

### ext2 implementation (`src/vfs/ext2.rs`)

Only touches blocks in the write range:

1. Compute first and last logical block from `[offset..offset+len)`
2. For each block:
   - `ensure_block()` — allocate if the file is being extended
   - Full-block write → write directly, no read
   - Partial-block write → read one block, modify, write back
3. Update `inode.size_lower` if file was extended
4. Write inode once

For the same 250KB download:

```
Write  1:  write 4 blocks  (4KB)
Write  2:  write 4 blocks  (4KB)
...
Write 62:  write 4 blocks  (4KB)
                            ------
Total I/O:                  ~250KB
```

~32x less I/O. O(1) per write regardless of file size.

### `sys_write` (`src/syscall.rs`)

```rust
// Before:
let mut data = read_file(&f.path).unwrap_or_default();
if f.position + count > data.len() { data.resize(...); }
data[f.position..].copy_from_slice(buf);
write_file(&f.path, &data)

// After:
write_at(&f.path, f.position, buf)
```

### memfs (`src/vfs/memory.rs`)

In-memory implementation: extends the `Vec<u8>` if needed, copies directly.
No block I/O involved.

## Also fixed: `O_TRUNC` in `sys_openat`

`sys_openat` previously ignored the `O_TRUNC` flag entirely. With the old
read-modify-write `sys_write`, this meant opening a file with `O_TRUNC` and
writing shorter data left trailing garbage from the old content. Now handled by
truncating the file to zero on open when `O_TRUNC` is set.
