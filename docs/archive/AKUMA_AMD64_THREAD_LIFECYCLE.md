# amd64: three lifecycle defects `cargo` found, and the two the probes found after

**2026-09-12.** Staging `cargo` in the guest (`RUST_TOOLCHAIN_AMD64.md` § session
4) turned a build into a lifecycle test, because that is what a build is: a few
hundred short-lived processes, each with a dozen threads, each exiting while
siblings are parked. Three defects fell out, none of which any boot self-test
could see. Then the probe suite — run for the first time on this target — found
two more that the build had been quietly surviving.

## 1. `DRAIN INCOMPLETE`: waking is not interrupting

```
[thread] DRAIN INCOMPLETE: 2 thread(s) still live in proc slot 7 — the reaper may free a live address space
```

`thread::drain` sets a group-exit flag, calls `wake_group`, and spins waiting
for the siblings to leave. `wake_group` called `sched::wake`, which makes a
parked thread **runnable** — and that is enough for exactly one kind of sibling:
one parked in the *futex* loop, because that loop re-checks `should_leave_now`.

A thread parked anywhere else — a pipe `read`, `poll`, the console — wakes,
re-evaluates **its own** condition (still no data), and parks again. It never
returns to syscall entry, which is the only other place `should_leave_now` is
tested, so it never learns the group is exiting. `drain` spun out its 100 000
rounds, printed the line above, and gave up; the process was never reaped, and
its parent waited forever.

The symptom was a `cargo` build that stopped dead at a random crate with
**nothing in cargo's own log** — the parent was in `wait4`, not failing. Killing
the stuck child by hand unstuck it, which is the tell: a real signal reaches a
parked thread and a wake does not.

**Fix:** `wake_group` arms `akuma_threading::request_thread_kill(tid)` alongside
the wake. That sets `PENDING_KILL`, which `should_interrupt_blocking_syscall`
reads — the one hook the park loop consults — so the sibling's wait returns
`EINTR`, unwinds to syscall entry and sees the flag. It is also what Linux does:
`exit_group` is a group-wide **kill**, not a group-wide nudge.

## 2. Inode mtimes divided by a million, twice

`amd64/src/fs.rs` passed `wall_clock_secs` — seconds — to `Ext2Filesystem::new`,
whose parameter is named `utc_time_us` and whose consumer is:

```rust
fn current_time(&self) -> u32 { ((self.time_fn)() / 1_000_000) as u32 }
```

So every inode this kernel wrote was stamped `epoch_seconds / 1e6`: **1789**,
i.e. 1970-01-01T00:29:49Z, and *constant for eleven and a half days at a
stretch*. Both values are `u64`, so nothing could catch it but the units in the
two names, which disagreed.

**The cost is `cargo`, and it is total.** Its fingerprinting is mtime-based:
sources staged from the host carry real 2026 mtimes, every artifact this kernel
writes carries 1970, so no output is ever newer than its input and **nothing is
ever up to date**. A no-op rebuild recompiles the world.

`proposals/NEXT_AGENT_AMD64_SELFHOST_FIRST_BUILD.md` § 3 ranked the clock
**first** among the three things most likely to stop `cargo`, and named the test
that finds it: *does a file written now have a plausible mtime?*

**The test that separates it from the other candidate** is worth keeping,
because `1789` also looks exactly like uptime-seconds on a box that has been up
1789 seconds — the numbers coincide because 1789 s is 1.789e9 µs and the epoch
is 1.789e9 s. Write two files two minutes apart: uptime advances, `epoch/1e6`
does not. It did not.

**Fix:** the function is `wall_clock_us` and returns `clock::now_us()`. A file
written now stamps `1789236660`, exactly equal to `date +%s`.

## 3. `Process table full (256 slots, 246 reclaimable RETIRED)`

A kernel panic that contradicts itself in its own message: 96% of the table was
collectable at the instant it gave up. The panic site
(`akuma-exec/src/process/table.rs`) is a **known open issue** — its comment says
`register_process` should return an error so spawn surfaces `EAGAIN`, blocked
only on threading a `Result` through every caller — and it requests a drain
before panicking, which cannot help the caller that is already dead.

What made it reachable here is amd64-specific: **every `pthread_create` on this
target registers a `Process` row** (`thread.rs::teardown`'s note), and all five
of this kernel's drain sites are on *process* exit/reap paths. None is on the
thread one. So a thread-churning workload retires rows that nobody collects.
`futextest` step 6 took the table from empty to full and panicked the box.

**Fix:** `sys_clone_thread` drains at **syscall entry**, before it claims
anything. Draining at teardown instead would be wrong for the reason
`register_process`'s own comment gives: reclaim runs `Process::drop`, which
frees page tables and releases an ASID, and a caller already holding those locks
self-deadlocks. Syscall entry is before any of that, and it is demand-driven —
the one path that consumes slots is the one that pays for collecting them.

## The probe suite, run on this target for the first time

Built for x86_64 from `userspace/forktest/c_stress/` (they existed only as
aarch64 binaries) and run in the Firecracker guest:

| probe | result |
|---|---|
| `futexops` | PASS — **0 divergences from Linux** |
| `futextest` | PASS — all 7 phases, 13 s (before fix 3: hung at phase 6, then panicked the kernel) |
| `futexkey` | PASS — a wake stays inside its own address space, both spellings |
| `futexkill` | PASS — `exit_group` → reaped in 0 ms, worst of 4 rounds |
| `pipewake` | PASS — all phases |
| `threadmax` | PASS — ceiling 64, 400× sequential spawn/join |
| `grandfork` | PASS — all steps |
| **`pthread_kill_eintr`** | **PHASE1 FAIL** — below |
| **`segvgroup`** | **FAIL from round 31** — below |
| `segvchild` | inconclusive: two cases pass, then it runs past 180 s |

## 4. A signal could not reach a parked thread at all — a stale `false`

```
PHASE1 FAIL: read() never returned within 100 attempts (handler ran 0 times)
```

`pthread_kill` to a thread blocked in `read()` neither ran the handler nor
returned `EINTR`. Worse, and the test that showed how general it was:
**`kill -9` on a process parked in a blocking read did nothing at all.**

`pend_signal_for_thread` sets the pending bit and `wake()`s the slot — which is
eligibility, not delivery. The thread wakes inside its wait, re-checks its own
condition, finds it unchanged and parks again, never reaching the syscall return
where `deliver_pending` runs. Defect 1 seen from user space.

The cause was one line in `amd64/src/exec_runtime.rs`:

```rust
// No signal delivery on this target at all — that is A2.
pthread_kill_eintr_enabled: false,
```

True when written; signal delivery landed 2026-09-11
(`AKUMA_AMD64_SIGNAL_DELIVERY.md`) and nothing came back to it. That flag is the
third arm of `should_interrupt_blocking_syscall`. Same class as the five `|_|
false` process hooks in `AKUMA_AMD64_STALE_FALSE_HOOKS.md`, and the same damage:
it read as a deliberate refusal while it was an expired one.

**Fixed:** `true`. `pthread_kill_eintr` PHASE1 now reports
`read() = -1 EINTR after 1 handler runs`.

The consequences compound, which is why it sat under so many symptoms: a process
that cannot be signalled cannot be killed; its parent waits forever; the orphan
is reparented to init and holds its `Process` row and thread slots for the life
of the boot. Four probe processes were found still resident, state `R`, ppid 1,
`TIME 0:00` — parked, unkillable, and holding table rows. That is the leak behind
defect 3's panic.

**Not** fixed by it: `segvgroup` still fails, at round **35** instead of 31. The
signal now arrives; the slots of a signal-killed process still do not come back.
Separate defect — see below.

### OPEN: thread slots leak (the one still standing)

```
round 31: child did not die of SIGSEGV (… exited=1 code=72) — pthread_create failed: leaked thread slots
```

`segvgroup` forks a child, has it spawn threads and take a `SIGSEGV`, and
repeats. From round 31 `pthread_create` fails: the slots of a process killed by
a signal are not returned. The console agrees from the other side — `[threads]
new high-water` climbs monotonically all boot with `terminated=0`, never
falling.

`threadmax` passes 400 rounds of *clean* spawn/join, so the ordinary path
returns slots. The leak is on the **signal-death** path specifically, which is
also where defect 3's `Process` rows come from. Candidate for the next session,
and the probe is deterministic, which is the expensive half.

## Verify

- `/probes/futextest` completes all 7 phases and the console shows no
  `Process table full`.
- `/probes/futexops` reports `0 divergence(s) from Linux`.
- A file written in the guest has `stat -c %Y` equal to `date +%s`.
- A `cargo` build that exits does not leave `DRAIN INCOMPLETE` on the console.

## Background

- [`RUST_TOOLCHAIN_AMD64.md`](RUST_TOOLCHAIN_AMD64.md) § session 4 — the cargo
  staging these came out of.
- `proposals/NEXT_AGENT_AMD64_SELFHOST_FIRST_BUILD.md` § 3 — predicted the clock
  first and named the test that finds it.
- `crates/akuma-exec/src/process/table.rs` — the open "full table panics"
  issue and why on-demand reclaim cannot live in `register_process`.
