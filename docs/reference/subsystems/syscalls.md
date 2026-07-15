# Syscalls / Linux ABI

Current-state architecture for syscall dispatch, the `sc-*` feature gates, and
Linux compatibility.

> **Stability: A (stable).** The "missing syscalls" cohort flared in Mar–May
> (Go/Bun/dash/git bring-up) and has been quiet since — those problems are
> resolved. The dispatch model (`handle_syscall` + rump interception) is
> settled. errno compliance is tracked in `archive/SYSCALL_ERRNO_COMPLIANCE_CHANGES.md`.

For memory syscalls see [`memory.md`](memory.md); for network syscalls see
[`networking.md`](networking.md) + [`rump-stack.md`](rump-stack.md).

## Dispatch

Linux-compatible ABI: syscall number in **x8**, args in **x0–x5**, return in
x0. Entry: EL0 sync exception → `src/exceptions.rs` → `handle_syscall`
(`src/syscall/mod.rs:582`).

`handle_syscall` flow:
1. Store `syscall_num` on the thread + process (`current_syscall`, `last_syscall`) — this is what `ps` prints.
2. Optional `SYSCALL_DEBUG_IO_ENABLED` tracing.
3. **Rump interception first:** `rump_proxy::intercept_box_syscall(syscall_num, args)` (`mod.rs:650`). If the current process is in a rump box and the syscall is socket-family (or operates on a rump-owned fd), it is forwarded to the box's `rump_server`. AF_UNIX socketpairs (nr 199) are excluded — always native.
4. **Native dispatch:** a big `match syscall_num` (`mod.rs:656`). Unknown → `ENOSYS` (-38) + `[ENOSYS] nr=NNN` log line (decode against the asm-generic table).

## The `src/syscall/` split

`src/syscall/mod.rs` is the dispatcher; per-family logic lives in submodules.
Each gated by a `sc-*` feature (default-on; minimal builds re-add selectively).

| Submodule | Family | Gate |
|---|---|---|
| `fs.rs` | open/read/write/stat/getdents/... | always |
| `mem.rs` | mmap/munmap/brk/mremap/membarrier | always |
| `net.rs` | socket/connect/bind/listen/sendto/recvfrom | always (smoltcp **or** rump-routed) |
| `pipe.rs` | pipe/fifo | always |
| `poll.rs` | poll/ppoll/epoll | `sc-epoll` (Tier 2) |
| `proc.rs` | fork/clone/execve/wait/exit | always |
| `signal.rs` | rt_sigaction/kill/tkill/sigreturn | always |
| `sync.rs` | futex | always |
| `term.rs` | ioctl (TIOCGWINSZ/TIOCSWINSZ) + rich terminal 307–313 | always |
| `time.rs` | clock_gettime/nanosleep | always |
| `log.rs` | kernel log (dmesg) | always |
| `aio.rs` | io_setup/io_submit/... | `sc-aio` |
| `container.rs` | box/join_box/core_init | `sc-containers` |
| `eventfd.rs` | eventfd | `sc-eventfd` (Tier 2) |
| `fb.rs` | framebuffer ioctl | `sc-framebuffer` |
| `msgqueue.rs` | SysV msg queues | `sc-sysv-ipc` |
| `pidfd.rs` | pidfd_open/waitid | `sc-pidfd` (Tier 2) |
| `timerfd.rs` | timerfd_create/settime | `sc-timerfd` |

## Feature gates & ExecRuntime stubs

The `sc-*` features are compile-time gates. **Tier 1** (`sc-aio, sc-sysv-ipc,
sc-framebuffer, sc-containers, sc-timerfd`) are pure dead weight when off —
nothing else references them. **Tier 2** (`sc-eventfd, sc-pidfd, sc-epoll`)
each need a no-op `ExecRuntime` callback stub when off (e.g.
`eventfd_close: noop_u32`, `epoll_destroy: noop_u32` — `src/main.rs:412,451`).

When adding a new syscall family: add a `sc-<name>` feature in `Cargo.toml`,
gate the submodule, and (if Tier 2) add the no-op stub + keep
`scripts/build_devbox.sh` and `overlays/devbox/run.sh` feature lists in sync.
See [`../../runbooks/add-syscall-feature.md`](../../runbooks/add-syscall-feature.md).

## Linux ABI compatibility

- **Syscall numbers:** asm-generic (aarch64) table. An `[ENOSYS] nr=NNN` log
  line means that number isn't dispatched — decode it against the table.
- **errno compliance:** negative return values are `-errno`. Tracked in
  `archive/SYSCALL_ERRNO_COMPLIANCE_CHANGES.md` + `archive/SYSCALL_HARDENING.md`.
- **musl compatibility:** `archive/MUSL_COMPATIBILITY.md`. musl is the userspace
  libc; the kernel aims to run unmodified musl-linked binaries.
- **`MAX_ARG_STRLEN`:** 128 KB release / 8 KB size / 4 KB extreme (`config.rs:147`). The Go forktest 128 KB fix is a notable regression guard.

## Blocking vs non-blocking

Syscalls that would block (read on empty pipe, waitpid, poll) follow the
blocking pattern in `archive/SYSCALL_BLOCKING.md`: register a `Waker` on a wait
queue, then `schedule_blocking()`. The producer fires the waker. See
[`scheduler.md`](scheduler.md) "Blocking & wait/wake".

**`SYSCALL_BLOCKING` rule:** never block inside a preemption-disabled closure.

## Porting a new binary (missing syscalls)

When a binary fails with `[ENOSYS] nr=NNN`:
1. Decode `NNN` against the asm-generic table.
2. Check `archive/<BINARY>_MISSING_SYSCALLS.md` — the per-binary porting notes
   (Go, Bun, Node, git, apk, curl, dash, xbps, crush). The whole cohort is
   resolved history.
3. Common gaps that bit many binaries: `socketpair` (199) for Rust std subprocess
   spawn; `fcntl(F_SETFD)` for c-ares DNS; `getrandom`; `ppoll`/`epoll`.

## Background

- `archive/SPLIT_SYSCALLS.md` — the split into `src/syscall/`.
- `archive/SYSCALL_HARDENING.md`, `archive/SYSCALL_ERRNO_COMPLIANCE_CHANGES.md`.
- `archive/MUSL_COMPATIBILITY.md`, `archive/TERMINAL_SYSCALLS.md`.
- `userspace/libakuma/docs/SYSCALLS.md` — the userspace syscall wrapper docs.
