# A zombie's tid was reissued before the reap — 2026-10-16 (ryzen)

**Status: FIXED (amd64) and host-tested; not yet run on hardware. The incident
that prompted it was NOT this bug's doing — see §1.**

## 1. What the incident actually was

The account written after the ryzen wedge was: "`kill -9 1468` (a zombie) resolved
1468 → tid 20 → delivered SIGKILL to pid 1619, the tid's new owner". The evidence
says otherwise.

- `ssh_tui_probe.py`'s cleanup, as it stood then, printed
  `cleanup: killing leaked ['1468', '1619']` (session transcript, 17:04:51 UTC) —
  **the script sent `kill -9 1468 1619`.** 1619 was named on purpose, as a live
  `claude` the closed PTY had left behind.
- `[kill] pid=1468 stale tid=20 now owned by pid=1619` is printed by
  `signal::slot_still_owned_by` **when it returns `false`** — that is
  `pend_signal_to_group` *dropping* tid 20, i.e. the guard working. On amd64 every
  `kill(2)` goes through `pend_signal_to_group` (`amd64/src/signal.rs::sys_kill`).
- `[signal] pid=1619 killed by signal 9` is 1619 taking its own SIGKILL;
  `pid=1622` is consistent with a thread row of 1619 (each thread is a `Process`
  here, and `fatal_default` prints `proc.pid`).
- The `[bkls>]` BKL-slow lines on cores 6 and 3 are in `klog-132.dmesg` **before**
  the kill, and after it the kernel kept logging (`PSTATS`, `herd` reload). There is
  one `[TRAMP-MISMATCH]` line, not a storm. The wedge fits the "SIGKILL of a live
  multi-threaded tree wedges ryzen" class (Chromium, 2026-10-09, `[BKL] stuck
  tag=501`) at least as well as anything stale-tid; it is not explained here.

## 2. The bug: a tid reused while its zombie still names it

On amd64 the task slot *is* the tid. `run_process` → `sched::finish` made a dead
leader's slot claimable at once (`x86_claim_slot` takes a `TERMINATED` slot
directly), while the process sat as a zombie, until its parent's `wait4`, still
recording `thread_id = Some(slot)` and its `THREAD_PID_MAP` row. The tid then named
two processes:

| window | map row for the slot | effect of signalling / unregistering the zombie |
|---|---|---|
| next occupant's claim → publish | still the **zombie's** | `slot_still_owned_by` says "ours": `kill(zombie)` pends + interrupts the **new occupant**; `retire_process` `mark_thread_terminated`s it; `exit_group_from_thread`'s `thread_for_pid` posts a kill request to it |
| after the publish | the new occupant's | guards hold (`[kill]`/`[unregister] … stale tid` lines), but every `p.thread_id == Some(tid)` scan finds the zombie → `[TRAMP-MISMATCH] … table scan found <dead pid>` |

AArch64 is not exposed: its slot only goes `TERMINATED → FREE` through the
collector, whose `on_thread_cleanup` unregisters the process first.

## 3. The fix: the tid is not reissued until the reap

Like a Linux pid, a zombie's tid now stays its own until it is reaped.

- `akuma-threading`: `SLOT_HELD_BY[slot]` (pid, 0 = none),
  `hold_slot_for_zombie` / `release_slots_held_by(pid)` / `slot_held_by`, and
  `slot_reusable(state, on_cpu, held_by)` — the one rule both `x86_claim_slot` and
  `x86_reap_terminated_slots` apply. Not scrubbed with occupant state.
- `akuma-exec`: `hold_exited_thread_slot(pid, tid)` (only if the map still says
  the slot is `pid`'s); `retire_process` calls `release_slots_held_by(pid)` **after**
  its `mark_thread_terminated`, so that terminate can never land on a new occupant.
  Release is by pid, so a kill path that already cleared `thread_id` cannot strand
  a hold.
- `amd64/src/usermode.rs::run_process`: the hold is the first thing after
  `enter_user` returns — while the slot is still running (a slot is only claimable
  once `TERMINATED`, so the hold cannot race a claim) and before
  `spawn_record_exit` lets the parent reap (a hold set after a reap would never be
  released).

The zombie's records stay true until the reap because nothing else can be on that
slot: `kill(zombie)` returns 0 and pends on the zombie's own dead slot (scrubbed
at the next claim), and the table scan and the map agree.

**Cost:** one slot (of 512 on amd64) per unreaped zombie, as on Linux. A zombie
nobody reaps now pins a slot until reboot; init's orphan sweep is what bounds that.

Also: `akuma_exec::register_threading` was split out of `init` so the host tests
register the real threading table (a reap reaches `mark_thread_terminated`).

## 4. Proof

- `crates/akuma-exec/src/process/signal.rs`, `ryzen_2026_10_16_zombie_tid_tests`:
  real `Process` rows; a zombie's slot is not reissuable, `kill(zombie, 9)`
  succeeds with consistent records, the reap returns the slot (also with
  `thread_id` already cleared), no hold on a slot the map gives to someone else.
  With the hold disabled the first two fail.
- `crates/akuma-threading/src/lib.rs`, `ryzen_2026_10_16_slot_hold_tests`: the
  `slot_reusable` truth table and per-pid release.

## 5. Still open

- **Unreaped `claude` zombie.** Separate bug, and now it costs a slot each.
  sshd (`fork-sessions`) leaves the bridge loop with `?` when the client vanishes,
  without `teardown_child` or a reap; the child is reparented to init (herd) when
  the session process exits, whose orphan sweep should reap it. 1468 survived at
  least two probe runs as `Z`. Next look: `PPid:` in its `/proc/<pid>/status`, and
  whether that sshd session process exited.
- **The wedge itself** (§1, last bullet).
- A thread that dies without `amd64::thread::teardown` (fault / hard kill) is the
  separate no-slot-recycler row ([`AKUMA_AMD64_NO_SLOT_RECYCLER.md`](AKUMA_AMD64_NO_SLOT_RECYCLER.md)).

## Background

- [`AKUMA_AMD64_NO_SLOT_RECYCLER.md`](AKUMA_AMD64_NO_SLOT_RECYCLER.md) — why amd64
  claims `TERMINATED` slots directly.
- [`STALE_THREAD_SLOT_KILL.md`](STALE_THREAD_SLOT_KILL.md) — the original
  `slot_still_owned_by` guard.
- [`TRAMPOLINE_STALE_PROCESS_RELR.md`](TRAMPOLINE_STALE_PROCESS_RELR.md) — why
  `THREAD_PID_MAP` beats the `thread_id` scan.
