# Kernel Panic Fix: Large Memory Allocation Failure (Feb 2026)

## Root Cause
The kernel was experiencing a `!!! PANIC !!!` with the message `memory allocation of 25690112 bytes failed`. 

This was caused by the kernel attempting to allocate a contiguous buffer large enough to hold an entire file's contents in several high-level filesystem APIs. Specifically:
1. **`sys_read`**: The implementation was calling `fs::read_file(path)`, which allocated a buffer for the *entire* file, even if the user only requested a few bytes.
2. **`append_file`**: The Ext2 implementation was performing a "read-modify-write" operation by loading the whole file into memory, appending data, and writing it back.
3. **`AsyncFile`**: The kernel-side async file handle was also using `read_file` for every `read()` and `write()` operation.

When log files (like those managed by the `herd` supervisor) reached sizes around 25MB, the kernel allocator (which manages physical memory) could no longer satisfy these large contiguous allocation requests, leading to an immediate panic.

## Fixes

### 1. Bounded Syscall Memory Usage
Modified `src/syscall.rs` to ensure `sys_read` never allocates more than **64KB** at a time. It now uses the `read_at` API to fetch only the requested chunk directly from the filesystem driver into a small temporary kernel buffer before copying it to userspace.

### 2. Efficient Filesystem Operations
- **Ext2 Optimization**: Updated `src/vfs/ext2.rs` to implement `append_file` using `write_at` starting at the end of the file. This eliminates the need to read the existing file content.
- **Safety Limit**: Added a **16MB safety cap** to `read_inode_data` in Ext2. If a file (or corrupt inode) exceeds this size, the kernel will return an error instead of attempting a massive allocation that would cause a panic.

### 3. VFS `read_at` Implementation
- Implemented efficient `read_at` methods for `MemoryFilesystem` (`src/vfs/memory.rs`) and `ProcFilesystem` (`src/vfs/proc.rs`).
- These implementations now copy data directly from the underlying storage (Vec or BTreeMap) without cloning the entire file/buffer first.

### 4. Async I/O Refactoring
- Updated `src/async_fs.rs` to use `read_at` and `write_at` inside the `AsyncFile` struct.
- This ensures that kernel tasks (like the SSH server or internal scripts) do not trigger full-file reads when performing partial I/O.

## Conclusion
These changes move the kernel away from a "load-everything" model to a "stream-on-demand" model for filesystem access. This drastically reduces the kernel heap's peak memory usage and ensures stability regardless of the size of files stored on disk or managed in memory.
