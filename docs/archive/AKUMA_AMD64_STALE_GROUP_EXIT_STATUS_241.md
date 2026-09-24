# amd64: "every exec returns 241 until a power cycle" — a stale per-slot group death status (2026-09-24)

**Status: FIXED** in the working tree (uncommitted at time of writing). Verified
in QEMU with a boot self-test that fails without the fix and passes with it.
**Not yet verified on the trashcan metal or the ryzen Firecracker guest.** The
live check there is below (§7).

The symptom was first recorded as a side trap in
[`AKUMA_AMD64_KOT_REPLICA_WEDGE.md`](AKUMA_AMD64_KOT_REPLICA_WEDGE.md) lead 4.
It was attributed there to a `kill -9` on a thread-PID and, in akuma-miot's
`HANDOFF.md`, to the amd64 no-slot-recycler class. Neither was the cause. The
cause is one flag that `sys_spawn` never cleared.

## 1. Symptom

Seen on the trashcan metal (2026-09-22) and the ryzen Firecracker guest
(2026-09-23), both while running akuma-miot's `kot`:

- Right after something stopped `kot` (`deploy.sh retire-old` killing the old
  node by PID, or herd stopping or restarting the service), sshd still
  authenticated, but **every exec returned status 241 with no output**. It
  stayed that way for minutes, until a power cycle.
- On ryzen-fc: `[herd] Service kot exited with code  241`, three times, herd
  restarting it each time, then giving up.

Nothing in the kernel log explains it. There is no `[Fault]`, no `[kill] …
stale tid=…` line, and no allocation failure.

## 2. What 241 is

A signal death on amd64 carries exit code `-(sig)`: `kill_current_from_fault`
(`amd64/src/usermode.rs`) and `exit_current_from_signal` both use it, as does
the AArch64 fatal path (`akuma-exec/src/process/signal.rs`). For SIGTERM that
is `-15`, and **`-15 & 0xff` = 241**. So every one of those execs was a process
**killed by SIGTERM**, not a failed spawn.

It read as an exit code because of a second bug. amd64's private
`sys_waitpid` (Akuma syscall 303, which sshd's bridge and herd both poll)
encoded every status as `(code & 0xff) << 8`, a normal exit, where glue's
`wait4` uses `encode_wait_status` and reports `WIFSIGNALED`. So sshd sent
`exit-status 241` instead of `exit-signal TERM`, and herd's
`WaitStatus::signaled()` never fired. The one clue that would have said
"SIGTERM" was erased before anyone saw it.

## 3. Mechanism

`amd64/src/thread.rs` keeps two flags **per process slot**:

| flag | set by | read by |
|---|---|---|
| `GROUP_EXIT` | a multithreaded `exit_group` (`set_group_exiting`) | `should_leave_now`: every non-main thread leaves ring 3 at its next syscall |
| `GROUP_EXIT_STATUS` | `notify_group_of_thread_fatal`, when a non-main thread dies by a default-action signal; value `-(sig)` | `signal::deliver_pending`, **first**, before any pending-signal or disposition logic: the thread exits as that signal |

Process slots are recycled. Before this fix, only the `fork` path
(`usermode.rs`, beside its SPAWN-row write) cleared both flags. `execve`
cleared `GROUP_EXIT` only, and `sys_spawn` cleared neither, even though the
doc comment on `clear_group_exiting` said "`execve` and `sys_spawn` both put a
new program in an existing slot".

The sequence:

1. `kot` (tokio, many worker threads) is sent SIGTERM, by herd's
   `kill_service_to` or by a bare `kill <pid>`, whose default signal is SIGTERM.
   `deliver_signal` pends it on **every** thread in the group.
2. A worker takes it first. SIGTERM's default action is fatal, so
   `notify_group_of_thread_fatal` stamps `GROUP_EXIT_STATUS[slot] = -15`.
3. kot exits as SIGTERM (herd logs "exited with code 241"), is reaped, and its
   SPAWN row is freed. **The stamp stays.**
4. `sys_spawn` picks the **lowest** free SPAWN row, `(SPAWN_SLOT_BASE..PROC_SLOTS).find(..)`,
   so the next spawn lands in exactly that slot. At its first syscall return,
   `deliver_pending` sees `-15` and exits it as SIGTERM before it can print
   anything.
5. The child is reaped and the slot freed, still stamped, so step 4 repeats for
   every spawn after it: sshd's `/bin/sh` for each exec, herd's restarts of kot
   itself. Nothing ever clears the stamp, so it lasts until reboot.

Any multithreaded process losing a thread to a default-action signal poisons
its slot the same way. A tokio worker's SIGSEGV would leave `-11`, and every
later spawn would read as exit 245. kot is simply the program on these boxes
that is both heavily threaded and routinely stopped by signal.

The stale `GROUP_EXIT` half is quieter. A single-threaded successor never tests
it, because the main thread always answers `should_leave_now() == false`. But a
*threaded* successor in that slot would see each new thread leave ring 3 at its
first syscall.

## 4. Why the stale-tid guard never fired

The first suspect was the recycled-thread-slot class that
[`STALE_THREAD_SLOT_KILL.md`](STALE_THREAD_SLOT_KILL.md),
[`GRACE_EXPIRED_HARD_KILL_ORPHANS.md`](GRACE_EXPIRED_HARD_KILL_ORPHANS.md) and
[`KTG_STALE_TID_EXIT_STAMP_J4_HANG.md`](KTG_STALE_TID_EXIT_STAMP_J4_HANG.md)
fixed on AArch64: a signal addressed through `Process::thread_id` landing on
whoever inherited the slot. Its guard, `slot_still_owned_by` in
`akuma-exec/src/process/signal.rs`, prints
`[kill] pid=… stale tid=… now owned by pid=…`. That print *does* reach the
console on amd64, because `boot.rs` registers `serial::puts` as the
`safe_print!` hook. It was never seen because the signal was never misdirected.
It went to kot, correctly. What leaked was per-**process**-slot state, which
the thread-slot scrub (`akuma_threading::scrub_thread_slot`, run at every
`x86_claim_slot`) does not cover and was never meant to.

This is the same *shape* as the AArch64 class, though: recycled index, state
keyed by it, reset on only some of the paths that reissue it. See §8.

## 5. Fix

`amd64/src/usermode.rs`:

- **`spawn_process_task`** now calls `clear_group_exiting(proc_slot)` and
  `clear_group_exit_status(proc_slot)`. It is the one slot claim every
  non-`fork` process goes through: `sys_spawn`, init, and the boot self-tests.
  One site rather than one per caller, which is how the `sys_spawn` gap
  happened. `fork` keeps its own pair.
- **`sys_waitpid`** encodes through
  `akuma_syscalls_glue::proc::encode_wait_status`, the same encoder `wait4`
  uses. A signal death now reads back as `WIFSIGNALED`. Every existing caller
  and self-test decodes `(st >> 8) & 0xff` for normal exits only, so none
  changed.

`execve` still clears only `GROUP_EXIT`. That was left alone deliberately: a
set `GROUP_EXIT_STATUS` there means a sibling died by signal *while* the main
thread was exec'ing, and the process should still die, as Linux's exec does
when a group exit is in progress.

## 6. Verification

Boot self-test **`spawn_stale_group_state_test`** (`amd64/src/usermode.rs`,
registered in `boot.rs` right after `spawn_test`):

1. Runs `sys_spawn`'s own free-slot search.
2. Stamps that slot with what a SIGTERM'd tokio worker leaves (`-15`) plus
   `GROUP_EXIT`.
3. Spawns `/bin/hello` and checks that it took the poisoned slot, was **not**
   `WIFSIGNALED`, and ran to its own status `0x7f`.

QEMU `microvm`, TCG, SMP=1, `sh amd64/run.sh`:

| build | suite | the new test |
|---|---|---|
| fix applied | **787 passed, 0 failed** | 5/5 OK |
| fix reverted (the two clears removed; waitpid fix kept) | **760 passed, 27 FAILED** | `not killed by a signal: got 0xf want 0x0` |

The negative control reproduces the field symptom, not just the one check.
The other 25 failures are unrelated later tests whose spawns all landed in the
same poisoned slot. That is "every exec fails until reboot" in miniature, and
the waitpid half of the fix is what made it report `0xf` (SIGTERM) instead of
241.

## 7. Not verified yet

- **On hardware.** Deploy to the trashcan or ryzen-fc, then `herd stop kot`, or
  `kill` its pid, then run an ssh exec. Before the fix that exec returned 241
  forever. After it, the exec should work, and herd should log kot as
  **"killed by signal"** rather than "exited with code 241".
- **The "fails at the ~12th–15th ssh session with kot running" report**
  (akuma-miot `HANDOFF.md`, "Open theory", 2026-09-24). This fix explains it
  only if something SIGTERM'd, or otherwise signal-killed, a kot thread in that
  window. If it recurs with this fix in, look at herd's log for a "killed by
  signal" line first. That line is only possible now.

## 8. Still open, found on the way (not linked to any failure)

- `slot_still_owned_by` treats a **missing** `THREAD_PID_MAP` entry as
  "still owned" (`_ => true`). That default was copied from
  `unregister_process`, where terminating is a deliberate backstop. For
  *pending a signal* it is the wrong default, and it is silent.
- `amd64::thread::wake_group` (the `exit_group` drain) calls
  `request_thread_kill(t.task)` on every `THREADS` row of the process slot
  with no ownership check. A row left behind by a thread that died without
  `teardown` would arm `PENDING_KILL` on whoever holds that task slot now. On
  amd64 that causes spurious `EINTR`s, not deaths, which is why it is not this
  bug. It is the same shape as the AArch64 grace-kill bug.
- Per-slot tables on this target are reset at *some* reissue points. Before
  adding one, list every path that hands the slot out, the rule
  `scrub_thread_slot` already states for thread slots.

## Background

- [`AKUMA_AMD64_KOT_REPLICA_WEDGE.md`](AKUMA_AMD64_KOT_REPLICA_WEDGE.md): lead 4, where the 241 symptom was first written down.
- [`AKUMA_AMD64_EPOLLET_REARM_KOT_WEDGE.md`](AKUMA_AMD64_EPOLLET_REARM_KOT_WEDGE.md): the *other* kot-on-amd64 failure, the deaf node, fixed 2026-09-23.
- [`AKUMA_AMD64_BKL_NETWORKING.md`](AKUMA_AMD64_BKL_NETWORKING.md): the earlier `kill -9` on a thread-PID wedge (a banner-exchange timeout, not a 241; not re-examined here).
- [`STALE_THREAD_SLOT_KILL.md`](STALE_THREAD_SLOT_KILL.md), [`GRACE_EXPIRED_HARD_KILL_ORPHANS.md`](GRACE_EXPIRED_HARD_KILL_ORPHANS.md), [`KTG_STALE_TID_EXIT_STAMP_J4_HANG.md`](KTG_STALE_TID_EXIT_STAMP_J4_HANG.md): the AArch64 recycled-slot class, checked first and ruled out here.
- akuma-miot `HANDOFF.md`: "The akuma box can stop spawning after a `kill`", "`ryzen-akuma-amd64` is dead right now", "Open theory: Akuma's disk issues / spawn failures".
