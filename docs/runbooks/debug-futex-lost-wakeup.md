# Debug a futex lost wakeup

**Symptom.** A process sits with a frozen syscall count forever at low CPU, and
`[THR-DUMP]` shows one or more of its threads inside an unreturned `futex`:

```
tid=36 st=? pid=44 tgid=44 sc=63 tsc=98 a0=0x3d9135e8 a1=0x80 elr=0x3004839c
```

`tsc=98` is `__NR_futex`; `st=?` is WAITING; `a0`/`a1` are the saved `uaddr` and
`op` from the thread's trap frame. `a1=0x80` is `FUTEX_WAIT|FUTEX_PRIVATE`
(untimed), `a1=0x89` is `FUTEX_WAIT_BITSET|FUTEX_PRIVATE` (timed).

This runbook separates the four things that produce that one symptom. Do the
steps in order — each rules out a whole class, and the wrong guess here is
expensive (see "Two hypotheses that look right and are not" below).

## 0. Do not start from the driver's stall heuristic

`scripts/loop_selfhost_kernelbuild.py` and friends declare a stall when no new
`Compiling` line appears for N seconds. That is meaningless once the dependency
graph is cached: there is then only ever **one** `Compiling` line, so a
legitimately long final compile is indistinguishable from a wedge.

Use trace liveness instead. `/proc/<pid>/syscalls` is a per-process ring of
completed syscalls, newest last:

```python
import subprocess, re
SSH = ['ssh','-o','StrictHostKeyChecking=no','-p','2622','root@localhost']
tip = lambda p: subprocess.run(SSH+[f'/bin/busybox tail -1 /proc/{p}/syscalls'],
                               capture_output=True, text=True).stdout
```

A byte-identical tip across two samples 60 s apart, while a control process
(sshd, cargo) advances in the same window, is a wedge. An in-flight syscall is
NOT in this file — it is recorded on completion — so a wedged process's tip is
the last call it *finished*, not the one it is stuck in.

## 1. Read `[FUTEX-DUMP]` — is the waiter queued at all?

Printed every 30 s next to `[THR-DUMP]`/`[PIPE-DUMP]` under
`DEADLOCK_THREAD_DUMP_ENABLED` (`src/config.rs`, on by default).

```
[FUTEX-DUMP] 6 keys
  key tgid=44 uaddr=0x300c2340 waiters=1
    tid=30 bitset=0xffffffff queued_for=274865996us hist=EpWuXEpuSepWuXEp
```

The invariant is **parked in `FUTEX_WAIT` ⇒ present in `FUTEX_WAITERS`**, and
which side of it you are on decides everything that follows:

- **Waiter IS queued** → no wake reached that key. Go to step 2.
- **Waiter is NOT queued** → a `[FUTEX-ORPHAN]` line names it. That is a kernel
  bug by construction; go to step 3.

`queued_for` and `hist` come from the per-tid event ring
(`FUTEX_ORPHAN_DIAG` in `src/config.rs`). `hist` is the last 16 futex-table
transitions, oldest first:

| char | event |
|---|---|
| `E` | enqueued (first entry into the wait) |
| `e` | re-enqueued after a spurious wake |
| `S` | removed itself at `key` in the wait loop |
| `W` | popped by `futex_do_wake` |
| `P` | dropped by `futex_purge_tid` (terminate / slot recycle) |
| `Q` | moved to a requeue target |
| `D` | `futex_dequeue` (timeout / EFAULT cleanup) |
| `A` | `futex_remove_tid_anywhere` |
| `p` / `u` | park / unpark around `schedule_blocking` |
| `X` | `sys_futex` returned to user |

A healthy cycle reads `EpWuX` (enqueue, park, woken, unpark, return) repeating.
`--------------Ep` means this thread's very first futex call parked and never
came back.

## 2. Waiter is queued — read `[FUTEX-WAKERING]`

When any waiter has been queued longer than `STUCK_WAITER_US` (60 s), the dump
follows with the recorded wakes for that key, up to three times per boot:

```
[FUTEX-WAKERING] for stuck waiter tgid=39 uaddr=0x3d90f5e8
  same-addr tgid=17 uaddr=0x3d90f5e8 woken=1 by_tid=28 ts=53510333us
  same-addr tgid=19 uaddr=0x3d90f5e8 woken=1 by_tid=29 ts=48257821us
```

`same-addr` is every recorded wake on that `uaddr` under **any** tgid; `bucket`
is the stuck tgid's own last 16 wakes. Three readings:

- **An entry with the stuck `uaddr` but a different `tgid`** → the waker computed
  a different key namespace. That is a wrong-key lost wakeup.
- **An entry with the right key and `woken=0`** → the wake ran while the waiter
  was between its `S` (self-remove) and its re-enqueue. Check `hist`.
- **No entry for that key at all, and an empty `bucket`** → *no wake was ever
  issued in that namespace*. The bug is upstream of the futex code: whoever was
  supposed to wake never got there. Go to step 4.

The example above is the third case: two *other* rustc processes did the
identical wake successfully at the identical address, and tgid 39 issued none.

## 3. Waiter is not queued — `[FUTEX-ORPHAN]`

```
[FUTEX-ORPHAN] tid=21 tgid=539 uaddr=0x300c2340 last_ev_ts=…us now=…us hist=…
```

Read the tail of `hist` for the path that removed it: `W` = a wake popped it and
then failed to make it runnable (defect in the wake/scheduler handoff); `P` = the
slot-purge hook fired for a live thread; `S` with no following `e` = it removed
itself and never re-enqueued.

An orphan at an address shared by several processes (`0x300c2340` is musl's
`__thread_list_lock` in every musl binary) is the cross-process signature — see
step 5.

## 4. No wake was ever issued — look for a thread that died silently

The commonest cause is not a futex bug at all: a thread was killed from outside
and never ran its own exit epilogue. musl's `pthread_join` parks on
`&t->detach_state`, which **only the exiting thread's userspace ever sets**, so
there is no kernel-side substitute for it.

```
[kill] tid=27 (pid=75) terminated by tid=4 (pid=0) victim_state=4
```

is the tracer for that (rate-limited to 32 per boot, in
`mark_thread_terminated`). `victim_state` uses `thread_state`: 2=RUNNING,
3=TERMINATED, 4=INITIALIZING, 5=WAITING. A killer of `tid=0`/`tid=4` with
`pid=0` is a kernel thread, i.e. the spawn/cleanup path, not a peer.

Cross-check `ps` against `[THR-DUMP]`: a process that appears in `ps` with no
matching `tid=` line has **no live thread** — see
[`../archive/STALE_THREAD_SLOT_KILL.md`](../archive/STALE_THREAD_SLOT_KILL.md).

Also check for a fault in a freshly cloned thread:

```
[Fault] Data abort from EL0 at FAR=0x10, ELR=0x3801c58c, ISS=0x7
[Fault] Process 85 (rustc) SIGSEGV after 0.00s
[Fault] SIGSEGV in clone_thread, calling exit_group
[signal] sig 11 needs sigaltstack but slot 27 has none — re-pending
```

That is a separate **open** bug (thread-spawn), not a lost wakeup — it has its
own runbook now: [`debug-thread-spawn-segv.md`](debug-thread-spawn-segv.md).

As of 2026-08-05 this is the *only* remaining shape in the `-j4` self-host
wedge. A full retry run produced **zero `[FUTEX-ORPHAN]` lines**: every stuck
waiter was correctly queued at a correctly address-space-scoped key, and the
wedged waiters were all musl `pthread_join` on `detach_state`
(`0x3d90f5e8`/`0x3d90b5e8`) — joining threads the kernel had killed with
`[Fault] SIGSEGV in clone_thread`. If your dump looks like that, stop reading
this runbook: the futex layer is behaving and the bug is upstream of it.

## 5. Cross-process key collision (FIXED 2026-08-04, keep the probe)

Akuma has no ASLR, so every copy of a binary places its globals at the same
virtual address. A futex key that is the virtual address alone therefore names
one queue shared by every copy: `FUTEX_WAKE(addr, 1)` in process A pops the FIFO
head, which may be process B's waiter — B is woken spuriously, the wake is
counted as delivered, and A's own waiter stays parked forever.

`futex_key_tgid` used to put **every non-private op** in that namespace. musl's
`__tl_lock`/`__tl_unlock` wait and wake on `&__thread_list_lock` with `priv=0`,
and `pthread_create` hands the kernel that same address as the
`CLONE_CHILD_CLEARTID` word — so every thread create and exit in every musl
process went through one global queue. It now keys by address space unless the
address is in a genuinely shared mapping (`is_shared_file_mapping`), which is
what Linux's `get_futex_key` does for an anonymous page regardless of
`FUTEX_PRIVATE`.

Regress with `userspace/forktest/c_stress/futexkey.c` — deterministic, no stress
loop. **Do not** use "8 concurrent copies of `futextest_rs`" as the A/B
instrument: it was measured at 95/96 completions on *both* arms of this very fix
and cannot tell them apart.

## Verify

Build the kernel under test, then in the guest:

```sh
/tmp/futexkey     # cross-process key leak, deterministic
/tmp/futexops     # 5 Linux-divergence probes (requeue/bitset/EFAULT/WAKE_OP)
/tmp/futextest_rs # 7 phases: spawn+join, fan-out, condvar, barrier, park/unpark
```

Expected on a healthy kernel:

```
=== FUTEXKEY DONE — 0 divergence(s) from Linux ===
=== FUTEXOPS DONE — 0 divergence(s) from Linux ===
=== FUTEXTEST DONE — all phases passed ===
```

`futexkey` printing `FAIL shared_wake_stays_in_own_address_space … woken=1`
means the key namespace regressed. Run the same binaries under
`docker --platform linux/arm64` to see the Linux baseline; every probe passes
there.

Then confirm the diagnostics themselves still work: an idle VM must print
`[FUTEX-DUMP] table empty` with **no** `[FUTEX-ORPHAN]` lines, and a healthy
busy VM must show `hist=` fields cycling `EpWuX` rather than freezing.

## Two hypotheses that look right and are not

1. **"`futex_key_tgid` degraded to tgid=0 via `read_current_pid() == None`."**
   Instrumented and measured at zero occurrences across boot, 8-way and 16-way
   thread-churn runs. With `VFORK_FASTPATH_ENABLED`, `read_current_pid` resolves
   through `THREAD_PID_MAP` and returns before that branch is reachable. A
   `tgid=0` entry in a trace is not evidence it fired.
2. **"The waiter must be un-queued, or it would have been woken."** Both states
   occur and they have opposite causes. Read `[FUTEX-DUMP]` before theorising —
   the whole point of steps 1-3 is that the two are not interchangeable.

## Background

- [`../archive/SELFHOST_DEVBOX_SMOLTCP.md`](../archive/SELFHOST_DEVBOX_SMOLTCP.md)
  — "Open issue #2: a genuine lost-wakeup hang", the original investigation and
  the two disproven wrong-key hypotheses.
- [`../archive/FUTEX_REQUEUE_LOST_WAKEUP.md`](../archive/FUTEX_REQUEUE_LOST_WAKEUP.md)
  — the five-op audit against Linux that `futexops.c` came out of.
- [`../archive/STALE_THREAD_SLOT_KILL.md`](../archive/STALE_THREAD_SLOT_KILL.md)
  — "process survives with no thread", the shape step 4 looks for.
- [`../reference/subsystems/syscalls/sync.md`](../reference/subsystems/syscalls/sync.md)
  — current futex behaviour and known divergences.
