# amd64: a non-blocking `read(0)` on the console parked forever — the `ssh`-client typing freeze

**Date:** 2026-09-10
**Status:** FIXED (`amd64/src/fd.rs`, `read_console`).
**Symptom:** typing into the `ssh` **client** on amd64 does nothing. The remote
prompt and remote echo freeze; the session looks alive (it connected, the banner
arrived) but nothing the user types reaches the far end and nothing comes back.
**Grade:** the fix is A (one flag test, covered by a boot check). The two
*adjacent* divergences §6 records are C — expect surprises there.

## 1. Not two flag stores

The first diagnosis was that amd64 had **two disjoint `O_NONBLOCK` stores** and
that `fcntl` wrote one while the pipe/stdin read path consulted the other. That
was true, and it was fixed a batch earlier by something else:

- `amd64::fd::sys_fcntl` used to write a *local* `FDS.nonblock` set.
- Since **4b batch 3c** (`AKUMA_AMD64_4B_FOLD_BATCH3C.md`) it is a one-line
  forward to `akuma_syscalls_glue::fs::sys_fcntl`, whose `F_SETFL` arm calls
  `Process::set_nonblock` → `self.fds.nonblock`.
- `fd::is_nonblocking` reads `cur_table().nonblock`, and `cur_table()` is
  `&p.fds` for a registered process.

`p.fds.nonblock` and `self.fds.nonblock` are the same `BTreeSet`. **One store,
two readers, and they agree** — there is a boot check for it now (§5).

So the store was not the defect. The defect is one function.

## 2. The defect: `read_console` had no flag test at all

`akuma_syscalls_glue::fs::sys_read`'s `Stdin`/`DevTty` arm ends its wait with

```rust
if super::net::fd_is_nonblock(fd_num as u32) {
    super::poll::epoll_on_fd_drained(fd_num as u32);
    return EAGAIN;
}
```

and the comment above it records that **this arm itself once lacked the check**:
*"a caller that set it (mio, for the same reason crossterm needs `EPOLLET`
semantics to work at all) got parked in `schedule_blocking(u64::MAX)`
regardless, indistinguishable from a real hang."* That is the canonical
behaviour, it is shared code, and the AArch64 kernel gets it.

**amd64 never reaches that arm for fd 0.** `fd::sys_read`'s preamble asks
`console_end(fd)` *first*:

```rust
match console_end(fd) {
    Some(ConsoleEnd::Read) => return read_console(buf, len),   // ← here
    Some(ConsoleEnd::Write) => return errno::EBADF,
    None => {}
}
akuma_syscalls_glue::fs::sys_read(fd, buf, len)
```

and `console_end` answers `Some(ConsoleEnd::Read)` for a bound
`FileDescriptor::Stdin` — which is exactly what `SharedFdTable::with_stdio` puts
at fd 0 for **every registered process on this target**. So fd 0 landed in

```rust
fn read_console(buf: u64, len: usize) -> u64 {
    loop {
        /* drain the line discipline */
        let Some(byte) = crate::input::getb() else {
            crate::sched::yield_now();
            continue;                 // ← forever, whatever the flags say
        };
        ...
    }
}
```

a function written before the glue fix existed, with no `O_NONBLOCK` test, no
`EINTR` test, and no way out but a keystroke or Ctrl-D.

## 3. Why only amd64, when AArch64's behaviour is the canonical one

**Because the two kernels serve the same descriptor with different functions.**
It is the same console split every batch of the 4b fold has hit:

| | AArch64 | amd64 |
|---|---|---|
| what fd 0 is | `FileDescriptor::Stdin` | `FileDescriptor::Stdin` |
| where a `read(0)` goes | `glue::fs::sys_read`'s `Stdin` arm | `fd::read_console` |
| how the bytes arrive | a `ProcessChannel` (SSH/PTY plumbing) | the UART / PS/2, polled by number |
| `O_NONBLOCK` honoured | yes, since the mio fix | **no** |

Glue's arm cannot simply be used here. It reaches the console through
`akuma_exec::process::current_channel()`, **no process on this target has a
channel**, and its `current_channel().is_none()` fallback returns
`proc.read_stdin()`'s zero — a *spurious EOF* on the console a shell is reading
from. That is why the preamble exists at all, and it is why the flag test had to
be added to `read_console` rather than by deleting the preamble.

The same asymmetry is the whole content of `AKUMA_AMD64_4B_FOLD_BATCH4B.md`
(`poll`, `ioctl`) and of batch 3a (`read`/`write`/`lseek`). This is the fourth
time the answer has been "the console is by-number here", and §6 is the standing
argument for stopping that.

## 4. Why it froze typing rather than just blocking

`userspace/sshd/src/client/protocol.rs` sets **both** its socket and its stdin
non-blocking (lines ~376-377) and its interactive pump is a loop of

1. `read(0, …)` → expect data or `EAGAIN`,
2. service the socket,
3. wait for either.

Step 1 never returned. The pump was parked inside the kernel on its **first**
`read(0)`, before the socket was serviced even once, so it was not that typing
was slow — the network half of the pump never ran. Remote echo, remote prompts
and keepalives all stopped, which reads as a dead terminal rather than as a
blocked read.

## 5. The fix, and the check

`read_console` takes the fd and reads the flag once, before the loop (only this
thread's own `fcntl` could change it, and that call is inside this one):

```rust
fn read_console(fd: u64, buf: u64, len: usize) -> u64 {
    let nonblock = is_nonblocking(fd);
    loop {
        /* drain the line discipline — a full line is data */
        let Some(byte) = crate::input::getb() else {
            if nonblock { return errno::EAGAIN; }
            crate::sched::yield_now();
            continue;
        };
        ...
    }
}
```

In **canonical** mode a partial line is not data: `drain_canon_ready` yields
nothing until a terminator arrives, so a non-blocking reader gets `EAGAIN` with
its half-typed line still in `canon_buffer`. That is what Linux does.

The check is `fd::console_nonblock_test`, and **where it runs is part of the
finding**. Written first as three lines inside `fd::smoke_test`, it failed with

```
fd: a non-blocking read of an idle console is EAGAIN, not a park   [FAIL] got 0x0 want 0xfffffffffffffff5
```

— `0`, not `-EAGAIN`. `boot::self_tests` calls `wire_console_and_syscalls()`
(and therefore `fd::init_console`) **after** `fd::smoke_test`, so `CONSOLE` is
still `None` there and `read_console` returns 0 (no console → EOF) before it can
reach any flag test. The test is its own function, called immediately after that
line, for that reason. Seven checks:

```
fd: fcntl(stdin, F_SETFL, O_NONBLOCK)                            [OK]
fd: fcntl(stdin, F_GETFL) reports it back                        [OK]
fd: and is_nonblocking agrees — one flag store                   [OK]   ← §1
fd: a non-blocking read of an idle console is EAGAIN, not a park  [OK]   ← §2
fd: fcntl(stdin, F_SETFL, 0) clears it                           [OK]
fd: is_nonblocking cleared                                       [OK]
fd: the flag is what changed, not the arm                        [OK]
```

The `EAGAIN` check assumes an **idle** console, which holds on all three rigs
during the suite (QEMU's serial is fed from `/dev/null`; the bare-metal box has
no working keyboard). A keystroke landing inside it reads as one failing check,
never as a hang.

The flag is cleared before returning on purpose: `run_init` inherits this
descriptor table, and an init whose stdin is non-blocking reads its console as a
stream of `EAGAIN`.

## 6. Adjacent, found and **not** fixed

Three things this investigation walked past. None is required for the pump to
run, and each is a behaviour change on a path this session could not exercise
from ring 3, so they are recorded rather than guessed at.

1. **`EINTR` is still missing from `read_console`.** Every other read arm
   returns it on `should_interrupt_blocking_syscall()`. Adding it here would
   make a pending signal that this target never *delivers* turn a blocking
   console read into a spin, so it needs the signal-delivery path looked at
   first, not a one-line addition.

2. **The console is always cooked.** `console_ioctl` accepts `TCSETS` as a
   **no-op** and `read_console` calls `process_canon_input` unconditionally —
   it never asks `TerminalState::is_canonical()`. So a program that puts its
   terminal in raw mode (which the `ssh` client does, and every full-screen app
   does) still gets line-buffered input with local echo, and only sees its
   keystrokes on Enter. With the fix in §5 the pump *runs*, but interactive use
   still wants this. Two halves: wire `console_ioctl`'s `TCSETS` into the
   `CONSOLE` `TerminalState`, and make `read_console` branch on
   `is_canonical()`.

3. **`/dev/tty` reads are an instant EOF.** `console_end` answers `None` for
   `FileDescriptor::DevTty` (only `Stdin`/`Stdout`/`Stderr` are matched), so a
   `/dev/tty` read falls through to glue's `Stdin`/`DevTty` arm, hits the
   `current_channel().is_none()` fallback, and returns 0. A pager that opens
   `/dev/tty` to read keys from sees EOF immediately.

All three dissolve into the same piece of work: **give an amd64 process a
terminal-capable `ProcessChannel`** — the deferred `/proc/<pid>/fd/0` +
`delegate_pid` item in `AKUMA_AMD64_4B_FOLD_BATCH2A.md` § `/proc`. Then the
console preamble goes away, glue's `Stdin` arm serves fd 0 (with its own
`EAGAIN`, its `EINTR`, its raw/cooked branch and its `epoll` edge re-arm),
`poll`'s `Stdin` arm works without the hook batch 4b added, `ioctl` folds with
no tty preamble, and `/dev/tty` resolves. It is one coherent piece and it is the
highest-value thing left in `fd.rs`.

## Verify

```bash
# The boot check, on QEMU:
SMP=1 INIT=/bin/busybox INITARGS=uname,-a sh amd64/run.sh -display none < /dev/null \
  | grep -a "EAGAIN, not a park"
#   fd: a non-blocking read of an idle console is EAGAIN, not a park   [OK]
```

| gate | before | after |
|---|---|---|
| QEMU/TCG `SMP=1` | 609/0 | **616/0** |
| QEMU/TCG `SMP=4` | 619/0 | **626/0** |
| host tests | 1372 | **1372** |
| clippy, both kernels | clean | **clean** |
| `apk update` (QEMU) | OK | **OK** — 28 641 packages |
| `apk add file` (QEMU) | OK | **OK** — 3 packages, 11.0 MiB |

(+7 checks, all of them `console_nonblock_test`.) The `apk` runs carry
`WARNING: … failed to preserve …: owner` and report `3 errors`; that is
pre-existing and unrelated — amd64 dispatches no `chown`/`fchownat` at all.

Firecracker and the bare-metal box are **not** re-run here. The change is one
`if` inside a function neither VMM configuration reaches differently, but the
baseline table in `AKUMA_AMD64_4B_FOLD_BATCH4B.md` § Verification still owes
both.

## Background

- `docs/archive/AKUMA_AMD64_4B_FOLD_BATCH4B.md` — the `poll`/`select`/`ioctl`
  fold in the same session, whose `poll_console_state` hook is what makes fd 0
  *pollable* on this target and therefore what the fixed pump waits on.
- `docs/archive/AKUMA_AMD64_4B_FOLD_BATCH3C.md` — the `fcntl` fold that closed
  the two-store half of the original diagnosis (§1).
- `docs/archive/AKUMA_AMD64_4B_FOLD_BATCH3A.md` — where the `read`/`write`
  console preamble came from, and §1b's "glue reads a hook amd64 never
  registered" pattern.
- `docs/archive/AKUMA_AMD64_4B_FOLD_BATCH2A.md` § `/proc` — the
  `ProcessChannel`/`delegate_pid` work §6 points at.
- `docs/archive/NCA_FD_NONBLOCK_TOCTOU.md` — why the flags are keyed by fd
  *number*, and why `dup` therefore loses `O_NONBLOCK`.
- `docs/archive/TTY_SHENANIGANS.md` round 3 — the AArch64 side's own
  interactive-shell architecture, and why its `ioctl` answers `ENOTTY` on a pipe
  where this one must not.
