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

### Key Fixes:
- **`CHDIR`**: Corrected from a custom number (306) to the Linux standard (49).
- **Argument Order**: Verified all arguments (like `flags` and `mode`) match the positions expected by Linux-compiled binaries.

## 3. Error Handling

Syscalls now correctly return `EFAULT` (-14) when an invalid pointer is provided. This prevents kernel panics and allows userspace applications to handle the error gracefully.

## 4. Verification

A reproduction tool `crash_test` was created in userspace to deliberately pass `NULL` and invalid pointers to the kernel.
- **Before**: Kernel would crash with `Sync from EL1` and `FAR=0x0`.
- **After**: Kernel remains stable and returns `-14` to the calling process.
