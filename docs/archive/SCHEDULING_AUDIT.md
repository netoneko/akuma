# Scheduling audit: docs vs code

**Date:** 2026-08-18
**Status:** Complete. A code-level review — every claim in the scheduling
runbooks/reference docs was checked against `crates/akuma-exec/src/threading/`,
`src/exceptions.rs`, `src/timer.rs`, and `src/syscall/poll.rs`. No VMs were run
for this audit; every measurement cited below is from
[`SCHEDULING_INVESTIGATION.md`](SCHEDULING_INVESTIGATION.md) and is marked as
such. The output is: the mechanism confirmed, the doc drift listed, the open
defects consolidated, and a recommendation on fix-vs-refactor.

## Executive summary

1. **The terminal-stutter mechanism is confirmed in code, not just in
   measurement.** A thread whose sleep deadline expires is flipped `READY` by
   the wake-pass in `schedule_indices`
   (`crates/akuma-exec/src/threading/mod.rs:2443-2468`) and then **joins the
   back of the round-robin queue** — nothing distinguishes it from a thread
   that has been running all along. The one queue-jump mechanism in the
   scheduler (`PREEMPT_WAKE_TID`) is (a) only set by *explicit*
   `ThreadWaker::wake` calls, never by the deadline wake-pass, and (b)
   **compiled out** (`WAKEUP_LOCALITY_HINT = false`, `mod.rs:590`). So
   eligibility ≠ execution, exactly as
   [SCHEDULING_INVESTIGATION §5](SCHEDULING_INVESTIGATION.md#5-every-poll-interval-knob-in-the-kernel-is-currently-inert)
   measured.
2. **The principled fix has existing plumbing.** "Wake preemption" (investigation
   fix option 1) does not need new machinery — it needs the deadline wake-pass
   to set a run-next hint, which is a few lines where `WAKEUP_LOCALITY_HINT`
   already gates one. See [Recommendation](#recommendation-fix-now-vs-extract-a-scheduler-crate).
3. **The reference doc has drifted.** `scheduler.md` line references are stale
   by ~600 lines, and it understates `MAX_THREADS` by 4× on default builds
   (says 64; it is 256, and only 64 on the `size` profiles). Details in
   [Doc drift](#doc-drift-found-by-this-audit). `thread-lifecycle.md` and
   `debug-futex-lost-wakeup.md` checked out clean on every claim sampled.
4. **The load-bearing invariants are real and intact** — the park/wake
   handshake (`publish_waiting_and_take_pending_wake`), the CAS guards on
   state transitions, the ON_CPU gate, POOL-across-the-whole-switch. Verified
   in source; listed in [What checked out](#what-checked-out).

## The scheduler as the code actually is

Single global run queue, one slot table, tick-driven preemption:

| Stage | Where (current) | What it does |
|---|---|---|
| Tick | `src/timer.rs:63-136` | Periodic CNTV tick at `config::TIMER_INTERVAL_US` (10 ms, `src/config.rs:834`). One handler services the alarm queue (`kernel_timer::on_timer_interrupt`), the preemption watchdog, then rings the scheduler SGI **on the current core** (`trigger_sgi_self` under smp-shared). |
| SGI entry | `mod.rs:2985` `sgi_scheduler_handler_with_sp` | `POOL.try_lock()`; on contention skips the tick (`[SGI] POOL contended` every 1000 skips, `mod.rs:3012-3017`). POOL held across the entire switch (decision + save + load). |
| BKL gate | `src/exceptions.rs:2622-2642` | EL0-preempted scheduler SGI runs BKL-free (M5c step 2, `reconcile_for_spsr_no_ticket`); EL1-preempted falls through to `enter_kernel()` at `:2665` and runs BKL-held (`:2680`). |
| Decision | `mod.rs:2425` `schedule_indices` | 1) wake-pass; 2) preemption-disabled gate; 3) network boost every `NETWORK_THREAD_RATIO` ticks; 4) experimental never-scheduled preference (off); 5) wakeup-locality hint (**off**); 6) global round-robin scan from `round_robin_idx + 1`; 7) per-core idle fallback. |
| Wake-pass | `mod.rs:2443-2468` | CAS any `WAITING` thread whose `WAKE_TIMES` deadline passed to `READY`. **Sets no run-next hint.** `sev` for WFI parkers. |
| Switch | `mod.rs:2615` `commit_switch` | ON_CPU latch both sides, CPU-time billing, outgoing→`READY` via atomic RMW (won't overwrite `TERMINATED`/`WAITING`), incoming→`RUNNING`, `LAST_CORE` record. |
| Park | `mod.rs:3563` `schedule_blocking` | Sticky-flag handshake; publish `WAITING` + re-check flag under `IrqGuard` (`publish_waiting_and_take_pending_wake`, `mod.rs:3534`); voluntary self-SGI to switch out immediately; WFI park loop with `TERMINATED`-safe resume. |
| Wake | `mod.rs:3363` `ThreadWaker::wake` | Generation-gated; sticky `WOKEN_STATES` flag; CAS `WAITING→READY`; SGI + cross-core `wake_core(last_core)` nudge. The run-next hint it *could* set is gated off. |

Blocking syscalls cap each iteration's sleep at
`effective_poll_interval_us` (`src/syscall/poll.rs:66` — 10 ms normally, 1 ms
for rump fds) via `deadline = abs_deadline.min(now + cap)` (`poll.rs:893,1004,1109`).
That cap bounds when the thread becomes **eligible**; the wake-pass + round-robin
determine when it **runs**.

## Verification of the investigation's §5 (inert poll knobs)

Each link in the claimed chain, confirmed in source:

1. `nanosleep`/`ppoll`/`epoll_pwait` all park via `schedule_blocking(deadline)`
   where `deadline ≤ now + cap`. ✔ (`poll.rs` sites above)
2. At the next tick after the deadline, the wake-pass CASes the thread `READY`.
   ✔ (`mod.rs:2443-2468`)
3. Nothing gives that thread priority: the round-robin scan
   (`mod.rs:2564-2599`) picks strictly by rotation order among `READY`
   threads; the hint path that could jump the queue is dead code
   (`WAKEUP_LOCALITY_HINT = false`, and it is only armed by `ThreadWaker::wake`,
   never by the wake-pass). ✔
4. Therefore wake latency ≈ (number of runnable threads) × tick + overhead —
   the measured ~35.5 ms floor at ~3 runnable threads and ~13 ms slope per
   additional thread (SCHEDULING_INVESTIGATION §1-§2, single config). ✔
   arithmetic consistent.

The `RUMP_BLOCKING_POLL_INTERVAL_US` comment (`poll.rs:51-59`) models the cost
as "up to 10 ms of idle wait before the poller re-checks" — i.e. it assumes
eligibility ≈ execution. On today's scheduler that assumption is false, which
is the code-level restatement of the investigation's finding that the knob is
inert and its history deserves re-verification when rump is picked up.

## What checked out

Claims sampled from the docs and verified true in code:

- **Park/wake handshake** exactly as specified in `scheduler.md` →
  "Park/wake handshake — the invariant": `WOKEN_STATES` read in
  `schedule_blocking`/`publish_waiting_and_take_pending_wake` only; the
  store-then-check pair runs under `IrqGuard`; both `SeqCst`. (`mod.rs:3534-3549`)
- **`mark_thread_terminated` is an unconditional store callable from any
  state**, and the terminal-purge vs slot-scrub split matches
  `thread-lifecycle.md` §1. (`mod.rs:1080-1134`)
- **`commit_switch` uses atomic RMW for outgoing→READY** with the
  TERMINATED/WAITING guard, and `resume_running_unless_terminated` in the park
  loop matches. (`mod.rs:2644-2650`, `mod.rs:3555-3561`)
- **`idle_halt`** disables preemption across the WFI, drops the BKL first
  under smp-shared, and shifts `start_time_us` by the halt duration — the
  accounting fix `scheduler.md` describes. (`mod.rs:2899-2953`)
- **`yield_now` under masked IRQs is detected and warned** — the
  `YIELD_WITH_IRQS_MASKED` tripwire lives where `debug-smp.md`'s "load-bearing
  rule" says the hazard is. (`mod.rs:2856-2874`)
- **The `[SGI] POOL contended` skip** prints every 1000th skip and names the
  *interrupted* thread — matching the doc's warning not to read it as the
  holder. (`mod.rs:3012-3017`)
- **Watchdog constants** (100 ms warn / 5 s panic-named-but-never-panics) now
  live in `akuma-primitives/src/preempt.rs:62,65`, re-exported through
  `threading/types.rs:60`; behaviour as documented.

## Doc drift found by this audit

All in [`../reference/subsystems/scheduler.md`](../reference/subsystems/scheduler.md)
unless noted. Reference docs are supposed to be current-state; these are the
corrections this audit produced (applied same day):

| Claim (doc) | Reality (code) |
|---|---|
| `schedule_indices` at `threading/mod.rs:1874` | `:2425` |
| `mark_thread_terminated()` at `:1022`, store at `:1067` | `:1080`, store at `:1125` |
| `THREAD_STATES` at `threading/mod.rs:316` | `:292`; state constants `types.rs:64` (doc said `:76`, and `types.rs:126` for the states list — both stale) |
| `POOL` at `threading/mod.rs:2775` | `:2744` |
| `yield_now()` at `:2221` | `:2856` |
| `schedule_blocking` at `:2555` | `:3563` |
| `Waker` at `:2470` | `ThreadWaker` struct `:3351`, `wake()` `:3363` |
| BKL-free SGI gate at `exceptions.rs:1893-1908`, `enter_kernel` `:1931`, BKL-held SGI `:1946` | `:2622-2642`, `:2665`, `:2680` |
| Watchdog warn/panic at `types.rs:66`/`:69` | `akuma-primitives/src/preempt.rs:62`/`:65` (re-exported `types.rs:60`) |
| "**`MAX_THREADS`=64 slots** (`config.rs`)" | **256** on default builds, 64 only under the `size`/`extreme-size` profile; defined in `akuma_primitives::preempt` (`preempt.rs:57-59`), re-exported via `threading/types.rs:25` and `config`. `thread-lifecycle.md` §1 already had this right. |
| `with_irqs_disabled` at `runtime.rs:256` | not re-verified line-exact (not on the scheduler critical path of this audit) |

One stale *code* comment: `mod.rs:2153` ("Thread 0 gets boosted when this
reaches NETWORK_THREAD_RATIO") — the boost correctly targets the
**registered** network thread (`NETWORK_THREAD_ID`, set by
`set_network_thread_id`, `mod.rs:76-79`), as `scheduler.md` §"Scheduling
weights" says. Left in place; noted here so the next reader of that comment
trusts the doc, not the comment.

Also recorded for the fix candidate's sake: `TIMER_INTERVAL_US` has exactly
one source of truth (`src/config.rs:834`) but **two enable call sites** —
`src/main.rs:950` (BSP) and `src/smp_shared.rs:950` (secondaries) — both
passing the same constant, plus a fallback default inside
`src/timer.rs:42` (`AtomicU64::new(10_000)`, overwritten at boot). Changing
the constant is sufficient; changing only the timer.rs fallback is not.

## Open defects register (scheduling-adjacent, consolidated)

| # | Defect | Status | Where |
|---|---|---|---|
| 1 | Short-sleep floor ≈ one round-robin pass; all poll-interval knobs inert | OPEN, root-caused, one-line fix candidate measured on one config | [SCHEDULING_INVESTIGATION](SCHEDULING_INVESTIGATION.md); mechanism confirmed above |
| 2 | `POOL`-gate wedge under fork/thread churn (box unscheduled, watchdog blind) | OPEN, reproduces; holder not yet identified — `POOL_OWNER` discriminator proposed | `scheduler.md` → "The observed wedge" |
| 3 | Residual untimed-futex lost-wake variant under SMP + preemption pressure (hist ends `Ep`, wake never issued) | OPEN | `debug-futex-lost-wakeup.md` §4a residual |
| 4 | `[FPCACHE]`/OOM coupling: none direct, but scheduler round length grows with runnable threads, so every latency-sensitive path degrades under load — the *appearance* of subsystem bugs | Not a defect per se; the reason networking was wrongly blamed for the stutter | Investigation §3 |
| 5 | rump knob history unverifiable on today's scheduler | OPEN for whoever picks rump up | Investigation §5 + Matrix B note |

## Recommendation: fix now vs extract a scheduler crate

> **Outcome (later the same day):** the fix-now path was taken and it held up.
> `WAKE_DEADLINE_PREEMPT = true` landed in the wake-pass (arming
> `PREEMPT_WAKE_TID` for the earliest-deadline woken thread), plus the 1 ms
> tick — **profile-gated** off for `extreme-size` (10 ms), which is the
> extraction-adjacent compromise this section didn't anticipate: the seam
> that mattered was the `kernel_profile_extreme` cfg, not a crate boundary.
> A/B on release SMP=1: sleep/poll 1 ms floors went ~35-41 ms → 1.0-1.1 ms,
> 0 terminal stalls (were ~1010/1500), pipe 10.4 → 3.25 µs/iter, download
> 6.3 → 3.4 s, suite 284/0. SMP=4 and devbox-smoltcp clean; 4 MB extreme
> floor boots and forks fine. Notable negative result preserved in the
> investigation: preemption at a 10 ms tick **alone** regressed the download
> ~50 % — the two changes only work together. Full data and the open
> follow-ups: [`SCHEDULING_INVESTIGATION.md`](SCHEDULING_INVESTIGATION.md) →
> Resolution. The extraction advice below stands for whenever policy
> complexity actually grows.

**Fix now, in place, behind an existing-style gate. Do not extract a crate for
this.** Reasoning:

1. **The candidate fixes are tiny and local.**
   - Option A (blunt): `TIMER_INTERVAL_US` 10 000 → 1 000 — one constant,
     `src/config.rs:834`.
   - Option B (principled): wake preemption — in the deadline wake-pass, set a
     run-next hint for the thread just readied. The plumbing already exists
     (`PREEMPT_WAKE_TID` consumed at `mod.rs:2545-2554`); the missing piece is
     arming it from the wake-pass (`mod.rs:2463`) instead of only from
     `ThreadWaker::wake`, and gating it on its own const rather than
     repurposing `WAKEUP_LOCALITY_HINT` (whose comment documents a different,
     falsified rationale — keep the experiments distinct).
2. **Old-vs-new gating already works without any extraction.** The codebase's
   established A/B pattern is an in-tree toggle measured by a probe:
   `WAKEUP_LOCALITY_HINT` and `PRIORITIZE_NEVER_SCHEDULED` (const gates in the
   crate), `sched_bklfree_el0_enabled` / profiler toggles (runtime gates),
   `ncaprobe` + boot-suite as instruments. Two `cargo build`s, `cmp` the
   `.bin`s, run Matrix A from the investigation. That is precisely the
   "gating" you wanted, at zero refactor cost.
3. **Extraction buys no test capability that doesn't already exist.**
   `akuma-exec` is host-testable today (`park_wake_race_tests`,
   `state_transition_guard_tests` run on the host target), and the boot suite
   covers SMP behavior in-VM. A separate crate would have to drag along
   `Context`, the asm switch, `runtime()` hooks, the BKL and POOL
   relationships — the highest-churn, highest-defect-density surface in the
   kernel (every archive doc cited above touches it) — for a behavior question
   that a const flag answers.
4. **Extraction risk is real, not hypothetical.** The scheduler's correctness
   is a lattice of ordering invariants (park/wake SeqCst pairing, ON_CPU
   latch windows, POOL-across-switch, per-core voluntary flags). The last
   several defects here were *ordering* bugs; a refactor that moves code
   across crate boundaries risks perturbing exactly those for zero behavioral
   gain, and per `docs/README.md` stability grades this subsystem's neighbor
   docs are graded C for churn.

**Suggested order of operations** (small, each step measurable):

1. Land nothing yet. Run Matrix A/B (10 ms vs 1 ms) per the investigation's
   ground rules — it is the decision instrument for option A and the baseline
   for option B.
2. If A is clean everywhere including `extreme-size` idle CPU: land A as the
   default and keep the rump question flagged (investigation Matrix B note).
3. Implement B behind a new const (e.g. `WAKE_DEADLINE_PREEMPT: bool = false`
   beside `WAKEUP_LOCALITY_HINT`), A/B it against both A and baseline with
   `ncaprobe pollbench`/`sleepbench`. If B delivers A's latency win at the
   10 ms tick, prefer B as the long-term default and treat A as the stopgap.
4. Whichever lands, re-check defect #2's repro load (`bssfork` churn at
   SMP=4) — anything that makes waiters runnable sooner slightly raises
   scheduler-entry frequency; the `POOL` wedge must not get easier to hit.

If, later, policy complexity grows (multiple priority classes, tickless), *then*
extracting a policy module (just `schedule_indices`'s pick logic, behind the
existing `runtime()`/atomics seams — not the switch machinery) becomes worth
it. That seam is already clean: the pick is a pure function of the atomics.

## Background

- [`SCHEDULING_INVESTIGATION.md`](SCHEDULING_INVESTIGATION.md) — the
  measurements this audit's mechanism verification rests on, and the open
  measurement matrix (the handoff).
- [`../reference/subsystems/scheduler.md`](../reference/subsystems/scheduler.md)
  and [`thread-lifecycle.md`](../reference/subsystems/thread-lifecycle.md) —
  the reference docs audited (scheduler.md corrected by this audit).
- [`../runbooks/debug-smp.md`](../runbooks/debug-smp.md),
  [`../runbooks/debug-futex-lost-wakeup.md`](../runbooks/debug-futex-lost-wakeup.md),
  [`../runbooks/debug-ssh-latency.md`](../runbooks/debug-ssh-latency.md),
  [`../runbooks/debug-async-subprocess-hang.md`](../runbooks/debug-async-subprocess-hang.md)
  — the runbooks audited.
- [`SMP_SHARED_ONCPU_GATE.md`](SMP_SHARED_ONCPU_GATE.md),
  [`THREAD_STATES_RACES_TID_GENERATIONS.md`](THREAD_STATES_RACES_TID_GENERATIONS.md)
  — the archive records behind the ON_CPU / CAS-guard invariants verified above.
