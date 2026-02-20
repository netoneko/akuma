# Syscall Hardening and Linux ABI Alignment

This document describes the improvements made to the Akuma syscall interface to enhance security and compatibility with standard Linux binaries (like GNU Make).

## 1. Pointer Validation

The kernel now rigorously validates all pointers passed from userspace. Previously, the kernel would blindly dereference or write to any address provided in a syscall, leading to crashes (Data Aborts) or potential privilege escalation if kernel memory was overwritten.

### `validate_user_ptr`
All buffer pointers are checked to ensure:
- They are above the process info page (`>= 0x1000`).
- The entire range (ptr to ptr+len) is below the userspace limit (`< 0x4000_0000`).

### `copy_from_user_str`
A new helper function `copy_from_user_str` safely copies null-terminated strings from userspace. It performs bounds checking and ensures the string is valid UTF-8.

## 2. Linux ABI Alignment

To improve compatibility with standard AArch64 tools, several filesystem syscalls were updated to match the Linux syscall ABI. A major change was the removal of the explicit `path_len` argument, which is not part of the standard Linux ABI for these calls.

| Syscall | Number | Linux-Compatible Arguments |
|---------|--------|---------------------------|
| `OPENAT` | 56 | `dirfd`, `path_ptr`, `flags`, `mode` |
| `NEWFSTATAT` | 79 | `dirfd`, `path_ptr`, `stat_ptr`, `flags` |
| `MKDIRAT` | 34 | `dirfd`, `path_ptr`, `mode` |
| `UNLINKAT` | 35 | `dirfd`, `path_ptr`, `flags` |
| `RENAMEAT` | 38 | `olddirfd`, `oldpath_ptr`, `newdirfd`, `newpath_ptr` |
| `CHDIR` | 49 | `path_ptr` |
| `FACCESSAT` | 48 | `dirfd`, `path_ptr`, `mode` |
| `FACCESSAT2` | 439 | `dirfd`, `path_ptr`, `mode`, `flags` |
| `CLOCK_GETTIME` | 113 | `clock_id`, `timespec_ptr` |
| `PIPE2` | 59 | `fds_ptr`, `flags` |
| `CLONE` | 220 | `flags`, `stack`, `parent_tid`, `tls`, `child_tid` |
| `EXECVE` | 221 | `path_ptr`, `argv_ptr`, `envp_ptr` |
| `WAIT4` | 260 | `pid`, `status_ptr`, `options`, `rusage` |

### Key Fixes:
- **`PIPE2`**: Added a stub implementation using temporary files (`/tmp/pipe_r`, `/tmp/pipe_w`). This is a temporary measure to allow GNU Make to function while a full kernel-side pipe implementation is developed.
- **Process Spawning (`vfork/execve`)**: Added bridging support for the Linux `vfork` pattern. `CLONE` handles the initial fork request, and `EXECVE` translates the Linux-style execution request into Akuma's internal thread-based `spawn` architecture.
- **`CHDIR`**: Corrected from a custom number (306) to the Linux standard (49).
- **`ENOSYS`**: Unknown syscalls now correctly return `-38` (`ENOSYS`) instead of `-1` (`EPERM`). This prevents applications from misinterpreting missing functionality as a permission error.
- **Argument Order**: Verified all arguments (like `flags` and `mode`) match the positions expected by Linux-compiled binaries.

## 3. Error Handling

Syscalls now correctly return `EFAULT` (-14) when an invalid pointer is provided. This prevents kernel panics and allows userspace applications to handle the error gracefully.

## 4. Verification

A reproduction tool `crash_test` was created in userspace to deliberately pass `NULL` and invalid pointers to the kernel.
- **Before**: Kernel would crash with `Sync from EL1` and `FAR=0x0`.
- **After**: Kernel remains stable and returns `-14` to the calling process.
