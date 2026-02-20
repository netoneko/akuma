# libakuma Syscall Documentation

This document provides an overview of the syscall interface provided by `libakuma` and notes on the recent ABI migration.

## Migration to Linux-Compatible ABI

In February 2026, the Akuma kernel and `libakuma` were updated to strictly follow the Linux ARM64 (AArch64) syscall ABI for standard operations. This allows better interoperability with tools compiled for Linux.

### Key Changes for Developers

If you are writing manual syscalls or updating low-level code, note the following changes:

1.  **Removal of `path_len`**: Standard filesystem calls (`open`, `mkdir`, `unlink`, etc.) no longer require an explicit string length. The kernel now expects a null-terminated string and performs its own length validation.
2.  **`CHDIR` mapping**: The `CHDIR` syscall number is now **49** (matching Linux), not 306.
3.  **New Wrappers**: Added `fstatat(dirfd, path, flags)` to support directory-relative operations.

## Pointer Safety

All pointers passed to syscalls must be within the valid userspace range:
- **Valid range**: `0x1000` to `0x3FFF_FFFF`
- **Invalid addresses**: Addresses below `0x1000` (including `NULL`) or above `0x4000_0000` (kernel space) will cause the syscall to return `-14` (`EFAULT`).

## Common Syscalls

| Function | Description |
|----------|-------------|
| `open(path, flags)` | Opens a file at the given path. |
| `read(fd, buf)` | Reads data from a file descriptor into a buffer. |
| `write(fd, buf)` | Writes data from a buffer to a file descriptor. |
| `fstat(fd)` | Retrieves metadata for an open file. |
| `fstatat(dirfd, path, flags)` | Retrieves metadata for a path relative to a directory. |
| `chdir(path)` | Changes the current working directory. |
| `mkdir(path)` | Creates a new directory. |
| `unlink(path)` | Deletes a file. |
| `rename(old, new)` | Renames or moves a file/directory. |

## Error Codes

Most syscalls return a negative integer on failure. Common values include:
- `-2` (`ENOENT`): No such file or directory.
- `-4` (`EINTR`): System call interrupted.
- `-14` (`EFAULT`): Bad address (invalid pointer).
- `-22` (`EINVAL`): Invalid argument.
