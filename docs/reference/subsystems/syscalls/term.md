# term syscalls

`ioctl` (29) + the rich terminal syscalls (307–313) and `get_cpu_stats`
(314). Source: `src/syscall/term.rs`. For the SSH-side terminal state
(`TerminalState`, PTY winsize routing via `ChildStdout(pid)`, raw/cooked
key translation), see [`../ssh.md`](../ssh.md) "Terminal handling" — not
duplicated here.

> **Stability: B (watch) — one OPEN wedge.** A `TIOCSWINSZ`/`TIOCGWINSZ`
> propagation bug was fixed across both SSH paths as recently as Jul 5 2026;
> otherwise low, steady churn (mostly new ioctl codes for audio/tap features,
> not firefighting). The recurring lesson: **`TerminalState` is per-PTY-spawn,
> not per-session** — a `pty` spawn gives the child a fresh `Arc`, so a
> multiplexed daemon (sshd) must reach the child's state through its
> `ChildStdout(child_pid)` fd, not its own.
>
> **OPEN (2026-08-10):** the blocking stdin-read path
> (`poll_input_event` here + `read`'s Stdin arm) takes
> `term_state_lock`/`input_waker` with `disable_preemption()` (not IRQs
> masked) and can spin there long enough under SMP contention to wedge the
> whole VM — see "Blocking stdin read" below and
> [`archive/DEVBOX_ISSUES.md`](../../../archive/DEVBOX_ISSUES.md) Issue 2.

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

## Blocking stdin read — `poll_input_event` (and `read`'s Stdin arm)

`sys_poll_input_event` (`src/syscall/term.rs:376-448`) and `sys_read`'s
`Stdin` arm (`src/syscall/fs.rs:384-453`) share one poll-wait shape: register
a waker, try to drain, park via `schedule_blocking`, clear the waker on the
way out. Both block in the kernel under `smp-shared`.

The **waker handshake** is the load-bearing part. It is built around two
critical sections taken with **preemption disabled** (not IRQs masked),
around the per-process `Arc<Spinlock<TerminalState>>` and its nested
`input_waker: Spinlock<Option<Waker>>`:

```mermaid
sequenceDiagram
    participant R as Reader (poll_input_event / read Stdin)
    participant TS as term_state_lock<br/>(Spinlock<TerminalState>)
    participant IW as input_waker<br/>(Spinlock<Option<Waker>>)
    participant W as Writer<br/>(write_to_process_stdin / close_process_stdin)
    participant S as schedule_blocking<br/>(park + sticky-wake)

    Note over R: loop until data / deadline / signal
    R->>R: disable_preemption()
    R->>TS: lock()
    R->>IW: set(thread_waker)
    R->>R: enable_preemption()
    Note over R: preemption off ONLY for the register
    R->>R: read_stdin() — non-blocking drain
    alt data available
        Note over R: break loop, return bytes
    end
    R->>S: schedule_blocking(deadline)
    Note over R: park; preemption force-enabled<br/>inside schedule_blocking
    Note over W: somewhere on another core,<br/>BKL-held writer arrives
    W->>W: disable_preemption()
    W->>TS: lock()
    W->>IW: take() -> wake(R)
    W->>R: sticky-wake (WOKEN_STATES)
    W->>W: enable_preemption()
    Note over R: scheduled back, re-enters loop
    R->>R: disable_preemption()
    R->>TS: lock()
    Note over R: ← term.rs:432 / fs.rs:448<br/>WATCHDOG FIRES HERE if contended
    R->>IW: take() (clear stale waker)
    R->>R: enable_preemption()
```

### Hazard: preemption-disabled spin on a contended inner lock

The two `disable_preemption()` → `term_state_lock.lock()` blocks are the
fragile spots. `disable_preemption()` only raises this thread's per-thread
preemption counter (`PREEMPTION_DISABLED`, `threading/mod.rs:1791`) — it does
**not** mask IRQs and is **not** a cross-core lock. If the spinlock is
contended when the reader reaches line 432 (post-wake cleanup), the reader
spins with preemption disabled, and the preemption watchdog
(`PREEMPTION_DISABLED_SINCE`) accumulates without bound.

This is the precise site of the open `meow`-TUI wedge documented in
[`archive/DEVBOX_ISSUES.md`](../../../archive/DEVBOX_ISSUES.md) Issue 2
(watchdog: `disabled at src/syscall/term.rs:432` for 94 s) and is the same
non-compliance that [`locking.md`](../locking.md) § "The per-syscall BKL
opt-out list" → gate 2 lists as **blocking the conversion of `read`'s Stdin
arm** off the BKL: every `terminal_state`/`input_waker` site uses
`disable_preemption()` rather than `with_irqs_disabled()`, so a nested IRQ's
unconditional `enter_kernel()` can deadlock AB-BA against a peer holding the
BKL and waiting on this lock. Full mechanism analysis (root-caused) and the
decided fix plan (bound the spin with a per-attempt `try_lock`, restructure
the waker handshake) live in
[`archive/TERM_POLL_INPUT_PREEMPTION_FIX.md`](../../../archive/TERM_POLL_INPUT_PREEMPTION_FIX.md).

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
- `archive/DEVBOX_ISSUES.md` Issue 2 — the live `meow`-TUI wedge pinpointing
  `term.rs:432`. `archive/TERM_POLL_INPUT_PREEMPTION_FIX.md` is the
  mechanism analysis of the lock pattern that causes it, and the fix plan.
