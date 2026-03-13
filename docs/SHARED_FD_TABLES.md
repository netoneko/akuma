# Shared File Descriptor Tables (CLONE_FILES)

## Problem

Linux `clone()` with `CLONE_VM | CLONE_FILES` — the standard flags used by
`pthread_create` via musl — **shares** the file descriptor table between parent
and child threads. Akuma previously **copied** it, creating a per-thread
snapshot. This caused three classes of bugs:

1. **Cross-thread fd invisibility.** When the main thread opened new sockets
   after spawning worker threads, the workers could not see them.  Bun's event
   loop depends on worker threads polling fds created by the main thread —
   causing `bun install` to hang during the download phase.

2. **Epoll EEXIST errors.** Epoll instances live in a global `EPOLL_TABLE`
   keyed by id.  After clone, both parent and child held `EpollFd(id)` entries
   pointing to the *same* instance.  If either thread called
   `epoll_ctl(EPOLL_CTL_ADD)` for an fd the other had already registered, the
   kernel correctly returned `EEXIST` — but the application did not expect it
   because on Linux there would be only one fd table entry.

3. **Stale fd references.** If the parent closed fd 5 and opened something new
   as fd 5, the child's copy still referred to the old socket.  Epoll readiness
   checks (`epoll_check_fd_readiness`) use `current_process().get_fd(fd_num)`,
   so the wrong thread's table could produce wrong readiness results.

## Design

### SharedFdTable

A new struct in `crates/akuma-exec/src/process/mod.rs` bundles all fd-related
state:

```rust
pub struct SharedFdTable {
    pub table: Spinlock<BTreeMap<u32, FileDescriptor>>,
    pub cloexec: Spinlock<BTreeSet<u32>>,
    pub nonblock: Spinlock<BTreeSet<u32>>,
    pub next_fd: AtomicU32,
}
```

The `Process` struct holds it via `Arc`:

```rust
pub fds: Arc<SharedFdTable>,
```

### clone_thread (CLONE_VM) — shared

```rust
fds: parent.fds.clone(),  // Arc::clone — same table
```

No pipe reference counts are bumped because there is still only one fd table
entry for each pipe fd.  All threads see the same fds, the same cloexec set,
and the same nonblock set.  `next_fd` is an `AtomicU32` inside the shared
struct, so concurrent `alloc_fd` calls from different threads allocate unique fd
numbers without races.

### fork_process — deep copy

```rust
fds: Arc::new(parent.fds.clone_deep_for_fork()),
```

`clone_deep_for_fork` creates a separate `BTreeMap` with cloned entries, bumps
pipe reference counts, and strips `EpollFd` entries (epoll instances are not
reference-counted, so the child must not destroy the parent's instance on
close).

### Cleanup semantics

`cleanup_process_fds` checks `Arc::strong_count(&proc.fds)`.  If other threads
still reference the shared table (count > 1), the exiting thread skips fd
cleanup entirely — the table stays alive for the remaining threads.  Only when
the last thread exits (count == 1) are sockets closed, pipes decremented, and
the table cleared.

This matches Linux behavior: the shared fd table is destroyed when its last
reference is dropped.

## Impact on Subsystems

### Epoll

With a shared fd table, all CLONE_VM threads that inherit the same epoll fd
operate on the same `EpollInstance` **and** see the same fd mappings.  A thread
adding fd 5 to epoll and another thread calling `epoll_wait` will both resolve
fd 5 to the same `FileDescriptor` entry.

An additional compatibility fix makes `EPOLL_CTL_ADD` idempotent: if the fd is
already in the interest list, the kernel silently updates it (MOD semantics)
instead of returning `EEXIST`.  This handles same-thread re-registration
patterns used by Bun's event loop initialization.

### Pipes

Pipe reference counts track the number of distinct fd table entries referencing
a pipe, not the number of threads.  With `clone_thread` sharing the table, a
pipe fd appears once — so its reference count stays at 1 regardless of how many
threads exist.  With `fork_process`, the deep copy creates a second entry, so
the reference count is bumped.

### Sockets

Socket indices in the global socket table are referenced by fd table entries.
Shared tables mean all threads see socket opens/closes immediately.  When one
thread calls `close(fd)`, the socket is removed from the shared table and all
other threads' subsequent `get_fd(fd)` calls return `None`.

### TimerFd / EventFd

These work identically to sockets — shared visibility, single cleanup on last
thread exit.

## Relationship to Bun and Opencode Fixes

| Issue | Root cause | Fix |
|-------|-----------|-----|
| `bun install` hangs after resolution | Worker threads can't see sockets opened by main thread | Shared fd table |
| `opencode` EEXIST crash | Epoll ADD on fd already registered by same/other thread | Idempotent ADD + shared table |
| `process.stdout.columns` undefined | WriteStream constructor fails due to EEXIST, leaving stdout undefined | Idempotent ADD |

## ioctl FIONBIO / FIONREAD Support

The shared fd table also required proper `ioctl` support for non-terminal
file descriptors.  Previously, all ioctls on fd > 2 returned `ENOTTY`,
which caused Bun to crash when setting sockets to non-blocking mode.

### FIONBIO (0x5421) — Set/clear non-blocking

Reads a 4-byte int from the user pointer.  If non-zero, marks the fd
non-blocking in the process fd table (`proc.set_nonblock(fd)`); if zero,
clears it.  All read/write syscall paths already check `fd_is_nonblock()`
from the process-level set, so no additional propagation is needed.

### FIONREAD (0x541B) — Bytes available for read

Returns the actual byte count based on fd type:

| FD type   | Source                          |
|-----------|---------------------------------|
| PipeRead  | `pipe_bytes_available(id)` — pipe buffer length |
| Socket    | `smoltcp recv_queue()` — TCP/UDP receive buffer |
| EventFd   | 8 if counter > 0, else 0        |
| TimerFd   | 8 if timer expired, else 0      |
| Stdin     | `channel.stdin_bytes_available()` |
| File      | `file_size - position`          |
| Other     | 0                               |

### FIOCLEX / FIONCLEX (0x5451 / 0x5450)

Set or clear the close-on-exec flag for any fd.  These are handled before
the `fd > 2` ENOTTY guard for terminal-specific ioctls.

## Files Changed

- `crates/akuma-exec/src/process/mod.rs` — `SharedFdTable` struct, Process
  field replacement, method redirects, clone_thread/fork_process/cleanup updates,
  `stdin_bytes_available()` on ProcessChannel
- `src/syscall/fs.rs` — `sys_close_range` updated to use `proc.fds.table`
- `src/syscall/poll.rs` — `EPOLL_CTL_ADD` made idempotent
- `src/syscall/term.rs` — FIONBIO/FIONREAD/FIOCLEX/FIONCLEX on any fd
- `src/syscall/pipe.rs` — `pipe_bytes_available()` helper
- `src/syscall/net.rs` — `socket_recv_queue_size()` helper
- `src/tests.rs` — Test process uses `SharedFdTable::new()`, pipe/fd table tests
