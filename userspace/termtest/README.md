# termtest

Terminal-syscall exerciser for Akuma. Two modes:

- **Default (no args):** a short, manual/interactive smoke test of the
  terminal syscalls — get/set terminal attributes, raw mode, cursor
  positioning, screen clear, and both non-blocking and blocking
  `poll_input_event`. Run it over SSH and type when it asks.
- **`--stress [N]`:** a self-contained reproduction of the
  `poll_input_event`/`term_state_lock` preemption wedge documented in
  [`docs/archive/TERM_POLL_INPUT_PREEMPTION_FIX.md`](../../docs/archive/TERM_POLL_INPUT_PREEMPTION_FIX.md).
  No human typing or second SSH session required.

## Running

```
ssh -p 2222 root@localhost /bin/termtest              # interactive smoke test
ssh -p 2222 root@localhost /bin/termtest --stress      # stress mode, default child count
ssh -p 2222 root@localhost /bin/termtest --stress 12   # stress mode, 12 children
```

For the cross-core shape the wedge doc describes, boot with at least two
cores (`SMP=2` or higher) before running `--stress`.

## How `--stress` reproduces the wedge

`fork()` (`crates/akuma-exec/src/process/mod.rs`) clones the parent's
`terminal_state`, `channel`, and `stdin` as shared `Arc`s into the child —
unlike a pty spawn (e.g. a login shell under `sshd`), which deliberately mints
a **fresh** `TerminalState` per session so concurrent SSH sessions don't share
one `input_waker` slot. Plain `fork()` does not make that distinction, so
`termtest --stress` uses it to put N children on the exact same
`Arc<Spinlock<TerminalState>>` on purpose.

Each child then loops:

1. A blocking `poll_input_event` call with a short timeout — the exact
   register/wait/clear loop the wedge doc's mechanism analysis (§9) is about.
2. A terminal ioctl (`TIOCSWINSZ` or the `TCGETS`-equivalent
   `get_terminal_attributes`) — a second, independent path that takes the same
   `term_state_lock`.
3. A heartbeat print every 50 iterations, then exits after 300 iterations.

The parent forks all N children up front, then polls `waitpid_status` on each
one, printing progress, until either everyone has exited or a 30-second join
timeout expires.

**Healthy run:** every pid prints its heartbeats, all N children exit, and the
parent prints `PASS`.

**Wedged run:** one or more pids stop printing heartbeats mid-run (they are
spinning inside the kernel, not making syscalls), `qemu` CPU usage rises, and
the SSH session itself typically stops responding — the same signature as the
original incident (`docs/archive/DEVBOX_ISSUES.md` Issue 2). If the SSH
session survives long enough to see it, the parent's join loop reports exactly
which pids never finished within the 30-second timeout and prints `FAIL`.

## Background

- [`docs/archive/TERM_POLL_INPUT_PREEMPTION_FIX.md`](../../docs/archive/TERM_POLL_INPUT_PREEMPTION_FIX.md)
  — the mechanism analysis and fix this test exists to exercise.
- [`docs/reference/subsystems/syscalls/term.md`](../../docs/reference/subsystems/syscalls/term.md)
  — current-state reference for the terminal syscalls, "Blocking stdin read".
