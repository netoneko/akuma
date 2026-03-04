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

## 6. Memory Management Fixes for V8/Node.js

**Files:** `src/syscall.rs`, `src/process.rs`, `src/exceptions.rs`

Three interconnected bugs caused V8/Node.js crashes. All stem from the
kernel's handling of PROT_NONE lazy regions and the mismatch between
Akuma's eager allocation model and Linux's lazy demand-paging model.

### Bug 1: mprotect Eager Allocation Exhausting Physical Memory

`sys_mprotect` was **eagerly allocating a physical page for every
unmapped page** in the requested range. V8/musl's pattern:

1. `mmap(PROT_NONE, 128MB)` — reserves 32,768 pages of VA (lazy, no
   physical pages)
2. `mprotect(base, 128MB, PROT_RW)` — "commits" the region

On Linux, step 2 only updates VMA permission flags. No physical pages
are allocated; they are demand-paged on first access. On Akuma, step 2
iterated over all 32,768 pages and called `alloc_page_zeroed()` for each
unmapped one. On a 256MB system with ~50MB of free physical pages, OOM
occurred around page 12,000. The remaining pages were silently skipped,
but `mprotect` returned 0 (success). This had two consequences:

- **Starved subsequent allocations**: V8's later eager mmaps (for
  metadata, code objects, etc.) failed due to OOM, leaving gaps in the
  address space that V8 later crashed accessing (`FAR=0x680c0000`).
- **Wrong page permissions**: Pages that *were* allocated by mprotect
  got correct RW flags, but pages demand-paged *after* OOM got the
  wrong flags (see Bug 2).

**Fix:** `mprotect` now only updates flags for already-mapped pages.
Unmapped pages are left alone — demand paging allocates them on first
access:

```rust
// Before (eager — exhausts physical memory):
if update_page_flags(va, flags).is_err() || !is_mapped(va) {
    if prot != 0 {
        if let Some(frame) = alloc_page_zeroed() { ... }
    }
}

// After (matches Linux semantics):
if is_mapped(va) {
    update_page_flags(va, flags);
}
```

### Bug 2: Demand Paging Mapped Anonymous Pages Read-Only

`from_prot(0)` (PROT_NONE) returned `RO` (read-only), not zero. The
demand paging handler used these stored flags:

```rust
let map_flags = if flags != 0 { flags } else { RW_NO_EXEC };
```

Since `RO != 0`, the fallback to `RW_NO_EXEC` never triggered. Pages in
PROT_NONE lazy regions were demand-paged as **read-only**. When musl's
thread setup wrote to the stack (which was mmap'd PROT_NONE then
mprotect'd to PROT_RW), the write hit a read-only page.

**Fix:** Anonymous demand paging (`LazySource::Zero`) now always uses
`RW_NO_EXEC`, regardless of stored flags. File-backed segments (ELF
code/data) keep their stored flags to preserve correct permissions (RX
for code, RW for data):

```rust
let map_flags = match source {
    LazySource::File { .. } => if flags != 0 { flags } else { RW_NO_EXEC },
    _ => RW_NO_EXEC,  // anonymous always RW
};
```

### Bug 3: Exception Handler Ignored Permission Faults

The demand paging handler only checked for **translation faults**
(DFSC 0x04, 0x08 — page not present). **Permission faults** (DFSC 0x0C
— page present but wrong access rights) were not handled:

```
[Fault] Data abort from EL0 at FAR=0x31a47be8, ISS=0x4f
```

ISS=0x4f → DFSC=0x0F → permission fault level 3. The page existed in
the page table (mapped read-only by Bug 2) but the write was denied.
This crash affected both Node.js and Bun — both use musl's thread
creation which does `mmap(PROT_NONE)` + `mprotect(PROT_RW)` + clone.

**Fix:** Added permission fault handling in both the data abort and
instruction abort handlers. When a permission fault occurs on a page
within a lazy region, the handler upgrades permissions to `RW_NO_EXEC`
(data) or `RX` (instruction) via `update_page_flags()`, which includes
TLB invalidation:

```rust
if is_permission_fault {
    if let Some(_) = lazy_region_lookup(far) {
        proc.address_space.update_page_flags(page_va, RW_NO_EXEC);
        return;  // retry the faulting instruction
    }
}
```

### Additional Memory Fixes

**Removed unsafe mmap hint handling.** Non-fixed mmaps with a non-zero
`addr` hint were being honored blindly without checking for overlap with
existing lazy regions. The hint code was removed; non-fixed mmaps now
always go through the bump allocator.

**`munmap_lazy_regions_in_range`.** Replaced the old single-region
`munmap_lazy_region` with a loop that processes all overlapping lazy
regions when an unmap range spans multiple regions.

**`munmap_lazy_region_overlapping` suffix fix.** The old suffix-removal
path returned the *requested* page count rather than the *actual* pages
freed from the lazy region.

**VA recycling in `sys_munmap`.** Freed lazy-region VA ranges are now
pushed to `proc.memory.free_regions` for reuse.

**`MAP_FIXED` cleanup.** `sys_mmap` with `MAP_FIXED` now calls
`munmap_lazy_regions_in_range` to remove overlapping lazy regions.

**`MAP_FIXED_NOREPLACE` (0x100000).** Recognized as a distinct flag.

### Bug 4: Kernel-Side Pointer Validation Rejected Lazy Pages

**File:** `src/syscall.rs`, `src/mmu.rs`

`validate_user_ptr` checked `is_current_user_range_mapped` to ensure
userspace pointers were backed by page table entries. With the mprotect
fix (Bug 1), pages in lazy regions were no longer pre-allocated. When
a syscall like `epoll_pwait` validated its output buffer — which might
reside on a thread stack page not yet demand-paged — the check failed
and the syscall returned `EFAULT`.

libuv's event loop hit this immediately:

```
Assertion failed: errno == EINTR (../../deps/uv/src/unix/linux.c: uv__io_poll: 1474)
```

`epoll_pwait` returned -1 with errno=EFAULT (not EINTR). libuv aborted.

**Fix:** Added `ensure_user_pages_mapped` — when a page in the
requested range is not mapped, the function checks if it falls within
a lazy region and demand-pages it from kernel context before proceeding.
This handles both anonymous (zero-fill) and file-backed lazy regions,
reusing the same demand paging logic as the exception handler. If a page
is neither mapped nor in a lazy region, validation still fails with
`EFAULT`.

### Bug 5: `sys_munmap` Blindly Unmapped Eagerly-Mapped Pages

**File:** `src/syscall.rs`

V8/Node.js allocates memory via `mmap` and later calls `munmap` on
sub-ranges within those allocations (e.g., to punch holes or trim
regions). The kernel tracked eager mmap regions by their start address,
so `sys_munmap` only matched exact start addresses. When V8 unmapped a
sub-range (e.g., `munmap(0x680c0000, 0x1000)` within an allocation
starting at `0x6809d000`), no eager region matched.

The code had two paths that blindly unmapped pages without checking
whether they belonged to tracked eager allocations:

1. **"Gaps" cleanup** — after processing lazy region unmaps, a second
   loop called `unmap_page` for ALL pages in the munmap range, including
   pages from adjacent eager allocations.

2. **"Partial unmap" fallback** — when neither eager (by start address)
   nor lazy regions matched, the code still unmapped the requested pages.

Both paths could destroy PTEs for pages that were part of eagerly-mapped
regions. V8 would then access a page it believed was mapped, trigger a
translation fault, and crash with SIGSEGV.

**Fix:** Removed both blind-unmap paths. When no tracked region matches,
`sys_munmap` returns 0 (success) without touching the page table — matching
Linux behavior where `munmap` on a range not backed by any VMA is a no-op.

### Bug 6: Exception Handler Had No Fallback for Eager Mmap Regions

**File:** `src/exceptions.rs`

When a translation fault occurs from EL0, the demand paging handler only
checked lazy regions. If a page was part of an eager mmap allocation but
its PTE was missing (due to any cause — race condition, table corruption,
or the munmap bug above), the handler had no way to recover. It logged
"no lazy region" and killed the process with SIGSEGV.

On real Linux, the kernel maintains a unified VMA (Virtual Memory Area)
list that covers both lazily and eagerly mapped regions. A page fault on
any valid VMA can be resolved. Our kernel lacked this for eager regions.

**Fix:** Added an eager mmap region fallback in the translation fault
handler. After the lazy region check fails, the handler iterates
`mmap_regions` to find if the faulting address is within a tracked eager
allocation. If found, it re-establishes the PTE using the original
physical frame (already allocated and tracked). A diagnostic log line
`[DP-eager]` is emitted when this path fires, providing visibility into
any underlying PTE-loss issues.

### Bug 7: Non-Atomic Page Table Entry Creation (Race Condition)

**File:** `src/mmu.rs`

The helper `get_or_create_table_raw` used a non-atomic read-check-write
sequence to create intermediate page table entries (L1→L2→L3 tables).
The syscall exception handler explicitly unmasks IRQs during syscall
handling (`msr daifclr, #2` in `sync_el0_handler`) to allow preemptive
scheduling. This made the following race possible:

1. Thread A (mmap syscall): reads L2[idx] → invalid
2. Thread A calls `alloc_page_zeroed()` (acquires PMM lock, zeros 4KB)
3. **Timer IRQ fires** — scheduler preempts Thread A
4. Thread B (same process, shared address space): page fault in same
   2MB range → exception handler calls `map_user_page`
5. Thread B reads L2[idx] → **still invalid** (Thread A hasn't written)
6. Thread B allocates L3_B, writes it to L2[idx], maps its PTE
7. Thread A resumes, allocates L3_A, **overwrites** L2[idx]
8. L3_B and all its PTEs are orphaned — those pages are now unmapped

This deterministically caused the `FAR=0x680c0000` crash: the 127-page
eager mmap at `0x6809d000` and a concurrent demand-paging fault competed
for the same L2 entry, and the loser's L3 table was silently destroyed.

**Fix (two layers):**

1. **IRQ guard:** `map_user_page`, `unmap_page`, and `update_page_flags`
   now wrap their page table walks in `IrqGuard` (RAII, saves/restores
   DAIF). This prevents preemption during the critical section. On a
   single-core system this eliminates the race entirely. Per-page IRQ-
   disabled time is ~1–5μs vs the 10ms timer — negligible latency impact.

2. **Atomic CAS (defense-in-depth):** `get_or_create_table_raw` was
   renamed to `get_or_create_table_atomic` and now uses
   `AtomicU64::compare_exchange` for the entry write. If two paths race
   (on a future multi-core configuration), the CAS loser frees its
   redundant allocation and retries using the winner's table. The leaf
   PTE write in `map_user_page` also uses CAS. This is the standard
   lockless page table insertion pattern used by Linux (`cmpxchg`).

### Kernel Tests

**File:** `src/tests.rs`

Added 10 mmap subsystem tests to `run_memory_tests()` to catch
regressions in the exact scenarios that caused the Node.js crashes:

- `alloc_mmap_non_overlapping` — multiple allocations return disjoint VA ranges
- `alloc_mmap_free_region_recycling` — freed VA ranges are reused; split remainders available
- `lazy_region_push_lookup` — region found inside, not found outside
- `lazy_region_munmap_full` — full removal leaves zero regions
- `lazy_region_munmap_prefix` — prefix trim adjusts start and size
- `lazy_region_munmap_suffix` — suffix trim adjusts size only
- `lazy_region_munmap_middle` — middle punch splits into two regions
- `lazy_region_munmap_multi` — range spanning two regions trims both
- `map_user_page_roundtrip` — map → is_mapped=true → clear PTE → is_mapped=false
- `eager_mmap_subrange_munmap` — sub-range munmap doesn't match any tracked region
