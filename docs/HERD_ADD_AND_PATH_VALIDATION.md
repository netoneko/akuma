# Herd Add Command and Path Validation Fixes

## Overview
This document summarizes the implementation of the `herd add` command and critical fixes to path validation in the kernel and userspace supervisor.

## Herd Improvements
The `herd` supervisor was updated to improve service bootstrapping and error visibility:

1.  **New `add <svc>` Command**: 
    - Creates a template configuration file in `/etc/herd/available/<svc>.conf`.
    - Automatically populates the config with default paths and retry settings.
2.  **Explicit Directory Setup**:
    - `ensure_directories()` is now called for all subcommands (not just the daemon), ensuring `/etc/herd/` subdirectories exist before usage.
3.  **Strict Error Handling**:
    - `write_file` now returns a boolean status.
    - `herd enable` and `herd add` now report errors if the configuration file cannot be persisted to the filesystem.

## Kernel-side Path Validation
Previously, `sys_openat` would return a valid file descriptor for any path, only failing later during `read` or `write` operations. This led to "silent" failures where applications thought a file was opened or created successfully.

1.  **Immediate Validation**: `sys_openat` now validates path existence using `vfs::exists` before allocating a file descriptor.
2.  **O_CREAT Handling**: For creation flags, the kernel now verifies that the parent directory exists using `vfs::split_path`.
3.  **Error Codes**: Added `ENOENT` (-2) support to system calls to provide standard Unix-like error feedback for missing paths.
4.  **Debug Logging**: Added kernel-level logging in `sys_mkdirat` to trace directory creation attempts.
