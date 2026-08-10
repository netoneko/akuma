# Process-per-session sshd (`fork-sessions`)

**Stability: B — verify behaviour.** Landed 2026-08-10, **off by default**.
Serves each SSH connection from its own forked process instead of as one future
in a single-process cooperative executor.

## Turning it on

Both halves, or neither:

```bash
SSHD_FORK_SESSIONS=1 userspace/build.sh --sshd-only          # userspace half
SSHD_FORK_SESSIONS=1 scripts/build_devbox_smoltcp.sh          # kernel half
SSHD_FORK_SESSIONS=1 overlays/devbox/run-smoltcp.sh           # and to run it
```

`run-smoltcp.sh` issues its own `cargo run`, so it needs the variable too —
without it the run rebuilds the kernel *without* `many-sessions` and silently
replaces the image `build_devbox_smoltcp.sh` just produced.

The kernel half is the `many-sessions` feature: it raises the per-listener
backlog from 8 to 32 and, on the size-constrained profiles, the smoltcp socket
budget from 32 to 128. Without it the stack RSTs past 8 simultaneous arrivals
and sshd cannot reach its own `max_sessions` of 24 no matter how it is built.

## What it does

`accept()` → `fork()` → the child closes the listener and owns that connection
for its whole life; the parent drops its copy of the accepted fd and goes back
to accepting. Children are reaped with `wait4(-1, WNOHANG)` via
`libakuma::wait_any()`.

`max_sessions` (default 24, `sshd.conf` `max_sessions=` or `--max-sessions`)
caps live session processes; over it, connections are closed immediately rather
than queued. 24 is chosen against the kernel's global `MAX_PROCESSES = 64`: a
fully-occupied session costs two slots (the sshd child plus the shell it
spawns), so 24 sessions is a 48-slot worst case.

## Why fork, and why it needed no kernel work

`docs/MISSING_SOCKET_MACHINERY.md` concluded that handing an accepted socket to
another process was unbuildable. It surveyed `sys_spawn`'s ABI, `SCM_RIGHTS` and
`/proc/<pid>/fd/<n>` and was right about all three — but with `fork()` no fd is
handed anywhere, it is inherited:

- `sys_clone` routes `flags & 0xFF == 0x11` to `fork_process`
  (`src/syscall/proc.rs`).
- `fork_process` sets `fds: Arc::new(parent.fds.clone_deep_for_fork())`.
- `clone_deep_for_fork` calls `socket_clone_ref` on every `Socket` fd
  (`crates/akuma-exec/src/process/fd.rs`).
- `remove_socket` refcounts on close, with a comment written for exactly this
  case: *"a fork child's exit no longer tears the socket out from under the
  parent's live fd"* (`crates/akuma-net/src/socket.rs`).

All of that predates this work. What was genuinely unknown was whether a
`no_std` libakuma binary could fork at all — `elftest` issues a raw `clone()`
but only `CLONE_VFORK|CLONE_VM` immediately followed by `execve`, so no libakuma
code had ever run in a forked child's own address space.
`userspace/forkprobe` settles that and stays in the tree as the regression
guard: it checks the fork itself, CoW isolation of post-fork heap writes, an
`accept()`ed socket used from a forked child (twice, sequentially, to exercise
the refcount), and 24 concurrent children.

## What it buys

- **Fault isolation.** `panic = "abort"` is process-wide, so under the
  cooperative executor one malformed packet on one connection killed every live
  session. Now it kills one.
- **Multi-core use.** Sessions are separate processes on the real scheduler.
- **Blocking is no longer contagious.** The standing rule that nothing reachable
  from a session may call `sleep_ms` (see `yield_now` in `main.rs`) stops being
  load-bearing — getting it wrong now slows one session instead of all of them.

## Verification

`scripts/sshd_concurrency_test.py` (devbox-smoltcp, `SMP=4`, 2026-08-10):

| Test | Result |
|---|---|
| A. isolation — 16 concurrent sessions, no cross-talk | PASS, 2.2s (serial floor ~32s) |
| B. starvation — 6×15-tick sessions vs 40 short ones | 6/6 complete every tick, 15.6s against a 15s floor |
| C. cap — 32 simultaneous vs `max_sessions=24` | exactly 24 served, 8 refused, server healthy after |
| D. fault isolation — SIGKILL one session | exactly 1 peer cut short, 3/4 unaffected, server serving |

Backlog sweep, same VM: 8/8, 16/16, 24/24 with `many-sessions`; **8/8, 12/16,
17/24 without it** — which is what pins the RST failures on the backlog rather
than on forking.

## Known open item

Under test B's specific mix (long-lived sessions plus rapid short-connection
churn), roughly **1% of connections fail at setup** — `ssh` exits 255 with an
empty stderr. Measured 3 failures in ~276 connections across 6 runs; the
cooperative build showed 0 in the same ~276. That is suggestive but not
conclusive at this sample size, and it did not reproduce at all in 192
connections at a steady concurrency of 16, nor in 92 connections replaying the
same long/short mix by hand — only through the full test-script path.

Timings never degrade (every B run lands within 0.6s of the 15s floor), so this
is a connection-setup failure, not starvation. Unresolved; it is a reason the
feature is gated off by default.

## Background

- `OPTIONAL_PARALLELISM.md` — the design note this implements, and where its
  blocking assumption was wrong.
- `docs/MISSING_SOCKET_MACHINERY.md` — the fd-passing survey, now carrying a
  superseded-for-this-use-case header.
- `LIMITATIONS.md` §1-§2 — the cooperative model's constraints, which still
  describe the default build.
- `PROTOCOL_UNDER_LOAD.md` — the crash that motivated wanting fault isolation.
