# Async child processes never complete: the lost pipe EOF edge

**Date:** 2026-08-17
**Status:** root-caused and fixed
**Symptom that started it:** `nca` running inside the VM reported
`execute_bash — Command timed out after 30s` for every shell tool call, and a
`git_status` tool call sat at `353s` and climbing.

## Executive summary

Two defects in the `PipeRead` path made every **edge-triggered** reader of a
child process's stdout/stderr hang forever at EOF. Any async runtime that uses
`epoll` with `EPOLLET` — tokio/mio, and by extension `nca`, `hyper`, anything
built on them — could not complete `Command::output()` on Akuma.

| # | Defect | Site | Fix |
|---|---|---|---|
| 1 | `read()` on an `O_NONBLOCK` pipe **ignored the flag and blocked** | `src/syscall/fs.rs`, `PipeRead` arm of `sys_read` | honour `fd_is_nonblock` → `EAGAIN`, matching the sibling `ChildStdout`/`UnixSocket` arms |
| 2 | A pipe's `EPOLLET` `EPOLLIN` edge could **never be re-armed**, so the EOF transition was silently swallowed | `sys_read` never called `epoll_on_fd_drained`; `epoll_check_fd_readiness` never reported `EPOLLHUP` | call `epoll_on_fd_drained` on every pipe read and on `EAGAIN`; report `EPOLLHUP` once the last writer is gone |

Defect 2 is the same bug class as defect 3 in
[`SOCKET_DELAYED_FIRST_BYTE_HANG.md`](SOCKET_DELAYED_FIRST_BYTE_HANG.md)
(the `EPOLLOUT` edge that was never re-armed, fixed earlier the same day) — the
`epoll_on_fd_drained` / `epoll_on_fd_write_blocked` pair was built for exactly
this and had simply never been wired into the pipe path.

## Why it hung, precisely

`pipe_can_read()` folds two different states into one bit:

```rust
!p.buffer.is_empty() || p.write_count == 0     // "has bytes" OR "at EOF"
```

Both report `EPOLLIN`. For a **level**-triggered watcher that is fine. For an
**edge**-triggered one it is fatal, because the interesting transition —
*drained-with-writer-alive* → *EOF* — produces no new bit. `sys_epoll_pwait`
computes `new_bits = revents & !last_ready`, and `last_ready` still held
`EPOLLIN` from the "has bytes" edge, so `new_bits == 0`.

This is what `read_to_end` does on every child's stdout, and where each step went
wrong before the fix:

| Step | What tokio does | Akuma before the fix |
|---|---|---|
| 1 | child writes → take the `EPOLLIN` edge | fine; `last_ready = EPOLLIN` |
| 2 | `read()` → 10 bytes | fine, **but no `epoll_on_fd_drained`**, so `last_ready` keeps `EPOLLIN` |
| 3 | `read()` again → expect `EAGAIN` | **blocks inside the kernel** (defect 1), stalling the reactor thread until the writer closed |
| 4 | child exits → expect a fresh EOF edge | `revents=0x1`, `last_ready=0x1` → `new_bits=0` → **`SUPPRESSED` forever** |

The result is a process that looks completely healthy: the child ran and exited,
the parent reaped it, the pipe sat at EOF with the data drained, and the reactor
kept calling `epoll_pwait` at ~30/s getting nothing. Nothing in any log looked
like an error.

## The evidence

### Minimal reproducer, and the Linux control

The whole failure is ~40 lines of tokio — `nca` is not required. `ncaprobe tokio`
(`userspace/ncaprobe`) runs `sh -lc "echo TOKIO_OUT"` through
`tokio::process::Command::output()` three times. The **identical static aarch64
binary** under Docker on real Linux:

```
--- LINUX ---                                  --- AKUMA (before) ---
call 0: OK in 1ms  stdout="TOKIO_OUT\n"        call 0: *** TIMED OUT after 10141ms ***
call 1: OK in 0ms  stdout="/\n"                call 1: *** TIMED OUT after 10123ms ***
call 2: OK in 0ms  stdout="850e00461a32\n"     call 2: *** TIMED OUT after 10244ms ***
```

Having the same binary pass on Linux is what turned this from "tokio is doing
something odd" into "this is a kernel bug", in one command.

### The mechanism, isolated

`ncaprobe eofedge` reduces it further: a child writes, stays alive 1 s, then
exits, and the probe takes the `EPOLLIN` edge, drains, and goes back to
`epoll_wait` for the EOF — `read_to_end`'s exact pattern.

```
--- LINUX ---                              --- AKUMA (before) ---
round 1: READY events=IN                   round 1: READY events=IN
   read = 6 "EARLY\n"                         read = 6 "EARLY\n"
   read = EAGAIN -> back to epoll_wait        read = 0  EOF        <-- at 1104ms!
round 2: READY events=HUP (0x10)
   read = 0  EOF
```

Two differences in five lines: Akuma's second `read()` **blocked for a second**
instead of returning `EAGAIN` (defect 1), and Akuma never delivered a second
edge at all (defect 2). Linux delivers the EOF as a distinct `EPOLLHUP`.

### The kernel's own account

With `SYSCALL_DEBUG_EPOLL_EDGE` (added by this investigation) the suppression is
explicit — fd 9 is the child's stdout, fd 10 the pidfd, fd 11 stderr:

```
[T21.07] [epoll] ET epfd=3 fd=9  rev=0x1 last=0x0 new=0x1 deliver
[T21.11] [epoll] ET epfd=3 fd=10 rev=0x1 last=0x0 new=0x1 deliver
[T21.11] [epoll] ET epfd=3 fd=11 rev=0x1 last=0x0 new=0x1 deliver
[T21.20] [epoll] ET epfd=3 fd=9  rev=0x1 last=0x1 new=0x0 SUPPRESSED
[T21.23] [epoll] ET epfd=3 fd=9  rev=0x1 last=0x1 new=0x0 SUPPRESSED     ... forever
```

Corroborating state from the same run: `[syscall] wait4(pid=125)` succeeded with
`exit_code=0` (the child *was* reaped), and the `[PIPE-DUMP]` nine seconds later
showed `pipe=49 bytes=0 readers=1 writers=0` — drained, at EOF, and nobody
coming back for it.

## What was ruled out, and how

Each of these cost a probe and each came back clean. Recording them because the
next person to see an epoll hang will suspect the same things:

| Hypothesis | Verdict |
|---|---|
| Raw `pipe`+`spawn`+`epoll(ET)`+`pidfd` is broken | **No** — `ncaprobe epoll main` passes in 1 round |
| Broken only on a worker thread | **No** — `epoll thread` passes |
| Race when the child exits before registration | **No** — `epoll main --late` and `epoll thread --late` pass |
| Lost wakeup: `epoll_ctl(ADD)` into an epoll another thread is parked on | **No** — `ncaprobe cross` passes; and `sys_epoll_pwait` re-scans every ≤10 ms regardless of wakers |
| `waitid(P_PIDFD, …)` (tokio's reaping call) misbehaves | **No** — byte-identical `siginfo` to Linux |
| Two epoll fds (3 and 5) are two instances, so tokio waits on the wrong one | **No** — they alias one instance, **and Linux does exactly the same**; `ncaprobe fds` |
| The fd table isn't shared across threads | **No** — same table from `block_on`, `tokio::spawn`, `spawn_blocking` and a plain `std::thread` |
| Stale `last_ready` surviving fd reuse across calls | **No** — `EPOLL_CTL_ADD` resets `last_ready` (`poll.rs`), and call 0 already failed |

## Method notes worth keeping

**Build the Linux control first.** The single highest-value step was running the
same musl binary under `docker run --platform linux/arm64`. Before that, every
observation had two possible owners (tokio or the kernel); after it, one.

**A probe per layer, not a probe per theory.** `ncaprobe`'s subcommands each
isolate one mechanism, so a pass eliminates a layer permanently instead of
weakening a hunch. Seven of the eight hypotheses above died in under a minute
each because the probe already existed.

**`PSTATS` is per-thread, not per-process.** An early reading of "tokio never
calls `read`" came from `PSTATS PID 122` — the main thread. The reads were on the
worker. `[PIPE-DUMP]`'s `bytes=` is the honest answer to "was this drained".

**Beware `interest_fds=0`.** `log_epoll_pwait_return`'s sixth argument is passed
as a literal `0` by its only caller, so that field is always zero and means
nothing. It briefly looked like evidence that the epoll interest list was empty.

**`tprint!` output can be dropped.** Exactly one `epoll_create1` appeared in the
log while two epoll fds demonstrably existed; the second line was lost to
throttling, not missing. `strace` on the Linux side settled it.

## Still open: the terminal half

The original report had a second, independent symptom — **ESC never reaches
`nca`**, and mouse-tracking sequences leak into the prompt as literal text
(`5;55;18M5;57;18M…`). That is *not* explained by the pipe bug and is **not
fixed**.

What is established:

- `isatty(0)` is true in an interactive SSH session and `TCGETS`/`TCSETS` on
  fd 0 work (`stty -g` → `lflag=0x3b` = `ISIG|ICANON|ECHO|ECHOE|ECHOK`).
- Kernel raw mode works for a plain single-threaded process:
  `stty raw -echo` then reading 3 bytes yields `1b 41 42` — ESC arrives intact.
- `nca` runs its **entire** crossterm TUI — `enable_raw_mode()`,
  `EnableMouseCapture`, `event::poll`/`read` — inside
  `tokio::task::spawn_blocking` (`crates/tui/src/repl.rs`), i.e. on a
  blocking-pool thread, not the main thread.
- `current_terminal_state()` (`crates/akuma-exec/src/process/children.rs`)
  resolves the termios state **per-TID first**, falling back to the process
  entry.

Every symptom matches "raw mode is not in effect on the thread doing the read":
keystrokes echoed by the kernel line discipline, delivered only on Enter, and
ESC — not a line terminator — never delivered at all. `ncaprobe raw split`
(set on the main thread, read on a spawned one) was written to confirm or refute
this; it needs a real tty and a keypress, and has not been run.

Two adjacent findings from the same dig, neither the cause here:

- `/dev/tty` and `/dev/pts` do not exist; busybox greets every session with
  `can't access tty; job control turned off`.
- `sys_ioctl` rejects every terminal ioctl on `fd > 2` outright
  (`if fd > 2 { return ENOTTY }`), so `/dev/tty` could not work even once
  created — it would land on fd ≥ 3 — and any program that dups its tty to a
  higher fd gets `ENOTTY`.

## Fixed in

- `src/syscall/fs.rs` — `PipeRead` honours `O_NONBLOCK`; both it and the
  `UnixSocket` arm call `epoll_on_fd_drained` on read and on `EAGAIN`
- `src/syscall/poll.rs` — pipe read ends report `EPOLLHUP` when the last writer
  is gone; `SYSCALL_DEBUG_EPOLL_EDGE` tracing
- `src/syscall/pipe.rs` — `pipe_hup()`
- `src/config.rs` — `SYSCALL_DEBUG_EPOLL_EDGE`
- `src/process_tests.rs` — `test_pipe_read_nonblock_returns_eagain`,
  `test_epoll_pipe_eof_edge_after_partial_drain`
- `userspace/ncaprobe` — the probe, kept for the next one

## Background

- [`SOCKET_DELAYED_FIRST_BYTE_HANG.md`](SOCKET_DELAYED_FIRST_BYTE_HANG.md) — the
  socket-side `EPOLLOUT` edge bug this rhymes with, and the origin of
  `epoll_on_fd_drained` / `epoll_on_fd_write_blocked`
- [`../runbooks/debug-async-subprocess-hang.md`](../runbooks/debug-async-subprocess-hang.md)
  — the procedure distilled from this investigation
- [`../runbooks/debug-delayed-first-byte.md`](../runbooks/debug-delayed-first-byte.md)
  — the sibling runbook for the network-client shape of the same class
