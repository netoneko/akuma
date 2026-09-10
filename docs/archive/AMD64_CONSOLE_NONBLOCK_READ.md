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
| host tests | 1372 | **1373** |
| clippy, both kernels | clean | **clean** |
| `apk update` (QEMU) | OK | **OK** — 28 641 packages |
| `apk add file` (QEMU) | OK | **OK** — 3 packages, 11.0 MiB |
| **`consoletty` (ring 3, as init)** | — | **41/41** |
| `amd64_ring3_check --smp 1 -n 40` / `-n 60` | — | **40/40 · 60/60**, `free` unmoved, heap +100 / +20 kB |
| `lazybuf` / `openflags` (QEMU, over ssh) | 8/8 · 20/20 | **8/8 · 20/20** |
| interactive `busybox sh` over ssh, `-tt` and plain | OK | **OK** |
| Firecracker/KVM `SMP=1` / `SMP=4` | 580/0 · 590/0 | **594/0 · 604/0** |
| bare metal `SMP=4` | 596/0 | **616/0** |
| `apk update` + `apk add file` on the **metal** | OK | **OK** |
| interactive `busybox sh` on the **metal** | OK | **OK** |

(+7 boot checks, all of them `console_nonblock_test`.) The `apk` runs carry
`WARNING: … failed to preserve …: owner` and report `3 errors`; that is
pre-existing and unrelated — amd64 dispatches no `chown`/`fchownat` at all.

### The ring-3 probe is the one that matters

`userspace/forktest/c_stress/consoletty.c`, run **as init on the serial line**
(`INIT=/probes/consoletty`), not over ssh — and that distinction is the probe's
whole reason for existing. Over ssh a process's fd 0 is a `PipeRead` served by
glue's pipe arm; only a process on the serial line has fd 0 as a
`FileDescriptor::Stdin`, which is the descriptor `sys_read`'s preamble claims
for `read_console`. A probe run over ssh would have passed against the broken
kernel.

41 checks, 0 failures. The load-bearing ones:

```
PASS a non-blocking read of an idle console returns -1
PASS with errno == EAGAIN (not a hang, not EOF, not EBADF)
PASS and again, so it is a state and not a one-shot
PASS poll(stdin, POLLIN, 0) on an idle console returns 0
PASS and revents is clear -- no POLLHUP/POLLERR
PASS so the read is EAGAIN again -- one flag store, two setters   (via FIONBIO)
```

The last one closes §1 from the other side: `FIONBIO(1)` is glue's `ioctl` arm
writing `Process::set_nonblock`, and the console read — a different function in
a different crate — sees it. Two setters, one store.

The `poll` line is the fix batch 4b's hook bought: without
`poll_console_state`, an unbound fd 0 is not in the process fd table at all, so
glue answers `FdState::Missing` → `POLLHUP | EPOLLERR`. A shell polling its own
stdin would be told the console was finished.

### The `ssh` **client** itself, out to a real OpenSSH server

§4 is about the client, so the client is what had to be run. Both stdin shapes,
because they are served by different code and the whole finding is that they are:

**On QEMU, client as init — stdin is the serial console** (`read_console`, the
function this doc is about), out through slirp to the host's OpenSSH:

```
-- running /bin/ssh --
[ssh] connecting to 10.0.2.2:22...
The authenticity of host '10.0.2.2:22' can't be established.
ED25519 key fingerprint is SHA256:TY9i7ysq+4zQbb23fz7Y3RDtntlRrkiDAvsYzmmtkHk=.
Are you sure you want to continue connecting (yes/no)? yes
[ssh] generating new identity key at /root/.ssh/id_ed25519
ssh: publickey authentication failed for user 'netoneko' (server offers: publickey,password,keyboard-interactive)
-- init exited --
```

**On the bare metal, client under an sshd exec channel — stdin is a pipe**
(glue's `PipeRead` arm), out over the Realtek NIC to the same server on the LAN:

```
[ssh] connecting to 192.168.1.203:22...
ED25519 key fingerprint is SHA256:TY9i7ysq+4zQbb23fz7Y3RDtntlRrkiDAvsYzmmtkHk=.
Are you sure you want to continue connecting (yes/no)? yes
[ssh] no /root/.ssh/id_ed25519; using sshd's host key as identity
ssh: publickey authentication failed for user 'netoneko' (server offers: publickey,password,keyboard-interactive)
rc=255
```

Read the two halves of that. The **fingerprint is byte-identical to the
server's real one** (`ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub` →
`SHA256:TY9i7ysq+4zQbb23fz7Y3RDtntlRrkiDAvsYzmmtkHk`), so the ED25519 key
exchange completed and transported the host key correctly. The **`yes` was
echoed and consumed** — on QEMU that is `read_console` in blocking canonical
mode, on the metal it is the pipe arm. And the rejection is a *real* OpenSSH
reply with the server's actual method list, which only a completed
`SSH_MSG_USERAUTH_FAILURE` can produce. Everything works except authorization,
because neither server authorizes the guest's key — deliberately not arranged.

**The trap in reading this, and it cost two runs.** The client blocks on that
host-key prompt, so a harness that sends nothing looks exactly like a hang: the
first metal attempts timed out at 60 s, and the guest console showed
`[SSHD] Accepted connection` with no auth lines after it, which reads as a
stalled handshake. It was the prompt. On QEMU a `yes` piped in at *t=0* is also
lost — the guest needs ~30 s to boot and the bytes are gone by then — so the
delay before sending is load-bearing. **Neither was a kernel stall.**

### The interactive shell, over ssh

Both channel shapes, since the `ioctl` preamble threads exactly this:

```
$ ssh -tt … busybox sh   (and the same without -tt)
/bin/sh: can't access tty; job control turned off
/ # echo IT_WORKS
IT_WORKS
/ # test -t 0 && echo TTY0 || echo NOTTY0
TTY0
/ # busybox stty size
24 80
```

The **prompt** is the check: busybox prints one only if `TCGETS` on fd 0
succeeded, and fd 0 there is a pipe. `TTY0` is `isatty(0)` over that pipe — the
amd64-only answer the preamble exists to keep. `busybox stty -a` additionally
decodes the whole `termios` and reports `intr = ^C; quit = ^\; erase = ^?;
kill = ^U; eof = ^D; susp = ^Z; min = 1; time = 0` — every one of those is a
`c_cc[]` index read at byte 17, which is independent confirmation of the offset
§6 of the batch-4b doc records glue getting wrong.

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
