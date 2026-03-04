# Infrastructure Required for libuv on Akuma

libuv is the event loop library underpinning Node.js, and is also used by Bun's
fallback paths. This document analyzes every subsystem libuv requires on Linux
and maps each to Akuma's current implementation status, identifies gaps, and
proposes an implementation order.

---

## Executive Summary

libuv's Linux backend depends on five core kernel subsystems: **epoll**,
**signals**, **process management**, **threading/futex**, and **filesystem
watching (inotify)**. Akuma has stubs or partial implementations for most of
these. The critical path to a functional event loop is:

1. Real epoll (the event loop backbone)
2. Signal infrastructure (sigaction + signal delivery via self-pipe)
3. Proper futex (thread synchronization)
4. Process spawning enhancements (fork stdio plumbing)

io_uring and inotify are optional — libuv gracefully degrades without them.

---

## 1. Epoll — Event Loop Backbone

**Priority: CRITICAL — nothing works without this**

### What libuv does

`uv__platform_loop_init()` calls `epoll_create1(O_CLOEXEC)` to create the
backend fd. The main poll loop (`uv__io_poll`) calls `epoll_pwait()` to block
for events. Watchers are registered via `epoll_ctl(EPOLL_CTL_ADD/MOD/DEL)`.

Every libuv handle type (TCP, UDP, pipe, signal, child process, fs events)
ultimately registers interest on the epoll instance. The event loop is
literally an `epoll_pwait()` call in a while loop.

### Current Akuma state

All three syscalls are **stubs**:

- `epoll_create1` — allocates an `EpollFd` file descriptor, no backing state
- `epoll_ctl` — no-op, returns 0
- `epoll_pwait` — returns 0 events immediately (yields if timeout != 0)

### What needs to be built

A real epoll implementation with these components:

```
struct EpollInstance {
    interest_list: BTreeMap<u32, EpollEntry>,  // fd -> (events, data)
}

struct EpollEntry {
    events: u32,      // EPOLLIN, EPOLLOUT, EPOLLET, etc.
    data: u64,        // opaque user data (epoll_event.data)
    fd_type: FdType,  // Socket, Pipe, EventFd, TimerFd — for readiness checks
}
```

**`epoll_ctl`** must:
- `EPOLL_CTL_ADD`: insert fd + event mask into the interest list
- `EPOLL_CTL_MOD`: update the event mask for an existing fd
- `EPOLL_CTL_DEL`: remove fd from the interest list

**`epoll_pwait`** must:
- Iterate the interest list and check readiness of each fd:
  - **Socket fds**: call existing `socket_can_recv_tcp()` / `socket_can_send_tcp()` (already used by `ppoll`)
  - **Pipe fds**: check pipe buffer non-empty (read) or pipe not full (write)
  - **EventFd fds**: check counter > 0 (read), always writable (write)
  - **TimerFd fds**: check `timerfd_read()` for expiration
- If any fds are ready, fill the `epoll_event` output array and return count
- If none ready and timeout > 0, yield and retry (poll loop with smoltcp poll)
- If timeout == 0, return 0 immediately
- If timeout == -1, block indefinitely until something is ready

**Key design decision**: Akuma's ppoll already implements this polling pattern
for sockets, eventfds, and stdin. The epoll implementation can reuse the same
readiness-check infrastructure, wrapped in the epoll_event struct format.

### Estimated complexity

Medium. The readiness-check logic already exists in `sys_ppoll()` and
`sys_pselect6()`. The main work is creating the `EpollInstance` data structure
and wiring up the epoll syscall triple to use it. ~300-500 lines.

---

## 2. Signal Infrastructure

**Priority: HIGH — needed for child process management and graceful shutdown**

### What libuv does

libuv's signal handling uses the classic **self-pipe trick**:

1. On `uv_signal_init()`, creates a pipe pair (`signal_pipefd[0..1]`)
2. Registers the pipe read-end with epoll via `POLLIN`
3. Installs a C signal handler via `sigaction()` that writes the signal number
   to the pipe write-end
4. When `epoll_pwait` returns with the pipe readable, libuv reads signal
   messages and dispatches to user callbacks

For child process management (`uv_spawn`), libuv registers a `SIGCHLD` handler
that calls `waitpid()` in a loop to reap children.

### Current Akuma state

- `rt_sigaction` (NR 134) — **stub**, returns 0 without registering any handler
- `rt_sigprocmask` (NR 135) — **stub**, returns 0
- `sigaltstack` — **stub**, returns 0
- No signal delivery mechanism exists
- `kill` syscall exists but only marks processes for termination
- `pipe2` works and is functional

### What needs to be built

**Phase 1 — Minimal sigaction (enough for libuv)**:

libuv only needs a handful of signals to actually work:
- `SIGCHLD` (17) — child process exit notification
- `SIGPIPE` (13) — must be ignorable (SIG_IGN)
- `SIGINT` (2), `SIGTERM` (15) — process termination

The kernel needs:

1. **Per-process signal action table**: store the registered handler address
   (or SIG_IGN/SIG_DFL) for each signal number

   ```
   struct SignalAction {
       handler: SignalHandler,  // UserFn(usize) | Ignore | Default
       mask: u64,               // sa_mask (can be ignored initially)
       flags: u32,              // SA_RESTART, SA_RESETHAND, etc.
   }
   ```

2. **Signal delivery via pending set**: when `kill(pid, sig)` is called (or a
   child exits), set a pending bit on the target process. On return-to-userspace
   (in `handle_syscall` or exception return), check pending signals and either:
   - If handler is `SIG_IGN`: clear the bit
   - If handler is `SIG_DFL`: terminate the process (for fatal signals)
   - If handler is a user function: divert execution to the handler (complex)

**Phase 2 — Self-pipe compatible delivery (simpler alternative)**:

Since libuv uses the self-pipe trick, we don't actually need to divert
userspace execution to a signal handler. Instead:

1. Store the signal action table (so `sigaction` returns previous action)
2. When a child process exits, set `SIGCHLD` pending on the parent
3. Make `epoll_pwait` check pending signals and return `EINTR` when one is
   pending — this is enough for libuv's signal pipe to work because the
   signal handler writes to the pipe, and epoll wakes up

However, the C-level signal handler installed by libuv (`uv__signal_handler`)
needs to actually execute in userspace. This requires either:
- **True signal delivery** (manipulate the userspace stack to call the handler
  on return from kernel) — complex but correct
- **Kernel-assisted self-pipe** (the kernel writes to the pipe directly when
  a signal is pending, bypassing the userspace handler) — simpler but a hack

**Recommended approach**: Implement `sigaction` to store handler info, but
have the kernel deliver signals by writing to the process's signal pipe
directly (if one is registered via epoll). This avoids the complexity of
userspace stack manipulation while giving libuv what it needs.

### Estimated complexity

Phase 1 (sigaction storage + SIG_IGN/SIG_DFL): ~200 lines, straightforward.
Phase 2 (kernel-assisted delivery for self-pipe): ~300 lines, medium.
True userspace signal delivery: ~500+ lines, hard (stack frame construction,
sigreturn syscall, signal mask management).

---

## 3. Futex — Thread Synchronization

**Priority: HIGH — needed for any multi-threaded userspace code**

### What libuv does

libuv uses pthreads internally for its thread pool (used for async filesystem
operations, DNS resolution, and user work items). The thread pool is 4 threads
by default (`UV_THREADPOOL_SIZE`). pthreads/musl implements mutexes and
condition variables on top of `futex()`.

The thread pool is initialized lazily on the first `uv_queue_work()` call.
Each thread pool worker calls `pthread_cond_wait()` (→ futex WAIT) to sleep
until work arrives, and `pthread_cond_signal()` (→ futex WAKE) to wake them.

### Current Akuma state

`sys_futex` exists but is **non-functional**:

```rust
FUTEX_WAIT => {
    // Checks value, then yields once and returns
    if atomic.load() != val { return EAGAIN; }
    yield_now();
    0
}
FUTEX_WAKE => {
    yield_now();
    val as u64  // claims to have woken `val` threads
}
```

This doesn't actually block/wake threads. A `FUTEX_WAIT` returns immediately
after one yield, so condition variables spin-wait instead of sleeping.

### What needs to be built

A proper futex wait queue:

```
static FUTEX_WAITERS: Spinlock<BTreeMap<usize, Vec<WaitEntry>>> = ...;

struct WaitEntry {
    thread_id: usize,
    woken: AtomicBool,
}
```

**`FUTEX_WAIT(uaddr, val)`**:
1. Atomically check `*uaddr == val` while holding the wait queue lock
2. If not equal, return `EAGAIN`
3. If equal, add current thread to the wait queue for `uaddr`
4. Put thread to sleep (park it — remove from scheduler run queue)
5. On wakeup, return 0

**`FUTEX_WAKE(uaddr, count)`**:
1. Lock wait queue for `uaddr`
2. Wake up to `count` threads (move them back to the scheduler run queue)
3. Return the number of threads actually woken

**Thread parking**: Akuma already has `yield_now()`. It needs a
complementary `park()` / `unpark(thread_id)` mechanism. `park()` removes the
thread from the round-robin scheduler. `unpark()` re-adds it.

**Timeout support**: `FUTEX_WAIT` with a non-NULL timeout needs a timer that
will unpark the thread after the specified duration. This can be implemented
with the existing `uptime_us()` timer — check the deadline on each scheduler
tick.

### Estimated complexity

~300-400 lines. The main difficulty is the park/unpark mechanism in the
scheduler, which requires careful interaction with the existing thread pool
and context switching code.

---

## 4. Process Spawning

**Priority: MEDIUM — needed for `uv_spawn` (child processes)**

### What libuv does

On Linux (non-Apple), libuv spawns processes by:

1. Creating pipes for stdio redirection (`pipe2`)
2. Calling `fork()` (or `clone()` with `CLONE_VFORK` when possible)
3. In the child: setting up stdio via `dup2()`, calling `execve()`
4. In the parent: closing unused pipe ends, registering `SIGCHLD` handler
5. Using `waitpid(pid, &status, WNOHANG)` (via SIGCHLD callback) to reap

### Current Akuma state

- `clone` / `clone3` — implemented (used for thread creation)
- `execve` — implemented
- `wait4` / `waitpid` — implemented
- `pipe2` — implemented
- `dup3` — implemented
- `fork` (clone without CLONE_VM) — implemented (copies address space)
- `dup2` — **missing** (libuv's stdio setup uses `dup2()` extensively)
- `SIGCHLD` delivery — **missing** (see Signal Infrastructure above)

### What needs to be built

1. **`dup2` syscall** (NR 23 on arm64): trivially implemented as
   `dup3(oldfd, newfd, 0)` — the existing `dup3` implementation handles the
   logic already

2. **`setsid` enhancement**: libuv calls `setsid()` in the child if
   `UV_PROCESS_DETACHED` is set. Current `setsid` implementation exists and
   should work.

3. **`SIGCHLD` delivery**: when a child process exits, the parent must be
   notified. See Signal Infrastructure (section 2).

4. **`setgroups` / `setuid` / `setgid`**: libuv calls these if
   `uv_process_options_t.uid/gid` are set. Can be stubs (return 0) for a
   single-user OS.

### Estimated complexity

Small — `dup2` is ~5 lines wrapping `dup3`. The real work is in signal delivery.

---

## 5. io_uring — Async Filesystem Operations

**Priority: LOW — libuv falls back to thread pool gracefully**

### What libuv does

libuv probes for io_uring at init time via `io_uring_setup()`. If the syscall
returns `-ENOSYS`, libuv falls back to its thread pool for all filesystem
operations (open, read, write, stat, rename, unlink, etc.). The thread pool
approach is the traditional and well-tested path.

io_uring is used for two purposes in libuv:
1. **Batching epoll_ctl operations** (the `ctl` ring) — falls back to direct
   `epoll_ctl()` calls
2. **Async filesystem I/O** (the `iou` ring) — falls back to thread pool

### Current Akuma state

Not implemented. `io_uring_setup` (NR 425 on arm64) is not handled.

### What needs to be built

**Nothing** — just ensure the syscall returns `ENOSYS`:

```rust
nr::IO_URING_SETUP => ENOSYS,
nr::IO_URING_ENTER => ENOSYS,
nr::IO_URING_REGISTER => ENOSYS,
```

libuv checks the return value and gracefully falls back. This is already what
happens if the NR is unhandled (Akuma returns `ENOSYS` for unknown syscalls).

### Estimated complexity

Zero. Already handled by the default syscall fallthrough.

---

## 6. Inotify — Filesystem Event Watching

**Priority: LOW — optional feature, not needed for core operation**

### What libuv does

`uv_fs_event_start()` calls `inotify_init1(IN_NONBLOCK | IN_CLOEXEC)` to
create an inotify instance, then `inotify_add_watch()` for each path. The
inotify fd is registered with epoll. When files change, the kernel writes
`inotify_event` structs to the fd, which libuv reads and dispatches.

### Current Akuma state

Not implemented. `inotify_init1` (NR 26), `inotify_add_watch` (NR 27),
`inotify_rm_watch` (NR 28) are not handled.

### What needs to be built

For initial bring-up, stub all three to return `ENOSYS`. libuv will report
`UV_ENOSYS` from `uv_fs_event_start()`, and Node.js will fall back to
polling-based file watching (if `fs.watch()` is used at all).

For full support later, inotify requires kernel-side VFS hooks that fire
when files are created, modified, deleted, or renamed. This is a substantial
feature.

### Estimated complexity

Stubs: 3 lines. Full implementation: 500+ lines with VFS integration.

---

## 7. Timers

**Priority: ALREADY MOSTLY WORKING**

### What libuv does

libuv does **not** use `timerfd` for its timer implementation. It manages
timers entirely in userspace with a min-heap and uses the epoll timeout
parameter to sleep until the next timer fires. The timer subsystem only
needs `clock_gettime(CLOCK_MONOTONIC)` for the current time.

`timerfd` is only used if the application explicitly creates timerfd
descriptors (e.g., Bun's event loop does this).

### Current Akuma state

- `clock_gettime` — **implemented** and functional
- `clock_getres` — **implemented**
- `timerfd_create/settime` — **implemented** with real timer state tracking
- `timerfd` read — **implemented** (returns expiration count)

### What needs to be built

The timerfd implementation needs to be wired into the epoll readiness checks.
When `epoll_pwait` iterates its interest list and encounters a `TimerFd`, it
should call `timerfd_read()` and report `EPOLLIN` if the timer has expired.
This is the same integration needed for all fd types in epoll.

### Estimated complexity

Already covered by the epoll implementation work.

---

## 8. Networking

**Priority: ALREADY MOSTLY WORKING**

### What libuv does

libuv creates sockets via `socket()`, sets them non-blocking, and registers
them with epoll. It uses `connect()`, `read()`/`write()`, `sendmsg()`/
`recvmsg()`, `getsockname()`, `getpeername()`, `setsockopt()`,
`getsockopt()`, and `shutdown()`.

### Current Akuma state

Most socket syscalls are implemented:
- `socket` (AF_INET, SOCK_STREAM, SOCK_DGRAM) — **implemented**
- `bind`, `listen`, `accept`, `connect` — **implemented**
- `sendto`, `recvfrom`, `sendmsg`, `recvmsg` — **implemented**
- `getsockname` — **implemented**
- `getsockopt`, `setsockopt` — **partial** (returns defaults)
- `shutdown` — **stub** (returns 0)
- `getpeername` — **missing**
- `socketpair` — **missing** (used for libuv's internal signaling pipes on
  some platforms, but libuv prefers `pipe2` on Linux)

### What needs to be built

1. **`getpeername`** (NR 205): return the remote address of a connected socket.
   ~20 lines using existing socket metadata.

2. **Non-blocking connect + `EPOLLOUT` notification**: libuv relies on
   `connect()` returning `EINPROGRESS` for non-blocking sockets, then waiting
   for `EPOLLOUT` on the epoll instance to know the connection completed. The
   `connect` syscall already returns `EINPROGRESS` for non-blocking sockets.
   The epoll implementation needs to check socket writable state for
   in-progress connections.

3. **`SO_ERROR` via `getsockopt`**: after an async connect completes, libuv
   reads `SO_ERROR` to check if the connection succeeded. Current
   `getsockopt` returns 0 for everything — needs to return real error state
   for `SO_ERROR`.

### Estimated complexity

`getpeername`: ~20 lines. `SO_ERROR`: ~30 lines. Connect+epoll integration
is part of the epoll work.

---

## 9. Proc Filesystem

**Priority: MEDIUM — many libuv info queries read from /proc**

### What libuv does

libuv reads various `/proc` files for system information:

| Path | Used by | Purpose |
|------|---------|---------|
| `/proc/self/cgroup` | `uv_get_constrained_memory()` | cgroup memory limits |
| `/proc/self/stat` | `uv_resident_set_memory()` | RSS in pages |
| `/proc/meminfo` | `uv_get_free_memory()` | available memory (fallback to sysinfo) |
| `/proc/cpuinfo` | `uv_cpu_info()` | CPU model names |
| `/proc/stat` | `uv_cpu_info()` | per-CPU usage counters |
| `/proc/loadavg` | `uv_loadavg()` | load averages (fallback to sysinfo) |
| `/proc/uptime` | `uv_uptime()` | system uptime (fallback to clock_gettime) |
| `/proc/version_signature` | `uv__kernel_version()` | kernel version detection |
| `/proc/self/exe` | general | executable path (already intercepted) |
| `/proc/self/fd/` | `uv__open_cloexec()` | fd introspection |

### Current Akuma state

- `/proc/self/exe` — **intercepted** in openat/readlinkat
- `/proc/self/cgroup`, `/proc/meminfo`, etc. — **not implemented**
- `sysinfo` syscall — **implemented** (libuv uses it as fallback)

### What needs to be built

For initial bring-up, most of these are **non-critical** because libuv has
fallback paths:
- Memory info falls back to `sysinfo()` — **already works**
- Uptime falls back to `clock_gettime(CLOCK_BOOTTIME)` — needs
  `CLOCK_BOOTTIME` support (can alias to `CLOCK_MONOTONIC`)
- Load average falls back to `sysinfo()` — **already works**
- CPU info: return failure, Node.js handles it gracefully

The one that matters is `/proc/version_signature` or `uname().release` for
kernel version detection. libuv parses the version to decide whether to use
io_uring features. Current `uname` returns "0.1.0" which parses as kernel
version 0.1.0 — this correctly disables all io_uring paths (they require
5.10+). No changes needed.

### Estimated complexity

Minimal for initial bring-up. Add `CLOCK_BOOTTIME` alias: 2 lines.

---

## 10. Miscellaneous Syscalls

### Already implemented (no changes needed)

| Syscall | NR | Status |
|---------|----|--------|
| `read` / `write` / `readv` / `writev` | 63/64/65/66 | Working |
| `openat` / `close` | 56/57 | Working |
| `fstat` / `newfstatat` | 80/79 | Working |
| `lseek` | 62 | Working |
| `mmap` / `munmap` / `mprotect` / `mremap` | 222/215/226/216 | Working |
| `brk` | 214 | Working |
| `pipe2` | 59 | Working |
| `dup` / `dup3` | 23/24 | Working |
| `fcntl` | 25 | Working |
| `getcwd` | 17 | Working |
| `getpid` / `getppid` / `gettid` | 172/173/178 | Working |
| `clone` / `clone3` | 220/435 | Working |
| `execve` | 221 | Working |
| `wait4` | 260 | Working |
| `nanosleep` | 101 | Working |
| `clock_gettime` | 113 | Working |
| `getrandom` | 278 | Working |
| `uname` | 160 | Working |
| `sysinfo` | 179 | Working |
| `prctl` | 167 | Stub (returns 0) |
| `eventfd2` | 19 | Working |
| `set_tid_address` | 96 | Working |
| `exit` / `exit_group` | 93/94 | Working |
| `prlimit64` | 261 | Working |
| `set_robust_list` | 99 | Stub (returns 0) |

### Missing but needed

| Syscall | NR (arm64) | Difficulty | Notes |
|---------|-----------|------------|-------|
| `dup2` | 1032 (via `dup3`) | Trivial | Wrap existing `dup3(old, new, 0)` |
| `getpeername` | 205 | Easy | Return remote addr from socket state |
| `socketpair` | 199 | Easy | Not strictly needed (libuv uses pipe2 on Linux) |
| `accept4` | 242 | Easy | accept + flags (SOCK_CLOEXEC, SOCK_NONBLOCK) |
| `sendfile` | 71 | Medium | Optional, libuv falls back to read+write |
| `statfs` / `fstatfs` | 43/44 | Easy | fstatfs already partially implemented |
| `truncate` / `ftruncate` | 45/46 | Easy | Needed for file ops |
| `copy_file_range` | 285 | Low | Falls back to read+write |

---

## Implementation Roadmap

### Phase 1: Functional Event Loop (core)

**Goal**: libuv's event loop initializes, polls, and dispatches I/O events.

| # | Task | Est. Lines | Depends On |
|---|------|-----------|------------|
| 1 | Epoll instance data structure + `epoll_create1` | 50 | — |
| 2 | `epoll_ctl` (ADD/MOD/DEL) | 80 | 1 |
| 3 | `epoll_pwait` with readiness checks for all fd types | 200 | 2 |
| 4 | Wire timerfd expiration into epoll readiness | 30 | 3 |
| 5 | Return `ENOSYS` for `io_uring_*` syscalls (if not already) | 5 | — |
| 6 | Return `ENOSYS` for `inotify_*` syscalls | 5 | — |

**Estimated total**: ~370 lines

### Phase 2: Thread Pool (async fs/dns)

**Goal**: libuv's 4-thread worker pool starts and processes work items.

| # | Task | Est. Lines | Depends On |
|---|------|-----------|------------|
| 7 | Thread park/unpark in scheduler | 100 | — |
| 8 | Real futex WAIT/WAKE with wait queues | 200 | 7 |
| 9 | Futex timeout support | 80 | 8 |

**Estimated total**: ~380 lines

### Phase 3: Signal Delivery (child processes)

**Goal**: `uv_spawn` works, SIGCHLD is delivered, clean shutdown on SIGTERM.

| # | Task | Est. Lines | Depends On |
|---|------|-----------|------------|
| 10 | sigaction storage (per-process signal table) | 100 | — |
| 11 | SIGCHLD generation on child exit | 50 | 10 |
| 12 | Signal delivery (kernel-assisted self-pipe) | 200 | 10, 1-3 |
| 13 | `dup2` syscall | 5 | — |
| 14 | `getpeername` syscall | 20 | — |

**Estimated total**: ~375 lines

### Phase 4: Polish

**Goal**: Node.js can actually start, load a script, and serve HTTP requests.

| # | Task | Est. Lines | Depends On |
|---|------|-----------|------------|
| 15 | `accept4` (accept + SOCK_CLOEXEC/NONBLOCK) | 15 | — |
| 16 | `SO_ERROR` in getsockopt | 30 | — |
| 17 | `CLOCK_BOOTTIME` alias | 5 | — |
| 18 | `/proc/self/fd` enumeration (for close_range) | 50 | — |

**Estimated total**: ~100 lines

---

## Total Estimated Effort

| Phase | Lines | Description |
|-------|-------|-------------|
| 1 | ~370 | Functional event loop |
| 2 | ~380 | Thread synchronization |
| 3 | ~375 | Signals + process management |
| 4 | ~100 | Polish and edge cases |
| **Total** | **~1,225** | **Full libuv support** |

The phases are partially parallelizable — Phase 2 (futex) and Phase 3
(signals) are independent of each other, though both depend on Phase 1
(epoll). Phase 1 is the clear first priority.

---

## What We Get for Free

Several things already work without changes:

- **Timer management**: libuv timers are userspace-only (min-heap + epoll timeout)
- **DNS resolution**: falls back to `getaddrinfo()` in thread pool → musl's resolver → existing `connect()`/`sendto()` to DNS server
- **File I/O**: libuv dispatches to thread pool → standard `openat`/`read`/`write` → existing VFS
- **Memory info**: `sysinfo()` fallback works
- **Kernel version detection**: returns "0.1.0" which correctly disables all bleeding-edge features
- **io_uring**: graceful fallback when setup returns ENOSYS
- **TTY handling**: libuv uses `ioctl(TIOCGWINSZ)` for terminal size — already stubbed

---

## Risk Assessment

| Risk | Impact | Mitigation |
|------|--------|------------|
| Epoll spin-waits burning CPU | High | Use smoltcp poll interval + yield pattern from ppoll |
| Futex deadlocks | High | Careful lock ordering; use interrupt-safe spinlocks |
| Signal delivery corrupts userspace state | High | Use kernel-assisted self-pipe (skip true signal delivery) |
| Thread pool exhausts 32-thread limit | Medium | libuv defaults to 4 workers; leave room for other threads |
| Node.js needs syscalls we haven't identified | Medium | Run with SYSCALL_DEBUG, fix missing NRs iteratively |

---

## Comparison: What Bun Needs in Addition

Bun's event loop uses the same epoll/timerfd/eventfd primitives but also:
- JavaScriptCore's JIT needs `mprotect(PROT_EXEC)` + cache maintenance (done)
- 128GB VA gigacage reservation (done)
- `CLONE_VM` thread support (done)
- Bun-specific syscalls like `membarrier` (stubbed)

The epoll and futex work benefits both runtimes. Completing Phase 1-2 of this
roadmap would unblock both Node.js and Bun's event loops simultaneously.
