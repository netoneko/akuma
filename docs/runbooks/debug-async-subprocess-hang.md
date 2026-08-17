# An async runtime's child process never completes

Use this when a guest program spawns a child, the child **runs and exits fine**,
and the parent never notices — `tokio::process::Command::output()`, Go's
`exec.Cmd`, or anything else that reads a child's stdout through an
**edge-triggered** `epoll`. The shape recorded in
[`../archive/TOKIO_PIPE_EPOLL_HANG.md`](../archive/TOKIO_PIPE_EPOLL_HANG.md):
`nca` reported `execute_bash — Command timed out after 30s` for every shell tool
call while the kernel log showed the child exiting in 50 ms.

> **Two defects behind this symptom were root-caused and fixed 2026-08-17.**
> If a child is hanging *now*, first confirm you are on a kernel that has them —
> the boot log must show `[Test] pipe_read_nonblock_returns_eagain PASSED` and
> `[Test] epoll_pipe_eof_edge_after_partial_drain PASSED`. A fresh hang on a
> kernel with those is a **new** defect, and the ladder below is how to localise
> it.

Symptoms that land you here:

- a tool/command times out at exactly the caller's own timeout, every time
- the same command run from a shell over SSH returns instantly
- the kernel log shows `[PROC-EXIT]` for the child and a successful
  `wait4`/`waitid` — the parent *did* reap it
- a `[PIPE-DUMP]` shows the child's pipe at `bytes=0 readers=1 writers=0`:
  drained, at EOF, and nobody coming back for it

## Rule out the obvious first

- **Is the child even running?** `[syscall] execve(...) PID n` followed by
  `[AS-EXEC] pid=n`. A PATH walk that ends in `execve: failed to read` for every
  entry is a missing binary, not this bug.
- **Is it slow rather than hung?** Compare `[PROC-EXIT]` for the child against
  the caller's timeout. This runbook is for "exited in milliseconds, caller
  waited 30 s".
- **Is it a lost *scheduler* wakeup?** It is not.
  `sys_epoll_pwait` re-scans every fd on a bounded interval whether or not a
  `Waker` ever fires, so a watcher cannot sleep through a state change
  indefinitely. If the fd is ready and nothing is delivered, the **readiness
  oracle** is lying — that is where to look.

  > The bound is **not** `BLOCKING_POLL_INTERVAL_US` = 10 ms, as this runbook
  > originally claimed. Measured on the pre-2026-08-18 kernel it is ~35 ms and
  > grows with runnable-thread count, because the cap sets when a thread
  > becomes *eligible*, not when it runs. See
  > [`../archive/SCHEDULING_INVESTIGATION.md`](../archive/SCHEDULING_INVESTIGATION.md)
  > §5. **Fixed 2026-08-18** (wake-deadline preemption + 1 ms tick): the
  > re-scan bound is now ~1 ms on all profiles except `extreme-size`, where it
  > is one 10 ms round. The logic is unaffected; only the number changed.

## What was already found

Both fixed 2026-08-17. Knowing their shapes is most of the value here.

| # | Defect | How it presented | Fix |
|---|---|---|---|
| 1 | `read()` on an `O_NONBLOCK` pipe **ignored the flag and blocked** | the reactor thread stalled inside the kernel until the child closed the pipe; only the `PipeRead` arm was affected, `ChildStdout` and `UnixSocket` already honoured it | `sys_read` checks `fd_is_nonblock` → `EAGAIN` |
| 2 | A pipe's `EPOLLET` `EPOLLIN` edge **could never be re-armed**, so the EOF transition was swallowed | first edge delivered, then `SUPPRESSED` forever; `read_to_end` waited on a pipe already at EOF | `sys_read` calls `epoll_on_fd_drained`; read ends report `EPOLLHUP` when the last writer goes |

Defect 2 is the **same class** as defect 3 in
[`debug-delayed-first-byte.md`](debug-delayed-first-byte.md) — the `EPOLLOUT`
edge that was never re-armed on sockets. If you are chasing a new one, that
class is the first thing to suspect: `epoll_check_fd_readiness` computing a
readiness bit that never *transitions*, so `revents & !last_ready` is always 0.

## The ladder

### 1. Confirm it is the kernel, not the runtime

Do this first; it is one command and it halves the search space.

```bash
userspace/ncaprobe/build-musl.sh --serve
```

```bash
# guest
curl -s -o /tmp/ncaprobe http://10.0.2.2:8899/ncaprobe && chmod +x /tmp/ncaprobe
/tmp/ncaprobe tokio
```

```bash
# host — the SAME binary on real Linux
cd userspace/ncaprobe/target/aarch64-unknown-linux-musl/release
docker run --rm --platform linux/arm64 -v "$PWD":/p:ro alpine /p/ncaprobe tokio
```

Passing on Linux and timing out on Akuma is proof of a kernel defect, and it
retires "maybe the app is misusing the API" permanently. See
`userspace/ncaprobe/README.md`.

### 2. Reduce to the mechanism

```bash
/tmp/ncaprobe eofedge
```

A child writes, stays alive 1 s, then exits; the probe takes the `EPOLLIN` edge,
drains, and waits for the EOF edge — `read_to_end`'s exact pattern. Healthy:

```
round 1: READY events=IN
   read = 6 "EARLY\n"
   read = EAGAIN -- back to epoll_wait for the EOF edge
round 2: READY events=IN|HUP (0x11)
   read = 0  EOF -- read_to_end can finish
```

Two things to check on that output, each pointing at one of the known defects:

- the second `read` must return **`EAGAIN` immediately**. If it instead blocks
  and eventually reports `read = 0 EOF` a second later, `O_NONBLOCK` is being
  ignored (defect 1's shape).
- a second edge must arrive at all, and carry **`HUP`**. `RESULT: *** EOF edge
  NEVER delivered ***` is defect 2's shape. It may land in round 2 or round 3 —
  the watcher re-scans on a 10 ms tick, so a `-> 0 (no edge)` round before it is
  normal, not a symptom.

### 3. Isolate the layer

Each subcommand eliminates one layer, so a pass is permanent progress:

```bash
/tmp/ncaprobe epoll main          # raw pipe+spawn+epoll(ET)+pidfd
/tmp/ncaprobe epoll thread        # ... on a worker thread
/tmp/ncaprobe epoll main --late   # child exits BEFORE registration
/tmp/ncaprobe cross               # epoll_wait parked while another thread ADDs
/tmp/ncaprobe waitid              # waitid(P_PIDFD) — the reaping call
/tmp/ncaprobe fds                 # open fds, which are epolls, aliasing
```

Run each on Linux too. All six pass on a healthy kernel.

### 4. Make the kernel say it

Set `SYSCALL_DEBUG_EPOLL_EDGE = true` in `src/config.rs`, rebuild, reboot. One
line per ready fd per scan. The signature of a lost edge is a single `deliver`
followed by an unbroken run of `SUPPRESSED` on the same fd:

```
[T21.07] [epoll] ET epfd=3 fd=9 rev=0x1 last=0x0 new=0x1 deliver
[T21.20] [epoll] ET epfd=3 fd=9 rev=0x1 last=0x1 new=0x0 SUPPRESSED    ... forever
```

`rev` is readiness now, `last` is the edge already reported, `new` is what the
caller gets. `rev != 0` with `new == 0` for the whole hang **is** the bug: the
kernel knows the fd is ready and is choosing not to say so.

Prefer this over `SYSCALL_DEBUG_NET_ENABLED`, which floods the same trace with
TCP/UDP/DNS noise.

### 5. Read the corroborating dumps

- `[PIPE-DUMP]` — `bytes=` answers "was this drained" honestly. `writers=0` means
  EOF is available *now*.
- `[THR-DUMP]` — `tsc=` is the syscall the thread is in (22 = `epoll_pwait`,
  98 = `futex`); `a0` is its first argument, so `a0=0x3` is `epfd=3`.
- **`PSTATS` is per-thread, not per-process.** "The process never calls `read`"
  read off the main thread's counters is a mistake this investigation made — the
  reads were on the worker.
- **`interest_fds=` in `[epoll] pwait ret` is always 0** and means nothing; its
  only caller passes a literal.

## Verify

A fix is done when all of these hold:

1. `cargo clippy --release -- -D warnings` and the `extreme-size` clippy line
   from `.git/hooks/pre-commit` are both clean — the pipe readiness path
   compiles in a build with no `sc-epoll`.
2. Boot log shows `[Test] pipe_read_nonblock_returns_eagain PASSED` and
   `[Test] epoll_pipe_eof_edge_after_partial_drain PASSED`, and the suite is
   otherwise green.
3. `/tmp/ncaprobe tokio` completes all three calls with real stdout, e.g.
   `OK in 197ms status=Some(0) stdout="TOKIO_OUT\n"`.
4. `/tmp/ncaprobe eofedge` reports `EAGAIN` then `round 2: READY events=IN|HUP`.
5. `/tmp/ncaprobe epoll main` finishes `ALL DONE after 1 rounds`.
6. The same three probes still pass under Docker on Linux — if a change makes
   Akuma pass and Linux fail, the probe was changed, not the kernel fixed.
7. SSH still works and `[herd] Started sshd` still appears. The pipe read path
   is what sshd's own bridge uses; breaking it takes the box off the network.

Numbers a healthy kernel produced on 2026-08-17 (`MEMORY=2048`, `-smp 1`):

| Measurement | Result |
|---|---|
| `ncaprobe tokio` (3 calls) | all OK, ~200 ms each |
| `ncaprobe eofedge` | `EAGAIN` at ~75 ms, `IN\|HUP` at ~1.1 s (round 2 or 3) |
| `ncaprobe epoll main` | `ALL DONE after 1 rounds` |
| `ncaprobe tokio` on Linux | all OK, ~1 ms each |

## Background

- [`../archive/TOKIO_PIPE_EPOLL_HANG.md`](../archive/TOKIO_PIPE_EPOLL_HANG.md)
  — the original investigation, the eight ruled-out hypotheses, and the method
  notes.
- [`debug-delayed-first-byte.md`](debug-delayed-first-byte.md) — the
  network-client shape of the same edge-triggered class.
- [`../archive/SOCKET_DELAYED_FIRST_BYTE_HANG.md`](../archive/SOCKET_DELAYED_FIRST_BYTE_HANG.md)
  — where `epoll_on_fd_drained` / `epoll_on_fd_write_blocked` came from.
- [`../reference/subsystems/config-flags.md`](../reference/subsystems/config-flags.md)
  — `SYSCALL_DEBUG_EPOLL_EDGE` and the other tracing knobs.
- [`../../userspace/ncaprobe/README.md`](../../userspace/ncaprobe/README.md)
  — the probe, the Linux A/B, and how to add another.
