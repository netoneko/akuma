# Pipes — `akuma-syscalls-glue`'s

**Stability: B** (the bodies are glue's, shared with amd64 and AArch64; this
kernel's part is the wiring, verified by the battery's pipeline and `PK1` lines).

Since the cleanup plan's step 2b (2026-11-01) a pipe on this kernel is glue's:
`pipe`/`pipe2` create it in glue's one `akuma_pipes::PipeTable`, the descriptors
are `FileDescriptor::PipeRead`/`PipeWrite` in the process's `SharedFdTable`, and
`read`/`write`/`poll` are glue's arms (through `akuma-syscalls-glue-amd64`).
What this kernel supplies:

- **Release hooks** (`exec_twin.rs`): `pipe_close_read`/`pipe_close_write`/
  `pipe_clone_ref`/`pipe_write` are glue's own, so a fork's deep clone takes a
  reference per end, and every close, `dup2` displacement, close-on-exec sweep
  and exit drops one.
- **The wake sink** (`main.rs`, beside the vfs-glue mount): a parked reader or
  writer is woken by slot number with a fresh handle — amd64's `pipe::wake_sink`.
- **Exit closes the table before the reap** (`child::close_process_fds`): a
  pipeline's reader gets EOF when its last writer *exits*, not when the writer's
  parent gets round to `wait4`.
- **SIGPIPE**: a write with no reader left raises SIGPIPE, whose default action
  ends the writer inside glue; the slot-purge hook reports that to `wait4`
  (`child::on_thread_terminated`).

The refcount and EOF rules themselves are `akuma-pipes`' (host-tested).

## Background

[`TAOXICHUAN_CLEANUP_PLAN.md`](../../../docs/archive/TAOXICHUAN_CLEANUP_PLAN.md)
(step 2b: the move, and the `yes | head` hang the SIGPIPE bridge fixes);
`docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md` (stage 8 M3, the
pipes this replaced).
