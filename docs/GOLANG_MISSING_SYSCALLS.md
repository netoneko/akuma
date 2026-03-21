# Go Runtime — Missing / Incomplete Syscall Support

Tracked gaps and fixes required to run Go binaries on Akuma.

## Milestone Status

| Milestone | Status |
|-----------|--------|
| `CGO_ENABLED=0 go build -n` (dry run, no compilation) | **Fixed** (2026-03-21) |
| `CGO_ENABLED=0 go build` (actual compilation) | **In progress** — crashes during compilation |

---

## 1. Signal delivery ignores `SA_ONSTACK` — crash in Go runtime

**Status:** Fixed (2026-03-15) in `src/exceptions.rs`
**Component:** `src/exceptions.rs` — `try_deliver_signal`

### Symptom

Any Go binary that uses goroutines crashes immediately with:

```
signal 11 received but handler not on signal stack
mp.gsignal stack [0xa44a0000 0xa44a8000], mp.g0 stack [...], sp=0xa44e2ad8
fatal error: non-Go code set up signal handler without SA_ONSTACK flag
```

### Root cause

`try_deliver_signal` never checked the `SA_ONSTACK` flag or used the process's `sigaltstack` fields, causing signals to be delivered on the goroutine stack instead of the gsignal stack.

### Fix

In `try_deliver_signal`, check `SA_ONSTACK` and `sigaltstack` fields before selecting the stack for delivery.

---

## 2. Re-entrant SIGSEGV — infinite signal delivery loop

**Status:** Fixed (2026-03-15) in `src/exceptions.rs`

### Fix

At the top of `try_deliver_signal`, detect re-entrant delivery by checking whether the current `sp_el0` already falls within the sigaltstack range. If so, kill the process to prevent an infinite loop.

---

## 3. Kernel heap exhaustion — `go build` panics the kernel

**Status:** Fixed (2026-03-15) in `src/main.rs`, `src/allocator.rs`

### Fix

- Dynamic kernel heap sizing based on system RAM.
- `alloc_error_handler` now kills the offending userspace process instead of panicking the kernel.

---

## 4. EPOLLET edge not reset after `read()` on TCP sockets — model response hang

**Status:** Fixed (2026-03-15) in `src/syscall/fs.rs`

### Fix

Call `epoll_on_fd_drained()` after every successful TCP `read()` in `sys_read` to clear the `EPOLLIN` event, allowing Go to poll correctly.

---

## 5. `restart_syscall` (nr=128) returns ENOSYS — Go runtime crash after signal

**Status:** Fixed (2026-03-15) in `src/syscall/mod.rs`

### Fix

Added an explicit case for 128 (`restart_syscall`) that returns `EINTR` instead of `ENOSYS`.

---

## 6. `waitid` (syscall 95) — `go build` crashes waiting for child processes

**Status:** Fixed (2026-03-15) in `src/syscall/proc.rs` + `src/syscall/mod.rs`

### Fix

Added `sys_waitid` as a wrapper over existing child-channel infrastructure.

---

## 7. `pidfd_open` (syscall 434) + `waitid(P_PIDFD)` — Go busy-polls with nanosleep

**Status:** Fixed (2026-03-15) in `src/syscall/pidfd.rs` + `src/syscall/proc.rs`

### Fix

Implemented `pidfd_open` and integrated it with `epoll` and `waitid` readiness checks.

---

## 8. `CLONE_PIDFD` not handled — Go netpoller uses garbage fd

**Status:** Fixed (2026-03-15) in `src/syscall/proc.rs`

### Fix

`sys_clone3` now writes the child's `pidfd` back to the user-provided pointer if `CLONE_PIDFD` is requested.

---

## 9. `MOUNT_TABLE` spinlock held during disk I/O

**Status:** Fixed (2026-03-15) in `src/vfs/mod.rs`

### Fix

Released mount table locks before calling filesystem I/O closures.

---

## 10. Signal state not reset on `execve`

**Status:** Fixed (2026-03-15) in `crates/akuma-exec/src/process/mod.rs`

### Fix

Reset custom signal handlers to `SIG_DFL` and disable the altstack on `execve` per POSIX.

---

## 11. `tgkill` (syscall 131) returns ENOSYS

**Status:** Fixed (2026-03-15) in `src/syscall/signal.rs`

### Fix

Added `sys_tgkill` which forwards to `sys_tkill`.

---

## 12. `msgctl` / `msgget` / `msgrcv` / `msgsnd` (syscalls 186-189)

**Status:** Implemented (2026-03-16) in `src/syscall/msgqueue.rs`

### Fix

Full SysV message queue implementation with box-based isolation.

---

## 13. `CLONE_VFORK` does not block parent

**Status:** Fixed (2026-03-16) in `src/syscall/proc.rs`

### Fix

Added `VFORK_WAITERS` to explicitly block parent threads until child exits/execs.

---

## 14. `go build` deadlocks — goroutine scheduler eventfd event missing

**Status:** Fixed (2026-03-16) in `src/syscall/sync.rs`

### Fix

Moved futex value reading inside the lock to eliminate the missed-wakeup race.

---

## 15–18. Signal frame corruption, VFORK race, user_va_limit

**Status:** Fixed (2026-03-16)

### Fixes
- Implemented robust `rt_sigreturn` signal frame state restoration (mask, FPSIMD).
- Fixed race between `fork` and `vfork_complete` by pre-inserting into `VFORK_WAITERS`.
- Increased `user_va_limit` to 48-bit address space.

---

## 19. Pipe refcount race in `dup3` / `fcntl`

**Status:** Fixed (2026-03-17)

### Fix
- Atomic FD swapping in `sys_dup3`.
- Explicit `pipe_clone_ref` in `fcntl` duplicating pipe fds.

---

## 20. Stale `PENDING_SIGNAL` on thread slot reuse

**Status:** Fixed (2026-03-17)

### Fix
- Explicitly clear `PENDING_SIGNAL` during thread slot recycling.

---

## 21. `pipe_write` silent data loss

**Status:** Fixed (2026-03-17)

### Fix
- `pipe_write` now returns `Result<usize, i32>` with `EPIPE` on error.
- `sys_write` correctly propagates `EPIPE` error codes.

---

## 49. Broken Pipe / Premature Pipe Destruction (2026-03-20)

**Status:** Fixed (2026-03-20)

### Fix
- **Error Codes**: `sys_read`/`sys_write` return `EBADF` for invalid FDs.
- **Cleanup**: `SharedFdTable` implements `Drop` to ensure resources (pipes, etc.) are closed only when the last reference to the FD table is gone.

## 50. Pipe Read Performance (Quadratic Slowdown) (2026-03-20)

**Status:** Fixed (2026-03-20)

### Fix
- Replaced `Vec<u8>` with `VecDeque<u8>` in `KernelPipe` for efficient $O(1)$ reads.

## 51. `ChildStdout` streaming hangs — parent busy-looping on non-blocking read (2026-03-20)

**Status:** Fixed (2026-03-20)

### Fix
- **Blocking Reads**: Added `reader_thread` to `ProcessChannel`, allowing reads to block until data is written.
- **`epoll` Readiness**: Updated `epoll_check_fd_readiness` to correctly check for `ChildStdout` data availability.

---

## 52. `find /proc` errors on dead-process `fd` directory (2026-03-21)

**Status:** Fixed (2026-03-21) in `src/vfs/proc.rs`

### Symptom

```
/usr/bin/find: failed to opendir /proc/49/fd: No such file or directory
```

A dead process (PID retained in the syscall log) appeared in `ls /proc` but its `fd` subdirectory returned `ENOENT` when `find` tried to open it — an inconsistent directory listing that caused `find` to exit with code 1 and confused Go's build tooling.

### Root cause

`read_dir` for `<pid>/` always added a `"fd"` `DirEntry`, even when the process only existed via its retained syscall log (i.e. `process_exists(pid)` was false). Opening that directory then returned `NotFound` because the `<pid>/fd` handler also gated on `process_exists`.

### Fix

Gate the `"fd"` entry on `Self::process_exists(pid)`. Dead processes show only `"syscalls"` in their directory listing — consistent with what can actually be opened.

---

## 53. epoll EINTR + dup3 invariant — kernel regression tests (2026-03-21)

**Status:** Tests added (2026-03-21) in `src/process_tests.rs`, `src/sync_tests.rs`

### Background

Go crashed with `FAR=0xffffffffffffffea` (-22 = EINVAL used as a pointer) after SIGURG delivery. The crash indicated that either `sys_dup3` was returning EINVAL for a valid call (wrongly treating a non-matching fd pair as same-fd), or that `epoll_pwait` was not returning EINTR when a signal was pending — leaving Go's signal handler unsatisfied.

### Tests added

- **`test_dup3_no_einval_for_valid_args`**: Verifies the three `sys_dup3` invariants — `oldfd==newfd` → EINVAL, valid pair → `newfd`, bad `oldfd` → EBADF. Catches any regression where EINVAL leaks into valid dup paths.
- **`test_pipe_close_write_wakes_epoll_poller`**: Verifies `pipe_close_write` both drains pollers and sets `pipe_can_read` (EOF) simultaneously — the core of Go's parent-waits-for-compile-stdout workflow.
- **`test_epoll_eintr_when_signal_pending`**: Verifies `sys_epoll_pwait` returns `-EINTR` immediately when `is_current_interrupted()` is true, without blocking. Essential for Go's goroutine preemption via SIGURG.

---

## 54. si_code wrong for NULL dereferences — SIGSEGV treated as software signal (2026-03-21)

**Status:** Fixed (2026-03-21) in `src/exceptions.rs`

### Symptom

Go crashed with `PC=0x20000000, sigcode=-6, addr=0x0`. Go's SIGSEGV handler checks `si_code` to distinguish memory faults (`SEGV_MAPERR=1`) from software-sent signals (`SI_TKILL=-6`). With `si_code=-6` on a NULL deref, Go treated it as a goroutine preemption signal and tried to preempt a goroutine at the bogus fault PC.

### Root Cause

`try_deliver_signal` used `fault_addr == 0` as a proxy for "software signal" to set `si_code`:
```rust
let si_code: i32 = if fault_addr == 0 { -6i32 } else { 1i32 };
```
NULL dereferences have `FAR=0` but are hardware faults (`is_fault=true`), so they got `si_code=-6` incorrectly.

### Fix

Added `is_fault: bool` parameter to `try_deliver_signal`. Hardware fault call sites pass `true`; software signal call sites pass `false`. The `si_code` is now `if is_fault { 1 } else { -6 }`.

---

## 55. procfs non-stdio fd reads returning ENOENT (2026-03-21)

**Status:** Fixed (2026-03-21) in `src/vfs/proc.rs`, `src/syscall/fs.rs`

### Symptom

`cat /proc/<pid>/fd/<n>` for fd > 1 returned ENOENT. fd 0 and 1 worked.

### Root Cause

`read_symlink` returned virtual paths like `"pipe:[5]"` for non-File fds. `sys_openat` called `resolve_symlinks` which chased this to `crate::fs::exists("pipe:[5]")` → false → ENOENT. fd 0/1 accidentally worked because `get_fd(0/1)` returned `None` (stdin/stdout aren't in the fd table for old processes), so `read_symlink` returned `Err` and `resolve_symlinks` left the path unchanged.

Additionally, `exists`, `metadata`, and `read_at` all short-circuited at `fd_num <= 1`.

### Fix

- `read_symlink` now only returns a resolvable path for `File` fds. Other fd types return `Err` so `resolve_symlinks` leaves the path unchanged.
- `readlinkat` falls back to `proc_fd_description()` (new pub fn) for non-File fds, which returns the virtual description string (`"pipe:[5]"`, `"socket:[n]"`, etc.).
- `exists` now checks `proc.get_fd(fd_num).is_some()` for fd > 1.
- `metadata` returns metadata with size=0 for any valid fd > 1.
- `read_at` returns the fd description string for fd > 1.

---

## 56. `CGO_ENABLED=0 go build` crashes during compilation (2026-03-21)

**Status:** In progress

### Background

`CGO_ENABLED=0 go build -n` (dry run — resolves dependencies and prints commands without executing them) now works. `CGO_ENABLED=0 go build` (actual compilation — invokes the Go compiler and assembler) still crashes.

The `-n` path exercises: process spawning, pipes, epoll, signal delivery, `/proc` reads, `waitpid`. These are all fixed. The actual build path additionally runs the `compile` and `asm` toolchain binaries inside Go's build graph, which stress different kernel paths.

### Known current failure point

To be determined — need a kernel crash log from a `go build` run to identify the next failing syscall or kernel bug.

### Likely candidates

- **`clone3` / `clone` with new flags**: The Go toolchain spawns many compiler workers; any unhandled clone flag causes EINVAL.
- **`prlimit64` / `getrlimit`**: Compiler may query resource limits.
- **`fcntl(F_DUPFD_CLOEXEC)`**: Used during pipe setup for compiler subprocesses.
- **`/proc/self/fd` enumeration**: Compiler may walk its own fd table to close inherited fds.
- **`mmap` anonymous with `MAP_FIXED`**: Go's compiler allocates large arenas; partial unmaps or MAP_FIXED collisions may fault.
- **`sched_getaffinity`**: Some Go versions call this to determine GOMAXPROCS.
- **Signal mask inheritance across `clone`**: Child processes need the correct signal mask from the parent.
