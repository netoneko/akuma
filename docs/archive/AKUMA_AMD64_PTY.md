# Pseudo-terminals: `/dev/ptmx` + `/dev/pts/N` (both kernels)

**Date:** 2026-10-05. **Status:** implemented; host-tested; verified end to
end in QEMU (amd64, SMP=1 and SMP=4) with `ptyprobe` (31/31). On-metal and
rio verification: see §6.
**Why:** every terminal emulator's `openpty`/`forkpty` failed, so rio ran its
shell on pipes behind a userspace "pipe pty" — no tty in the shell, no job
control, no `vi`/`top`/`less`, `ssh` from inside rio broken
(`AKUMA_AMD64_WGPU_KERNEL_WORK.md` §2.1, spec in
`AKUMA_AMD64_RIO_FBDEV_BUILD.md` "Kernel spec: Linux ptys").

## 1. Shape

| layer | where | what |
|---|---|---|
| the pair | `crates/akuma-pty` (new, `forbid(unsafe_code)`, no deps, 41 host tests) | fixed buffers, N_TTY line discipline, kernel `termios`/`winsize` wire layouts, `VMIN`/`VTIME` read decisions, poll readiness, hangup rules, a fixed waiter set |
| the kernel half | `crates/akuma-syscalls-glue/src/pty.rs` | 64-slot table, `/dev/ptmx`, `/dev/pts/N`, `/dev/tty` in a pty session, ioctls, blocking/`O_NONBLOCK` I/O, SIGINT/SIGQUIT/SIGTSTP/SIGWINCH/SIGHUP, sessions |
| descriptors | `FileDescriptor::PtyMaster(N)` / `PtySlave(N)` (`akuma-exec-core`) | refcounted through new `ExecRuntime` hooks `pty_clone_ref`/`pty_close` (fork, exit) and glue's close/`dup3`/`close_range`/exec-sweep arms |
| readiness | `FdState::Pty { can_read, can_write, hup }` (`akuma-syscalls-poll`) | `EPOLLHUP` unmaskable, like a pipe's |
| controlling tty | `Process::ctty: AtomicU32` (`akuma-exec`) | encoded `(slot, generation)`; inherited by fork/clone, kept by exec, cleared by `setsid`/`TIOCNOTTY` |

Glue serves it on **both** kernels: amd64's `read`/`write`/`ioctl`/`poll`/
`openat`/`close` all reach glue's arms for these descriptors (amd64's
`console_end` answers `None` for a bound non-console descriptor, so a slave
`dup2`'d onto 0/1/2 is not mistaken for the console).

## 2. What is implemented

- `open("/dev/ptmx")` → new locked pair, master fd; `TIOCSPTLCK`/`TIOCGPTLCK`,
  `TIOCGPTN`, `TIOCGPTPEER`; `open("/dev/pts/N")` (`EIO` while locked or after
  the master closed). `stat`/`fstat` report Linux's numbers (ptmx 5:2, slave
  136:N, unique inode) so `ttyname(3)` works; `/proc/<pid>/fd/N` names them.
- Termios ioctls on either side act on the one (slave) termios, as Linux's
  `tty_mode_ioctl` does: `TCGETS`, `TCSETS[W|F]` (F flushes input),
  `TIOCGWINSZ`/`TIOCSWINSZ` (a change sends `SIGWINCH` to the foreground group),
  `TIOCGPGRP`/`TIOCSPGRP`, `TIOCGSID`, `TIOCSCTTY` (`EPERM` if a live session
  owns it, unless arg 1), `TIOCNOTTY`, `FIONREAD`, `TIOCOUTQ`, `TCFLSH`,
  `TCSBRK`/`TCSBRKP`/`TCXONC` (no-ops), `TIOCPKT 0`.
- Line discipline: ICRNL/INLCR/IGNCR/ISTRIP, canonical lines with erase
  (UTF-8 aware under `IUTF8`), kill, word-erase, literal-next, EOF as a
  zero-length line, ECHO/ECHOE/ECHOK/ECHOKE/ECHOCTL/ECHONL, ISIG
  (`^C`/`^\`/`^Z` → the foreground group, input flushed unless `NOFLSH`),
  OPOST/ONLCR/OCRNL; non-canonical `VMIN`/`VTIME`, including the inter-byte
  timer.
- Hangup: last slave close → master reads drain then `EIO`, polls `POLLHUP`,
  writes `EIO`; last master close → slave reads EOF, writes `EIO`, `SIGHUP` +
  `SIGCONT` to the session leader and the foreground group.
- Sessions on amd64: `setsid` is real everywhere (new group, no ctty);
  `getpgid`/`getpgrp`/`getsid`/`setpgid` are real **only for a process whose
  session a pty controls** and keep their old answers (1 / accepted no-op) for
  the console world, whose `^C` routing depends on them (the `Getpgid` arm in
  `amd64/src/usermode.rs` says why).

## 3. Allocations

One per pair, at `open("/dev/ptmx")`, all fallible (`try_reserve_exact`,
`ENOMEM` on failure): an 8192-entry `u16` input ring (16 KiB), an 8 KiB output
ring and a 4095-byte canonical line — ~28 KiB, freed when neither side has a
descriptor. The table itself is `.bss` (64 slots). Nothing on the read, write,
poll or ioctl paths allocates: transfers go through a 1 KiB stack chunk, waiters
are a fixed 8-slot array per pair (a waiter that finds it full parks with a
10 ms cap instead of indefinitely), signal targets a fixed array like
`kill_process_group`'s. Signals and wakes are performed after the table lock is
dropped (a default disposition terminates inline, which closes descriptors,
which takes this lock — the pipe module's 2026-07-24 deadlock shape).

## 4. The bug the first boot found

The first QEMU run passed every device-level check and then stalled with the
shell at its prompt. `with_pair` handed out *every* waiter after *every*
operation — including the one that had just registered the caller — so a
blocked reader woke itself and spun, and a `poll` registration never stuck.
Fix: the pair tracks whether anything observable changed and only then
releases waiters (`PtyPair::take_wakes`); pinned by
`a_reader_that_finds_nothing_stays_registered`.

## 5. Divergences, pinned

No output flow control (`^S`/`^Q` are data), no packet mode, no
`VREPRINT`/`VDISCARD`/`ECHOPRT`, tab erase is one column, `IUTF8` on initially,
writing the master after the slave hung up is `EIO`, opening a slave never
acquires a controlling tty implicitly (every caller in the tree uses
`TIOCSCTTY`), `TIOCSCTTY` does not insist the caller is a session leader.
`kill(-pgid)` is still unsupported by `sys_kill` (job control signals sent by
the shell to a background group do not arrive; the kernel-raised `^C`/`^Z`
do).

## 6. Verification

- `cargo test -p akuma-pty` (41), `-p akuma-syscalls-poll` (pty readiness).
- `userspace/forktest/c_stress/ptyprobe.c`: posix_openpt/unlockpt/ptsname,
  hangup both ways, `ttyname`, termios offsets, a real `sh -i` driven from the
  master (echo, `stty size` = 50 132, `tty`, `/dev/tty`, job control `set -m`,
  SIGWINCH to a foreground job, `^C` killing `sleep 20` in ~700 ms,
  edge-triggered epoll ×2, `EIO`+`POLLHUP` on exit, shell reaped). 31/31 in
  QEMU at SMP=1 and SMP=4.
- On the trashcan (kernel `8bece079`, 2026-10-05): `ptyprobe` 31/31. rio not yet run against it.

## Background

`AKUMA_AMD64_WGPU_KERNEL_WORK.md`, `AKUMA_AMD64_RIO_FBDEV_BUILD.md`,
`TOKIO_PIPE_EPOLL_HANG.md` (the edge re-arm rule the read paths follow).
