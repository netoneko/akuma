# `clock_gettime` answered `EINTR`: codex's TUI panicked on a terminal resize (2026-11-17)

## Symptom

`codex` (0.169.0, a static Rust binary) launched from herdr on the ryzen box drew its TUI and then
crashed, with `Interrupted system call`. Reproduced on the amd64 kernel with a pty and no herdr:
start `codex --no-daemon` on a pty, then resize it and `kill -WINCH` it a few times.

```
The application panicked (crashed).
Message:  called `Result::unwrap()` on an `Err` value: Os { code: 4, kind: Interrupted, ... }
Location: library/std/src/sys/pal/unix/time.rs:101
```

`time.rs` is `Timespec::now`, i.e. `Instant::now()`/`SystemTime::now()`, which is
`clock_gettime(..).unwrap()`. Linux's `clock_gettime` never fails with `EINTR`, and std relies on it.
(`codex exec` never hit it: no TTY, so no `SIGWINCH`. Without `--no-daemon` the TUI exits earlier with
codex's own "no complete local package" error, which is a packaging message, not this bug.)

## Cause

`akuma-syscalls-glue`'s syscall prologue answers `EINTR` to **every** syscall while the thread's
interrupt flag is set, except the signal-state and signal-sending calls (`never_interrupted`).
The flag is raised when a signal is posted (`interrupt_thread`) and is cleared by
`signal_frame_installed` when a handler frame is built. Between the two, the thread's next
syscall — any syscall — sees the flag, and `is_current_interrupted` **consumes** it, so the `EINTR`
meant for a blocking wait goes to whatever syscall ran first.

For a terminal resize the window is real: herdr signals codex, codex's tokio workers are all calling
`clock_gettime` for timers, and one of them lands between the post and the delivery.
The earlier `alarm(1); pause(); getpid()` fix cleared the flag *after* delivery and left this window.

## Fix

`never_interrupted` now also lists the calls that cannot block and that Linux never fails with
`EINTR`: `clock_gettime`, `clock_getres`, `getpid`/`getppid`/`gettid`, the uid/gid getters, `uname`,
`sysinfo`, `getrandom`, `getrlimit`/`prlimit64`, `getrusage`, `times`, `sched_yield`,
`sched_getaffinity`, `set_tid_address`. They no longer consume the flag, so the blocking call it was
meant for still sees it. Shared glue, so both architectures.

## Verification

- `cargo build -p akuma --target x86_64-unknown-none --release --no-default-features --features headless` builds.
- **Not yet boot-verified.** The box this was found on was running the unfixed kernel, and there was
  no QEMU or `/dev/kvm` to boot the fixed one. To check: boot the new kernel, run the pty repro
  (`codex --no-daemon` on a pty, 30 resizes with `SIGWINCH`), expect no panic.

## Still open

The list is a patch on a design that is the wrong way round: a prologue that returns `EINTR` for
every syscall not on an exclusion list. Blocking calls already test `should_interrupt_blocking_syscall`
themselves; the prologue check should become an allowlist of the calls that rely on it, and any new
non-blocking syscall is otherwise exposed to this again. Linux also restarts `SA_RESTART` waits
instead of failing them.

## Background

`SIGNAL_FATAL_DEFAULT_LINUX.md`, `PTHREAD_KILL_EINTR_DELIVERY_STARVATION.md`,
`CTRL_C_SIGINT_DELIVERY.md`.
