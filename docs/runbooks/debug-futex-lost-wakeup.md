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

> **`[kill]` lines cannot rule anything out under `smp-shared`.** That tracer fires
> only when `killer != idx`, and the whole deferred-kill path has the victim
> **self**-mark at its EL1→EL0 boundary — so `killer == idx` and a thread killed from
> outside leaves no `[kill]` line at all. §4a below used "zero `[kill]` lines in the
> entire boot" as positive evidence that nothing had been killed; that inference is
> invalid, and it cost a session.
>
> Use **`[TERM]`** instead, which attributes *every* termination to its call site
> (`#[track_caller]` on `mark_thread_terminated`) and resolves the owner through
> `THREAD_PID_MAP`:
>
> ```
> [TERM] tid=23 pid=Some(92) by_tid=17 state=5 pending_kill=false at=…/process/mod.rs:1225
> ```
>
> `by_tid` ≠ subject means cross-thread; `state=5` is WAITING (the victim was parked);
> `pending_kill=false` at a `kill_thread_group` site means it was killed as a
> "straggler" without being one. See
> [`../archive/GRACE_EXPIRED_HARD_KILL_ORPHANS.md`](../archive/GRACE_EXPIRED_HARD_KILL_ORPHANS.md).

You no longer have to cross-check `ps` against `[THR-DUMP]` by hand for this.
**`[PROC-ORPHAN]`** is printed next to `[THR-DUMP]` and names every ACTIVE, un-exited
process with no live thread in `THREAD_PID_MAP` — silent on a healthy system:

```
[PROC-ORPHAN] pid=92 tgid=14 no live thread; recorded thread_id=Some(23) now owned by pid=Some(103)
```

Such a process is unschedulable by construction: it can never reach `exit_group`, is
never reaped, and its parent's `wait4` blocks forever. **The parent is what looks
stuck, and it looks stuck in a futex** — which is why this class keeps being
mis-filed as a lost wakeup. If the same pid repeats across dumps, stop reading this
runbook: the futex layer is fine.

Then get the killer from the `[TERM]` line for that process's `recorded thread_id`.
See [`../archive/STALE_THREAD_SLOT_KILL.md`](../archive/STALE_THREAD_SLOT_KILL.md)
and [`../archive/GRACE_EXPIRED_HARD_KILL_ORPHANS.md`](../archive/GRACE_EXPIRED_HARD_KILL_ORPHANS.md).

Do **not** trust `[THR-DUMP]`'s own `pid=` column when attributing a slot: it resolves
via `find_pid_by_thread`, the same `p.thread_id` table scan the trampoline bug was
about, so a stale process at a lower slot index wins the attribution. The futex tables
*are* trustworthy — their `tgid` is recorded at enqueue time via `read_current_pid`,
which goes through `THREAD_PID_MAP`.

Also check for a fault in a freshly cloned thread:

```
[Fault] Data abort from EL0 at FAR=0x10, ELR=0x3801c58c, ISS=0x7
[Fault] Process 85 (rustc) SIGSEGV after 0.00s
[Fault] SIGSEGV in clone_thread, calling exit_group
[signal] sig 11 needs sigaltstack but slot 27 has none — re-pending
```

That is a separate **open** bug (thread-spawn), not a lost wakeup — it has its
own runbook now: [`debug-thread-spawn-segv.md`](debug-thread-spawn-segv.md).

This was for a while the *only* remaining shape in the `-j4` self-host wedge. One
retry run produced **zero `[FUTEX-ORPHAN]` lines**: every stuck waiter was
correctly queued at a correctly address-space-scoped key, and the wedged waiters
were all musl `pthread_join` on `detach_state` (`0x3d90f5e8`/`0x3d90b5e8`) —
joining threads the kernel had killed with `[Fault] SIGSEGV in clone_thread`. If
your dump looks like that, stop reading this runbook: the futex layer is behaving
and the bug is upstream of it.

It is **not** the only shape — see §4a.

## 4a. Orphans with no dead thread at all (FIXED 2026-08-05)

The check in §4 has a false negative, and §3's "orphan at a shared address ⇒
cross-process collision ⇒ §5" has one too. Both were built on runs where a
thread had died. This shape has **no corpse**:

```
[FUTEX-ORPHAN] tid=21 tgid=13 uaddr=0x300c2340 last_ev_ts=28894214us now=930202300us hist=XEpWuXXXXXXXXEpW
[FUTEX-ORPHAN] tid=22 tgid=13 uaddr=0x300c2340 last_ev_ts=27009750us now=930202921us hist=XXXXXXXXXXXXXEpW
[FUTEX-ORPHAN] tid=23 tgid=13 uaddr=0x300c2340 last_ev_ts=26913810us now=930203369us hist=XXXXXXXXXXXXXEpW
  key tgid=13 uaddr=0x3cda5fc4 waiters=1
    tid=16 bitset=0xffffffff queued_for=923645878us hist=--------------Ep
```

What separates it from §4 and §5, and why each of the obvious readings is wrong:

- **Zero `[kill]` lines and zero `[Fault]` lines in the entire boot.** So §4
  does not apply: nothing was killed while holding the lock. Grep for both
  before you assume a dead thread; the absence is the diagnosis.
- **Every orphan is the same `tgid`.** So §5 does not apply either — the
  cross-process key collision needs two tgids on one address. `0x300c2340` is
  `__thread_list_lock`, which is at the same VA in every musl binary, so a
  shared *address* is not by itself evidence of a shared *key*.
- `hist=--------------Ep` on the queued waiters means enqueued, parked, and
  never woken — no prior wake cycle at all, so it is not a wake that got lost
  mid-handoff.

Reproduce: the final `akuma` crate of the in-VM self-host build at `-j4` wedges
this way ~27 s in, **deterministically** (2 for 2). The same crate at `-j1`
compiles in 68 s — see [`selfhost-kernel-build.md`](selfhost-kernel-build.md)
§5.1. That `-j1` fixes it says the trigger is concurrency inside one process, not
a cross-process key.

Do not use a `Compiling`-line stall or guest-side CPU time to detect this wedge
(§0, and busybox `ps` reports no per-process CPU time); parse `queued_for` out
of the last `[FUTEX-DUMP]` block instead.

### The answer was in the last event of `hist`, not in the futex table

`hist=...EpW` reads: **E**nqueued, **p**arked, then **W**oken — `futex_do_wake`
popped this tid and called its waker. The event that never follows is `u`
(`schedule_blocking` returned). So the futex layer did its whole job and the
thread still never ran again: this is a **scheduler** handoff defect, and every
minute spent on key namespaces, wake counts and musl's lock protocol was spent in
the wrong subsystem. §3 already names this reading of `W`; trust it.

The defect, in `crates/akuma-exec/src/threading/mod.rs`:

- `ThreadWaker::wake` is two steps — set the sticky `WOKEN_STATES[tid]` flag,
  then, **only if the target is already `WAITING`**, flip it to `READY` and ring
  the scheduler.
- `WOKEN_STATES` was read in exactly one place, `schedule_blocking`. Nothing in
  the scheduler ever reconsiders a `WAITING` thread because of it.
- `schedule_blocking` published `WAITING` and *then* asked to be switched out
  (`voluntary_schedule_flag` + a self-SGI) before its park loop re-read the flag.

So a waker landing between `schedule_blocking`'s entry check and its `WAITING`
store saw `RUNNING`, armed the flag, and left. The victim then published
`WAITING`, was switched out by its own SGI, and no longer existed as far as any
future wake was concerned. Untimed waits (`FUTEX_WAIT` with no timeout, which is
what musl's `__tl_lock` issues) have no deadline to rescue them, hence "forever".

Fixed by `publish_waiting_and_take_pending_wake`, which does the `WAITING` store
and the flag re-check under a local IRQ mask — a context switch on this core can
only arrive via IRQ, so the pair is atomic against being descheduled, and against
a peer core the two `SeqCst` variables leave no losing interleaving.

Why `-j4` and not `-j1`: the window is a handful of instructions wide and needs a
*concurrent* waker on another core to land inside it. Nothing about it is
specific to musl, to `__thread_list_lock`, or to rustc — that address is simply
where a single lost wake does the most damage, because `pthread_create` holds
that lock across `__clone` and every thread create and exit in the process
serialises behind it.

Regression test: `park_wake_race_tests` in the same file — two `std::thread`s
race the park and the wake over one slot, and it fails on iteration 0 against the
pre-fix ordering. Prefer it to any in-VM stress repro; see
[`../reference/subsystems/scheduler.md`](../reference/subsystems/scheduler.md)
-> "Park/wake handshake" for the invariant it encodes.

Still open at the time of the fix: the in-VM `-j4` build has **not** been re-run,
so `-j1` remains the documented recipe for the final crate.

> **Residual variant (2026-08-07, not yet fixed).** A focused probe
> (`userspace/selfhost_repro/jobserver_stress.rs`) reproduces a *related* lost
> wake under real 4-core SMP + CPU-hog preemption pressure: untimed
> `Condvar`/`Barrier` waits (`a1=0x80`) hang 4/4 while timed waits cycle (their
> deadline rescues them). `hist` ends `Ep` (wake never *issued* on the key), not
> §4a's `EpW` (issued but dropped) — so it may be upstream of this fix or a
> different window in the same handshake. `futextest_rs` does **not** reproduce
> it (1:1 patterns miss the window). Two mitigations (a direct cross-core wake
> SGI, and a periodic bounded re-park for untimed waits) were tried and
> **confirmed not to fix it** — `[FUTEX-DUMP]` shows the periodic re-park
> cycling healthily (`hist=uSepuSepuSep...`) but never observing a real `W`,
> because the futex *value* never changes: whatever thread should call
> `notify_all` never gets there. No futex-layer safety net can rescue a wake
> that's never issued. Full diagnosis + a 25-second deterministic repro in
> [`../archive/J4_WRITE_PERM_FAULT_AND_HALF_WRITTEN_LINKER_OUTPUT.md`](../archive/J4_WRITE_PERM_FAULT_AND_HALF_WRITTEN_LINKER_OUTPUT.md)
> §7.9–§7.10.
>
> That same session found `alarm()`/`pause()` completely broken (`ppoll(NULL,
> 0, ...)` never blocked, then never returned once it did, then blocking
> syscalls couldn't be interrupted by a signal with no registered handler) —
> unrelated to the futex investigation, now fixed, see §7.11 of the archive
> doc above. Any future probe can use `alarm()` for an in-guest timeout again
> instead of an external `kill -9` watchdog.

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
