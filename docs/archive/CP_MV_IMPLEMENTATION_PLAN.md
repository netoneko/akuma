# cp and mv Implementation Plan

This document outlines the plan for adding `cp` and `mv` commands to the built-in shell and providing the corresponding syscalls in `libakuma`.

## Overview

Currently, the built-in shell has a limited `mv` implementation that only works for files (not directories) and uses a suboptimal read-write-delete cycle. There is no `cp` command, and no `rename` syscall is available to userspace applications.

## 1. VFS Layer Enhancements

### `Filesystem` Trait Update (`src/vfs/mod.rs`)
- Add `rename(&self, old_path: &str, new_path: &str) -> Result<(), FsError>` method to the `Filesystem` trait.
- Provide a default implementation that returns `FsError::NotSupported`.

### VFS API Update (`src/vfs/mod.rs`)
- Add a public `rename(old_path: &str, new_path: &str) -> Result<(), FsError>` function.
- This function will:
    1. Resolve both paths.
    2. If both paths are on the same filesystem, call that filesystem's `rename`.
    3. If they are on different filesystems, or if `rename` returns `NotSupported`, it should return an error (or optionally implement a cross-FS copy+delete, but atomic rename is preferred). For now, we will focus on intra-FS rename.

### Memory Filesystem Implementation (`src/vfs/memory.rs`)
- Implement `rename` for `MemoryFilesystem`. This is a simple `BTreeMap` operation within the parent directory (or across directories in the same `MemoryFilesystem`).

### Ext2 Filesystem Implementation (`src/vfs/ext2.rs`)
- Add a stub for `rename` that returns `NotSupported` (to be implemented properly in the future).

## 2. Kernel Filesystem API

### `src/fs.rs`
- Add `pub fn rename(old_path: &str, new_path: &str) -> Result<(), FsError>` as a wrapper around `vfs::rename`.

### `src/async_fs.rs`
- Add `pub async fn rename(old_path: &str, new_path: &str) -> Result<(), FsError>`.

## 3. System Calls

### Syscall Numbers (`src/syscall.rs`)
- Define `RENAMEAT: u64 = 38` (matching Linux arm64).

### Syscall Handler (`src/syscall.rs`)
- Implement `sys_renameat(olddirfd: i32, oldpath_ptr: u64, oldpath_len: usize, newdirfd: i32, newpath_ptr: u64, newpath_len: usize, flags: u32)`.
- For now, ignore `olddirfd`, `newdirfd`, and `flags` (assume `AT_FDCWD`).

## 4. Userspace Library (`libakuma`)

### `userspace/libakuma/src/lib.rs`
- Add `RENAMEAT` to the `syscall` module.
- Add `pub fn rename(old_path: &str, new_path: &str) -> i32` wrapper.

## 5. Shell Commands (`src/shell/commands/fs.rs`)

### `MvCommand` Update
- Refactor to use `async_fs::rename`.
- Support directory renaming (enabled by VFS `rename`).
- Fall back to copy+delete ONLY if `rename` fails with `NotSupported` and it's a file.

### `CpCommand` Implementation
- Add `CpCommand` to `src/shell/commands/fs.rs`.
- Implement file copying using `async_fs::read_file` and `async_fs::write_file`.
- Support `-r` flag for recursive directory copying.
- Register the command in `src/shell/commands/mod.rs`.

## 6. Verification Plan
- **Unit Tests:** Add tests in `src/fs_tests.rs` for `rename`.
- **Shell Tests:** Verify `cp` and `mv` manually in the shell.
- **Userspace Test:** Create a small userspace app that uses `libakuma::rename` to verify the syscall.
