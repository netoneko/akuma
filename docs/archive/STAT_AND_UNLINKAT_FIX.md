# Fix: stat() Inode Bug, unlinkat() dirfd Bug, dup3 + Kernel Pipes

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

## Root Cause 3: Pipes Broken in Dash (dup3 missing + pipe2 stub)

Running `echo hello | sha256sum` in dash printed `hello` to the terminal and sha256sum hung forever.

Dash sets up pipes by:
1. Calling `pipe2()` to create read/write fd pair
2. Forking children, using `dup3()` to redirect pipe fds to stdin/stdout
3. Children `execve` into the actual commands

Two problems:

1. **`dup3` (syscall 24) was unimplemented**: The kernel logged `Unknown syscall: 24` and the fd redirections silently failed. `echo` wrote to the terminal instead of the pipe, and `sha256sum` read from the terminal instead of the pipe.

2. **`pipe2` was a file-based stub**: It created two separate temp files (`/tmp/pipe_r`, `/tmp/pipe_w`). Even if dup3 had worked, data written to the write file wouldn't appear when reading the read file — they were independent files with no shared buffer.

### Fix

**Implemented `dup3` syscall**: Clones the file descriptor entry from `oldfd` to `newfd`, replacing any existing entry at `newfd`. Added `set_fd(fd, entry)` method to Process for inserting at a specific fd number.

**Implemented real kernel pipes**: Replaced the file-based stub with proper in-kernel pipe infrastructure:

- `KernelPipe` struct: shared `Vec<u8>` buffer, `write_closed` flag, optional reader thread ID for wake-on-data
- Global `PIPES` table (BTreeMap indexed by pipe_id)
- New `FileDescriptor::PipeRead(pipe_id)` and `FileDescriptor::PipeWrite(pipe_id)` variants
- `sys_read` for PipeRead: blocking loop that returns data when available, EOF (0) when write end closed
- `sys_write` for PipeWrite: appends to buffer and wakes blocked reader
- `sys_close` for PipeWrite: marks write end closed, wakes reader (delivers EOF)
- Process exit cleanup handles pipe fds (closes write end → unblocks reader)

## Files Changed

| File | Change |
|------|--------|
| `src/vfs/mod.rs` | Added `inode: u64` to `Metadata` struct |
| `src/vfs/ext2.rs` | Set `inode: inode_num` in `metadata()` |
| `src/vfs/memory.rs` | Added `path_inode()` FNV-1a helper, set `inode` in `metadata()` |
| `src/vfs/proc.rs` | Inline FNV-1a hash for `inode` in `metadata()` |
| `src/syscall.rs` | `sys_fstat` / `sys_newfstatat`: populate `st_dev`, `st_ino`, `st_nlink`, `st_blksize` |
| `src/syscall.rs` | `sys_unlinkat`: resolve dirfd-relative paths, handle `AT_REMOVEDIR` flag |
| `src/syscall.rs` | New `sys_dup3`: duplicate fd to specific fd number |
| `src/syscall.rs` | New kernel pipe infrastructure (`KernelPipe`, `PIPES` table, pipe_create/read/write/close) |
| `src/syscall.rs` | Rewrote `sys_pipe2` to use kernel pipes instead of file stubs |
| `src/syscall.rs` | `sys_read`/`sys_write`/`sys_close`: handle `PipeRead`/`PipeWrite` fd types |
| `src/process.rs` | Added `PipeRead(u32)` and `PipeWrite(u32)` to `FileDescriptor` enum |
| `src/process.rs` | Added `set_fd()` method for inserting at specific fd number |
| `src/process.rs` | `cleanup_process_fds`: close pipe ends on process exit |

## Known Remaining Issue

`sys_mkdirat` also ignores its `dirfd` argument. It works in practice because `mkdir` typically receives absolute paths, but it will break if a program uses dirfd-relative paths with `mkdirat`.
