# Crush: Missing or Partial Syscalls and VFS Features

This document tracks the syscalls and kernel features that were found to be missing or insufficient during the porting of `crush` (and its `modernc.org/sqlite` dependency) to Akuma OS.

## 1. POSIX File Locking (`fcntl`)

**Symptoms:** `SQLITE_PROTOCOL (15)` error.
**Status:** Partially Implemented.
**Details:** 
- The kernel's `sys_fcntl` implements `F_GETFD`, `F_SETFD`, `F_GETFL`, and `F_SETFL`.
- It **does not** implement locking commands: `F_SETLK`, `F_SETLKW`, `F_GETLK`.
- SQLite's default VFS (and the `modernc.org/sqlite` pure Go driver) relies on these for database concurrency.
**Workaround:** Use `nolock=1` in the SQLite DSN to bypass these checks.

## 2. Shared Memory Mappings (`mmap`)

**Symptoms:** `SQLITE_PROTOCOL (15)` or crashes when enabling WAL mode.
**Status:** Missing `MAP_SHARED`.
**Details:**
- SQLite's Write-Ahead Logging (WAL) mode requires `MAP_SHARED` to synchronize the WAL index between multiple processes or connections.
- Akuma currently treats all mmaps as private or does not correctly implement the shared visibility required by WAL.
**Workaround:** Use `PRAGMA journal_mode = DELETE` to avoid the need for shared memory.

## 3. Directory Management (`mkdirat` / `MkdirAll`)

**Symptoms:** `SQLITE_CANTOPEN (14)` when creating the `.crush` directory.
**Status:** Incomplete / Buggy.
**Details:**
- `MkdirAll` (recursive directory creation) in Go depends on reliable `mkdir` and `stat` behavior.
- Issues were observed where relative paths or recursive creation failed due to VFS path resolution edge cases.
- Specifically, the `.` and `..` resolution in some VFS contexts might be inconsistent.
**Workaround:** Ensure absolute paths are used for database and data directory initialization.

## 4. File Synchronization (`fsync`)

**Status:** No-op.
**Details:**
- Akuma's `fsync` syscall currently returns `0` (success) without performing actual disk synchronization.
- While this doesn't "break" the app, it makes it vulnerable to database corruption on power loss.
**Workaround:** Set `PRAGMA synchronous = OFF` to acknowledge this reality and gain a small performance boost.

## 5. Large Stack Requirements

**Symptoms:** `exit code 137` (OOM/Stack Overflow).
**Status:** Configurable.
**Details:**
- The `modernc.org/sqlite` driver is a transpilation of SQLite C code to Go, resulting in very deep stacks.
- Standard Akuma stack sizes (clamped at 2MB) are insufficient for this engine and the Go runtime under load.
**Resolution:** Increased `USER_STACK_SIZE_OVERRIDE` to 8MB in `src/config.rs`.

## Summary of Applied Fixes

To run `crush` on Akuma, the following DSN and Pragmas are mandatory:

```go
dsn := "file:/path/to/crush.db?nolock=1"
// PRAGMA journal_mode = DELETE
// PRAGMA synchronous = OFF
```
