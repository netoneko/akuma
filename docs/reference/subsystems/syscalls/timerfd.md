# timerfd syscalls

`timerfd_create` (85) / `timerfd_settime` (86) / `timerfd_gettime` (87).
Source: `src/syscall/timerfd.rs`. Gated `sc-timerfd` (Tier 1 — pure dead
weight when off, no `ExecRuntime` stub needed; see
[`../syscalls.md`](../syscalls.md) "Feature gates & ExecRuntime stubs").

> **Stability: B (watch).** No timerfd-specific bugs since Mar 2026, but it
> sits directly on the epoll/eventfd wait path that *did* have a serious
> hang (see Background). The recurring lesson: **a `copy_to_user` failure
> must propagate as `EFAULT`, never be silently swallowed** — an ignored
> `Result` here used to let userspace read a stale/zeroed `itimerspec` as if
> the write had succeeded.

## State

Global table keyed by timer id, same shape as eventfd's:

```rust
static TIMERFD_TABLE: Spinlock<BTreeMap<u32, TimerFdState>>
```

`TimerFdState { armed_at_us, initial_us, interval_us, expirations_consumed,
pollers }` (`timerfd.rs:5-11`). All times are stored as microseconds
(`crate::timer::uptime_us()`), converted to/from Linux `timespec` at the
syscall boundary (`timespec_to_us_safe` / `us_to_timespec_safe`,
`timerfd.rs:31-49`).

## timerfd_create

`sys_timerfd_create` (`timerfd.rs:66`): allocates a timer id and a
`FileDescriptor::TimerFd(id)` fd. No entry is placed in `TIMERFD_TABLE` yet —
a timer exists but is disarmed (`initial_us` absent) until `settime`.
`clockid`/`flags` are accepted but not otherwise validated or stored.

## timerfd_settime

`sys_timerfd_settime` (`timerfd.rs:77`): looks up the timer id from the fd
(`EBADF` if the fd isn't a `TimerFd`), then:

1. If `old_value != 0`: validate the pointer, then write the *previous*
   `itimerspec` back (interval + remaining time), or all-zero if the timer
   was never armed. Every `copy_to_user_safe` here now checks its `Result`
   and returns `EFAULT` on failure (see stability note above) — the previous
   code discarded the `Result` with `let _ =`.
2. Validate `new_value`, decode `it_interval` (offset 0) and `it_value`
   (offset 16) into microseconds.
3. `TFD_TIMER_ABSTIME` (flag `1`): `initial_us` is treated as an absolute
   deadline and converted to a relative one (`initial_us - now`).
4. `initial_us == 0 && interval_us == 0` disarms: the entry is removed from
   `TIMERFD_TABLE` rather than left as a zeroed no-op state.

## timerfd_gettime

`sys_timerfd_gettime` (`timerfd.rs:137`): read-only mirror of the write path
in `settime` — reports `it_interval` and remaining `it_value`, or zeroes for
a disarmed/unknown timer. Same `EFAULT`-on-copy-failure discipline.

## Expiration counting (read path)

Not a syscall — `timerfd_read` (`timerfd.rs:164`) and `timerfd_can_read`
(`timerfd.rs:51`) back `sys_read`/poll readiness on a `TimerFd`. Both compute
`total_expirations` from `elapsed` since `armed_at_us` (one-shot: `elapsed >=
initial_us`; periodic: `1 + (elapsed - initial_us) / interval_us`) and diff
against `expirations_consumed`. `read()` returns the delta and advances the
counter; `EAGAIN` if nothing new has expired.

## Background

- `archive/BUN_MISSING_SYSCALLS.md` "timerfd_create/settime/gettime" — the
  original bring-up notes for these three syscalls (bun's libuv event loop).
- `archive/KNOWN_ISSUES.md` #6 "bun HTTPS fetch hangs" — not a timerfd bug
  itself, but the symptom (event loop stuck on 4-second timerfd ticks) traced
  to `epoll_pwait`'s deadline handling in `src/syscall/poll.rs`; see
  [`../scheduler.md`](../scheduler.md) for the blocking/wake model timerfd
  readiness feeds into.
- `archive/SHARED_FD_TABLES.md` "TimerFd / EventFd" — fd-table teardown
  treatment alongside eventfd.
