# pipe syscalls

`pipe2` — anonymous pipe creation. Source: `src/syscall/pipe.rs`.
For the pipe buffer, SIGPIPE/`cmd | head` wedge behaviour, and PTY/TTY
mechanics, see [`../vfs.md`](../vfs.md) "Pipes, PTY, TTY" — not duplicated
here.

> **Stability: A (stable).** Dormant since the epoll-integration work
> (Mar 2026); only a clippy pass since. The recurring lesson: **pipe
> lifetime is refcounted, not owned** — `write_count`/`read_count` track how
> many fds reference each end, and the buffer is only freed when both hit
> zero.

## pipe2 (59)

`sys_pipe2` (`src/syscall/pipe.rs:207`) is the only syscall entry point in
this family — aarch64 Linux has no bare `pipe`, only `pipe2`.

1. Validate the 8-byte `int[2]` output pointer (`EFAULT` if not mapped).
2. `pipe_create()` allocates a new pipe ID (`NEXT_PIPE_ID`, global atomic)
   and inserts a `KernelPipe { buffer, write_count: 1, read_count: 1,
   pollers }` into the global `PIPES` map.
3. Allocate two fds on the calling process: `PipeRead(id)` and
   `PipeWrite(id)` (see [`../vfs.md`](../vfs.md) "File descriptors" for the
   `FileDescriptor` enum).
4. If `flags & O_CLOEXEC`, mark both fds close-on-exec
   (`proc.set_cloexec`).
5. Copy `[fd_r, fd_w]` back to the user pointer.

**`O_NONBLOCK` is not applied here.** The `pipe2(2)` flag is accepted but
`sys_pipe2` never calls `proc.set_nonblock` on the new fds — non-blocking
pipe I/O is instead configured after the fact via `ioctl(fd, FIONBIO, ...)`
(`src/syscall/term.rs`) or `fcntl(F_SETFL)`. Callers that pass `O_NONBLOCK` to
`pipe2()` and never follow up will get blocking pipe reads.

**Once the flag *is* set, `read()` honours it — but only since 2026-08-17.**
The `PipeRead` arm of `sys_read` ignored `fd_is_nonblock` outright and parked in
`schedule_blocking(u64::MAX)` on an empty pipe with a live writer, while both
sibling arms (`ChildStdout`, `UnixSocket`) honoured it. The cost was not a slow
read but a stalled *runtime*: an async reactor performs that read on its own
thread, so the whole executor sat inside the kernel until the child closed the
pipe. Regression: `pipe_read_nonblock_returns_eagain` in the boot suite;
[`../../../archive/TOKIO_PIPE_EPOLL_HANG.md`](../../../archive/TOKIO_PIPE_EPOLL_HANG.md).

**A read end reports `POLLHUP` once the last writer is gone** (`pipe_hup`),
whether or not the caller asked for it, as Linux does. This is load-bearing for
edge-triggered watchers rather than cosmetic: `pipe_can_read` folds "has bytes"
and "at EOF" into a single `POLLIN`, so without a distinct bit the
*drained → EOF* transition yields `revents & !last_ready == 0` and the EOF edge
is swallowed. See [`poll.md`](poll.md) for the edge bookkeeping.

## Fd allocation & refcounting

- Each `pipe2()` call always creates a **fresh** `KernelPipe` — there is no
  fifo/named-pipe path in this file (no `mkfifo` syscall is dispatched to
  `pipe.rs`; despite the syscalls.md submodule table calling this family
  "pipe/fifo", only anonymous pipes are implemented).
- `pipe_clone_ref(id, is_write)` bumps `write_count`/`read_count` on
  `dup`/`dup2`/`fork` of a pipe fd — see [`proc.md`](proc.md) "clone / fork"
  for how fork deep-copies the fd table (pipe refs bumped, not moved).
- `pipe_close_read`/`pipe_close_write` decrement the matching count and
  destroy the `KernelPipe` only when **both** counts reach zero.

## Background

- `archive/PIPE_TTY_FIX.md` — TTY processing over pipe-backed channels.
- `archive/STAT_AND_UNLINKAT_FIX.md` — `stat`/`unlinkat`/`dup3` interaction
  with kernel pipes.
- `archive/SPLIT_SYSCALLS.md` — the split into `src/syscall/`.
