# Pipes — pipe(2)/pipe2(2) over the pulled crate

**Stability: B** (the crate is host-tested and shared with the amd64 kernel;
the glue here is small and stable since stage 8 M3).

Everything that *decides* lives in
[`crates/akuma-pipes`](../../../crates/akuma-pipes): end refcounts, the 64 KiB
FIFO, the read/write/EOF rules. `src/pipes.rs` is what a composition root
owns: the one `PipeTable<()>` instance, the `OpenFile::Pipe(id, is_write)`
payload, and the copy loops (~110 lines).

```mermaid
flowchart LR
    subgraph task fd table (akuma-fdtable decisions)
        R["fd → Pipe(id, false)"] 
        W["fd → Pipe(id, true)"]
    end
    R <-->|read / EOF| T["akuma-pipes PipeTable<br/>id → 64 KiB FIFO + end refcounts"]
    W -->|write / EPIPE / EAGAIN| T
```

This figure is small enough to live only here (no `diagrams/` source).

## The refcount rule this kernel keeps by hand

One table reference per descriptor naming an end, because an `OpenFile` here
is a plain value (indices), not a handle:

- `pipe2` starts the pair at 1/1.
- `dup`/`dup2`/`dup3`/`fcntl(F_DUPFD)` and the fork arm's walk over
  `fork_copy`'s payloads each add one (`pipes::clone_ref`).
- `close`, a displaced `dup2` target, exec's cloexec sweep, and a task's exit
  (`child_exit` → `release_open_fds`) each drop theirs (`pipes::release`).

Miss fork or the exit and EOF never arrives; miss a dup and the pipe dies
under a live end. The load-bearing sweep is the exiting **fork child**'s — a
pipeline member's exit is what turns the last writer's close into the reader's
EOF.

## Semantics on a run-to-completion kernel

- Wakes are `()` dropped on the floor: nothing blocks (the M2 fork finishes
  the writer before the reader is forked; the scheduler that could block is
  M4's). No poller is ever registered.
- An empty pipe with live writers would block on Linux; here it reads as 0 —
  the same answer a blocking read gives once the buffer drains and the last
  writer closes. The two states are indistinguishable **by construction**.
- `BrokenPipe` is `EPIPE` (32, named locally — the shared err table's only
  other EPIPE user is the SIGPIPE path, which is out of scope). A full buffer
  that accepted nothing is `EAGAIN`.
- `write_no_sigpipe` is the kernel-memory twin for the `ExecRuntime::pipe_write`
  hook (akuma-exec's stdin pump — unreachable until `fork_process`): a full
  pipe answers `Ok(0)`, the short write that reports backpressure.
- `pipe` (x86_64 22, musl's raw `SYS_pipe`) is `pipe2(fds, 0)`; the dispatcher
  keys the arm on the literal because the ring3 table has no name for it.
- poll/select readiness uses the crate's own predicates (`readable`/
  `writable`) — no second opinion.

## Background

`docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md` (stage 8 M3),
`scripts/kvm/goose/taoxichuan-09a-pipes.md` (the session brief).
