# Syscalls Added for Bun Runtime Support

Bun (the JavaScript runtime) requires a broad set of Linux syscalls beyond what
Akuma originally implemented. This document catalogs every syscall added or
stubbed specifically to support bun, grouped by subsystem.

---

## Event Loop / I/O Multiplexing

Bun's event loop is built on µWebSockets/libuv which rely on epoll and timerfd.
These are now fully functional.

### `epoll_create1` (NR 20)

Creates an `EpollFd` file descriptor backed by a global `EPOLL_TABLE`. Each
epoll instance maintains an interest list of file descriptors to monitor.

### `epoll_ctl` (NR 21)

Fully implemented. Supports `EPOLL_CTL_ADD`, `EPOLL_CTL_MOD`, `EPOLL_CTL_DEL`.
Tracks requested events and user data for each fd in the interest list.

### `epoll_pwait` (NR 22)

Polls the network stack and checks fd readiness for sockets, eventfds, timerfds,
and pipes. Returns ready events with proper `EPOLLIN`/`EPOLLOUT` flags. Supports
timeouts and yields between poll iterations to avoid busy-spinning.

**Important:** On ARM64, `epoll_event` is 16 bytes (not 12 like on x86_64). The
struct has 4 bytes of padding between the `events` (u32) and `data` (u64) fields.

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

No-op stub (returns 0). Bun uses `tkill` for crash handling and assertion
failures. Full per-thread signal delivery is not implemented, but
process-level SIGSEGV delivery via `try_deliver_signal` handles the
critical JIT speculation failure path.

---

## File Descriptor Management

### `close_range` (NR 436)

Stub that returns 0. Bun calls this during startup to close inherited file
descriptors. A proper implementation would iterate and close fds in the
specified range.

### `ftruncate` (NR 46)

Truncates a file to the specified length. Implemented for ext2 — updates the
inode size fields. Only supports shrinking files (extending would require
block allocation). Used by bun when writing package files.

### `fchown` (NR 55)

Stub that returns 0. Ownership changes are ignored since Akuma doesn't track
file ownership.

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

### `/proc/self/fd/N` and `/proc/<pid>/fd/N` (via `readlinkat`)

Bun calls `readlinkat("/proc/self/fd/N")` to resolve file descriptor N to its
underlying file path. This is a standard Linux pattern for discovering what
file an fd refers to.

Procfs now implements `read_symlink()` and `is_symlink()` for these paths:
- `File` fd → actual file path (e.g., `/package.json`)
- `Socket` fd → `socket:[N]`
- `PipeRead`/`PipeWrite` fd → `pipe:[N]`
- `EpollFd` → `anon_inode:[eventpoll]`
- `TimerFd` → `anon_inode:[timerfd]`
- `EventFd` → `anon_inode:[eventfd]`
- `DevNull` → `/dev/null`
- `DevUrandom` → `/dev/urandom`
- `Stdin`/`Stdout`/`Stderr` → `/dev/stdin`, `/dev/stdout`, `/dev/stderr`

### `/dev/urandom` and `/dev/random`

Bun requires `/dev/urandom` for cryptographic randomization (JavaScriptCore's
`WTF::cryptographicallyRandomValues`). If the open fails, bun deliberately
crashes at `FAR=0xBBADBEEF`. Implemented as a virtual `DevUrandom` file
descriptor. See `docs/DEV_RANDOM.md`.

### `readlinkat` errno handling (NR 78)

`sys_readlinkat` now returns the correct errno:
- `ENOENT` (-2) when the path doesn't exist
- `EINVAL` (-22) when the path exists but is not a symlink

Previously returned `EINVAL` for all failures, which bun's Zig runtime
mapped to `error.NotLink` (fatal). The fix allows bun to gracefully
handle missing cache paths during `install` setup.

### `getdents64` symlink d_type (NR 61)

`sys_getdents64` now reports `DT_LNK` (10) for symlinks instead of
`DT_REG` (8). Added `is_symlink` field to the VFS `DirEntry` struct
and wired ext2's `FT_SYMLINK` file type through `read_dir`.

---

## Memory Management

### User stack size (config)

Increased `USER_STACK_SIZE` from 512 KB to 2 MB. Bun's initialization
(JSC setup, JIT compilation) uses ~596 KB of stack, which overflowed
the 512 KB limit. The access jumped 80 KB past the single 4 KB guard
page, so the guard never triggered — the fault address was simply
unmapped. Updated in `src/config.rs`, `userspace/libakuma/src/lib.rs`.

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

## File Allocation

### `fallocate` (NR 47)

Preallocates disk space for a file without writing data. Bun calls
`fallocate(fd, 0, 0, size)` before writing downloaded package files to
ensure contiguous block allocation and early ENOSPC detection.

Implemented with real ext2 block preallocation — iterates logical blocks
in the `[offset, offset+len)` range and calls `ensure_block()` for each,
which allocates physical blocks via the ext2 block bitmap. Updates
`i_size` if `offset + len` exceeds the current file size.

Only `mode == 0` (default preallocation) is supported. Other modes
(e.g. `FALLOC_FL_PUNCH_HOLE`, `FALLOC_FL_KEEP_SIZE`) return
`EOPNOTSUPP`.

### `renameat2` (NR 276)

Extended rename with flags. Bun calls `renameat2` with
`RENAME_NOREPLACE` (flags=0x1) to atomically move downloaded packages
into the install cache without overwriting existing entries.

Implemented with `RENAME_NOREPLACE` support: checks `vfs::exists()` on
the target path before calling `fs::rename()`, returning `EEXIST` if the
target already exists. `RENAME_EXCHANGE` (flags=0x2) is accepted and
delegated to plain rename. Other flag combinations return `EINVAL`.

---

## Socket Options

### `SO_LINGER` (SOL_SOCKET optname=13)

No-op stub (returns 0). Controls whether `close()` blocks until pending
data is sent. Akuma's TCP teardown is handled internally by smoltcp;
linger behavior has no effect on a local virtio-net link.

### `TCP_CORK` (IPPROTO_TCP optname=3)

No-op stub (returns 0). Holds small TCP segments and coalesces them into
full-sized frames before sending. The opposite of TCP_NODELAY. Bun/libuv
sets this around HTTP response writes. On a local virtio-net link, the
extra small packets from not corking have negligible impact.

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
the caller. Changed to a no-op stub (returns 0). Per-thread signal
delivery (targeting a specific TID) is not yet implemented; process-level
signal delivery handles the critical SIGSEGV path.

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

## Syscalls Returning ENOSYS

These syscalls return `ENOSYS` (-38) because they are not implemented.
Userspace applications should check for this error and fall back to
alternative methods.

### `io_uring_setup` / `io_uring_enter` / `io_uring_register` (NR 425, 426, 427)

Return `ENOSYS`. Bun probes io_uring support at startup. When io_uring is
not available, bun should fall back to epoll. Newer versions of bun
(post-December 2023) default to epoll anyway.

### Linux AIO syscalls (NR 0-4)

`io_setup` (NR 0) and `io_destroy` (NR 1) are **implemented** with a
kernel-side context table (`src/syscall/aio.rs`). `io_setup` allocates an
AIO context and writes its ID to the user pointer; `io_destroy` removes it.
`io_submit` (2), `io_cancel` (3), and `io_getevents` (4) still return
`ENOSYS` — no actual I/O submission is supported yet.

**Bug 1 (2026-03-14):** The initial `io_setup` implementation returned
`EEXIST` when `*ctx_idp` was non-zero, per the Linux man page requirement that
callers zero the pointer before calling `io_setup`. Bun does not guarantee this
— it passes whatever is in uninitialized/reused memory. This caused EEXIST
(-17 = `0xFFFFFFFFFFFFFFEF`) to be used as a pointer, crashing immediately with
a WILD-DA diagnostic:

```
[WILD-DA] *** FAR=0xffffffffffffffef is -17 (EEXIST) - syscall error used as pointer! ***
[T44.06] [WILD-DA] pid=53 FAR=0xffffffffffffffef ELR=0xcc8e5f3c last_sc=0
```

**Fix:** `sys_io_setup` now only returns `EEXIST` when the existing value in
`*ctx_idp` is a **live entry** in the `AIO_CONTEXTS` table. Garbage/uninitialized
values are silently overwritten.

**Bug 2 (2026-03-14):** The AIO context was stored as a small sequential integer
(1, 2, 3…) written to `*ctx_idp`. Linux's `aio_context_t` is actually the
**virtual address** of a kernel-mapped ring buffer (`struct aio_ring`). Bun
immediately dereferences the returned ctx as a pointer; writing `ctx=1` caused
a null dereference crash:

```
[io_setup] nr_events=4009873536 ctx=1
[DP] no lazy region for FAR=0x0 pid=63 (pid has 56 lazy regions)
[WILD-DA] pid=63 FAR=0x0 ELR=0xcc8f7750 last_sc=0
```

**Fix:** `sys_io_setup` now allocates a real page from the PMM, maps it into the
process's user address space (RW, no-exec), and writes a valid `struct aio_ring`
header into it:
- `magic = 0xa10a10a1` — tells glibc's io_getevents to use the ring directly
- `head = tail = 0` — ring is empty (no pending events)
- `nr = min(nr_events, 126)` — capped to fit in one 4 KB page

The ring's virtual address is written to `*ctx_idp` as the context value, matching
Linux behavior. Since `io_submit` still returns `ENOSYS`, no events are ever
enqueued; glibc's ring-polling path reads `head == tail` and returns 0 immediately
without making any syscall.

### `inotify_init1` / `inotify_add_watch` / `inotify_rm_watch` (NR 26, 27, 28)

Return `ENOSYS`. File watching is not implemented. Bun uses inotify for
watching file changes during development.

---

## ENOSYS Crash Pattern (Bun Bug)

**Important:** Bun versions have a bug where they use the ENOSYS return
value as a pointer without checking for errors. This causes a distinctive
crash:

```
[WILD-DA] *** FAR=0xFFFFFFFFFFFFFFDA is -38 (ENOSYS) - syscall error used as pointer! ***
[WILD-DA] pid=44 FAR=0xffffffffffffffda ELR=0x2d1a700 last_sc=0
```

The address `0xFFFFFFFFFFFFFFDA` is `-38` in 64-bit two's complement, which
is `ENOSYS`. The kernel now logs this pattern explicitly to aid debugging.

**Debugging steps when you see this crash:**

1. Check `last_sc` value - this is the most recent syscall number tracked
2. Look for syscalls returning ENOSYS just before the crash
3. The ELR value shows where in bun's code the crash occurred

**Common culprits:**
- `io_uring_setup` (syscall 425) - io_uring probe
- `pidfd_open` (syscall 434) - process fd creation

Note: `io_setup` (syscall 0) is now implemented and no longer returns ENOSYS,
so it is no longer a crash risk from this pattern.

The kernel includes tests for this pattern in `src/tests.rs`:
- `test_enosys_syscalls_return_proper_errno` - verifies all ENOSYS syscalls
- `test_enosys_is_negative_38` - documents the crash address pattern

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

## `bun run` Performance History

### Signal Delivery (RESOLVED)

The original `bun run` crash was **100% reproducible** with identical fault
address, instruction pointer, and register state on every run:

```
FAR=0x2346b2ad68  ELR=0x4416d74  ISS=0x45
x19=0x203ffbad58  x20=0x203ffbad68  SP_EL0=0x203ffbabc0
```

The root cause was the kernel's failure to deliver SIGSEGV to the process's
registered signal handler. JSC's JIT uses SIGSEGV handlers for speculation
failure recovery — speculative code intentionally triggers faults on bad
paths, and the signal handler redirects execution to a fallback.

**Fix:** Full signal delivery implemented via `try_deliver_signal` +
`do_rt_sigreturn` + lazy signal frame demand-paging. The kernel now sets
up an `rt_sigframe` on the user stack and redirects ELR to the registered
handler. `bun run` survives JIT speculation faults and executes JS correctly.

### VA Space Regression (RESOLVED)

An attempt to fix `bun install` by doubling `compute_stack_top` constants
(`MIN_MMAP_SPACE` 128GB→256GB, `MAX_STACK_TOP` 256GB→512GB) pushed the
user stack from ~130 GB to ~274 GB. This caused two problems:

1. **SIGSEGV on stack access** — The stack at L1[256] (~274 GB) suffered
   L3 translation faults at runtime. Signal delivery failed because the
   stack page was not in any lazy/mmap region (it was eagerly mapped by
   the ELF loader). Process killed with `FAR=0x403ff6bbb4`.
2. **~2x performance regression** — JSC's conservative GC scans the VA
   space from `stack_top` downward via mremap probes. Doubling the VA
   space doubled the number of mremap syscalls and increased page table
   cache pressure.

**Fix:** Reverted constants to 128GB/256GB. The original `bun install`
crash was actually caused by missing signal delivery, not insufficient VA
space.

### mremap GC Scanning

JSC's conservative GC probes the VA space via mremap:

```
old_sz=0x1000 → new_sz=0x2000, flags=0x0 (no MREMAP_MAYMOVE)
```

Addresses descend monotonically page-by-page from the top of the VA space
(~129 GB). The kernel now distinguishes mapped pages (returns ENOMEM) from
unmapped pages (returns EFAULT), which allows the GC to skip large
unmapped ranges more efficiently.

### Heap Optimization

Moving kernel thread stacks from the talc heap to PMM-backed contiguous
pages freed ~5 MB of heap, allowing the heap to shrink from 16 MB to 8 MB.
This gives 8 MB more physical RAM to userspace, reducing demand-paging
overhead during bun's initialization.

### Stack Overflow (RESOLVED)

`bun install express` crashed with SIGSEGV at a deterministic address
~596 KB below `stack_top`:

```
FAR=0x203ff6bbb4  SP_EL0=0x203ff6ba80  stack_top=0x20_4000_0000
```

The kernel's `USER_STACK_SIZE` was 512 KB, but bun's initialization
(JSC setup, JIT compilation) uses ~596 KB of stack. The access at
596 KB jumped 80 KB past the single 4 KB guard page, so the guard
page never triggered — the fault address was simply unmapped.

**Fix:** Increased `USER_STACK_SIZE` from 512 KB to 2 MB (matching
Linux's typical default of 8 MB, but conservatively sized since Akuma
eagerly maps all stack pages). Updated in `src/config.rs`,
`userspace/libakuma/src/lib.rs`, and tests.

### Symlink d_type in getdents64 (RESOLVED)

`sys_getdents64` reported all non-directory entries as `DT_REG=8`,
including symlinks. Bun checks `d_type` to identify symlinks in
`node_modules` and would fail if a symlink is reported as a regular
file.

**Fix:** Added `is_symlink` field to the VFS `DirEntry` struct, wired
ext2's `FT_SYMLINK` file type through `read_dir`, and updated
`sys_getdents64` to emit `DT_LNK=10` for symlinks.

### readlinkat ENOENT vs EINVAL (RESOLVED)

`bun install express` exited with `error: An internal error occurred
(NotLink)` during startup, before any directory listing.

Root cause: `sys_readlinkat` returned `EINVAL` for all non-symlink
paths, including paths that don't exist. On Linux, `readlinkat`
returns `ENOENT` for missing paths and `EINVAL` only when the path
exists but is not a symlink. Bun's Zig runtime maps `EINVAL` from
`readlinkat` to `error.NotLink` (fatal), but maps `ENOENT` to
`error.FileNotFound` (handled gracefully). Bun calls `readlinkat`
on cache paths during install setup; when the cache doesn't exist,
the wrong errno caused a hard failure.

**Fix:** `sys_readlinkat` now checks `vfs::exists()` and returns
`ENOENT` for missing paths, `EINVAL` for existing non-symlinks.

### /proc/self/fd/N symlinks (RESOLVED)

After the readlinkat fix, `bun install express` failed with
`error: An internal error occurred (NotDir)`.

Root cause: Bun calls `readlinkat("/proc/self/fd/6")` to resolve
fd 6 to its underlying file path. This is a standard Linux pattern —
`/proc/self/fd/N` entries are symlinks to the actual file paths.
Akuma's procfs didn't implement these symlinks; `read_symlink`
returned `NotFound` which mapped to `ENOENT`, then bun tried to
open the path as a directory and got `ENOTDIR`.

**Fix:** Implemented `read_symlink()` and `is_symlink()` in procfs
for `/proc/<pid>/fd/<n>` and `/proc/self/fd/<n>`. Returns the
actual path for File descriptors, or pseudo-paths like `socket:[N]`,
`pipe:[N]`, `anon_inode:[eventfd]` for other fd types.

### Current Performance

`bun run /public/cgi-bin/akuma.js` execution time progression:

| State | Time |
|-------|------|
| main branch (before signal delivery) | 2.6s (crashed) |
| After signal delivery + doubled VA space | 7.0s |
| + heap optimization (16→8 MB) | 4.6s |
| + VA space revert (256→128 GB) | **1.87s** |

The 1.87s result is faster than main because main crashed before
completing; the 2.6s figure was time-to-crash, not time-to-completion.

### Bugs Fixed During bun install Investigation

**Demand paging race:** `map_user_page()` ignored the CAS result, causing
page tracking corruption on preemption. Fixed in `crates/akuma-exec/src/mmu.rs`.

**Kernel heap exhaustion:** 40+ TCP sockets consumed 5MB+ of buffers.
Fixed by increasing heap to 16MB and reducing per-socket buffers to 32KB.

**Stub syscalls implemented:**
- `setsockopt` - TCP_NODELAY, SO_KEEPALIVE, SO_REUSEADDR, SO_LINGER, TCP_CORK, buffer sizes
- `rt_sigprocmask` - signal mask manipulation
- `sigaltstack` - alternate signal stack
- `prctl` - PR_SET_NAME, PR_GET_NAME, etc.
- `ftruncate` - file truncation for ext2
- `fallocate` - ext2 block preallocation
- `renameat2` - rename with RENAME_NOREPLACE

---

## Implementation Status

**`bun install express` works.** Typical install time is 3-10 seconds.

Fully implemented:
- **epoll** — Full I/O multiplexing for TCP/UDP sockets, eventfd, timerfd, pipes
- **timerfd** — Timer state tracking, expiration counting, read support
- **futex** — WAIT, WAKE, REQUEUE, CMP_REQUEUE with proper validation
- **DNS** — UDP sockets work with epoll for async DNS resolution
- **TCP** — Non-blocking connect with EINPROGRESS, smoltcp backend
- **Signal delivery** — rt_sigaction, rt_sigprocmask, sigaltstack, SIGSEGV handling
- **procfs** — `/proc/self/exe`, `/proc/self/fd/N` symlinks, `/proc/self/maps`
- **setsockopt** — TCP_NODELAY, SO_KEEPALIVE, SO_REUSEADDR, SO_LINGER, TCP_CORK, buffer sizes

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

## Bun Install Express (FIXED 2026-03-10)

`bun install express` now works. Packages download and install in ~5-10 seconds.

### Root Cause: `epoll_event` Struct Alignment on ARM64

The crash was caused by using the wrong `epoll_event` structure layout.

On **x86_64**, `epoll_event` is packed (12 bytes):
```c
struct epoll_event {
    uint32_t events;   // offset 0
    uint64_t data;     // offset 4
} __attribute__((packed));
```

On **ARM64**, `epoll_event` is NOT packed (16 bytes with natural alignment):
```c
struct epoll_event {
    uint32_t events;   // offset 0
    uint32_t _pad;     // offset 4 (padding)
    uint64_t data;     // offset 8
};
```

The kernel was using 12-byte packed layout. When writing events, the `data`
field went to offset 4. When bun (compiled for ARM64) read events, it expected
`data` at offset 8. The misaligned read produced garbage pointers like
`0x452524xxx`, causing SIGSEGV.

The Linux kernel header documents this:
```c
#ifdef __x86_64__
#define EPOLL_PACKED __attribute__((packed))
#else
#define EPOLL_PACKED
#endif
```

### Fix

In `src/syscall/poll.rs`:
- Changed `#[repr(C, packed)]` to `#[repr(C)]`
- Added `_pad: u32` field
- Updated size calculations from 12 to 16 bytes

### Remaining Work

1. **AF_UNIX sockets** — Returns EAFNOSUPPORT. Needed for bun subprocess IPC.
2. **process_vm_readv** (NR 270) — Returns ENOSYS. Used by crash handler only.
3. **Performance** — TCP buffer sizes and epoll polling could be optimized.

---

## Bun HTTP Server (FIXED 2026-03-10)

`bun run http.js` (Node.js `http.createServer`) now accepts connections and
responds to HTTP requests.

### Two Bugs Fixed

#### Bug 1: epoll never reported EPOLLIN for a listening TCP socket

**Root cause:** `socket_can_recv_tcp()` in `src/syscall/net.rs` only handled
`SocketType::Stream`, returning `false` for `SocketType::Listener`. So when bun
registered its listening socket with epoll (EPOLLIN), epoll_pwait never fired —
even after curl connected. Bun's event loop blocked in epoll_pwait indefinitely.

**Symptom:** Connection established (TCP handshake succeeded), request sent, no
response. `ps` showed bun threads running normally.

**Fix:** Extended `socket_can_recv_tcp()` with a `Listener` branch that scans
the backlog handles for any in `tcp::State::Established`. When one exists, the
listener reports EPOLLIN, waking bun to call `accept4`.

```rust
socket::SocketType::Listener { handles, .. } => {
    handles.iter().any(|&h| {
        akuma_net::smoltcp_net::with_network(|net| {
            net.sockets.get::<smoltcp::socket::tcp::Socket>(h).state()
                == smoltcp::socket::tcp::State::Established
        }).unwrap_or(false)
    })
}
```

---

#### Bug 2: `accept4` / `accept` blocked instead of returning EAGAIN

**Root cause:** `socket_accept()` in `crates/akuma-net/src/socket.rs` always
called `wait_until()` regardless of whether the listening socket was non-blocking.
libuv (bun's I/O layer) always sets listening sockets to non-blocking and calls
`accept4` in a loop until `EAGAIN`, draining all pending connections before
returning to the event loop. Since our `accept4` blocked on the second call
(after the first connection was accepted and the backlog was empty), bun's
event loop thread was stuck in `accept4` and never processed the accepted socket.

**Symptom:** Bun accepted the first connection (fd visible in epoll logs), then
silenced — no read from the accepted socket, no response.

**Fix:** `socket_accept()` now takes a `nonblock: bool` parameter. When
`nonblock` is true and no established connection is pending, it returns
`Err(EAGAIN)` immediately. Both `sys_accept` and `sys_accept4` now pass
`fd_is_nonblock(fd)` for the listening socket fd.

Added `has_pending_connection(idx)` as a shared helper:

```rust
fn has_pending_connection(idx: usize) -> bool {
    // scans backlog handles for any in Established state
}

pub fn socket_accept(idx: usize, nonblock: bool) -> Result<...> {
    if nonblock {
        if !has_pending_connection(idx) { return Err(libc_errno::EAGAIN); }
    } else {
        wait_until(|| has_pending_connection(idx), None)?;
    }
    // ... extract handle and create Stream socket
}
```

---

#### Bug 3: `accept4`/`accept` returned -1 (EPERM) instead of -EAGAIN

**Root cause:** When `socket_accept` returned `Err(EAGAIN)`, both `sys_accept`
and `sys_accept4` fell through to the generic `!0u64` return at the end of the
function, which is -1 = errno=EPERM. libuv treats EPERM from `accept4` as a fatal
error (not a "no more connections" signal), causing it to close the server and
eventually exit bun with code 0.

**Symptom:** bun handled the first few requests successfully, then got stuck (only
timer activity visible), then exited with code 0.

**Fix:** Both `sys_accept` and `sys_accept4` now match on `socket_accept`'s
`Result` and return `(-e as i64) as u64` for errors, properly encoding EAGAIN as
-11 so libuv breaks its accept loop normally.
```

#### Bug 5: EPOLLHUP not emitted for fully-closed TCP sockets, causing epoll spin (FIXED 2026-03-14)

**Symptom:** The HTTP Client thread consumed 70.4% CPU in the READY state during an
opencode session. It was calling `epoll_pwait` in a tight loop — never sleeping in
`schedule_blocking`. The PSTATS showed no accumulated `epoll_pwait` time despite
millions of iterations, because each call returned immediately without blocking.

**Root cause:** `socket_can_recv_tcp()` in `src/syscall/net.rs` contained the condition:
```rust
s.can_recv() || !s.is_active() || !s.may_recv()
```
The `!s.is_active()` branch fires for a fully-closed smoltcp TCP socket (state=Closed).
This made `epoll_check_fd_readiness` unconditionally report `EPOLLIN` for dead sockets,
so `epoll_pwait` never blocked. The correct Linux behavior for a fully-closed socket is
`EPOLLHUP`, not `EPOLLIN`.

**Fix:**

1. Changed `socket_can_recv_tcp()` condition to exclude dead sockets:
```rust
s.can_recv() || (s.is_active() && !s.may_recv())
```

2. Added `socket_is_dead_tcp()` helper that returns `true` when `!s.is_active()`.

3. In `epoll_check_fd_readiness()` (`src/syscall/poll.rs`), dead TCP sockets now emit
`EPOLLHUP` instead of `EPOLLIN`:
```rust
if super::net::socket_is_dead_tcp(idx) {
    ready |= EPOLLHUP;
} else {
    if requested & EPOLLIN != 0 && super::net::socket_can_recv_tcp(idx) { ready |= EPOLLIN; }
    if requested & EPOLLOUT != 0 && super::net::socket_can_send_tcp(idx) { ready |= EPOLLOUT; }
    if requested & EPOLLRDHUP != 0 && super::net::socket_peer_closed_tcp(idx) { ready |= EPOLLRDHUP; }
}
```

**Impact:** HTTP Client thread drops from 70.4% CPU to near 0% when idle, blocking
correctly in `schedule_blocking` between actual state changes.

---

#### Bug 4: EPOLLIN not reported after remote peer closes connection (FIXED 2026-03-11)

**Symptom:** After bun sends an HTTP response and the client closes the connection
(sends FIN), bun's event loop hung indefinitely. `socket_can_recv_tcp()` for a
`SocketType::Stream` socket checked `can_recv() || !is_active()`, but neither
condition is true in TCP `CloseWait` state (remote sent FIN, no more buffered data,
but socket is still "active"). So epoll never reported `EPOLLIN`, and libuv never
called `recv()` to get the EOF. Also, `EPOLLRDHUP` (0x2000) was silently ignored —
libuv registers this flag specifically to detect half-close events.

**Root cause files:**
- `src/syscall/net.rs` — `socket_can_recv_tcp()` missing `|| !s.may_recv()`
- `src/syscall/poll.rs` — `EPOLLRDHUP` not defined or handled in `epoll_check_fd_readiness`

**Fix:**

1. `socket_can_recv_tcp()` now includes `!s.may_recv()`:
```rust
socket::SocketType::Stream(h) => {
    akuma_net::smoltcp_net::with_network(|net| {
        let s = net.sockets.get::<smoltcp::socket::tcp::Socket>(*h);
        s.can_recv() || !s.is_active() || !s.may_recv()
    }).unwrap_or(false)
}
```

2. Added `EPOLLRDHUP` constant and `socket_peer_closed_tcp()` helper, wired into
`epoll_check_fd_readiness()`:
```rust
if requested & EPOLLRDHUP != 0 && super::net::socket_peer_closed_tcp(idx) {
    ready |= EPOLLRDHUP;
}
```

---

## Known Issues

### `bun install @google/gemini-cli` Hangs or Crashes (UNDER INVESTIGATION)

**Symptoms observed:**

1. **Out of Memory (OOM):** The most common crash with large packages.
   ```
   [DA-DP] pid=49 va=0x903f1918 anon alloc failed, 0 free pages
   [signal] sig 11 frame page 0x903f1000 not mappable
   [Fault] Process 50 (HTTP Client) SIGSEGV after 78.66s
   ```
   
   **Key indicator:** "0 free pages" means the PMM has exhausted all
   physical memory. Bun's worker threads crash one by one because:
   - They access lazy-mapped pages
   - Demand paging fails (no free pages)
   - Signal frame allocation also fails
   - Process is killed with SIGSEGV
   
   **Solution:** Run with more RAM. For `@google/gemini-cli`:
   ```bash
   MEMORY=2048M cargo run --release  # 2GB recommended
   ```
   
   Memory requirements:
   - Small packages (express): 256MB-512MB
   - Medium packages (typescript): 512MB-1GB
   - Large packages (@google/gemini-cli): 1GB-2GB+

2. **Hang during resolution:** Bun makes DNS queries (sendto to 10.0.2.3:53)
   then appears to hang. The process is running but no progress is made.
   This may be related to epoll/poll not waking properly for UDP socket
   responses from the DNS server.

3. **ENOSYS crash:** With certain syscall patterns:
   ```
   [WILD-DA] *** FAR=0xffffffffffffffda is -38 (ENOSYS) - syscall error used as pointer! ***
   panic: Segmentation fault at address 0xFFFFFFFFFFFFFFDA
   ```
   This is caused by bun not checking the return value of a syscall that
   returns ENOSYS and using it as a pointer. See "ENOSYS Crash Pattern" above.

4. **JIT cache coherency warnings:**
   ```
   [JIT] IC flush + replay #1 bogus nr=12297829382473034410 ELR=0x300183d8
   ```
   These indicate instruction cache issues with bun's JIT compiler. The
   kernel handles this by flushing the instruction cache and retrying.

**Debugging guidance:**

- Run with `MEMORY=2048M` or higher for complex package installations
- Check for "0 free pages" messages - indicates OOM
- Check for `[syscall] nr=... -> ENOSYS` messages before crashes
- The `last_sc` value in crash logs shows the most recent syscall tracked
- JIT bogus syscall numbers (> 500) trigger automatic IC flush/retry

**Memory layout with 2GB RAM:**
- Kernel code/stack: ~128MB
- Kernel heap: 16MB
- User pages: ~1.8GB (~460,000 pages)

**Potential areas to investigate:**

- OOM killer implementation (currently just kills faulting process)
- Memory pressure notifications to userspace
- UDP socket polling in epoll (DNS responses may not wake epoll correctly)
- JIT code execution after mprotect PROT_EXEC changes
