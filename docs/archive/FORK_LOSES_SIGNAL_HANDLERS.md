# `fork()` handed the child an empty signal table

**Status: FIXED 2026-08-20.** `fork`/`vfork` gave the child a freshly
constructed `SharedSignalTable` — every disposition `Default` — instead of a
copy of the parent's. POSIX inherits dispositions across `fork`; only `execve`
resets caught handlers. One line in `fork_process`, one in `vfork_process`.

Found while chasing a much smaller-looking symptom: an nginx worker that
would not die from `kill`.

## The symptom

An nginx worker, idle, parked in `epoll_pwait`. `kill <worker>` returned 0 and
the worker kept running — through two SIGTERMs, indefinitely. `kill -9`
worked. Both master and worker were plainly alive in `ps`, no CPU burn, no log
output.

The obvious reading was "signals don't interrupt `epoll_wait`", which is a
real enough class of bug and is *almost* what this was. It is worth writing
down how that reading was wrong, because the real defect is an order of
magnitude broader.

## What the evidence actually said

The kernel prints a `[kill-dbg]` line per `kill(2)`:

```
[T177.58] [kill-dbg] pid=41 sig=15 tids=1
```

One target thread found, signal pended. What never appeared afterwards was a
`[signal] deliver sig=15` line — while the *master* process was visibly
receiving and dispatching SIGCHLD (`[signal] Delivering sig 17 to handler
0x10046af0`) throughout. So delivery machinery worked; something about this
process, or this signal, or this wait, did not.

`/proc/41/syscalls` (the per-process syscall ring) ended on an `epoll_ctl`,
with no entry for the `epoll_pwait` that followed — i.e. the worker was inside
`epoll_pwait` and it had not returned for the ~10 s since the kill.

Then the experiment that broke it open, which cost nothing:

```
curl http://localhost:8080/     # wake the worker for an unrelated reason
```

The worker **died instantly** — and the request came back **empty**. So:

- the signal *was* pended on the right thread, with the right process,
- it *was* acted on the moment the syscall returned,
- and what it did was **terminate the process**, not run nginx's
  graceful-shutdown handler, which would have finished the in-flight request
  first.

That last detail is the whole diagnosis. A `Default` disposition explains
every observation at once, where "epoll ignores signals" explains only the
first.

## Root cause

`fork_process` (`crates/akuma-exec/src/process/mod.rs`):

```rust
signal_actions: Arc::new(SharedSignalTable::new()), // Fork creates fresh table
```

`vfork_process` did the same. The comment says exactly what the code does; it
is the *intent* that was wrong. Linux/POSIX: `fork` inherits every disposition
(handler, flags, mask, restorer); `execve` resets the ones that are caught, and
that part Akuma already had right (`Process::load_image` maps `UserFn` →
`Default` and preserves `Ignore`).

Two consequences, in this order:

1. **The worker could not be interrupted.**
   `current_thread_has_pending_interrupt` — the predicate every blocking loop
   consults via `should_interrupt_blocking_syscall` — only reports an
   interrupt for a `UserFn` handler without `SA_RESTART`. That is correct
   (`SIG_DFL` and `SIG_IGN` should not manufacture `EINTR`; a fatal default is
   applied inline elsewhere). With the disposition reset to `Default`, the
   predicate answered "nothing to deliver" every 10 ms, forever, and
   `epoll_pwait` never returned.
2. **When it finally did return, the signal killed it.** The default action for
   SIGTERM is terminate, so the worker was hard-killed mid-request instead of
   shutting down gracefully.

## Why it hid for so long

`fork` immediately followed by `exec` is the overwhelmingly common shape, and
`exec` resets caught handlers anyway — so for a shell, for `spawn`, for every
`box run`, the wrong table is indistinguishable from the right one. The bug is
only observable in a process that forks and **stays in the same image**:
master/worker daemons. nginx installs its handlers in the master before
forking and never re-installs them in the worker, which is textbook and which
is exactly what this broke.

It also means the failure mode was never "signals are broken" — 63 of 64
signals behave identically under both tables for any process that exec'd.

## Fix

`SharedSignalTable::clone_for_fork()` — a private copy of the array under the
lock — used by both `fork_process` and `vfork_process`. `SignalAction` is
`Copy`, so the child's table is independent and a later `sigaction` on either
side is invisible to the other. `CLONE_SIGHAND`/`clone_thread` is untouched:
it passes the parent's `Arc`, which is the sharing POSIX asks for there.

Host regressions: `fork_signal_inheritance_tests` in
`crates/akuma-exec/src/process/signal.rs` — inheritance, privateness of the
copy in both directions, and that untouched signals stay `Default`.

## Verification

Same VM, same nginx, single worker, before and after:

| | before | after |
|---|---|---|
| `kill <worker>` (SIGTERM) while parked in `epoll_pwait` | survives indefinitely; only `kill -9` works | worker exits promptly, master respawns it |
| in-flight request when the signal lands | connection dies, empty response | request completes |
| `kill <master>` | leaves the worker orphaned | whole tree shuts down |

The proof that the *handler* runs — rather than a default action that merely
happens to look similar — is **SIGWINCH**, whose default disposition is
*ignore*:

```
kill -WINCH <master>   ->  worker shuts down gracefully, master stays alive
```

Nothing observable could happen under the old table, since `Default` for
SIGWINCH is to discard it. nginx uses SIGWINCH for exactly this
(shut workers down, keep the master), so the observed behaviour is only
reachable through nginx's own handler.

## Background

- [`NGINX_MISSING_SYSCALLS.md`](NGINX_MISSING_SYSCALLS.md) — the investigation
  this came out of; Issue E and the fixes before it.
- [`../reference/subsystems/syscalls/signal.md`](../reference/subsystems/syscalls/signal.md)
  → "Disposition inheritance across fork and exec".
- [`SIGNAL_DELIVERY_FORKTEST_EVIDENCE.md`](SIGNAL_DELIVERY_FORKTEST_EVIDENCE.md)
  §D — the prior lesson this one rhymes with: disposition is process-wide, mask
  is per-thread. The missing third clause was what happens to disposition at
  `fork`.
