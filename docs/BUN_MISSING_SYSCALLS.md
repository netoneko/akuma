# Syscalls Added for Bun Runtime Support

Bun (the JavaScript runtime) requires a broad set of Linux syscalls beyond what
Akuma originally implemented. This document catalogs every syscall added or
stubbed specifically to support bun, grouped by subsystem.

---

## Event Loop / I/O Multiplexing

Bun's event loop is built on µWebSockets/libuv which rely on epoll and timerfd.
These are currently stubs — they allocate virtual file descriptors but do not
actually multiplex I/O. Bun can initialize with them but will not have a
functional event loop until they are fully implemented.

### `epoll_create1` (NR 20)

Creates a virtual `EpollFd` file descriptor. No actual epoll instance is
maintained; the fd exists to satisfy the event loop startup sequence.

### `epoll_ctl` (NR 21)

No-op stub. Returns 0 for all operations (`EPOLL_CTL_ADD`, `MOD`, `DEL`).

### `epoll_pwait` (NR 22)

Returns 0 events immediately. If `timeout != 0`, yields the current thread
first to avoid busy-spinning.

### `timerfd_create` (NR 85)

Creates a virtual `TimerFd` file descriptor. No timer is actually armed.

### `timerfd_settime` (NR 86)

No-op stub. If `old_value` is non-NULL, zeroes the output `itimerspec`.
Returns 0.

### `timerfd_gettime` (NR 87)

Zeroes the output `itimerspec` (indicating no armed timer). Returns 0.

### `eventfd2` (NR 19)

Creates a virtual `EventFd` file descriptor backed by a shared `AtomicU64`
counter. Supports `read` (returns and clears the counter, blocking if zero
and `EFD_NONBLOCK` is not set) and `write` (adds to the counter). Used by
bun for internal thread signaling.

---

## Scheduling

### `sched_setaffinity` (NR 122)

No-op stub. Returns 0. Akuma runs a single-core virtual machine, so CPU
affinity has no effect.

### `sched_getaffinity` (NR 123)

Writes a single-CPU affinity mask (bit 0 set) into the user buffer.
Returns 0.

### `sched_yield` (NR 124)

Calls `threading::yield_now()` to voluntarily yield the current thread's
time slice. Returns 0.

### `sched_setparam` (NR 118)

No-op stub. Returns 0.

### `sched_getparam` (NR 119)

Writes a zeroed `sched_param` struct (priority = 0) to the user buffer.
Returns 0.

---

## Signals

### `tkill` (NR 130)

Sends a signal to a specific thread. Currently calls `return_to_kernel(-sig)`
to terminate the process with the given signal number. Used by bun's crash
handler and assertion failures.

---

## File Descriptor Management

### `close_range` (NR 436)

Stub that returns 0. Bun calls this during startup to close inherited file
descriptors. A proper implementation would iterate and close fds in the
specified range.

---

## System Information

### `sysinfo` (NR 179)

Fills a 112-byte `sysinfo` struct with:
- `uptime`: seconds since boot (from system timer)
- `totalram`: total physical pages × page size
- `freeram`: free physical pages × page size (from `pmm::free_count()`)
- `mem_unit`: 1

Used by bun's allocator (mimalloc) to size its arenas proportionally to
available memory.

### `uname` (NR 160)

Fills a `utsname` struct (5 × 65-byte fields):
- `sysname`: "Akuma"
- `nodename`: "akuma"
- `release`: "0.1.0"
- `version`: "0.1.0"
- `machine`: "aarch64"

Bun checks `machine` to confirm it's running on a supported architecture.

### `clock_getres` (NR 114)

Returns 1-nanosecond resolution (`tv_sec=0, tv_nsec=1`) for all clock IDs.
Used by bun's high-resolution timer initialization.

### `membarrier` (NR 283)

Returns 0 (indicating no supported membarrier commands). Bun's JIT compiler
queries this to decide whether to use full memory barriers or the lighter
`MEMBARRIER_CMD_PRIVATE_EXPEDITED` path.

---

## Virtual Filesystem

### `/proc/self/exe` (via `openat` and `readlinkat`)

Bun reads `/proc/self/exe` to locate its own binary for:
- Self-reexecution (`bun run`)
- Locating adjacent resources

Both `sys_openat` and `sys_readlinkat` intercept this path and redirect to the
current process's binary name (e.g., `/bin/bun`).

### `/dev/urandom` and `/dev/random`

Bun requires `/dev/urandom` for cryptographic randomization (JavaScriptCore's
`WTF::cryptographicallyRandomValues`). If the open fails, bun deliberately
crashes at `FAR=0xBBADBEEF`. Implemented as a virtual `DevUrandom` file
descriptor. See `docs/DEV_RANDOM.md`.

---

## Memory Management

### `madvise` (NR 233)

Changed to a no-op (returns 0). Previously attempted to honor `MADV_DONTNEED`
by unmapping pages, but this crashed the kernel when applied to lazy-mapped
pages with no backing physical frame.

### `mremap` hardening

`sys_mremap` now validates `old_addr` against `user_va_limit()` and checks
the source buffer with `validate_user_ptr` before copying. Without this, bun's
attempts to mremap could pass kernel-space addresses, causing data aborts from
EL1.

---

## Other Stubs (pre-existing, also used by bun)

| Syscall | NR | Notes |
|---------|----|-------|
| `prctl` | 167 | Returns 0 (no-op) |
| `flock` | 32 | Returns 0 (single-user OS) |
| `umask` | 166 | Returns 0o022 (ignores argument) |
| `getrusage` | 165 | Zero-fills rusage struct |
| `msync` | 227 | Returns 0 (no swap/persistent mmap) |
| `process_vm_readv` | 270 | Returns ENOSYS |

---

## Implementation Status

Most of the event loop syscalls (epoll, timerfd) are stubs that return success
without doing real work. This allows bun to complete its initialization
sequence, but the event loop is non-functional — bun cannot actually serve
requests or run timers.

**To make bun fully functional, these need real implementations:**

1. **epoll** — Real I/O multiplexing over socket/pipe/timerfd descriptors
2. **timerfd** — Timer expiration with epoll integration
3. **eventfd** — ✅ Already functional (atomic counter with blocking read)
4. **clone/futex** — Thread creation and synchronization (partially implemented)
5. **signal handling** — `rt_sigaction`, `rt_sigprocmask` with proper delivery
