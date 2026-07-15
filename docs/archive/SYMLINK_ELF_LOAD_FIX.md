# Symlink ELF Load Fix

## Problem

External commands executed from the built-in SSH shell failed with:

```
Error: Failed to load ELF: Invalid ELF format: File read failed
```

This affected any command backed by a symlink (e.g. busybox applets like `ls`,
`cat`, `grep`), while the same commands worked correctly when launched from
`dash` via `execve`.

## Root Cause

The built-in SSH shell's process spawn path
(`spawn_process_with_channel_ext` in `src/process.rs`) did not resolve
symlinks before loading the ELF binary. The `sys_execve` syscall
(`src/syscall.rs`) calls `crate::vfs::resolve_symlinks()` early on, but the
kernel-side spawn path used by the built-in shell skipped this step.

### Why it breaks

On ext2, `lookup_path` returns the inode of the path's final component
without following symlinks. For a fast symlink (target ≤ 60 bytes), the
target string is stored directly in the inode's block-pointer fields and
`sectors_used` is 0.

When the ELF loader tried to `read_at` on the symlink inode:

1. `inode.size_lower` contained the target path length (e.g. 13 for
   `/bin/busybox`), not the binary size.
2. `get_block_num` interpreted the target path bytes in `direct_blocks` as
   physical block numbers, producing garbage.
3. `read_block` with the garbage block number failed → `"File read failed"`.

### Why dash worked

`dash` uses the `execve` syscall, which calls `resolve_symlinks()` at the
top of `sys_execve`. So `/bin/ls` → `/bin/busybox` was resolved before any
ELF loading took place.

## Fix

Added `resolve_symlinks()` at the entry of `spawn_process_with_channel_ext`
(`src/process.rs`). The resolved path is used for all file I/O (reading the
ELF, stat, on-demand loading), while the original symlink path is preserved
in `argv[0]` so busybox-style multi-call binaries can identify which applet
to run.

```rust
let resolved = crate::vfs::resolve_symlinks(path);
let elf_path = &resolved;

// argv[0] keeps the original path
full_args.push(path.to_string());
```

All other spawn functions (`spawn_process`, `spawn_process_with_channel`,
`spawn_process_with_channel_cwd`) delegate to `spawn_process_with_channel_ext`,
so the fix covers every kernel-side spawn call site.

---

# Busybox --install "Operation not permitted"

## Problem

`busybox --install` reported "Operation not permitted" for every symlink it
tried to create:

```
busybox: /usr/bin/cpio: Operation not permitted
busybox: /bin/date: Operation not permitted
```

## Root Cause

`sys_symlinkat` in `src/syscall.rs` returned `!0u64` (= `-1` = `-EPERM`) for
**all** errors from `create_symlink`, regardless of the actual failure reason:

```rust
Err(_) => !0u64,
```

The real errors were most likely `FsError::NotFound` (parent directory like
`/usr/bin` or `/usr/sbin` does not exist) or `FsError::AlreadyExists` (symlink
was already created on a previous boot), but userspace always saw EPERM
because the errno was hardcoded.

## Fix

Replaced the blanket `!0u64` with the existing `fs_error_to_errno` helper,
which maps each `FsError` variant to the correct Linux errno:

```rust
Err(e) => fs_error_to_errno(e),
```

Now busybox will see the real error (`ENOENT` for missing directories,
`EEXIST` for duplicate symlinks, etc.) and can react accordingly.

---

# Ext2 Corruption on Repeated busybox --install

## Problem

Running `busybox --install` a second time corrupts the ext2 filesystem,
destroying files like `/etc/sshd/authorized_keys` and making SSH auth
impossible. The disk becomes unusable and must be recreated.

`busybox --install` creates hundreds of symlinks across `/bin`, `/sbin`,
`/usr/bin`, and `/usr/sbin`. On the second invocation most of these paths
already exist, and the resulting error-handling paths likely trigger
corruption in the ext2 metadata.

## Possible Causes

1. **Inode allocation leak on partial failure.** `create_symlink_internal`
   allocates an inode before writing the directory entry. If the directory
   write fails (e.g. out of space in the directory block), the inode is
   consumed but never referenced, corrupting the free-inode count and
   potentially the block group descriptor.

2. **Unlink path corrupts metadata.** When busybox receives `EEXIST` it may
   attempt to `unlink` the existing symlink before recreating it. The ext2
   `delete` implementation must update the directory entry chain, decrement
   `hard_links`, free the inode, and for slow symlinks free the data block.
   A bug in any of those steps (e.g. writing a stale inode back, double-free
   of a block, or incorrect bitmap update) would corrupt the filesystem.

3. **Block group descriptor drift.** The superblock and block group
   descriptors track free inode/block counts. If these aren't updated
   atomically with the bitmap changes, bulk operations (hundreds of symlink
   creates/deletes) can cause the counts to drift, leading to double
   allocation of inodes or blocks.

## Status

Not yet investigated in detail. Workaround: only run `busybox --install`
once on a fresh disk. If the disk is corrupted, recreate it with
`scripts/create_disk.sh` and `scripts/populate_disk.sh`.

---

## Files Changed

- `src/process.rs` — `spawn_process_with_channel_ext`: resolve symlinks
  before ELF load
- `src/syscall.rs` — `sys_symlinkat`: return proper errno instead of
  hardcoded EPERM
