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

## Files Changed

- `src/process.rs` — `spawn_process_with_channel_ext`
