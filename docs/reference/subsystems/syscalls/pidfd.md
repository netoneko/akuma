# pidfd syscalls

`pidfd_open` plus the kernel-side pidfd table (`create`/`get_pid`/`can_read`/
`close`) that `waitid`, `epoll`, and `CLONE_PIDFD` all read. Source:
`src/syscall/pidfd.rs`. The `waitid` syscall itself lives in
[`proc.md`](proc.md) (`proc.rs:903`); `epoll` readiness checks live in
[`../syscalls.md`](../syscalls.md)'s `poll.rs` (`sc-epoll`). Gated by
`sc-pidfd` (Tier 2 — needs the `pidfd_close` `ExecRuntime` no-op stub when
off, `src/main.rs:460-463`).

> **Stability: A (stable, dormant).** The Go netpoller pidfd cohort
> (`archive/GOLANG_MISSING_SYSCALLS.md`, `archive/GOLANG_IPC.md`) is resolved
> and quiet since March 2026. The recurring lesson: **a pidfd is just an
> indirection to a `ChildChannel`** — every one of these bugs was a missing
> or stale link between the `PidFd` table and the real exit-notification
> path (`ChildChannel::has_exited`/`set_exited`), never the fd-table
> plumbing itself.

## The pidfd table

`PIDFD_TABLE`: a `Spinlock<BTreeMap<u32, KernelPidFd>>` mapping an
opaque, monotonically-increasing pidfd id (`NEXT_PIDFD_ID`) to the
`target_pid` it tracks (`pidfd.rs:3-8`). This id is **not** the fd number —
`sys_pidfd_open` wraps it in `FileDescriptor::PidFd(id)` and allocates a
real fd via `proc.alloc_fd`.

- `pidfd_create()` (`pidfd.rs:10`, `pub`) — also called directly from
  `sys_clone`'s `CLONE_PIDFD` handling (`proc.rs:447`) via
  `sys_pidfd_open(new_pid, 0)`, then `set_cloexec` — Linux always sets
  `O_CLOEXEC` on a `clone3(CLONE_PIDFD)` pidfd (a leaked-fd bug fixed per
  `archive/GOLANG_IPC.md`).
- `pidfd_can_read()` (`pidfd.rs:25`) — the readiness predicate `epoll` and
  `waitid(P_PIDFD)` both check: a **stale** pidfd (target already reaped and
  removed from `PIDFD_TABLE`) reads as readable (`true`) so a caller doesn't
  block forever on an id nobody was tracking; otherwise it's
  `ChildChannel::has_exited()`.
- `pidfd_close()` (`pidfd.rs:34`) — the `ExecRuntime.pidfd_close` callback
  invoked on `FileDescriptor::PidFd` drop (`proc.rs:712`, `fs.rs:1329`).

## sys_pidfd_open

`sys_pidfd_open(pid, flags)` (`pidfd.rs:40`, syscall 434):

1. Rejects any flag outside `O_NONBLOCK | O_CLOEXEC` with `EINVAL`.
2. Requires `get_child_channel(pid)` to exist — i.e. `pid` must be a live or
   recently-exited **child** of the caller. Returns `ESRCH` if not; Akuma
   can't distinguish Linux's "no such pid" from "exists but isn't my child"
   here, so it always reports `ESRCH` rather than guessing `EINVAL`.
3. Creates the table entry, allocates an fd, applies `O_CLOEXEC`/
   `O_NONBLOCK` from `flags`, and logs `[pidfd] open pid=... → fd=...`.

## Background

- `archive/GOLANG_MISSING_SYSCALLS.md` §7–8 — the original `pidfd_open` +
  `CLONE_PIDFD` implementation, and the epoll-readiness integration that
  replaced Go's busy-poll nanosleep loop.
- `archive/GOLANG_IPC.md` — the missed-wakeup and cloexec-leak bugs found
  under Go's 300-child compile-farm workload.
- `archive/BUN_MISSING_SYSCALLS.md` — bun's `pidfd_open` → `wait4` fallback
  before this was implemented.
