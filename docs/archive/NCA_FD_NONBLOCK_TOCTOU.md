# Concurrent spawns from a multi-threaded process: a stale `O_NONBLOCK` bit

**Date:** 2026-08-18
**Status:** root-caused and fixed, verified by A/B
**Symptom that started it:** `nca` self-hosting inside Akuma (`cargo build -p nca-cli` from
`/tmp/native-cli-ai`) intermittently died with a real `rustc` panic — not the kernel-level
spawn failure this looked like at first:

```
thread 'rustc' (26) panicked at /rustc/.../library/std/src/sys/process/unix/unix.rs:155:21:
the CLOEXEC pipe failed: Os { code: 11, kind: WouldBlock, ... }
```

`code: 11` is `EAGAIN`/`EWOULDBLOCK`. rustc's own `--emit=link` step spawns the system linker
as a child process the same way cargo spawns rustc — via `std::process::Command`'s
child-error-pipe handshake — and got `EAGAIN` reading a pipe that is supposed to be a
**blocking** read.

## Executive summary

`nonblock` (and `cloexec`) are tracked per **raw fd number** in `SharedFdTable`, not tied to
the underlying pipe/file object's identity. `sys_close` cleared `cloexec` immediately after
freeing the fd's table slot, but cleared `nonblock` only *after* the resource-cleanup match
arm below it — a window in which a concurrent thread on the same (`CLONE_THREAD`/shared) fd
table could `alloc_fd` the very same fd number for an unrelated new pipe, inherit the
previous occupant's stale `nonblock` bit, and get spurious `EAGAIN` on what its caller
expected to be a blocking read. Worse, this closing thread's now-late `clear_nonblock` call
would then **wipe out** whatever the new owner had legitimately set on that fd in the
meantime.

This only manifests under real concurrent spawning from a **multi-threaded** parent process
— `cargo`/`rustc` qualify, a single-threaded shell forking children does not, which is why
this eluded detection for so long.

## Why concurrency was the missing ingredient

`docs/archive/NCA_MISSING_SYSCALLS.md` §1 had already established (2026-08-17) that a
`std::process::Command` spawn of a real, heavy `rustc` invocation with piped stdio —
matching cargo's own spawn shape exactly, including its injected `CARGO_PKG_*` environment
— never failed across 80 combined sequential iterations (`ncaprobe bigspawn`,
`userspace/ncaprobe`). Nightly vs. Alpine-stable `rustc`, envp size, and the exact
environment cargo injects were all independently ruled out the same way, on 2026-08-18
(stable Rust was removed from the guest via `apk del rust cargo` to make the nightly-only
case airtight; behaviour was identical either way).

What changed the picture: spawning the same real invocation from **4 concurrent OS threads**
(`ncaprobe bigspawn-threads 4 10`, one process, real threads, not 4 separate shell-forked
processes) reproduced real `rustc` panics reliably — **24 of 40 spawns failed**, all with the
same `WouldBlock` signature, none via a plain shell's `&`/`wait` at matched concurrency and
duration (40/40 clean there). That isolated the trigger to fork/spawn happening
**concurrently from sibling threads of one multi-threaded process** — exactly cargo's own
execution shape (its job-scheduling thread pool), and exactly what a sequential probe or a
single-threaded shell can never exercise.

## The mechanism, precisely

`crates/akuma-exec/src/process/fd.rs`'s `SharedFdTable` keeps three separate structures
guarded by separate spinlocks: `table` (fd → `FileDescriptor`), `cloexec` (a `BTreeSet<u32>`),
`nonblock` (a `BTreeSet<u32>`) — all threads of one process share one `SharedFdTable`
(`Arc<SharedFdTable>`), correctly matching POSIX fd-table semantics.

`src/syscall/fs.rs`'s `sys_close` (before the fix):

```rust
if let Some(entry) = proc.remove_fd(fd) {       // (1) slot freed — fd is now allocatable
    proc.clear_cloexec(fd);                      // (2) cloexec cleared promptly
    match entry {                                // (3) real syscalls: pipe_close_*, remove_socket, ...
        ...
    }
    proc.clear_nonblock(fd);                     // (4) nonblock cleared LATE
    0
}
```

Between (1) and (4) — the span of an entire match arm doing real cleanup syscalls — the fd
number is sitting in the table as available. A concurrent thread's `alloc_fd`
(`fd.rs`'s `alloc_fd_from`) picks the "lowest available fd", which can be this exact number,
and inserts its own brand-new `FileDescriptor::PipeRead`/`PipeWrite` at it immediately. If
that new pipe's fd number happens to be the one a *third* thread's `std::process::Command`
child-error-pipe handshake is about to `read()`, and the previous occupant had `O_NONBLOCK`
set (plausible — pipes get `O_NONBLOCK` set and cleared routinely, e.g. by `mio`, which is
what crossterm's default backend and cargo's own internals both sit on), that read gets
`EAGAIN` instead of blocking. And when this closing thread's own delayed `clear_nonblock(fd)`
*finally* runs, it clears whatever the **new**, unrelated owner had legitimately set —
corrupting a second, live pipe on top of misreporting the first.

`sys_close_range` had the identical shape (same late `clear_nonblock` after the same
resource-cleanup loop body). `alloc_fd_from` itself had no defensive clearing at all — a
freshly handed-out fd number carried forward *whatever* flags the previous occupant left,
regardless of which code path freed it.

A related, non-racy gap in the same family: `sys_dup3` explicitly sets/clears `cloexec` on
`newfd` per its `flags` argument, but never touched `nonblock` at all. On real Linux, `dup2`/
`dup3` share the underlying *open file description* between `oldfd` and `newfd`, so both
report the same `O_NONBLOCK` status — correct behaviour requires copying it explicitly here,
since Akuma tracks it per fd number rather than per open-file-description.

## The fix

`src/syscall/fs.rs`:
- `sys_close` — moved `proc.clear_nonblock(fd)` up next to `proc.clear_cloexec(fd)`,
  immediately after `remove_fd`, before the resource-cleanup match runs.
- `sys_close_range` — same reordering in its non-`CLOSE_RANGE_CLOEXEC` branch.
- `sys_dup3` — copies `oldfd`'s `nonblock` status onto `newfd` after `swap_fd`.

`crates/akuma-exec/src/process/fd.rs`:
- `alloc_fd_from` — defensively clears `nonblock`/`cloexec` for the chosen fd number
  *before* inserting the new entry into `table`, so a freshly allocated fd can never be
  observed with a stale flag regardless of which path freed the number (`sys_close`,
  `sys_close_range`, or anything else not yet audited).

## Verification

Same A/B technique as `TOKIO_PIPE_EPOLL_HANG.md`: private, port-isolated QEMU boot
(`scripts/cargo_runner.sh`'s `INSTANCE=` mechanism, `snapshot=on` against `devbox.img` so the
live disk is untouched), `SMP=4`/`4096M` to match real conditions, identical
`ncaprobe bigspawn-threads 4 10` before and after:

```
PRE-FIX:  24 ok, 24 failed out of 40 (all "the CLOEXEC pipe failed: ... WouldBlock")
POST-FIX: 40 ok,  0 failed out of 40
```

Also host-unit-testable, since the fix moved `alloc_fd_from` from `Process` onto
`SharedFdTable` itself (a thin delegating wrapper is all `Process` keeps now) — the same
reason `reserve_write_pos` lives there and is covered by `reserve_write_pos_tests`:
`crates/akuma-exec/src/process/fd.rs`'s `alloc_fd_tests` module, three tests —
`reused_fd_number_does_not_inherit_a_stale_nonblock_flag` (single-threaded, the direct
case), `allocation_does_not_disturb_other_fds_flags` (the fix's clear must be scoped to only
the fd being allocated), and `concurrent_reuse_never_observes_a_stale_nonblock_flag` (real
`std::thread`s, one repeatedly freeing a fd's slot without clearing its `nonblock` bit — the
exact pre-fix `sys_close` window, held open deliberately — racing another reallocating that
same slot thousands of times). All three fail against the pre-fix `alloc_fd_from` (verified
by temporarily reverting it) and pass against the fix.

## What this does NOT explain

A real `cargo build -p nca-cli -j4` against the fixed kernel still hits the *original*,
differently-shaped failure this investigation started from:

```
error: could not compile `libc` (build script)
Caused by:
  could not execute process `rustc ...` (never executed)
Caused by:
  Bad address (os error 14)
```

`os error 14` is `EFAULT`, not `EAGAIN` — a different errno, and (per
`NCA_MISSING_SYSCALLS.md` §1) one the kernel's own `SYSCALL_ERRNO_DIAG_ENABLED` logging never
attributes to a real syscall return. Pipe-ID reuse was checked and ruled out as a shared
cause: `pipe_create` allocates from a monotonic `AtomicU32` counter
(`src/syscall/pipe.rs`, `NEXT_PIPE_ID`), never reused, so two concurrent pipes can never
collide on the same `pipe_id` the way fd *numbers* can collide under the bug above. Fork's
pipe-refcounting (`clone_deep_for_fork`, `crates/akuma-exec/src/process/fd.rs`) was also
checked and is correct — it calls `pipe_clone_ref` for every inherited pipe/socket fd, so a
child closing its copy cannot prematurely EOF the parent's. This is a **second, distinct**
bug in the same fd/spawn family — see
[`NCA_CARGO_SPAWN_EFAULT.md`](NCA_CARGO_SPAWN_EFAULT.md).

## Background

- [`NCA_MISSING_SYSCALLS.md`](NCA_MISSING_SYSCALLS.md) §1 — where this whole investigation
  started, and the still-open `EFAULT` half.
- [`TOKIO_PIPE_EPOLL_HANG.md`](TOKIO_PIPE_EPOLL_HANG.md) — the sibling nca-input-freeze
  investigation the same day, same A/B methodology (private isolated QEMU boots,
  `ncaprobe`), a different fd/read defect (`Stdin`'s missing `O_NONBLOCK` check and missing
  `epoll_on_fd_drained` re-arm) in the same general "per-fd flag/state correctness under
  concurrency" territory.
- `userspace/ncaprobe` — `bigspawn`/`bigspawn-threads` subcommands, written for this
  investigation, kept as the regression harness.
