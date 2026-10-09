# amd64: `kill -9` went through the AArch64 hard-kill path, and the box paid for it

**Written:** 2026-10-09. **Status: root-caused, fixed, verified under
Firecracker/KVM on the trashcan (8 vCPUs oversubscribed on 4 cores) with a
negative control — NOT yet run on the ryzen metal where the hangs happened**
(§7). Every number below is
from a run named in §6; nothing is inferred from reading alone.

## 1. The symptom

Two hard hangs on the ryzen laptop in half an hour (2026-10-09, amd64 bare
metal, SMP=8, wifi), each right after a multi-process Chromium tree was
SIGKILLed — once by a `killall chromium` loop, once by a single `kill -9` of
the browser pid. No ssh, no ping, power button. The last klog ends:

```
[BKL] stuck: owner=2 waiter=7 tag=501 serving=385141856 folded=8 (aff0+1)
[bkls>] core=5 ... owner=2 spins=33554432
[TRAMP-MISMATCH] tid=76 THREAD_PID_MAP=28743 but table scan found 28637 — using 28743
[unregister] pid=28637 stale tid=76 now owned by pid=28743
[signal] pid=28619 killed by signal 15 (default action)
```

The handoff's suspicion was "the same class we fixed on aarch64: cleanup that
is interrupted or racing leaves wrong state". It is that class, with one
difference that is the whole bug: on amd64 the victim's cleanup was never
*interrupted* — it was never *run*, because the kill took a path written for
the other kernel's exit machinery.

## 2. What `kill -9` did on amd64 until this fix

`Syscall::Kill` was forwarded to glue (`amd64/src/usermode.rs`), whose
`sys_kill` calls `akuma_exec::process::deliver_signal`. For every signal but 9
that pends the signal on each thread of the group, raises the interrupt bit so
a parked thread's wait returns `EINTR`, and lets the thread take the signal at
its own next return to ring 3 — on amd64 `signal::deliver_pending` →
`Next::Fatal` → `exit_current_from_signal` → the thread unwinds through
`run_thread` → `teardown`, the leader through `run_process`, whose epilogue
drains the rest. That is the path a SIGTERM took, and it is sound.

For SIGKILL `deliver_signal` instead runs `kill_thread_group(pid, l0, -9)` and
`kill_process_with_signal(pid, 9)`: the AArch64 teardown. Read against this
target, every step of it is wrong in a way that was invisible from the
shared crate:

| the shared path assumes | what amd64 has |
|---|---|
| PHASE 1 posts `request_thread_kill` and each sibling consumes it at its EL1→EL0 boundary (`take_thread_kill_request` in the sync-EL0 handler) and self-terminates | **no caller of `take_thread_kill_request` at all.** `PENDING_KILL` is read only by `should_interrupt_blocking_syscall`, so a sibling's wait returns `EINTR`, it goes back to ring 3, retries, and gets `EINTR` again — a busy loop for the whole grace |
| the grace loop ends early when the siblings die | they never do, so **every `kill -9` of a threaded process waits the full 2 s `KILL_GRACE_US`**, then hard-terminates the "stragglers" — all of them |
| `mark_thread_terminated` on a sibling is a last resort for a thread stuck in a non-yielding loop | it lands on threads that are RUNNING in ring 3 on another core (`[kill] … victim_state=2`), parked (`=5`), or READY (`=1`). A thread so marked is never scheduled again and **never runs `thread::teardown`**: its `THREADS` row, its `clear_child_tid` write and futex wake, and the `Process` row behind it are all left standing |
| `kill_process_with_signal` stamps the leader zombie, publishes the exit to the parent, then `mark_thread_terminated`s the leader's slot | the leader's `run_process` epilogue never runs: no `thread::drain`, no `release_shared_write_mappings` (MAP_SHARED writeback), no `fbdev::release`, no `spawn_record_exit`; the parent reaps a process whose siblings may still be executing for up to 2 s |
| `kill(2)`'s `pid` is a `u32` | `pid_t` is signed. `kill(-pgid, sig)` arrived as `0xffff_ffff_ffff_ffcb`, was looked up as a pid, and answered `ESRCH`. **kami's `kill_group` (SIGTERM, wait, SIGKILL on `-pgid`) reached nothing, and its `group_alive` check always said "gone"**; the kill that actually landed was `child.kill()` on the browser pid. `kill -9 <thread tid>` answered `ESRCH` too |
| `tkill(tid, 9)` | called `exit_current_from_signal(9)` on the **caller**, whatever `tid` named |

The leaked rows compound. `amd64::thread::THREADS` is indexed by the
process *slot*; the next process to occupy a slot inherits the dead rows,
`live_count` counts them, its exit spins the whole 100 000-round drain
budget, prints `DRAIN INCOMPLETE`, and `wake_group` posts `request_thread_kill`
plus a wake to task slots those rows no longer own — a sticky `EINTR` on
whoever holds the slot now, since nothing on this target consumes the flag
but the slot scrub. The table is 448 rows system-wide; a Chromium session is
~100 threads; four `kill -9`s fill it.

This is the fourth recurrence of the class `GRACE_EXPIRED_HARD_KILL_ORPHANS.md`
§5 said would keep returning while a bare slot number is directly actionable
— and the first on amd64, where the question is not "is the slot recycled"
but "does this target have the boundary the request is waiting for". It does
not.

## 3. Reproducing it without the metal

`userspace/forktest/c_stress/killtree.c` (static musl, built by
`userspace/kami/probe/akuma/push.sh`): a "browser" in its own process group
with eight worker threads — untimed `FUTEX_WAIT`, `epoll_wait(-1)`,
`ppoll(NULL)`, a pipe `read`, `recvmsg` on an `AF_UNIX` `SOCK_SEQPACKET` pair
after one `SCM_RIGHTS` exchange, a compute loop with no syscall, a
`nanosleep` loop, and a writer on a `MAP_SHARED` mapping of an unlinked
`/tmp` file — which forks a "zygote" with the same threads, which forks a
"renderer". A separate "bystander" process (own group, same threads) counts
every wait that returns an error. Four kill modes, round-robin:
`kill(browser, SIGKILL)` then the rest; `kill(-pgid, SIGKILL)`;
`kill(-pgid, SIGTERM)`, 300 ms, `SIGKILL`; `kill(<zygote worker tid>, SIGKILL)`.
Verdict per round: time to reap the browser, time until every member is a
zombie or gone, bystander errors.

**Linux first** (the trashcan's Ubuntu 6.14, 8 rounds): every round `ok`,
reap 2–7 ms, tree 2–7 ms (301 ms in the SIGTERM mode, which is its own
sleep), bystander 0. Linux corrected the probe once: killing only the browser
leaves its children alive on Linux too, so mode 1 takes the rest after the
reap.

In the guest: `cd /root/cdp-probe/akuma && MEM=2048 VCPUS=8 sh run-fc.sh
killtree.sh killtree` on the kami image (`run-fc.sh` injects the probe and
the runner; the round table is in `kami-fc.log`).

## 4. What the unfixed kernel did (62653aaf, Firecracker on the trashcan)

**4 vCPUs, 12 rounds** — no hang, every round "ok", and every row of the
table in §2 visible:

```
round 0: SIGKILL browser, then the rest     reap=2078 ms tree=6130 ms   status=sig9
  kill(pgid=-53, 9): No such process
round 1: SIGKILL group                      reap=6134 ms tree=6134 ms
round 2: SIGTERM group, then SIGKILL        reap=2333 ms tree=2333 ms
  kill(futex=26, 9): No such process
round 3: SIGKILL a zygote worker tid        reap=6119 ms tree=6120 ms
…
worst tree teardown 6151 ms, bystander errors 0
```

2 s per threaded process, 6 s per tree, `ESRCH` for the group and for a
thread tid. Round 2 is the SIGTERM path, which is native and would take
300 ms — it took 2.3 s because the leader's `drain` found the rows the
previous rounds' hard kills had leaked in the same slot:

```
[thread] DRAIN INCOMPLETE: 17 thread(s) still live in proc slot 11
[thread] DRAIN INCOMPLETE: 41 thread(s) still live in proc slot 10
[thread] DRAIN INCOMPLETE: 65 thread(s) still live in proc slot 10
```

and 12 `[kill] tid=N (pid=M) terminated by tid=6 (pid=16) victim_state=1|2|5`
— the killer (the probe, pid 16) hard-terminating READY, RUNNING and parked
threads of another process from its own core.

**8 vCPUs on 4 cores, 40 rounds asked** — `[BKL] stuck: owner=3 waiter=1
tag=501` (the ryzen line) in round 5; from round 11 on, `[BKL] stuck` with
`owner=0` (lock free, queue frozen), `tag=11` (`munmap`) and `tag=26`
(`msync`); then in round 22:

```
  [clone] thread table full
  [clone] clone: thread table full
member level 2: workers failed: No such process
```

the leaked rows had filled the 448-row table. The probe never printed round
22's verdict; the guest sat at 390 % host CPU printing `[BKL] stuck` lines
until `run-fc.sh`'s 600 s timeout killed it. That is the wedge, reproduced
in a VM in under ten minutes.

## 5. The fix

- **`amd64/src/signal.rs::sys_kill`** — this target's own `kill(2)`.
  Decodes the `pid_t` through `akuma_exec::process::kill_target` (`> 0` a
  process, `0` the caller's group, `-1` everyone but init and the caller,
  `< -1` a group; the sign-extended register is recovered by truncation —
  host-tested, including that case). Every signal, SIGKILL included, goes
  through `pend_signal_to_group`. Group delivery walks the process table once
  into a fixed `[Pid; MAX_PROCESSES]` and addresses each thread group through
  its leader, so no `Vec` on a path that runs while the box is in trouble and
  no double delivery through a thread's own row.
- **`akuma_exec::process::pend_signal_to_group`** — the non-SIGKILL body of
  `deliver_signal`, split out and made `pub`. `deliver_signal` itself is
  unchanged for AArch64: its SIGKILL arm still runs `kill_thread_group`, and
  nothing on amd64 reaches it any more.
- **`sys_tkill`/`sys_tgkill`** — SIGKILL is pended like every other signal
  (`take_pending_signal` cannot mask bit 9 and `next_delivery` reaches
  `fatal_default` for it); `SIG_IGN` is not honoured for 9/19; and a fatal
  default signal to a parked thread also raises the interrupt bit, since a park
  loop re-parks for anything but a `UserFn` handler.
- **`amd64::thread::wake_group`** drops rows whose task slot's
  `THREAD_PID_MAP` owner belongs to another thread group, or is absent on a
  dead/free slot (`[thread] dropped N stale thread row(s)`). A missing owner
  on a *live* slot is kept: `clone_thread`'s row and map entry are two
  writes, and dropping a row for a thread about to start would let the reaper
  free the address space under it. With the native kill path no row leaks in
  the first place; this is the backstop.
- **`akuma_threading::cross_thread_terminations()`** — the `[kill]` tracer's
  counter, exposed for the boot check.
- **Boot check `usermode::kill_test`** (`userspace/amd64/killprobe`, raw
  `clone` like `threadprobe`): two threads in untimed `FUTEX_WAIT`, one in a
  syscall-free compute loop, the main thread parked; the boot thread sends
  `sys_kill(pid, 9)` and asserts the group finished, within one second, with
  no cross-thread termination, every `THREADS` row back, status `-9`,
  `kill(pid, 0)` on the zombie `0`, `kill(-pid, 0)` on the empty group
  `ESRCH`, and no leaked frame.

### 5.1 Two more, found by the boot check on its first run

The first boot of the fixed kernel printed `738 passed, 5 FAILED`, all five
in `kill:`, and the five together named two defects on the same path:

- **`notify_group_of_thread_fatal` re-entered the hard path.** A sibling
  that takes a fatal signal tells the leader through `deliver_signal(tgid,
  sig)` — and for SIGKILL that is `kill_thread_group` again, through a door
  `sys_kill` no longer used. The two `FUTEX_WAIT` parkers took SIGKILL
  first, notified, and the leader was hard-terminated from a parker's core:
  `no thread was terminated by a peer: got 0xa want 0x8`, `every thread row
  came back: got 0x2`, and the second not gone inside a second (the grace).
  It pends now (`pend_signal_to_group`).
- **A signal death read as an exit code.** `GROUP_EXIT_STATUS` carries
  either `-(sig)` or `GROUP_EXIT_CODE_FLAG | code`, with the flag at bit 30;
  `deliver_pending` tested the flag alone, and bit 30 is set in every
  negative word, so `-9` became `exit_group(0xf7)`: `the status is a SIGKILL
  death: got 0xf7 want 0xfffffffffffffff7`. The same misread turns a
  SIGTERMed group into exit **241** — the number kami's daemon logged as
  "chromium exited: exit status: 241" (recorded in `userspace/kami/README.md`
  as unexplained) and the number `AKUMA_AMD64_STALE_GROUP_EXIT_STATUS_241.md`
  chased to a different, also real, cause. The sign is tested first now.
- The fifth was the test's own expectation: Linux keeps a zombie in its
  process group until it is reaped, so `kill(-pgid, 0)` is `0` before the
  reap and `ESRCH` after. The check now asserts both.

### 5.2 The one the hard kill had been hiding on both kernels

With the boot check green, `killtree` at 8 vCPUs still left **one thread per
process** behind (`[thread] DRAIN INCOMPLETE: 1 thread(s) still live`), and
the first round's browser was never reaped. Eight blocking families, one
survivor: the `AF_UNIX` `recvmsg` worker. Every untimed park in
`akuma-syscalls-glue/src/unixsock.rs` — `accept`, the three send paths, the
receive, and the no-table-entry pipe read — went straight to
`park_indefinitely` without asking `should_interrupt_blocking_syscall`,
which is the one hook a pended signal has to break a wait: the pend wakes
the thread, the loop sees no data, and it parks again. Every other family
(`futex`, `epoll`, `ppoll`, pipe `read`, `nanosleep`, the pty) had the
check; this is the family `KTG_GRACE_EXPIRY_KILL_INTERRUPT.md`'s 2026-08-30
sweep missed, invisible on AArch64 because the grace-expiry hard kill took
the thread out 2 s later. Fixed by the same `EINTR`-before-park the others
use (`unixsock.rs`, six sites). This one is **shared**, so it changes
AArch64 too: a Chromium-shaped process parked in `AF_UNIX` now dies in
milliseconds there instead of two seconds.

Also found: the probe's "`kill -9 <thread tid>`" mode is Linux-only. On
amd64 `gettid()` is the task slot — a number space disjoint from pids — so
`kill(tid, 9)` addresses whatever *process* carries that pid (round 3 of
the first fixed run most likely killed the bystander or the probe itself,
which then waited forever on its pipe). The probe skips that mode when the
main thread's `gettid()` is not its `getpid()`; the tid/pid namespace is an
open item in `docs/handoff-kernel-chromium-support.md` and stays open.

Allocations on the new paths: none. `sys_kill`'s group walk is a stack array;
`wake_group`'s scan writes nothing but `None` into existing rows.

## 6. Verified

| gate | result |
|---|---|
| `cargo test -p akuma-exec -p akuma-threading` (host) | 93 + 28 passed, 0 failed (4 new `kill_target_tests`) |
| `cargo clippy -p akuma-exec -p akuma-threading -- -D warnings` | clean |
| `cargo clippy -p akuma-amd64 --target x86_64-unknown-none --release -- -D warnings` | 34 errors, **all pre-existing** (`hda.rs`, `kbd.rs`, `rtw89_sta.rs`, `splash.rs`, `smp.rs`, `fd.rs`, `idt.rs`, `mm.rs`, `power.rs`, `usermode.rs:5983/6256` — the debt `docs/handoff-kernel-chromium-support.md` lists); none in the changed code |
| `cargo check --release` (AArch64 kernel; the shared crate changed) | clean |
| standard image boot, Firecracker, 2 and 4 vCPUs | see the verification table below |
| `killtree`, 8 vCPUs, 40 rounds / 4 vCPUs, 12 rounds | below |
| `chrome-once.sh` (Chromium still renders) / `probes.sh` | below |

All on the trashcan's Firecracker/KVM rig (4 cores, 15 GiB), kernel built
from the tree at `9f65582a` plus the dispatcher-routed boot check (§6.1):

| run | result |
|---|---|
| standard image boot, 2 vCPUs | **744 passed, 0 failed** (11 new `kill:` checks; the suite was 733) |
| standard image boot, 4 vCPUs | **744 passed, 0 failed** |
| `killtree 40 0`, **8 vCPUs** — the configuration that wedged the unfixed kernel in round 22 | **PASS, 40 rounds**; `kill -9` of a threaded process reaps in **13–104 ms** (was 2078–6463 ms), a three-process tree in **17–430 ms** (was 6.1 s; the SIGTERM mode's own 300 ms sleep is in that); bystander errors 0; `DRAIN INCOMPLETE` **0**, `[BKL] stuck` **0**, `[kill]` cross-core terminations **0**, stale rows **0**, `thread table full` **0** |
| `killtree 12 0`, 4 vCPUs | **PASS**, worst tree 309 ms, every detector 0 |
| `chrome-once.sh` (Chromium 142 headless on the kami image, 2 vCPUs, 4 GiB) | exit **0**, `shot.png` reads "JavaScript ran: 6 x 7 = 42"; the one `[kill]` line in its log is the stale-tid guard *refusing* a reissued slot, not a termination |
| `probes.sh` (bpprobe … thrprobe, chromeprobe) | every probe PASS, `chromeprobe: 16 passed, 0 failed` |
| negative control: the old `Syscall::Kill => to_glue` arm restored, everything else kept | **739 passed, 5 FAILED**, all `kill:` — `gone within a second` (the 2 s grace), `no thread was terminated by a peer: got 0xf want 0x8` (seven cross-core hard kills), `every thread row came back: got 0x3` (three leaked rows), `status is a SIGKILL death: got 0xffff…ffff` (no status reached the epilogue), `a zombie still counts for its process group: got ESRCH` (no group decode). The box was then reset to the tree and rebuilt to the same md5 as the passing kernel |
| for the record, the first fixed kernel before §5.1 | 738 passed, **5 failed**, all `kill:` — the two defects §5.1 names |
| for the record, the kernel before §5.3 | same suites 744/0 and the same `killtree` passes, with exactly one `DRAIN INCOMPLETE: 1 thread(s)` per run: the bystander's `_exit(0)` with a spinning worker |

### 6.1 The boot check goes through the dispatcher, and why

The first negative control passed every `kill:` line with the old arm
restored — because the check called `crate::signal::sys_kill` directly, and
the defect was the *arm*, which forwarded to glue. A check that cannot tell
the fixed kernel from the broken one is not a check; it now kills through
`syscall_dispatch(62, …)`, the path a program takes.

### 5.3 The compute loop at `exit_group`

The last `DRAIN INCOMPLETE` left was not a kill at all: the bystander's main
thread `_exit(0)`s while its spinner is in a loop with no syscall, and
`exit_group` reaches a sibling only at syscall entry (`should_leave_now`) or
at syscall return (`deliver_pending`'s group-status check) — never on the
tick, which is the one place such a thread can be reached, and where a
*fatal signal* already leaves from. `signal::deliver_pending_on_tick` now
answers `TickOutcome::Exit` when the group is exiting (a non-main thread with
`should_leave_now`, or any thread with a `group_exit_status`, decoded as
`deliver_pending` decodes it), and `timer_dispatch` leaves through
`usermode::leave_current_from_tick` — the same unwind as a fault kill without
the group notification. `crate::thread::drain`'s "a real gap" note is closed
by it, for both `exit_group` and a sibling's fatal signal reaching a leader
that computes.


## 7. Not verified, and what to do

- **The ryzen metal.** The hangs were there; the reproduction and the fix
  were under Firecracker/KVM on the trashcan. The ryzen box was down (no
  route) for this whole session, so the second hang's klog (`klog-74` on its
  Akuma partition, `/mnt/p3` from Pop) is **unread**; read it before trusting
  that both hangs are this class. Then boot the fixed kernel there, run
  `killtree 40 0` from an ssh session (it is now safe to), and only then a
  Chromium kill loop.
- **Wifi.** The trashcan's Firecracker guest has no network device at all;
  the metal's kills happen with the `rtw89` driver active. Nothing here
  exercises that.
- **`[BKL] stuck` with `owner=0`** ("lock free, queue frozen") appeared during
  the unfixed storm and is the ticket-accounting leak `akuma-bkl` already
  self-heals; whether it still appears under the fixed kernel's load is in the
  table above, but its cause was not chased here.

## Background

- [`GRACE_EXPIRED_HARD_KILL_ORPHANS.md`](GRACE_EXPIRED_HARD_KILL_ORPHANS.md),
  [`STALE_THREAD_SLOT_KILL.md`](STALE_THREAD_SLOT_KILL.md),
  [`KTG_GRACE_EXPIRY_KILL_INTERRUPT.md`](KTG_GRACE_EXPIRY_KILL_INTERRUPT.md),
  [`KTG_STALE_TID_EXIT_STAMP_J4_HANG.md`](KTG_STALE_TID_EXIT_STAMP_J4_HANG.md)
  — the AArch64 class, whose fixes all assume the EL1→EL0 kill-consume
  boundary this target does not have.
- [`AKUMA_AMD64_THREAD_LIFECYCLE.md`](AKUMA_AMD64_THREAD_LIFECYCLE.md) §1, §6
  — `wake_group` arming `request_thread_kill`, and the group-fatal path this
  fix reuses for SIGKILL.
- [`AKUMA_AMD64_NO_SLOT_RECYCLER.md`](AKUMA_AMD64_NO_SLOT_RECYCLER.md) §3.1 —
  the `[TRAMP-MISMATCH]` lead ("a thread that skipped teardown") this names.
- [`AKUMA_AMD64_STALE_GROUP_EXIT_STATUS_241.md`](AKUMA_AMD64_STALE_GROUP_EXIT_STATUS_241.md)
  §8 — `wake_group`'s unchecked `request_thread_kill`, recorded there as open.
- [`AMD64_SMOLTCP_STALE_HANDLE_KILL9_WEDGE.md`](AMD64_SMOLTCP_STALE_HANDLE_KILL9_WEDGE.md)
  — the earlier `kill -9` wedge, a different cause on a single-threaded victim.
- `userspace/kami/README.md` § "Chromium on Akuma can wedge the kernel".
