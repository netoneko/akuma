# Tokio kernels: a live PTY reported hangup and stopped draining (2026-11-22)

## Symptom and cause

On Denver, `scala-cli run` under a Codex exec session could stall with the compiler and progress threads blocked writing to `/dev/pts/0`. The Codex event loop was parked in `epoll_pwait`; the PTY output buffer held 8192 bytes and its master reported `POLLHUP`, although JVM processes still held slave descriptors. The ordinary non-PTY run did not have this failure.

Both Tokio architecture runtimes wired `ExecRuntime::pty_clone_ref` and `pty_close` to no-ops. The shared descriptor code invokes the first on `dup` and `fork` and the second on descriptor-table cleanup. Explicit close in syscall glue, however, decrements the PTY's actual reference count. Thus closing a descriptor after an uncounted duplication could reduce the slave count to zero while other descriptors remained open. The resulting premature hangup and edge-triggered readiness state left the master undrained, eventually blocking writers. The same missing close hook could also leak references on descriptor-table cleanup. Denver, Taoxichuan and Shanghai use these Tokio runtimes on both architectures.

## Fix and verification

Both runtime tables now delegate the two hooks to `akuma_syscalls_glue::pty`. This uses the existing fixed-size PTY table and adds no allocation.

`userspace/forktest/ptyrefs/ptyrefs.rs` exercises master and slave `dup`, `fork`, output delivery and the final slave close; `overlays/ecosystem-demo/ptyrefs.py` runs it in a disposable guest. The probe passed on Linux, failed on the prior Denver build with premature `POLLHUP` after `dup`, and passed twice on the fixed Denver build. The separate non-leader exec leader-slot fix and its `execpost` probe are documented in [TOKIO_EXEC_LEADER_SLOT_REUSE.md](TOKIO_EXEC_LEADER_SLOT_REUSE.md).

A Codex-session JDK-only hello completed with exit 0 in 112 seconds on the fixed kernel, including a cold dependency download. The affected `scala-cli` command itself completed in the transcript; the overall agent response is not used as a substitute for its exit status.

The full `scripts/gate.py --with-jvm` passed, including both architecture builds and the Denver JVM boot gate. In that first gate run, the default Tokio kernels' `getcpu` results were expected XFAILs; Denver subsequently enabled `getcpu` by default.
The previously broken trial C project also returned under Codex in 96 seconds with exit 1 for its compile errors; this checks the command, not a rerun of the full agent trial.
A `ujson` hello also completed under Codex in 66 seconds with `UJSON_OK 7` and exit 0.
