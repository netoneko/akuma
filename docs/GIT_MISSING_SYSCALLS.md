# Git (Alpine apk) -- Missing Syscalls and Fixes

Alpine's `git` (via `apk add git`) failed to start with a fatal error
about `/dev/null`. This document records the root cause and the fix applied.

## Symptoms

```
[syscall] write(fd=2, count=85) "fatal: could not open '/dev/null"
[exception] Process 18 (/usr/bin/git) exited, calling return_to_kernel(128)
```

Git opens `/dev/null` very early in startup (for redirecting unwanted output
and as a safe fallback fd). When `openat("/dev/null", ...)` returned `ENOENT`,
git printed the fatal error and exited with code 128.

## Root Cause

Akuma had no `/dev` filesystem or virtual device support. The VFS only
mounted ext2 at `/` and procfs at `/proc`. Any access to `/dev/null`,
`/dev/zero`, `/dev/urandom`, etc. failed with `ENOENT`.

## Fix: Virtual `/dev/null` Device

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
