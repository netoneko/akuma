# Syscalls / Linux ABI

Current-state architecture for syscall dispatch, the `sc-*` feature gates, and
Linux compatibility.

> **Stability: A (stable) for dispatch.** The "missing syscalls" cohort flared
> in Mar–May (Go/Bun/dash/git bring-up) and has been quiet since — those
> problems are resolved. The dispatch model (`handle_syscall` + rump
> interception) is settled. errno compliance is tracked in
> `archive/SYSCALL_ERRNO_COMPLIANCE_CHANGES.md`. **Per-family grades vary** —
> `mem`/`net`/`signal`/`sync` are graded C (active risk, touched in the Jun
> 2026 memory+signal crisis); see the table below before trusting a specific
> family.

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
Each family now has its own current-state doc under
[`syscalls/`](syscalls/) — grades vary per family (a quiet family living next
to an actively-churning one doesn't inherit its risk).

| Submodule | Family | Gate | Doc | Grade |
|---|---|---|---|---|
| `fs.rs` | open/read/write/stat/getdents/... | always | [`syscalls/fs.md`](syscalls/fs.md) | A |
| `mem.rs` | mmap/munmap/brk/mremap/membarrier | always | [`syscalls/mem.md`](syscalls/mem.md) | **C** |
| `net.rs` | socket/connect/bind/listen/sendto/recvfrom | always (smoltcp **or** rump-routed) | [`syscalls/net.md`](syscalls/net.md) | **C** |
| `pipe.rs` | pipe/fifo | always | [`syscalls/pipe.md`](syscalls/pipe.md) | A |
| `poll.rs` | poll/ppoll/epoll | `sc-epoll` (Tier 2) | [`syscalls/poll.md`](syscalls/poll.md) | B |
| `proc.rs` | fork/clone/execve/wait/exit | always | [`syscalls/proc.md`](syscalls/proc.md) | A |
| `signal.rs` | rt_sigaction/kill/tkill/sigreturn | always | [`syscalls/signal.md`](syscalls/signal.md) | **C** |
| `sync.rs` | futex | always | [`syscalls/sync.md`](syscalls/sync.md) | **C** |
| `term.rs` | ioctl (TIOCGWINSZ/TIOCSWINSZ) + rich terminal 307–313 | always | [`syscalls/term.md`](syscalls/term.md) | B |
| `time.rs` | clock_gettime/nanosleep | always | [`syscalls/time.md`](syscalls/time.md) | A |
| `log.rs` | kernel log (dmesg) | always | [`syscalls/log.md`](syscalls/log.md) | A |
| `aio.rs` | io_setup/io_submit/... | `sc-aio` | [`syscalls/aio.md`](syscalls/aio.md) | B |
| `container.rs` | box/join_box/core_init | `sc-containers` | [`syscalls/container.md`](syscalls/container.md) | B |
| `eventfd.rs` | eventfd | `sc-eventfd` (Tier 2) | [`syscalls/eventfd.md`](syscalls/eventfd.md) | B |
| `fb.rs` | framebuffer ioctl | `sc-framebuffer` | [`syscalls/fb.md`](syscalls/fb.md) | A |
| `msgqueue.rs` | SysV msg queues | `sc-sysv-ipc` | [`syscalls/msgqueue.md`](syscalls/msgqueue.md) | A |
| `pidfd.rs` | pidfd_open/waitid | `sc-pidfd` (Tier 2) | [`syscalls/pidfd.md`](syscalls/pidfd.md) | A |
| `timerfd.rs` | timerfd_create/settime | `sc-timerfd` | [`syscalls/timerfd.md`](syscalls/timerfd.md) | B |

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
