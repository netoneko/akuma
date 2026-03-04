# libuv Infrastructure Implementation

This document describes the kernel subsystems implemented to support libuv,
the event loop library underpinning Node.js (and Bun's fallback paths). Based
on the analysis in `proposals/LIBUV_INFRASTRUCTURE.md`.

## What Was Built

Four subsystems were implemented or upgraded:

1. **Real epoll** — the event loop backbone
2. **Real futex with wait queues** — thread synchronization
3. **Signal infrastructure** — sigaction storage + tkill delivery
4. **Networking polish** — getpeername, accept4, SO_ERROR

---

## 1. Epoll (Event Loop Backbone)

**Files:** `src/syscall.rs`, `src/process.rs`

### Data Structures

```
EPOLL_TABLE: Spinlock<BTreeMap<u32, EpollInstance>>   (global)

EpollInstance {
    interest_list: BTreeMap<u32, EpollEntry>          // fd -> entry
}

EpollEntry {
    events: u32,    // EPOLLIN, EPOLLOUT, etc.
    data: u64,      // opaque user data
}
```

`FileDescriptor::EpollFd` was changed from a unit variant to `EpollFd(u32)`
carrying the epoll instance ID into the global table.

### epoll_create1 (NR 20)

Allocates an `EpollInstance` in `EPOLL_TABLE` with a monotonic ID, creates
an `EpollFd(id)` file descriptor in the process's fd table.

### epoll_ctl (NR 21)

Real ADD/MOD/DEL operations on the interest list:

- **EPOLL_CTL_ADD (1):** Reads the 12-byte `epoll_event` struct from
  userspace (using `read_unaligned` for the packed struct), inserts
  fd + events + data. Returns `EEXIST` if fd already registered.
- **EPOLL_CTL_MOD (2):** Updates events/data for an existing fd.
  Returns `ENOENT` if not found.
- **EPOLL_CTL_DEL (3):** Removes fd from interest list. Returns `ENOENT`
  if not found.

### epoll_pwait (NR 22)

Poll loop modeled on the existing `sys_ppoll`:

1. Calls `smoltcp_net::poll()` to drive the network stack
2. Snapshots the interest list (to avoid holding the lock during checks)
3. For each fd, resolves it via the process's fd table and checks readiness
4. If any fds are ready, fills the userspace `epoll_event` array and returns
5. If `timeout == 0`, returns 0 immediately (non-blocking poll)
6. If `timeout == -1`, loops indefinitely until something is ready
7. For positive timeouts, respects the deadline in milliseconds
8. Checks for process interruption (Ctrl+C) each iteration
9. Yields between iterations to avoid CPU spin

### Readiness Checks by FD Type

The `epoll_check_fd_readiness` helper dispatches on `FileDescriptor` variant:

| FD Type | EPOLLIN check | EPOLLOUT check |
|---------|---------------|----------------|
| Socket (TCP) | `socket_can_recv_tcp()` | `socket_can_send_tcp()` |
| Socket (UDP) | `udp_can_recv()` | `udp_can_send()` |
| EventFd | `eventfd_can_read()` (counter > 0) | Always ready |
| PipeRead | `pipe_can_read()` (buffer non-empty or writers closed) | N/A |
| PipeWrite | N/A | `pipe_can_write()` (readers still open) |
| TimerFd | `timerfd_can_read()` (expired) | N/A |
| Stdin | Channel has stdin data | N/A |
| Stdout/Stderr | N/A | Always ready |
| Other | Always ready | Always ready |

### New Readiness Helpers

Three helpers were added since ppoll didn't cover all fd types:

- **`pipe_can_read(id)`** — `buffer.len() > 0 || write_count == 0`
- **`pipe_can_write(id)`** — `read_count > 0`
- **`timerfd_can_read(timer_id)`** — mirrors `timerfd_read` logic without
  mutating state: checks if elapsed time since arming exceeds the initial
  interval and there are unconsumed expirations

---

## 2. Futex (Thread Synchronization)

**File:** `src/syscall.rs`

Replaced the stub futex (which just yielded once) with a real wait-queue
implementation.

### Data Structure

```
FUTEX_WAITERS: Spinlock<BTreeMap<usize, Vec<usize>>>
                         ^addr            ^thread_ids
```

Keyed by the virtual address of the futex word. For `CLONE_VM` threads
sharing an address space, the same VA maps to the same physical page,
so the VA works directly as the key.

### FUTEX_WAIT / FUTEX_WAIT_BITSET

1. Atomically checks `*uaddr == val` (returns `EAGAIN` if not equal)
2. Adds current thread ID to the wait queue for `uaddr`
3. Computes deadline from timeout (relative for WAIT, absolute for
   WAIT_BITSET)
4. Parks the thread via `schedule_blocking(deadline)` — the existing
   scheduler infrastructure handles timed wakeups
5. On wakeup, removes self from the wait queue
6. Returns `ETIMEDOUT` if woken by deadline rather than explicit wake

### FUTEX_WAKE / FUTEX_WAKE_BITSET

1. Removes up to `val` threads from the wait queue for `uaddr`
2. Wakes each via `get_waker_for_thread(tid).wake()` (marks READY,
   triggers scheduler SGI)
3. Returns the number of threads actually woken

### Public API

`futex_wake(uaddr, max_wake)` is exported for use by `CLONE_CHILD_CLEARTID`
cleanup in `process.rs`.

---

## 3. Signal Infrastructure

**Files:** `src/syscall.rs`, `src/process.rs`

### Per-Process Signal Action Table

Added to `Process` struct:

```rust
pub signal_actions: [SignalAction; 64]
```

Where:

```rust
pub enum SignalHandler { Default, Ignore, UserFn(usize) }

pub struct SignalAction {
    pub handler: SignalHandler,
    pub flags: u64,
    pub mask: u64,
    pub restorer: usize,
}
```

Initialized to all-Default in all four Process construction paths (create,
create from disk, fork, clone_vm). Inherited from parent on fork/clone.

### rt_sigaction (NR 134)

Reads/writes the Linux `struct sigaction` (32 bytes: handler, flags,
restorer, mask) from/to userspace. Stores in the per-process signal action
table. Returns previous action when `oldact` is non-NULL. Rejects
SIGKILL (9) and SIGSTOP (19) with `EINVAL`.

### tkill (NR 130)

Delivers signals based on the registered handler:

- **SIG_IGN:** Returns 0 (signal ignored)
- **SIG_DFL:** For fatal signals (SIGHUP, SIGINT, SIGQUIT, SIGABRT,
  SIGSEGV, SIGTERM, etc.), terminates the process via `sys_exit_group`
  with a negative signal exit code. Non-fatal signals are no-ops.
- **UserFn:** True userspace signal delivery (stack frame construction,
  sigreturn) is not yet implemented. For SIGABRT specifically (the
  libuv abort pattern), terminates cleanly. Other signals are no-ops.
- **SIGKILL (9):** Always fatal, ignores handler.

This prevents the crash chain where tkill(SIGABRT) was a no-op, causing
the process to fall through to a null pointer dereference when trying to
call the signal handler at address 0x0.

---

## 4. Networking Polish

### getpeername (NR 205)

Returns the remote address of a connected socket:

- **TCP:** Queries `socket.remote_endpoint()` from smoltcp
- **UDP:** Returns the default peer set by `connect()`
- Returns `ENOTCONN` if not connected

### accept4 (NR 242)

Like `accept` but with flags:

- `SOCK_CLOEXEC` (0x80000): marks the new fd close-on-exec
- `SOCK_NONBLOCK` (0x800): marks the new fd non-blocking

### getsockopt — SO_ERROR

Enhanced from a stub that returned 0 for everything. Now handles:

- **SO_ERROR (4):** Returns real connection error state for TCP sockets
  (0 if active, `ECONNREFUSED` if not)
- **SO_TYPE (3):** Returns 1 (SOCK_STREAM) or 2 (SOCK_DGRAM)
- **SO_SNDBUF/SO_RCVBUF:** Returns 128KB
- **SO_KEEPALIVE:** Returns 0 (disabled)

---

## 5. Graceful Degradation Stubs

### io_uring (NR 425-427)

`io_uring_setup`, `io_uring_enter`, `io_uring_register` all return `ENOSYS`.
libuv probes for io_uring at init and gracefully falls back to its thread
pool for filesystem operations.

### inotify (NR 26-28)

`inotify_init1`, `inotify_add_watch`, `inotify_rm_watch` all return `ENOSYS`.
libuv reports `UV_ENOSYS` from `uv_fs_event_start()`, and Node.js falls back
to polling-based file watching.

---

## What Remains

### Not Yet Implemented

- **True userspace signal delivery** — constructing a signal frame on the
  userspace stack, diverting execution to the handler, and returning via
  `sigreturn`. Currently only SIG_IGN, SIG_DFL, and the SIGABRT abort
  pattern are handled.
- **SIGCHLD generation** — when a child process exits, the parent should
  receive SIGCHLD. Required for `uv_spawn` child process management.
- **Kernel-assisted self-pipe** — libuv's signal loop expects the kernel
  to interrupt `epoll_pwait` with `EINTR` when a signal is pending. This
  requires integrating signal pending bits with the epoll poll loop.

### Already Working (No Changes Needed)

- `clock_gettime(CLOCK_BOOTTIME)` — already maps to monotonic via the
  catch-all arm in `sys_clock_gettime`
- `close_range` — already stubbed (returns 0)
- Timer management — libuv timers are userspace-only (min-heap + epoll timeout)
- DNS resolution — falls back to thread pool → musl resolver → existing sockets
- File I/O — thread pool → existing VFS syscalls
- io_uring — graceful fallback when setup returns ENOSYS

---

## 6. V8 Heap Cage Crash Fix

**Files:** `src/syscall.rs`, `src/process.rs`

### Root Cause

V8's pointer-compression heap cage size is derived from `sysinfo.totalram`.
With 256MB reported, V8 made two PROT_NONE reservations:

| Region | Requested | After alignment trim | Usable |
|--------|-----------|---------------------|--------|
| A (code/old-space) | 256MB + 60KB | `0x50000000-0x60000000` | 256MB |
| B (new-space/large) | 128MB + 60KB | `0x60010000-0x68010000` | 128MB |
| **Total** | ~384MB | | **384MB** |

V8's internal metadata needed ~384.75MB — the access at `FAR=0x680c0000`
was 704KB past the end of region B (`0x68010000`). A translation fault
killed the process.

### Fix: Report 1GB in sysinfo

Changed `sys_sysinfo` to report `totalram = 1GB` instead of 256MB. This
makes V8 reserve a proportionally larger heap cage with ample headroom.
Since the reservations are `PROT_NONE` (lazy, demand-paged), no extra
physical memory is consumed — only virtual address space, of which the
kernel provides ~131GB per process.

### Additional Fixes

**Removed unsafe mmap hint handling.** Non-fixed mmaps with a non-zero
`addr` hint were being honored blindly without checking for overlap with
existing lazy regions or eager allocations. On real Linux, the kernel
validates hints against its VMA tree and falls back to the regular
allocator on conflict. The hint code was removed; non-fixed mmaps now
always go through the bump allocator, which guarantees conflict-free
sequential addresses.

**`munmap_lazy_regions_in_range`.** Replaced the old single-region
`munmap_lazy_region` with a loop that processes all overlapping lazy
regions when an unmap range spans multiple regions. V8/musl frequently
does large unmap calls that cross region boundaries.

**`munmap_lazy_region_overlapping` suffix fix.** The old suffix-removal
path returned the *requested* page count rather than the *actual* pages
freed from the lazy region, causing over-unmapping of adjacent memory.

**VA recycling in `sys_munmap`.** Freed lazy-region VA ranges are now
pushed to `proc.memory.free_regions` so `alloc_mmap` can reuse them
instead of advancing the bump pointer past the gap.

**`MAP_FIXED` cleanup.** `sys_mmap` with `MAP_FIXED` now calls
`munmap_lazy_regions_in_range` to remove overlapping lazy regions before
placing the new mapping, matching Linux semantics.

**`MAP_FIXED_NOREPLACE` (0x100000).** Recognized as a distinct flag that
places the mapping at the exact address without overwriting existing
mappings (unlike `MAP_FIXED` which overwrites).

---

## 7. mprotect Eager Allocation Fix

**File:** `src/syscall.rs`

### Root Cause

The V8 heap cage crash persisted after the sysinfo fix (Section 6)
because `sys_mprotect` was **eagerly allocating a physical page for every
unmapped page** in the requested range. V8's startup pattern:

1. `mmap(PROT_NONE, 128MB)` — reserves 32,768 pages of VA (lazy, no
   physical pages)
2. `mprotect(base, 128MB, PROT_RW)` — "commits" the region

On Linux, step 2 only updates VMA permission flags. No physical pages
are allocated; they are demand-paged on first access. On Akuma, step 2
iterated over all 32,768 pages and called `alloc_page_zeroed()` for each
unmapped one. On a 256MB system with ~50MB of free physical pages, OOM
occurred around page 12,000. The remaining pages were silently skipped
(`alloc_page_zeroed()` returned `None`), but `mprotect` still returned 0
(success). V8 assumed all pages were committed and later crashed
accessing an unallocated page at `FAR=0x680c0000`.

### Timeline from kernel logs

```
[T12.37] mmap 128MB PROT_NONE reservation → 0x60010000-0x68010000
[T12.37] mprotect commits region → eagerly allocates ~12K pages, OOM skips rest
[T12.43] V8 continues initialization (epoll, eventfd, threads)
[T75.23] V8 accesses 0x680c0000 (page 32,944 from base) → translation fault
         [DP] no lazy region for FAR=0x680c0000 → process killed with SIGSEGV
```

### Fix

Changed `mprotect` to only update flags for already-mapped pages:

```rust
// Before (eager allocation — wrong):
if update_page_flags(va, flags).is_err() || !is_mapped(va) {
    if prot != 0 {
        if let Some(frame) = alloc_page_zeroed() {  // OOM here → silent skip
            map_user_page(va, frame.addr, flags);
        }
    }
}

// After (demand-paging — matches Linux):
if is_mapped(va) {
    update_page_flags(va, flags);
}
// Unmapped pages left alone — demand paging allocates on first access
```

This matches Linux semantics: `mprotect` is a metadata operation on the
VMA, not a physical page allocator. Physical pages are allocated one at a
time by the demand paging fault handler when V8 actually touches each
page, spreading memory cost across V8's real access pattern instead of
pre-allocating everything at once.
