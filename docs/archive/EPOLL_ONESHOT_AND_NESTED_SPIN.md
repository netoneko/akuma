# Claude Code spun at millions of syscalls a second: EPOLLONESHOT and nested epoll (2026-10-16)

**Status: FIXED 2026-10-16** (`4211281b` EPOLLONESHOT, `212c3b7a` nested epoll). Two independent
defects with one symptom, found one after the other on the ryzen while chasing why Claude Code
froze in an interactive terminal. The current-state rules are in
[`docs/reference/subsystems/syscalls/poll.md`](../reference/subsystems/syscalls/poll.md)
§ "EPOLLONESHOT" and § "Nested epoll"; this file is the incident record.

## Symptom

`claude` (Bun) in an interactive terminal on the ryzen made millions of `epoll_pwait` +
`clock_gettime` syscalls a second, each returning events it never read, and its UI stopped
taking keyboard input ("freezes after ~15 characters", "hangs when interrupted"). No sign of
real work.

## Defect 1 — nested epoll was always ready (~2 M syscalls/s)

`FileDescriptor::EpollFd` had no arm in `epoll_check_fd_readiness`, so it fell into
`Unmodelled` ("ready for whatever was asked"). An outer `epoll_wait` over an inner epoll
therefore returned at once, every time, with 2 events nobody consumed.

**Fix.** An epoll fd registered inside another is `EPOLLIN`-ready exactly while its own interest
list has something pending, and never writable (`FdState::Epoll { pending }`).
`poll.rs::epoll_instance_pending` copies the inner list out under `EPOLL_TABLE` (nothing is held
across a probe, so an instance containing itself recurses only to `EPOLL_MAX_NEST` = 4), probes
each entry through the same `epoll_check_fd_readiness_at`, and asks `Entry::would_report` — the
pure edge decision **without recording**. It must not use `scan_entry`: a probe from the outer
instance would consume the inner instance's edges. The outer waiter's `Waker` is passed down, so
a write to an inner pipe wakes the outer wait directly.

## Defect 2 — EPOLLONESHOT was not implemented (318 M syscalls in 72 s)

With defect 1 fixed the spin remained. A one-shot registration is reported once and then
disarmed until `EPOLL_CTL_MOD` (or a re-`ADD`). Nothing implemented the flag: it was masked off
as a registration bit and never acted on, and it was not on the divergence list either. Bun
registers a one-shot writability poll on stdout/stderr; those are always writable here, so every
`epoll_wait` returned the same event for ever (~4.4 M syscalls/s).

Found with the `[epoll-spin]` diagnostic added the commit before — it names the fd once a caller
has had the same first event back 200,000 times in a row:

```
[epoll-spin] pid=991 epfd=3 fd=5 revents=0x4 ready=2 kind=Some(Stdout)
```

**Fix.** `InterestList::scan_entry` sets `Entry::disarmed` when it reports a one-shot entry and
returns nothing while it is set; `MOD`/`ADD` clears it; `Entry::would_report` honours it too.

## Verification

- Host: `a_oneshot_entry_reports_once_then_stays_quiet_until_it_is_modified` and four siblings;
  `an_idle_nested_epoll_is_not_readable_...` and
  `would_report_does_not_consume_an_edge_scan_entry_does` (`cargo test -p akuma-syscalls-poll`,
  36 -> 48 tests).
- Live: `epollops` `oneshot_fires_once_until_modified` (PASS on Linux, 17/17) and
  `nested_epoll_readable_only_while_inner_ready` (PASS on Linux; FAIL before on both the QEMU
  aarch64 kernel and the ryzen amd64 kernel; PASS after on aarch64 QEMU). The amd64 kernel had
  not been rebuilt with the nested-epoll fix when this was written.

## Still not covered

`EPOLL_CTL_ADD` of an epoll into a loop, or nested deeper than 4, is accepted here where Linux
returns `ELOOP`/`EINVAL`; the divergence is pinned in `poll.md`'s table and only the ordinary
nested case has a test.

## Background

- [`docs/reference/subsystems/syscalls/poll.md`](../reference/subsystems/syscalls/poll.md) — the
  readiness map, interest list and `EPOLLET` decision these two fixes live in.
- [`AMD64_CLAUDE_LOGIN_INPUT_STUCK.md`](AMD64_CLAUDE_LOGIN_INPUT_STUCK.md) — a later, still-open
  Claude Code input loss whose stuck epoll returns one event per call.
