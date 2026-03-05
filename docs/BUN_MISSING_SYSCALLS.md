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

Creates a `TimerFd(id)` file descriptor backed by a global `TIMERFD_TABLE`.
Each timer gets a unique ID.

### `timerfd_settime` (NR 86)

Now functional. Stores the initial expiration and interval in
`TIMERFD_TABLE`. Supports `TFD_TIMER_ABSTIME`. If `old_value` is non-NULL,
writes the previous timer's remaining time. Disarms the timer if both
`it_value` and `it_interval` are zero.

### `timerfd_gettime` (NR 87)

Returns the remaining time until next expiration and the interval from
`TIMERFD_TABLE`. Returns zeroes if the timer is disarmed.

### `timerfd read()` (via `sys_read`)

Calculates the number of expirations since the timer was armed. Returns an
8-byte `uint64_t` expiration count. Returns `EAGAIN` if the timer has not
yet fired (non-blocking behavior).

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
pages with no backing physical frame. Bun calls `madvise` with
`MADV_POPULATE_READ` (14) to pre-fault pages, which our no-op silently ignores;
demand paging handles the actual faults later.

### `mprotect` cache maintenance (NR 226)

When `mprotect` adds `PROT_EXEC` permission, the kernel now flushes the data
cache (`DC CVAU`) and invalidates the instruction cache (`IC IVAU`) for every
cache line in the affected region, followed by `DSB ISH` + `ISB`. This ensures
JIT code written through the data cache is visible to the instruction fetcher.

### `mremap` hardening

`sys_mremap` now validates `old_addr` against `user_va_limit()` and checks
the source buffer with `validate_user_ptr` before copying. Without this, bun's
attempts to mremap could pass kernel-space addresses, causing data aborts from
EL1.

---

## Process / Thread Management

### `exit_group` (NR 94)

Previously mapped to `sys_exit` which only marked the calling thread as
exited, leaving CLONE_VM sibling threads running with potentially freed
page tables. Now calls `sys_exit_group` which invokes `kill_thread_group()`
to terminate all threads sharing the same address space before the page
tables are freed.

### `tkill` (NR 130) — fix

Previously called `return_to_kernel(-sig)` on the *calling* thread,
completely ignoring the target TID. This meant any `tkill` call would kill
the caller. Changed to a no-op stub (returns 0) since signal delivery is
not implemented.

### `nanosleep` (NR 101) — fix

Previously treated `x0` and `x1` as raw seconds/nanoseconds values instead
of pointers to `struct timespec`. Fixed to read the timespec from the user
pointer in `x0`. Validates the pointer and returns `EFAULT` for NULL.

---

## JIT Support (SCTLR_EL1 configuration)

Bun's JavaScriptCore JIT requires user-space cache maintenance instructions
(`DC CVAU`, `IC IVAU`) and `CTR_EL0` access. These are controlled by
`SCTLR_EL1` bits:

- **UCI (bit 26):** Allows EL0 `DC CVAU` and `IC IVAU` without trapping
- **UCT (bit 15):** Allows EL0 `MRS CTR_EL0` without trapping

Both bits are now set in `src/boot.rs`. Without UCI, these instructions
trapped to EL1 where the handler silently skipped them, causing the CPU to
execute stale instruction cache contents (garbage instructions, corrupted
syscall numbers).

---

## Process Monitoring

### `pidfd_open` (NR 434)

Returns `ENOSYS`. Bun calls `pidfd_open(child_pid, 0)` after `clone3` to
obtain a pollable file descriptor for the child process. Since Akuma does
not implement pidfds, the call fails and bun falls back to `wait4` (NR 260)
for child process status collection.

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

## `bun run` Crash Analysis (March 2026)

### Observed behavior

`bun --version` completes successfully (exit 0). `bun run <script>` crashes
with SIGSEGV (-11) during JS engine initialization. The crash occurs after
bun's event loop setup (epoll, timerfd, eventfd created) and after it spawns
worker threads via clone.

### Startup syscall sequence (from instrumented run)

```
rt_sigaction × 10   (SIGPIPE, SIGSEGV, SIGILL, SIGBUS, SIGFPE, SIGXFSZ, SIGTERM, SIGINT, SIGUSR1)
close_range(4, UINT32_MAX, CLOSE_RANGE_CLOEXEC)
clone pid 17→18     (child exits 0 — likely a pre-fork helper)
clone pid 17→19     (child exits 0)
epoll_create1()     → fd 13
timerfd_create × 3  → fd 14, 16, 17
timerfd_settime(id=3, 1s initial, 1s interval)
epoll_ctl(ADD fd 15 eventfd, ADD fd 17 timerfd)
munmap 0xbd0ae000 + 1.07GB   (mimalloc arena trim)
munmap 0x200000000 + 3.0GB   (mimalloc arena trim)
clone pid 17→20     (worker thread)
CRASH: FAR=0x2346b2ad68  ELR=0x4416d74  ISS=0x45
```

### Crash details

The faulting address `0x2346b2ad68` falls inside a region that was
munmapped during mimalloc's arena trimming (`munmap 0x200000000+3GB`).
The address is between the munmapped region and the remaining lazy region
at `0x2bd0ae000`. This suggests either:

1. The partial munmap implementation has a bug for these giant regions,
   leaving stale page table entries or failing to properly split lazy regions
2. A pointer calculated before the munmap points into the now-unmapped
   trimmed portion

Previous runs showed a different crash at `FAR=0x5` (near-null
dereference). The instability suggests the root cause may be
nondeterministic — timing-dependent or address-layout-dependent.

### Options for moving forward

**Option A — Investigate munmap for giant regions (most likely culprit).**
The crash address falls squarely in a munmapped region. The partial munmap
implementation (`munmap_lazy_region` in `process.rs`) handles prefix,
suffix, and middle-split cases but may have an edge case with multi-GB
regions or regions that span multiple lazy entries. If the lazy region
table isn't properly updated, a subsequent page fault in the trimmed
area won't find a backing region and will SIGSEGV.

**Option B — Implement `close_range` with `CLOSE_RANGE_CLOEXEC`.**
Bun calls `close_range(4, UINT32_MAX, 4)` where flag 4 =
`CLOSE_RANGE_CLOEXEC`. The stub returns success without marking any fds.
After clone, child processes may inherit fds they shouldn't. Low risk for
this specific crash but could cause subtle issues.

**Option C — Make epoll aware of timerfd expirations.**
Bun adds a 1-second timerfd to epoll and expects `epoll_pwait` to return
an event when it fires. Our stub always returns 0 events. If bun's event
loop logic makes decisions based on timer events (e.g., triggering GC or
initialization phases), the missing events could leave data structures
uninitialized.

**Option D — Add a syscall ring buffer for crash forensics.**
Log the last N syscalls (number, args, return value) in a fixed-size ring
buffer. On SIGSEGV, dump the buffer. This gives the exact sequence leading
to the crash without the cost of logging every syscall to serial.

### Recommendation

Start with **Option A**. The crash address is in a munmapped region,
which is the strongest signal. Verify that `sys_munmap` correctly handles
partial unmaps of regions larger than 4GB and that the lazy region table
is consistent after the trim. If munmap is correct, proceed to **Option D**
to capture the exact syscall that produces the bad pointer.

---

## Implementation Status

timerfd is now functional (timer state tracking, expiration counting,
read support). epoll and signal handling remain stubs.

**To make bun fully functional, these still need real implementations:**

1. **epoll** — Real I/O multiplexing over socket/pipe/timerfd descriptors
2. **epoll + timerfd integration** — epoll_pwait should return events when timerfds expire
3. **eventfd** — Already functional (atomic counter with blocking read)
4. **clone/futex** — Thread creation and synchronization (partially implemented)
5. **signal handling** — `rt_sigaction`, `rt_sigprocmask` with proper delivery
6. **close_range** — Proper implementation with `CLOSE_RANGE_CLOEXEC` flag support
