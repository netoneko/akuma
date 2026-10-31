# Taoxichuan: `cargo build` stalls after the first `rustc -vV` (regression hunt, 2026-10-31)

**Status: OPEN - cause not found; first bad commit bracketed to four commits.** Stability: C.

## Symptom
In the self-hosting guest (p3 root, `/usr/local/rust`, `HOME=/root`), `cargo build --release --target x86_64-unknown-none -p taoxichuan --features no-tests,trace-err -j4`
never creates `/tmp/tgt`. The console shows cargo exec `rustc` **once** (`rustc -vV`), then nothing; the vCPU sits at 100%. The fixed point
(`TAOXICHUAN_FIXED_POINT.md`, kernel `6af48c06`) built the same tree in 5m22s on the same disk.

## What the stall is
A periodic dump (`trace-err` builds; `main.rs`'s tick handler and `sock.rs`'s `poll_note`) caught cargo's main thread (slot 4) spinning:

```
[poll#14] s4 ready=1 5:1->1[R] 7:1->0[R]        fd 5 and 7 are pipe READ ends, POLLIN asked, fd 5 answers readable
[poll#15] s4 ready=2 5:1->1[R] 7:1->1[R]        then both, and so on for 30,000+ calls
[hang] s4 nr=7 a=0x6fffffff6994,0x2,-1 -> 2 rip=0x300698f3     (the last 40 syscalls of slot 4 are all this one)
```
`poll(fds, 2, -1)` returns "2 ready" every time and **no `read` follows - not even the first one that reported data**. The slots 5-11 the dump shows `IN nr=231`
(`exit_group`) are exited children awaiting reap: `INFLIGHT` is set at syscall entry and an exiting task never clears it, so that line is stale, not a hang.
`timeout=-1` takes `poll_arm`'s unbounded branch (a 60 s cap plus the thread-interrupt check); a user-visible `-1` is the only argument cargo's loop passes.

## Not the cause (each tested)
* **SMP.** The same stall at `smp=1`, and on the kernel from the start of the session (`1c89a293`, before any SMP commit). The SMP, `rt_sigsuspend`, sshd-keepalive and
  extraction work all landed after the regression exists.
* **`exit_group` from a thread** - `userspace/forktest/c_stress/groupexit_thread.c`, three modes, rc=3 each (docs: `AKUMA_AMD64_AGENT_STAGING_AND_ACCOUNTING.md` § 13).
* **Pipe wake / spawn** - `pipewake.c` (all phases ok), `spawnprobe.c` (PASS), `spawnalias.c` (3000 rounds, no divergence).
* **Poll + EOF on child pipes** - `scripts/kvm/taoxichuan/pollhup.c`, a copy of std's `read2` loop (two pipes, child exits, read to EOF): consumes both pipes and reaps the child.
  It answers `POLLIN` for EOF where Linux says `POLLHUP`, which this loop tolerates.
* **`rustc` itself** - `rustc -vV` run directly in the guest prints its version.

## The bisect (single run each, judged by `rustc` execs in the guest console after ~100 s; `scripts/kvm/taoxichuan/bis_run.sh`)
| commit | what it is | `rustc` execs | verdict |
|---|---|---|---|
| `6af48c06` | the fixed point (K0, cooperative sshd) | many, 3 in parallel | good |
| `eadfee8c` | init is pid 1 on a pool thread; sshd forks per session | 40 | good |
| `51533d4d` | the epoll family + `prctl` | 41 | good |
| `cf44f840` | `glm` WIP: edge-trigger drain hooks, **O_NONBLOCK pipe read/write**, `console_session` | 1 | **bad** |
| `d6ee7d97` | threading: fresh-thread trampoline releases the outgoing `ON_CPU` gate | 1 | bad |
| `68575856` | epoll reports a listener's accept edge | 1 | bad |
| `1c89a293`, HEAD | eventfd re-arm; this session | 1 | bad |

First bad commit is one of `3f5069cc`, `db199554`, the merge `8faf6580`, `cf44f840` - **`3f5069cc`/`db199554`/`8faf6580` are not yet built or tested individually.**
`git diff 51533d4d cf44f840 -- taoxichuan/src/pipes.rs` gives `read_arm(.., nonblock)` / `write_arm(.., nonblock)`: a non-blocking pipe read of an empty pipe now answers
`EAGAIN` instead of parking, and a full non-blocking write likewise. Cargo's `read2` puts both child pipes into `O_NONBLOCK` and then polls.

## Leading hypotheses (none proven)
1. The new `nonblock` argument is read from the wrong place for the `read` arms cargo reaches (the `a1`/`fd` bookkeeping of the drain hook shares the syscall site), so a
   non-blocking `read` is answered before it reaches the pipe, or it never issues - but then a `read` would still be in the syscall ring. It is not. **This does not explain the
   missing `read`.**
2. The loop is not `read2` but a helper thread's wait (jobserver / the interrupt path `tkill` -> `set_thread_interrupted`, which `glm` edited): fds 5 and 7 would be the
   token pipe and a shutdown pipe, and revents it never consumes. `poll_arm` also returns `EINTR` from `take_thread_interrupted`, which `cf44f840`'s hooks interact with.
3. A fork/spawn change leaves the parent holding an end it should have closed, so a pipe never reports its state the way the loop expects.

## Next
Build and test `3f5069cc`, `db199554`, `8faf6580` to name the commit; run `outputprobe.rs` (std `Command::output()` of `rustc -vV`, written, not yet run - the guest's `rustc`
compile of it produced no output) and the jobserver path; compare the syscall ring against the good kernel at the same point.

## Method notes
Tools: `scripts/kvm/taoxichuan/README.md`. A trace build prints the first 16 polls then every 4096th with `fd:events->revents[kind]`, and dumps the last syscalls of every slot at the 100th poll
and every 30 s of uptime (eight times). The ssh client drops a session that goes quiet for minutes and the guest build survives it - judge by the guest, not by the client.

## Background
`TAOXICHUAN_FIXED_POINT.md`, `TAOXICHUAN_SMP.md`, `J4_HANG_LIVE_AUTOPSY.md` (the same family on the aarch64 kernel: a loop polling pipes that never delivers the event it waits for).
