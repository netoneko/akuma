# Fix: stat() Returning Zero Inodes & unlinkat() Ignoring dirfd/flags

## Symptom

Running `rm -rf /usr/bin` produced:

```
/usr/bin/rm: "/" may not be removed
```

Even though the target was `/usr/bin`, not `/`. After fixing the stat issue, files and directories inside `/usr/bin` still weren't being deleted — `unlinkat` returned success but nothing was actually removed.

## Root Cause 1: stat() Returns st_ino=0 for Every File

Both `sys_fstat` and `sys_newfstatat` filled the `Stat` struct with `..Default::default()`, which set `st_dev=0` and `st_ino=0` for every file regardless of path.

The sbase `rm` command uses a `forbidden()` check (rm.c:15-44) that calls `stat("/")` and `stat(target_path)`, then compares the `(st_dev, st_ino)` pairs. Since every file returned `(0, 0)`, `rm` concluded every path was the root filesystem and refused to delete it.

### Fix

Added an `inode: u64` field to `vfs::Metadata` and populated it in each filesystem:

- **ext2**: Uses the real inode number from `lookup_path()` (e.g., root = inode 2).
- **memfs**: FNV-1a hash of the path.
- **procfs**: FNV-1a hash of the path.

Updated both stat syscalls to set `st_dev=1`, `st_ino=meta.inode`, `st_nlink` (2 for dirs, 1 for files), and `st_blksize=4096`.

## Root Cause 2: unlinkat() Ignores dirfd and flags

`sys_unlinkat` had two bugs:

1. **dirfd ignored**: When `rm` recurses into a directory, it opens the directory (getting fd N), reads entries with `getdents64`, then calls `unlinkat(N, "filename", 0)` for each entry. The kernel was passing the bare relative filename (e.g., `"nohup"`) directly to `remove_file()` without resolving it against the directory fd, so the VFS couldn't find the file.

2. **flags ignored**: After emptying a directory, `rm` calls `unlinkat(AT_FDCWD, "/usr/bin", AT_REMOVEDIR)` to remove the directory itself. The kernel ignored `AT_REMOVEDIR` (0x200) and always called `remove_file()`, which doesn't work on directories.

### Fix

Rewrote `sys_unlinkat` to:

- Resolve relative paths using the dirfd's path (or CWD when `AT_FDCWD`), matching the same resolution logic used by `sys_newfstatat`.
- Check `flags & AT_REMOVEDIR` and call `remove_dir()` for directories, `remove_file()` for regular files.

## Files Changed

| File | Change |
|------|--------|
| `src/vfs/mod.rs` | Added `inode: u64` to `Metadata` struct |
| `src/vfs/ext2.rs` | Set `inode: inode_num` in `metadata()` |
| `src/vfs/memory.rs` | Added `path_inode()` FNV-1a helper, set `inode` in `metadata()` |
| `src/vfs/proc.rs` | Inline FNV-1a hash for `inode` in `metadata()` |
| `src/syscall.rs` | `sys_fstat` / `sys_newfstatat`: populate `st_dev`, `st_ino`, `st_nlink`, `st_blksize` |
| `src/syscall.rs` | `sys_unlinkat`: resolve dirfd-relative paths, handle `AT_REMOVEDIR` flag |

## Known Remaining Issue

`sys_mkdirat` also ignores its `dirfd` argument. It works in practice because `mkdir` typically receives absolute paths, but it will break if a program uses dirfd-relative paths with `mkdirat`.
