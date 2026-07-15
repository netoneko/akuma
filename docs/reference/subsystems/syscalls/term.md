# term syscalls

`ioctl` (29) + the rich terminal syscalls (307–313) and `get_cpu_stats`
(314). Source: `src/syscall/term.rs`. For the SSH-side terminal state
(`TerminalState`, PTY winsize routing via `ChildStdout(pid)`, raw/cooked
key translation), see [`../ssh.md`](../ssh.md) "Terminal handling" — not
duplicated here.

> **Stability: B (watch).** A `TIOCSWINSZ`/`TIOCGWINSZ` propagation bug was
> fixed across both SSH paths as recently as Jul 5 2026; otherwise low,
> steady churn (mostly new ioctl codes for audio/tap features, not
> firefighting). The recurring lesson: **`TerminalState` is per-PTY-spawn,
> not per-session** — a `pty` spawn gives the child a fresh `Arc`, so a
> multiplexed daemon (sshd) must reach the child's state through its
> `ChildStdout(child_pid)` fd, not its own.

## ioctl (29)

`sys_ioctl` (`src/syscall/term.rs:6`) dispatches on `cmd`, in two tiers:

**fd-agnostic ioctls** (work on any fd, checked first):
`FIONBIO` (toggle O_NONBLOCK on the fd), `FIONREAD` (bytes available —
branches on the `FileDescriptor` variant: pipe, socket, eventfd, timerfd,
stdin, or file-size-minus-position), `FIOCLEX`/`FIONCLEX` (set/clear
close-on-exec), `TIOCSWINSZ` (see below), the OSS `/dev/dsp` ioctls
(`SNDCTL_DSP_*`, `ENOTTY` off a non-`DevDsp` fd), and (rump builds)
`TUNSETIFF` (accepted as a no-op on the one tap device, `ENOTTY`
otherwise).

**Real terminal ioctls** (`TCGETS`/`TCSETS*`/`TIOCGWINSZ`/`TIOCGPGRP`/
`TIOCSPGRP`): gated to `fd <= 2` (`ENOTTY` above that) **and** to a channel
where `is_terminal()` is true. A spawned child's stdin/stdout is a pipe
(`ProcessChannel`), not a real terminal — reporting `ENOTTY` there is
deliberate so `isatty()` (which probes `TCGETS`) reports false and shells
run non-interactively over the SSH-into-box bridge instead of launching a
line editor that hangs on an `ESC[6n` cursor query.

`TIOCSWINSZ` is the one exception to the fd<=2 terminal gate: it must also
work on a `ChildStdout(child_pid)` fd, because a `pty` spawn gives the
child its own fresh `TerminalState` Arc (deliberately not sharing sshd's,
so concurrent sessions don't alias one `input_waker` slot) — see
[`../ssh.md`](../ssh.md) "PTY / winsize" for why sshd must therefore target
the child's state rather than its own.

## Rich terminal syscalls (307–313)

`set_terminal_attributes`/`get_terminal_attributes` (raw/cooked mode),
`set_cursor_position`/`hide_cursor`/`show_cursor`/`clear_screen` (emit the
corresponding ANSI escape to the process's output channel), and
`poll_input_event` (blocking/non-blocking/timed read of stdin via the
`TerminalState`'s `input_waker`). See [`../ssh.md`](../ssh.md) "Terminal
handling" for the `EscapeState` key-translation machine that produces the
input these consume.

`get_cpu_stats` (314) is dispatched from this file but is unrelated to
terminals — it's a `/proc`-adjacent debugging syscall (per-thread CPU time,
state, owning pid/box) that ended up here rather than in its own module.

## Background

- `archive/SSH_TERMINAL_SIZE_FIX.md` — `TIOCGWINSZ`/`TIOCSWINSZ` not
  reflecting the SSH `pty-req`/`window-change` size, fixed independently
  on both the kernel built-in sshd and userspace sshd paths.
- `archive/SSH_TERMINAL_KEY_TRANSLATION_FIX.md` — the xterm escape-sequence
  translation feeding `poll_input_event`.
- `archive/RICH_TERMINAL_INTERFACE_OVER_SSH.md`, `archive/TERMINAL_SYSCALLS.md`.
- `archive/PIPE_TTY_FIX.md` — the `is_terminal()` / pipe-vs-tty gate above.
