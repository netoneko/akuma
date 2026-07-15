# eventfd syscalls

`eventfd2` (NR 19). Source: `src/syscall/eventfd.rs`. Gated `sc-eventfd`
(Tier 2 — see [`../syscalls.md`](../syscalls.md) "Feature gates & ExecRuntime
stubs" for what a Tier 2 gate obligates when the feature is off).

> **Stability: B (watch).** Quiet since the Mar 2026 Go bring-up fix. The
> recurring lesson: **a shared kernel object survives fork; only fd-table
> teardown should drop its last reference** — `eventfd_close` must refcount,
> never unconditionally remove.

## State

A single global table, not one entry per fd:

```rust
static EVENTFDS: Spinlock<BTreeMap<u32, KernelEventFd>>
```

`KernelEventFd { counter: u64, flags: u32, pollers: BTreeSet<usize>, ref_count: u32 }`
(`src/syscall/eventfd.rs:4-9`). The process-side `FileDescriptor::EventFd(id)`
is just an index into this table — multiple fds (across processes, after
`fork`) can point at the same `id`.

## eventfd2

`sys_eventfd2` (`eventfd.rs:128`): allocates a table entry via
`eventfd_create(initval, flags)`, then `proc.alloc_fd(FileDescriptor::EventFd(id))`.
`EFD_CLOEXEC` (0x80000) and `EFD_NONBLOCK` (0x800) are applied as fd flags on
top of the table entry, mirroring Linux; `EFD_SEMAPHORE` (1) changes read
semantics (below).

## read / write semantics

Not separate syscalls — dispatched through `sys_read`/`sys_write` against
`FileDescriptor::EventFd`:

- **`eventfd_read`** (`eventfd.rs:31`): `EAGAIN` if `counter == 0`. Otherwise,
  with `EFD_SEMAPHORE`: decrement by 1, return 1. Without it: return the full
  counter and reset to 0 (standard eventfd semantics). Either way, every
  waiting poller is popped from `pollers` and woken.
- **`eventfd_write`** (`eventfd.rs:60`): `counter.saturating_add(val)`
  (no overflow trap — Linux would `EAGAIN` at `u64::MAX`, this implementation
  just saturates), then wakes all pollers.
- **`eventfd_can_read`** / **`eventfd_is_nonblock`**: used by the poll/epoll
  readiness path (`poll.rs`), not called directly by userspace.

## Reference counting across fork/exec

`eventfd_clone_ref` / `eventfd_close` (`eventfd.rs:95,107`) exist because a
plain "remove on close" model broke the moment a process forked and shared
the eventfd across the fd-table copy. See "Background" — the fix added
`ref_count` (starts at 1 on create, `+1` per `clone_ref`, `-1` per `close`,
removed from `EVENTFDS` only at zero). `ExecRuntime.eventfd_clone_ref` /
`eventfd_close` are the fork/exit hooks that drive this (`src/main.rs:451,455`
wire the Tier 2 no-op stubs when `sc-eventfd` is off).

## Background

- `archive/GOLANG_IPC.md` "EventFd Use-After-Exec EBADF" — the ref-counting
  bug: `go build`'s `netpollBreak()` wrote to an inherited eventfd after the
  child's unconditional `eventfd_close` had already removed the shared table
  entry, killing the parent with a fabricated `EBADF`.
- `archive/SHARED_FD_TABLES.md` — eventfd's place in the fd-table teardown
  model alongside pipes/epoll/pidfd.
- `archive/NODEJS_LIBUV_IMPLEMENTATION.md` — `eventfd_can_read` in the libuv
  poll-readiness table.
