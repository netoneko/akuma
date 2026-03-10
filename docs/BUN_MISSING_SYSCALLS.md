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

### Root Cause: Missing Signal Delivery

The crash is **100% reproducible** with identical fault address, instruction
pointer, and register state on every run:

```
FAR=0x2346b2ad68  ELR=0x4416d74  ISS=0x45
x19=0x203ffbad58  x20=0x203ffbad68  SP_EL0=0x203ffbabc0
```

The root cause is **not** a munmap bug, pointer corruption, or memory leak.
It is the kernel's **failure to deliver SIGSEGV to the process's registered
signal handler**.

**How it works:** JSC (bun's JS engine) registers a SIGSEGV handler via
`rt_sigaction` during startup (11 signal handler registrations total). JSC's
JIT uses SIGSEGV handlers for speculation failure recovery — a standard JIT
technique where speculative code intentionally triggers faults on bad paths,
and the signal handler redirects execution to a fallback. On Linux, the kernel
delivers the signal by setting up a signal frame on the user stack and
jumping to the handler.

**What Akuma does:** The kernel stores the signal actions (via
`sys_rt_sigaction`) but never invokes them. When a data abort from EL0
cannot be resolved by demand paging, `exceptions.rs` unconditionally calls
`return_to_kernel(-11)` to kill the process. The comment in `sys_kill`
confirms: *"True userspace signal delivery not yet implemented."*

**Evidence:** The faulting address `0x2346b2ad68` has the same lower 12 bits
(`0xd68`) as x20 (`0x203ffbad68`) but completely different upper bits. This
is characteristic of a JIT speculation failure — the JIT compiled code
using a speculative type assumption, and when that assumption was wrong,
the resulting pointer is garbage. On Linux, JSC's SIGSEGV handler catches
this and restarts the operation with the correct (slower) path.

### Performance Bottleneck: 67M mremap Calls

Per-process syscall stats revealed that **99.95% of all syscalls are mremap**:

```
[PSTATS] PID 26 (/bin/bun) 177.01s: 67633543 syscalls (382085/s)
  mremap=67633152  clock_gettime=202  openat=22  mmap=22
  madvise=19  rt_sigprocmask=16  futex=15  rt_sigaction=11
```

Diagnostic logging showed every single mremap call has the same pattern:

```
old_sz=0x1000 → new_sz=0x2000, flags=0x0 (no MREMAP_MAYMOVE)
```

Addresses descend monotonically page-by-page from the top of the VA space
(~129 GB), scanning the entire address space 2-3 times. This is likely
JSC's conservative garbage collector probing memory pages.

All 67M calls immediately return ENOMEM (the kernel returns ENOMEM
whenever `flags & MREMAP_MAYMOVE == 0` and `new_size > old_size`, without
even checking if the page is mapped). On real Linux, unmapped pages would
return EFAULT, potentially allowing the GC to skip large unmapped ranges.

At ~2µs per syscall trap on TCG, the mremap overhead alone accounts for
~134 seconds of the 177-second runtime.

### Previous Crash Analysis (Superseded)

Earlier analysis suspected the crash was caused by `munmap` of giant
regions, pointer corruption from trimmed arenas, or nondeterministic
timing. These hypotheses are superseded by the signal delivery finding:

- The crash address is deterministic, not nondeterministic
- The crash happens AFTER all mmap/munmap activity completes
- The fault is in JIT-compiled code, not allocator code
- JSC expects the SIGSEGV to be caught by its signal handler

### Child Process Behavior

Bun spawns 3-4 child processes via fork+exec during initialization:
- Process 27: runs ~10s, exits 0 (pre-fork helper with JIT compilation)
- Process 28: runs ~0.6s, exits 0 (short-lived helper)
- Process 29: runs ~10s, exits 0 (worker thread with JIT compilation)
- Process 30: runs ~0.5s, exits 0 (short-lived helper)

The 10-second child processes are bun's fork+exec pattern for testing
environment capabilities. Their JIT compilation overhead on TCG emulation
explains the long runtime.

---

## Implementation Status

timerfd is now functional (timer state tracking, expiration counting,
read support). epoll remains a stub. Signal delivery is **fully
implemented** (try_deliver_signal + do_rt_sigreturn + lazy signal frame
demand-paging). set_robust_list, membarrier, and close_range have
functional implementations.

**To make `bun run` fully functional (priority order):**

1. **epoll** (CRITICAL for `bun run` network) — Real I/O multiplexing
   over socket/pipe/timerfd descriptors. Currently returns 0 events
   immediately regardless of what file descriptors are registered.
   bun's libuv event loop depends on epoll to know when TCP sockets are
   readable/writable. Without it, `bun run` can execute JS from disk but
   cannot do any network I/O.
2. **epoll + timerfd integration** — epoll_pwait should return events
   when timerfds expire. Needed for libuv's timer-driven callbacks.
3. **AF_UNIX sockets** — bun uses Unix domain sockets for internal
   subprocess communication (e.g. spawning worker processes). Akuma
   returns `EAFNOSUPPORT` for any socket domain other than `AF_INET`
   (domain != 2). This blocks bun's subprocess IPC.
4. **setsockopt** — Currently a no-op (returns 0). bun sets `SO_REUSEADDR`,
   `TCP_NODELAY`, `SO_KEEPALIVE`, `IPV6_V6ONLY`, and many others. Most
   are safe to ignore, but `SO_RCVBUF`/`SO_SNDBUF` affect buffering.

**To make `bun install express` work (in addition to above):**

5. **DNS resolution** — bun resolves `registry.npmjs.org` via musl's
   `getaddrinfo`, which sends UDP DNS queries. AF_INET+SOCK_DGRAM is
   supported in Akuma's socket layer, but epoll is needed to wait for
   the response. Without working epoll, DNS queries time out.
6. **HTTPS / TLS** — bun uses its built-in BoringSSL for TLS; the kernel
   just needs to pass raw TCP bytes. TCP itself is implemented (smoltcp),
   but again epoll is required to drive the connection.
7. **`inotify`** (NR 26/27/28) — Returns ENOSYS. bun uses inotify for
   file watching. Not needed for `install` but blocks `bun --watch`.
8. **`io_uring`** (NR 425/426/427) — Returns ENOSYS. bun probes for
   io_uring support and falls back to epoll if unavailable. ENOSYS is
   the correct fallback trigger.

**Lower priority (bun works without these):**

9. **mremap + lazy regions** (MEDIUM) — `sys_mremap` does not handle lazy
   (demand-paged) regions when MREMAP_MAYMOVE is set; the old lazy entry
   leaks.
10. **`sigaltstack`** (NR 132) — Returns 0 (no-op). bun may install an
    alternate signal stack for crash handler robustness; no-op is safe
    since signal delivery now uses the main stack.
11. **`fallocate`** (NR 47) — Not implemented (falls through to ENOSYS).
    bun uses fallocate for pre-allocating package cache files. The code
    path falls back gracefully to write().
12. **`statx`** (NR 291) — Not implemented. bun uses statx for extended
    file metadata (birth time). Falls back to stat/fstat.

## Known Bugs Found During Investigation

### syscall_name mapping errors (fixed)

The per-process syscall stats name table had incorrect mappings:
- `233 => "mremap"` was wrong — 233 is `madvise` (`nr::MADVISE = 233`)
- `216 => "mremap"` was missing — 216 is `mremap` (`nr::MREMAP = 216`)
- `228 => "madvise"` was wrong — 228 is not used by the kernel

This caused PSTATS to misattribute 67M mremap calls as "unknown" and
19 madvise calls as "mremap". Fixed in `crates/akuma-exec/src/process.rs`.

### log::Log backend not registered

The kernel never registers a `log::Log` backend at boot. All `log::info!`,
`log::debug!`, etc. calls from extracted crates (`akuma-exec`) are silently
dropped. Code that needs guaranteed output must use `(runtime().print_str)()`
instead. See `docs/KERNEL_SPLIT_BUGS.md` for details.
