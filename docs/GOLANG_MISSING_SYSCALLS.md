# Go Runtime — Missing / Incomplete Syscall Support

Tracked gaps and fixes required to run Go binaries on Akuma.

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
