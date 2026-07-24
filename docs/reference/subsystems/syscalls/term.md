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
state, owning pid/box) that ended up here rather than in its own module. It
fills a caller-provided array of `ThreadCpuStat` (one per thread slot) and
returns the count. It powers `userspace/top`.

### `ThreadCpuStat` wire format (48 bytes, `#[repr(C, align(8))]`)

The struct is defined **identically** in two places that must stay
byte-for-byte in sync: the kernel copy (`src/syscall/mod.rs`) and the
userspace copy (`userspace/libakuma/src/lib.rs`). Fields:
`tid: u32`, `pid: u32`, `box_id: u64`, `total_time_us: u64`, `state: u8`,
`last_core: u8`, `_reserved: [u8; 6]`, `name: [u8; 16]`.

- **`last_core`** (offset 25) — the MPIDR aff0 of the core the thread last
  ran on; `0xFF` means never scheduled. Populated from
  `threading::get_thread_last_core`, which reads a lock-free
  `LAST_CORE: [AtomicU8; MAX_THREADS]` array written in the scheduler's
  `commit_switch` (in `akuma-exec`). It is deliberately **not** a
  `ThreadSlot` field: `sys_get_cpu_stats` must not take `POOL.lock` (a
  nested user-copy fault while holding it self-deadlocks the core — see the
  `USER_COPY_FAULT_HANDLER` note in `threading/mod.rs`), so every per-thread
  value it reads comes from a lock-free atomic array. `top` renders this as
  its `CORE` column (`-` for the `0xFF` sentinel). Added when `_reserved`
  shrank `[u8; 7] → [u8; 6]`, keeping the 48-byte ABI.
- **`name`** — for a thread with an owning userspace process this is the
  process name. For a **kernel thread** (no process) the populator falls back
  to `threading::kernel_thread_name(tid)` (also lock-free) rather than
  leaving it blank: `kernel` (tid 0), `idle` (a per-core idle thread),
  `network` (the poller), `system` (a reserved system slot), else
  `kernel-thread`.

### CPU% is delta-based — a single sample is meaningless

`top` computes `CPU% = Δtotal_time_us / Δwall_time_us` between two
`get_cpu_stats` samples (`total_time_us` and `uptime` are both microseconds —
no unit mismatch). The percentage is therefore **only as meaningful as the
gap between samples**. In the interactive loop that gap is the ~1s input
poll. `top --once` prints after one iteration, so it now `sleep_ms(500)`s
between its two samples on purpose — without that pause the samples are taken
microseconds apart and every thread that happens to be on-core at that
instant reads ~100% (up to one per core) while everything else reads 0%.
Sustained ~100% readings for the `network` poller and `sshd`'s accept loop
are **real** busy-poll behavior, not a sampling artifact.

## Background

- `archive/SSH_TERMINAL_SIZE_FIX.md` — `TIOCGWINSZ`/`TIOCSWINSZ` not
  reflecting the SSH `pty-req`/`window-change` size, fixed independently
  on both the kernel built-in sshd and userspace sshd paths.
- `archive/SSH_TERMINAL_KEY_TRANSLATION_FIX.md` — the xterm escape-sequence
  translation feeding `poll_input_event`.
- `archive/RICH_TERMINAL_INTERFACE_OVER_SSH.md`, `archive/TERMINAL_SYSCALLS.md`.
- `archive/PIPE_TTY_FIX.md` — the `is_terminal()` / pipe-vs-tty gate above.
