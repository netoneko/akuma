# Crush: Missing or Partial Syscalls and VFS Features

This document tracks the syscalls and kernel features that were found to be missing or insufficient during the porting of `crush` (and its `modernc.org/sqlite` dependency) to Akuma OS.

## 1. POSIX File Locking (`fcntl`)

**Symptoms:** `SQLITE_PROTOCOL (15)` error.
**Status:** Stub-implemented (2026-03-15) in `src/syscall/fs.rs`.
**Details:**
- The kernel's `sys_fcntl` now implements `F_GETFD`, `F_SETFD`, `F_GETFL`, `F_SETFL`,
  `F_DUPFD`, `F_DUPFD_CLOEXEC`, `F_GETLK`, `F_SETLK`, and `F_SETLKW`.
- `F_GETLK`/`F_SETLK`/`F_SETLKW` are no-ops (return 0). Akuma has no per-file lock
  state, so advisory locks are silently accepted. This is sufficient for SQLite's
  single-process use case; multi-process locking correctness is not guaranteed.
- `F_DUPFD` / `F_DUPFD_CLOEXEC` duplicate the fd (like `dup()`) with optional
  `O_CLOEXEC` marking; needed by Bun's `WriteStream` fast path.
**Workaround:** `nolock=1` in the SQLite DSN is still recommended for robustness.

## 2. Shared Memory Mappings (`mmap`)

**Symptoms:** `SQLITE_PROTOCOL (15)` or crashes when enabling WAL mode.
**Status:** Missing `MAP_SHARED`.
**Details:**
- SQLite's Write-Ahead Logging (WAL) mode requires `MAP_SHARED` to synchronize the WAL index between multiple processes or connections.
- Akuma currently treats all mmaps as private or does not correctly implement the shared visibility required by WAL.
**Workaround:** Use `PRAGMA journal_mode = DELETE` to avoid the need for shared memory.

## 3. Path Resolution and CWD (`getcwd`, `chdir`, `openat`)

**Symptoms:** `SQLITE_CANTOPEN (14)` when opening `crush.db`.
**Status:** Inconsistent relative path handling.
**Details:**
- Go's `os.Getwd()` and `filepath.Abs()` depend on accurate `getcwd` syscall behavior.
- `libakuma`'s `getcwd` implementation returns the length *including* the null terminator, which is standard for Linux/Akuma but must be handled carefully by callers.
- VFS mount points and relative paths in URI-style SQLite connections (e.g., `file:.crush/crush.db`) can fail if the working directory or parent directories aren't perfectly resolved.
**Workaround:** 
- Use `filepath.Abs()` to convert all database paths to absolute before passing to `sql.Open`.
- Explicitly call `os.MkdirAll` on the parent directory.
- Manually `os.OpenFile(dbPath, os.O_RDWR|os.O_CREATE, 0644)` before `sql.Open` to ensure the file exists if the VFS/SQLite driver fails to create it via URI.

## 4. Subprocess Execution (`execve`, `clone3`, `wait4`)

**Symptoms:** `execve: path copy failed with -14` (EFAULT) and SIGSEGV at `FAR=0xfff2`.
**Status:** Partially broken for certain Go runtime paths.
**Details:**
- Go's `os/exec` (e.g., calling `git rev-parse`) uses `clone3` or `fork/exec`.
- In some cases, `execve` receives invalid pointers or attempts to access memory outside the mapped range, triggering `EFAULT`.
- This often happens when the Go runtime tries to resolve the executable path or environment variables in a way that Akuma's memory model doesn't expect.
**Workaround:** Avoid calling external binaries (like `git`) inside the Go app if possible, or ensure the environment is stripped of complex variables.

## 5. File Synchronization (`fsync`)

**Status:** No-op.
**Details:**
- Akuma's `fsync` syscall currently returns `0` (success) without performing actual disk synchronization.
- While this doesn't "break" the app, it makes it vulnerable to database corruption on power loss.
**Workaround:** Set `PRAGMA synchronous = OFF` to acknowledge this reality and gain a small performance boost.

## 6. Large Stack Requirements

**Symptoms:** `exit code 137` (OOM/Stack Overflow).
**Status:** Configurable.
**Details:**
- The `modernc.org/sqlite` driver is a transpilation of SQLite C code to Go, resulting in very deep stacks.
- Standard Akuma stack sizes (clamped at 2MB) are insufficient for this engine and the Go runtime under load.
**Resolution:** Increased `USER_STACK_SIZE_OVERRIDE` to 8MB in `src/config.rs`.

## Summary of Applied Fixes

To run `crush` on Akuma, the following DSN and Pragmas are mandatory:

```go
dsn := "file:/absolute/path/to/crush.db?nolock=1"
// PRAGMA journal_mode = DELETE
// PRAGMA synchronous = OFF
```
