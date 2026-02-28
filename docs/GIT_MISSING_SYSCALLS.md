# Git (Alpine apk) -- Missing Syscalls and Fixes

Alpine's `git` (via `apk add git`) required two kernel fixes before it
could run. This document records the root causes and fixes applied.

## Issue 1: No `/dev/null` Device

### Symptoms

```
[syscall] write(fd=2, count=85) "fatal: could not open '/dev/null"
[exception] Process 18 (/usr/bin/git) exited, calling return_to_kernel(128)
```

Git opens `/dev/null` very early in startup (for redirecting unwanted output
and as a safe fallback fd). When `openat("/dev/null", ...)` returned `ENOENT`,
git printed the fatal error and exited with code 128.

### Root Cause

Akuma had no `/dev` filesystem or virtual device support. The VFS only
mounted ext2 at `/` and procfs at `/proc`. Any access to `/dev/null`,
`/dev/zero`, `/dev/urandom`, etc. failed with `ENOENT`.

### Fix: Virtual `/dev/null` Device

Rather than implementing a full devfs, `/dev/null` is handled as a special
`FileDescriptor` variant (`DevNull`) in the process fd table. This avoids
filesystem overhead and matches how the kernel already handles pipes and
eventfds.

**Files:** `src/process.rs`, `src/syscall.rs`

### Changes

**1. New `DevNull` variant in `FileDescriptor` enum**

Added `DevNull` to the `FileDescriptor` enum in `src/process.rs`. It carries
no data -- there is no underlying file, buffer, or state to track.

**2. `sys_openat` intercepts `/dev/null`**

After path resolution and symlink following, if the final path equals
`/dev/null`, a `DevNull` fd is allocated instead of going through the VFS.
`O_CLOEXEC` is respected.

**3. `sys_read` returns EOF**

Reading from a `DevNull` fd returns 0 (EOF), matching Linux behavior.

**4. `sys_write` discards data**

Writing to a `DevNull` fd returns `count` (success), discarding all data.

**5. `sys_fstat` and `sys_newfstatat` return char device metadata**

Both stat syscalls return a `Stat` struct with:
- `st_mode = 0o20666` (S_IFCHR | 0666 -- character device, world-readable/writable)
- `st_rdev = makedev(1, 3)` (major 1, minor 3 -- Linux's `/dev/null` device numbers)
- `st_size = 0`

A `makedev(major, minor)` helper was added for constructing device numbers.

The `sys_fstat` function was also restructured from a single `if let` into a
`match` to handle `DevNull`, stdin/stdout/stderr, and pipe fds with
appropriate stat metadata.

**6. `sys_lseek` returns 0**

Seeking on `/dev/null` returns 0 (success), matching Linux behavior.

**7. No changes needed for `close`, `dup`, `dup3`, `fcntl`, `ioctl`**

- `close`: The `_ => {}` wildcard in the close cleanup already handles `DevNull` as a no-op.
- `dup`/`dup3`: Clone the `DevNull` variant via the existing `Clone` derive.
- `fcntl`: Operates on generic per-fd flags (cloexec, nonblock), no fd-type dispatch.
- `ioctl`: Returns `ENOTTY` for fd > 2, correct for `/dev/null`.

## Issue 2: `mkdirat` Returns Wrong Errno

### Symptoms

```
git clone https://github.com/netoneko/meow
Cloning into 'meow'...
/meow/.git/: Operation not permitted
[exit code: 1]
```

Kernel log showed two `mkdirat` calls for the `.git` directory:

```
[syscall] mkdirat: /meow/.git
[syscall] mkdirat: /meow/.git/
[syscall] write(fd=2, count=11) "/meow/.git/"
[syscall] write(fd=2, count=23) "Operation not permitted"
```

### Root Cause

`sys_mkdirat` had two bugs:

1. **Wrong errno for all errors.** It returned `!0u64` (`-1` = `-EPERM`) for
   every failure, regardless of the actual filesystem error. When git called
   `mkdir("/meow/.git/")` (trailing slash) after `/meow/.git` already existed,
   ext2 returned `AlreadyExists` but the syscall returned `EPERM` instead of
   `EEXIST`. Git handles `EEXIST` gracefully (the directory is already there)
   but treats `EPERM` as a fatal permissions error.

2. **Ignored `dirfd` parameter.** The `dirfd` argument was prefixed with `_`
   and never used. Relative paths were always resolved against the process CWD
   instead of the directory referenced by `dirfd`. This could cause wrong
   directory creation when programs use `mkdirat(fd, "subdir", mode)`.

### Fix

**File:** `src/syscall.rs`

Rewrote `sys_mkdirat` to match the pattern used by `sys_unlinkat`:

- **Proper path resolution:** If the path is absolute, canonicalize it
  directly. If relative, resolve it against `dirfd` (or CWD when
  `dirfd == AT_FDCWD`). Handles `AT_FDCWD` (-100) and real directory fds.

- **Proper errno return:** Uses `fs_error_to_errno()` to map filesystem
  errors to Linux errno values (`AlreadyExists` → `EEXIST`,
  `NotFound` → `ENOENT`, `NoSpace` → `ENOSPC`, etc.) instead of
  returning `-EPERM` for everything.

## Issue 3: `chmod` / `stat` Permissions Not Working

### Symptoms

```
ls -al /usr/libexec/git-core/git-remote-http
-rw-r--r--    1 0        0            800784 Jan 01  1970 /usr/libexec/git-core/git-remote-http
```

All files showed `644` (`-rw-r--r--`) permissions regardless of actual ext2
inode permissions. `chmod +x` appeared to succeed (returned 0) but had no
effect — the permission bits were never read from or written to the ext2 inode.

### Root Cause

Three bugs combined:

1. **`st_mode` hardcoded in stat syscalls.** Both `sys_fstat` and
   `sys_newfstatat` returned `0o100644` for all files and `0o40755` for all
   directories, ignoring the actual `type_perms` field stored in the ext2
   inode.

2. **`fchmod`/`fchmodat` were no-op stubs.** Both syscalls returned 0 without
   modifying anything. Programs like `chmod` and `install` thought they
   succeeded, but the inode was never updated.

3. **VFS `Metadata` lacked a `mode` field.** The struct only had `is_dir`,
   `size`, `inode`, and timestamps — no way to propagate the actual
   permissions from the filesystem to the syscall layer.

### Fix

**Files:** `src/vfs/mod.rs`, `src/vfs/ext2.rs`, `src/vfs/memory.rs`,
`src/vfs/proc.rs`, `src/syscall.rs`

1. **Added `mode: u32` to `Metadata`** — carries the full file type +
   permission bits (e.g., `0o100755` for an executable file).

2. **ext2 returns actual inode permissions** — `metadata()` now reads
   `inode.type_perms` and stores it in `meta.mode`. memfs and procfs return
   appropriate defaults (`0o40555` for procfs dirs, etc.).

3. **stat syscalls use `meta.mode`** — `sys_fstat` and `sys_newfstatat` use
   `meta.mode` directly instead of hardcoded values. Also added timestamps
   (`st_atime`, `st_mtime`, `st_ctime`) from metadata.

4. **Added `chmod()` to `Filesystem` trait** — default returns `NotSupported`,
   ext2 implementation updates `inode.type_perms` preserving the file type
   nibble and writing the updated inode to disk.

5. **Implemented `sys_fchmod` and `sys_fchmodat`** — both resolve the path
   and call `vfs::chmod()`. `fchmodat` handles `AT_FDCWD` and real directory
   fds for proper relative path resolution.

## Issue 4: `O_CREAT` Did Not Create File on Disk

### Symptoms

```
error: chmod on /meow/.git/config.lock failed: No such file or directory
fatal: could not set 'core.filemode' to 'true'
```

### Root Cause

`sys_openat` with `O_CREAT` (without `O_TRUNC`) only allocated a file
descriptor but never created the file on the ext2 filesystem. The file was
created lazily on first `write()` (ext2's `write_at` auto-creates missing
files). Git opens lockfiles with `O_CREAT | O_WRONLY | O_EXCL` (no `O_TRUNC`)
and calls `fchmod(fd, mode)` before writing any data. Since the file didn't
exist on disk yet, `fchmod` → `vfs::chmod` → ext2 `lookup_path` returned
`NotFound`.

### Fix

**File:** `src/syscall.rs`

Changed `sys_openat` to create the file on disk immediately when `O_CREAT` is
set and the file doesn't exist, rather than waiting for the first write. The
`O_TRUNC` path was also adjusted to only truncate when the file already
exists (previously it could create-then-truncate redundantly).

## Issue 5: `clone3` Syscall Not Implemented

### Symptoms

```
fatal: unable to find remote helper for 'https'
```

Kernel log showed pipes being created and immediately destroyed with no
`[syscall] clone(...)` or `[Process] Spawning ...` output, even though
`SYSCALL_DEBUG_INFO_ENABLED` was on. Git was unable to fork a child process
to run `git-remote-https`.

### Root Cause

Musl libc on Alpine (>= 1.2.4) uses `clone3` (syscall 435) in
`posix_spawn` before falling back to `clone` (syscall 220). The kernel had
no handler for syscall 435, so it returned `ENOSYS`. Depending on musl
version, the fallback to `clone` may not work correctly for all
`posix_spawn` use cases, causing the spawn to fail silently.

### Fix

**File:** `src/syscall.rs`

Added `clone3` support:

- New syscall constant `CLONE3 = 435` and dispatch entry.
- `sys_clone3` reads the `clone_args` struct from userspace, extracts
  `flags`, `exit_signal`, `stack`, and `stack_size`, then delegates to
  the existing `sys_clone` implementation.
- The `clone_args` struct follows the Linux ABI: flags and exit_signal are
  combined (clone3 separates them, clone combines them in the low bits),
  and stack is passed as base + size (clone3 uses base/size, clone uses
  top-of-stack).

## Issue 6: `O_CREAT` Ignoring `mode` Parameter (File Permissions Lost)

### Symptoms

```
fatal: unable to find remote helper for 'https'
```

Syscall tracing revealed that git stat'd `/usr/bin/git` and got
`mode=0o100644` (no execute bit). Git's `is_executable()` check requires
`S_IXUSR` and returned false. Without finding itself in PATH, git could
not derive `GIT_EXEC_PATH` and could not locate `git-remote-https` in
`/usr/libexec/git-core/`. Git gave up without even attempting fork/exec.

### Root Cause

`sys_openat` with `O_CREAT` created new files via `write_file(&path, &[])`
which always gives ext2's default permissions (`0644`). The `mode`
parameter from the syscall (e.g., `0755` for executables) was completely
ignored (named `_mode`). When `apk` extracted packages, executables were
created as `0644` instead of `0755`.

### Fix

**File:** `src/syscall.rs`

Changed `sys_openat` to apply the `mode` parameter after creating a new
file with `O_CREAT`:

```rust
if !file_existed && (flags & O_CREAT != 0) {
    let _ = crate::fs::write_file(&path, &[]);
    if mode & 0o7777 != 0 {
        let _ = crate::vfs::chmod(&path, mode & 0o7777);
    }
}
```

This ensures newly created files get the permissions specified by the
caller (e.g., `0755` for executables), matching Linux behavior.

**Note:** Requires re-populating the disk image so that packages are
extracted with the fix in place.

## Future Work

Other device files that programs commonly expect:

| Device | Behavior | Priority |
|--------|----------|----------|
| `/dev/zero` | Reads return zero bytes, writes discarded | Medium |
| `/dev/urandom` | Reads return random bytes | Medium |
| `/dev/random` | Same as urandom on modern Linux | Low |
| `/dev/tty` | Alias for controlling terminal | Low |
| `/dev/fd/N` | Alias for fd N (`/proc/self/fd/N`) | Low |

These can follow the same pattern: intercept in `sys_openat`, add a
`FileDescriptor` variant, handle in read/write/stat.
