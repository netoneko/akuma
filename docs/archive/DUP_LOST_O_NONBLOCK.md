# A `dup` lost `O_NONBLOCK`, and tokio's signal driver parked forever (2026-11-01)

**Stability: B**: fixed; verified on Taoxichuan (Mac TCG rig), amd64 self-tests and the AArch64 boot suite. Every kernel on `akuma-syscalls-glue` carried it.

## Symptom

Taoxichuan, with signal delivery (cleanup step B9): `tokioprobe` passed `RUNTIME` and `UNAME`, then stopped making
progress in `HTTPS`, and even its own `tokio::time::timeout` never fired. The guest stayed up. A slot dump showed tokio's
only worker thread WAITING inside `recvfrom(fd 9, buf, 128)`, nothing pending, and fd 9 a `UnixSocket` whose process
had `nonblock = false`.

## Cause

tokio's signal driver makes a non-blocking `socketpair` (`mio::net::UnixStream::pair`, `SOCK_NONBLOCK`), and reads a
`try_clone()` of the receiving end: an `fcntl(F_DUPFD_CLOEXEC)`. On a signal it drains that dup until `EAGAIN`.

`O_NONBLOCK` is a property of the open file *description* on Linux, so every dup shares it. Here it is a per-fd-*number*
bit (`akuma-fdtable`'s `nonblock` set), and glue's `sys_dup` and `F_DUPFD` did not copy it to the new number (`dup3`
did). The module doc and three tests pinned that as a deliberate divergence. So the dup was blocking. The first drain read
returned the handler's byte, and the second parked forever: the signal driver is on the runtime's only worker, so the
whole runtime stopped, timers included.

Why it never showed before: until B9 Taoxichuan ran no signal handlers, so nothing ever wrote to tokio's signal pipe and
the drain loop never ran. B9 delivered SIGCHLD to tokio's handler after the `UNAME` step's child exited, and the next drain
hung. The other kernels have the same `dup`; whether a program there ever drained a dup this way was never tested.

Found by bisect with a temporary `sigdbg=` command-line mask (it was the handler delivery, not the FP image or the
`SA_RESTART` rerun), then a slot dump of thread states and fd 9's kind and flag, then three probes that each ruled out a
cause: a non-blocking pair read from another thread (fine), from a thread started before the pair existed (fine), and
across a `posix_spawn` (fine). What remained was the dup.

## Fix

`sys_dup` and `F_DUPFD`/`F_DUPFD_CLOEXEC` copy the old number's `O_NONBLOCK` bit to the new one, as `dup3` already did,
and so do `akuma-fdtable`'s `dup`/`dup_min`. A dup now starts non-blocking when its original is, as on Linux. Still
divergent, and pinned by a test: a later `fcntl(F_SETFL)` on one of the two does not reach the other (Linux would share
it). amd64's boot check is now `fd: a dup keeps nonblock`.

## Verified

Host tests: `akuma-fdtable` 11/11 (the `dup`/`dup_min` cases now assert the bit is copied, and the remaining divergence),
`akuma-exec` 106/106. Taoxichuan, Mac TCG rig: tokioprobe at `smp=1` and `smp=4`, and three probes reading a non-blocking
socket through a dup (made by `SOCK_NONBLOCK`, `fcntl` and `FIONBIO`, and from a late reader) all get `F_GETFL=0x800` and
`EAGAIN` (`TAOXICHUAN_CLEANUP_PLAN.md`, the B9 row). amd64: self-tests 819/819 with `fd: a dup keeps nonblock`. AArch64:
boot suite 309 PASSED, 0 FAILED (its `pipe_dupfd_bumps_refcount` and `pipe_read_nonblock_returns_eagain` among them).

## Background

- [`AKUMA_FDTABLE_EXTRACTION.md`](AKUMA_FDTABLE_EXTRACTION.md): where the per-number model and the pinned divergences come from.
- [`NCA_FD_NONBLOCK_TOCTOU.md`](NCA_FD_NONBLOCK_TOCTOU.md): why a fresh number never inherits a stale bit.
- [`TAOXICHUAN_CLEANUP_PLAN.md`](TAOXICHUAN_CLEANUP_PLAN.md): step B9.
