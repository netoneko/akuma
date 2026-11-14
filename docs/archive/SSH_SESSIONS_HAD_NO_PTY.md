# ssh sessions had no terminal (2026-11-12)

## Symptom

In an interactive `ssh -tt` session, `sleep 30` followed by `^C` ran the whole 30 seconds, on both
kernels. Typed commands worked and echoed, but `busybox tty` printed `not a tty`. A non-interactive
`ssh -tt host 'cmd'` behaved the same way.

## Cause

Since 2026-11-05 every sshd build uses libakuma's `linux-abi` spawn: `fork`, `pipe2`, `dup3` and
`execve`, the way a Linux sshd builds a child. That `spawn_full` took the spawn flags and ignored
them. A `pty-req` session asked for `SPAWN_FLAG_PTY` and got two pipes, as an exec channel does.
So no terminal existed:

- `^C` was a `0x03` byte the shell read as data.
- sshd's `TIOCSWINSZ` on the read end of a pipe did nothing.
- sshd cooked `\n` into `\r\n` by hand (`cook_output`) because no line discipline was there to do it.

The native spawn's pty (a terminal-marked `ProcessChannel` behind `write_to_process_stdin`'s ISIG
branch, [`CTRL_C_SIGINT_DELIVERY.md`](CTRL_C_SIGINT_DELIVERY.md)) is only reached through the
native `SPAWN` syscall, which sshd no longer makes.

## Fix

- **`spawn_pty_linux`** (`userspace/libakuma/src/lib.rs`): `SPAWN_FLAG_PTY` on the `linux-abi`
  path now makes a real pseudo-terminal, as `openpty` + `login_tty` do.
  - The parent opens `/dev/ptmx` with `O_RDWR|O_NOCTTY|O_CLOEXEC`, unlocks it (`TIOCSPTLCK`),
    asks its number (`TIOCGPTN`), builds `/dev/pts/N` on the stack, and duplicates the master for input.
  - The child calls `setsid`, opens the slave, makes it its controlling terminal (`TIOCSCTTY`),
    and `dup3`s it onto 0, 1 and 2. It then runs `close_range(3, ..)` and `execve`.
  - The terminal is glue's pty (`crates/akuma-syscalls-glue/src/pty.rs`), the same on both
    kernels. Its master write turns INTR into `SIGINT` on the foreground group.
  - If `/dev/ptmx` does not open, or a step before the fork fails, the spawn falls back to pipes.
- **The child starts from default signal dispositions and an empty mask** (`rt_sigaction` with a
  zeroed `struct sigaction` for 1..=64, then `rt_sigprocmask(SIG_SETMASK, 0)`), as
  `posix_spawn`'s `SETSIGDEF`/`SETSIGMASK` would.
  - An ignored SIGINT survives `execve`, and ash hard-ignores a signal that was ignored when it
    started. An sshd that was itself started from a script's `&` job (whose SIGINT is ignored)
    therefore gave every session a dead `^C`.
  - The first AArch64 test hit exactly this: sshd ran as `(/tmp/sshd --port 4444 &)` and the
    `^C`-to-prompt time was 27 s, the rest of the sleep. With the reset it was 0.
- **`SpawnResult::terminal`** is true for a pty child. sshd now cooks output only for a requested
  pty that came back as pipes (`session.pty_requested && !res.terminal`); cooking a real pty's
  output a second time sent `\r\r\n`.
  - `bridge_process`'s parameter was renamed from `pty` to `cook`, which is what it always meant.

## Verified

amd64 (`headless`, SMP=2, sshd as init):

| Check | Result |
| --- | --- |
| `ssh -tt`, typed `sleep 30`, then `^C`, then `echo after-$((2+2))` | `after-4`; the session ended in 9 s (it was 30+ before) |
| Under a host pty set to 132x50, exec `busybox stty size; busybox tty` | `50 132` and `/dev/pts/0`; the same from the interactive shell |
| Exec `exec busybox sleep 30` and `busybox sleep 30; echo rc=$?`, `^C` at 6 s | ssh exits 255 (remote killed by a signal) at once |
| Plain `ssh host 'busybox tty; exit 3'` | `not a tty`, rc 3 (exec channels keep pipes) |
| 10-session harness, plain and `-tt` | OK |

AArch64 (SMP=2, the fresh sshd uploaded and run on a second port): the same size, tty, `^C` (prompt
back at once) and plain-exec results.

Clippy for sshd on both targets is clean apart from warnings that predate this change. Host tests:
sshd 65, box 175.

## Still open

- Not run on Taoxichuan. By construction it falls back to pipes if `/dev/ptmx` does not open
  there, which is the behaviour it had before this change.
- The test disks' sshd binaries only pick this up when they are rebuilt (`amd64/mkdisk.sh`, or a
  devbox build for AArch64).
- ash's line editor sends `ESC[6n` (cursor position query) at every prompt now that it sees a
  terminal. A real terminal answers it; a script driving `ssh -tt` through a pipe sees the bytes.
