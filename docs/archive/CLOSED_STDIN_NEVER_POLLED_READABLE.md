# A closed stdin never polled readable (2026-11-12)

## Symptom

On the amd64 `server` image (no interactive console), `init=/bin/busybox initargs=sh` started,
printed its prompt and then sat forever: 92 syscalls and no more, never `-- init exited --`. That
image closes init's stdin on purpose, so `read(0)` returns 0 and a shell should exit at once.

## Cause

Two halves, both in shared code, so both kernels had it:

1. **Readiness.** glue's `Stdin`/`DevTty` arm (`crates/akuma-syscalls-glue/src/poll.rs`) reported
   `FdState::Stdin { has_data: ch.has_stdin_data() }`. A closed stdin with an empty buffer read as
   "nothing to read", though `read` on it returns 0 immediately
   (`crates/akuma-syscalls-glue/src/fs.rs`, the `is_stdin_closed` arm).
2. **Wake.** `ProcessChannel::close_stdin` (`crates/akuma-exec/src/process/channel.rs`) only stored
   the flag. A thread already parked in `poll` on that stdin was never woken, unlike
   `write_stdin`, which wakes the pollers.

busybox ash's line editor polls fd 0 before every read, so it never reached the `read` that would
have returned 0. A shell without a terminal on fd 0 reads directly and was never affected, which is
why nothing caught this: `sshd` closes a session's stdin (`close_child_stdin`, 326) on client EOF,
but a session with a live client rarely sees that, and the boot suite's shells run `sh -c`.

## Fix

- glue: `has_data: ch.has_stdin_data() || ch.is_stdin_closed()`.
- `close_stdin` calls `wake_pollers()` after setting the flag.

Linux reports `POLLIN|POLLHUP` here; Akuma reports `POLLIN` only, recorded with the other
divergences in `docs/reference/subsystems/syscalls/poll.md`.

## Verification

- New amd64 boot test `closed_stdin_test` (`crates/akuma-kernel-tests-amd64/src/usermode_tests.rs`):
  spawn `sh -i` with `SPAWN_FLAG_PTY`, wait for its prompt, let it park, close its stdin, require
  it to exit. With both halves backed out it **fails** (`closed stdin: the parked shell saw EOF
  and exited [FAIL]`, SMP=2); with them it passes. Without `SPAWN_FLAG_PTY` the test passed even
  on the broken kernel: ash does not start its editor without a terminal, so the first version of
  the test proved nothing, and is kept only in its PTY form.
- `server` image, `init=/bin/busybox initargs=sh`: `-- init exited --` right after the prompt.

## Open

- `POLLHUP` is still not reported (the divergence above).
- AArch64 has no boot test of its own for this; the change is in shared code and the AArch64
  suite was run over it.
