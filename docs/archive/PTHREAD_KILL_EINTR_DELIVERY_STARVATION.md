# `pthread_kill` could not interrupt a blocking syscall under a fast signal source

**Date:** 2026-08-28
**Scope:** exposed by [`IDENTITY_CACHE_LAZY_RESTAMP.md`](IDENTITY_CACHE_LAZY_RESTAMP.md),
which made the syscall path fast enough to lose a race it had always been running.
**Status:** **FIXED — but only after a second fix.** The `DELIVERED_SIGNALS` mask
below is necessary and is retained; it was **not sufficient**, and the "FIXED"
claim originally recorded here was wrong. `scripts/verify_trim.py` — which this
document says was deliberately not run — reports
`smp1.ex.pthread_kill_eintr: TIMEOUT` on the very commit that landed it, and the
probe failed **9 of 10** by-hand runs at SMP=1. See
[§ Re-verified 2026-08-28](#re-verified-2026-08-28--the-first-fix-did-not-hold).
The guard is that probe, a Tier 3 exercise rather than a boot test.

## Summary

A blocking syscall reports `EINTR` by **observing** a pending-signal bit in its
wait loop. Signal *delivery* clears that bit. Delivery runs on the return-to-EL0
paths — including `rt_sigreturn`, which takes the next pending signal
immediately after restoring the previous handler's context.

So under a signal source fast enough to keep the
**deliver → handler → `rt_sigreturn` → deliver** chain saturated, the bit is
always already cleared by the time the blocked loop is scheduled to look at it.
The handler runs, and runs, and runs; the syscall it was supposed to interrupt
never returns:

```
PHASE1 FAIL: read() never returned; helper thread leaked (handler ran 64 times)
```

That is **starvation, not merely a race**: delivery has strict priority over
resuming the interrupted syscall, and nothing bounds how long it keeps that
priority.

## Why it surfaced now

It did not surface *now*; it became reachable now. The window the wait loop had
been winning by was the slow-path work in every syscall — and
[`IDENTITY_CACHE_LAZY_RESTAMP.md`](IDENTITY_CACHE_LAZY_RESTAMP.md) deleted it,
taking the identity cache from a 0.11 % hit rate to 99.999 %. The fix was
correct; the margin it removed was load-bearing for a bug nobody knew was there.

Reproduced 2/2 at SMP=1 with `host_timejumps: 0`, against an `ok` baseline on the
parent commit. **SMP=4 stayed green throughout** — more cores means the blocked
thread gets scheduled between deliveries — which is the trap: the multi-core arm
cannot see this class at all.

## How it was identified

Two measurements, neither of them a code reading.

**1. Bisect on the trigger.** `MAX_REPAIR_ATTEMPTS = 0` — the cache repair
disabled, every other line identical — passes in 8 s. `= 4` hangs. So the
trigger is cache-hit *speed*, not any branch the cache changed.

**2. It is a Heisenbug.** Adding a `safe_print!` trace to
`should_interrupt_blocking_syscall` made it **pass**, PHASE1 and PHASE2. The
trace is console I/O; it gives the loop back exactly the microseconds the cache
fix took away. The trace itself showed the window directly — one sample in
fourteen catching the signal:

```
[EINTRDBG] pend!=0 hit=1 intr=0 cfg=1 pend_intr=0 deliverable=1 rcp=1
[EINTRDBG] pend!=0 hit=1 intr=0 cfg=1 pend_intr=1 deliverable=1 rcp=1   <-- 1 of 14
```

A fix that makes a symptom disappear when you instrument it is not a fix. That
reading is what ruled out every "wrong branch" hypothesis below and pointed at
timing.

### What was ruled out, and why each looked right

- **`is_current_interrupted()` has two sources of truth.** It reads the
  process-wide channel (Ctrl-C / `sys_kill`) and used to `return` its answer,
  never consulting the per-thread channel. Genuinely wrong-looking, and the
  broken cache *had* been routing around it (a miss made `current_process_shared()`
  return `None` and fell through). **Fixing it changed nothing**, because the
  combining already happens one level up in `should_interrupt_blocking_syscall()`,
  and neither channel is where `tkill` lands anyway. Reverted rather than left in
  as an unverified behaviour change.
- **`read_current_pid()`** — does not touch the identity cache. Its `tgid`
  degradation window is real but unrelated.
- **The `term.rs` stdin wait loop** — never entered. A trace placed there
  produced zero output: PHASE1 blocks on a **pipe** (`pipe2=2` in `[PSTATS]`),
  not on stdin.

## Root cause

`crates/akuma-exec/src/process/children.rs`,
`current_thread_has_pending_interrupt()`:

```rust
let pending = crate::threading::pending_signals_raw(tid);
if pending == 0 { return false; }
```

`PENDING_SIGNALS` is cleared by `take_pending_signal` at the moment of delivery.
There are **three** delivery sites in `src/exceptions.rs` and the third is the
one that matters:

| line | path |
|---|---|
| 3895 | IC-flush / JIT replay |
| **4049** | **`rt_sigreturn` — takes the next pending signal right after restoring the handler's context** |
| 4142 | ordinary syscall return |

4049 is the chain. Each handler return immediately consumes the next signal, so
the interrupted syscall is never resumed long enough to notice it was
interrupted.

## The fix

A second per-thread mask, `threading::DELIVERED_SIGNALS`, that records what was
**delivered** rather than what is still pending — so the observation survives the
delivery that clears the pending bit.

- **Set** in `try_deliver_signal` itself, not at the seven call sites. Recording
  at the single chokepoint keeps the two in step: a new delivery path gets it for
  free.
- **Read** by `current_thread_has_pending_interrupt`, which now tests
  `pending | delivered` through the same mask / `SA_RESTART` / user-handler
  filter as before.
- **Consumed** — only the one signal's bit, and only once that filter has decided
  it produces an `EINTR`, so a single delivery cannot interrupt several later
  syscalls in a row.
- **Cleared** at syscall entry, so a record cannot leak into an unrelated later
  syscall and fabricate an `EINTR` there — **except for `rt_sigreturn`**, which
  is the syscall the handler returns *through*. Clearing there would erase the
  record belonging to the blocking syscall about to resume, which is the bug
  itself.

`SA_RESTART` semantics are unchanged, and that is the assertion that matters
most: an over-eager sticky flag would report `EINTR` for restartable handlers and
break every blocking syscall a Go program makes. PHASE2 tests exactly that and
passes.

## Verification

```
PHASE1 PASS: read() = -1 EINTR after 13 handler runs
PHASE2 PASS: SA_RESTART read() never reported EINTR, then returned 1 byte
RESULT: PASS
```

SMP=1, `MEMORY=2048`. Boot suite **98 `[PASS]`**, failure set
`{retired_reclaim_ab}` (the documented run-to-run flake), `host_timejumps: 0`.
Both identity boot tests pass. Tier 3 by hand: `forkprobe`, `elftest`,
`madvshared`, `mremapmove`, `cowstale`, `bssfork 20 8 1` — all PASS. All four
clippy configurations clean; host tests **858 / 0 failed**.

**`scripts/verify_trim.py` was NOT run**, deliberately: it opens with a blanket
`pkill -f qemu-system-aarch64` in four places, and another QEMU belonging to the
user was running. Run the full gate when the box is free.

## Re-verified 2026-08-28 — the first fix did not hold

Everything above this line is the original investigation, kept as written. This
section is the correction.

### What the gate said

`scripts/verify_trim.py --tier all` on `649cdb38` (the commit that landed the
mask), 659 s. Every other measurement sat exactly on the runbook's 2026-08-28
baseline — clippy 4/4 clean, host tests 858/0, `pass_marker` 99 at both SMP
levels, `fail_set` empty at both, `host_timejumps: 0` at both, `eager_mprotect`
KNOWN-FAIL as expected, `smp4.cowstale` UNEXPECTED (the documented pre-existing
~40 %). One line deviated:

```
smp1.ex.pthread_kill_eintr: TIMEOUT (still running after 420s)
smp4.ex.pthread_kill_eintr: ok
```

SMP=4 passing proves nothing — Lesson 2 below says so, and it was right.

By hand at SMP=1, `MEMORY=2048`, same commit, 10 runs: **1 PASS, 9
`PHASE1 FAIL: read() never returned; helper thread leaked (handler ran 45-57
times)`**. Every failing run also *never terminated* — main parked in `futex` on
`pthread_join` (`uaddr=0x10465f48`) with the phase-2 helper alive at
`cpu_us=25`, which is why the gate reports TIMEOUT rather than a FAIL marker.
**A probe that wedges on failure is a weak guard**; that is a second finding.

### How it was identified: measure from userspace, not from the console

Lesson 3 below says "if instrumenting it makes it pass, stop reading code" — but
the instrument it warns about is a `safe_print!` *in the wait loop*. The way out
is an instrument that costs the loop nothing: a **userspace** probe
(`pkdiag.c`) that records to memory and prints only after the run. It answered
in one build what three rounds of code reading could not:

| Measurement | Reading |
|---|---|
| `pthread_kill` vs raw `SYS_tkill` | Identical. Handlers land 100 % on the helper (`main=0 helper=65`) — the tid plumbing is correct |
| `read` return value | **Always** `-1/EINTR`. Nothing is lost — `DELIVERED_SIGNALS` does its job |
| 100-signal storm | `read` returns at ~1.20 s, i.e. **28 ms after the 100-attempt window closed**; `handler_total` 65 |
| **ONE** signal | `read` returns **13-27 ms** later, 1 handler run. So the storm is not the bug |
| Split of that round trip | `signal->handler` **9.5-20.5 ms**; `handler->read_ret` 0.24-6.9 ms |
| 50 ms cadence instead of 10 ms | **Passes**, one signal, 13.7 ms |
| Pipe *data* wake, no signals at all | `write -> read_ret` **~1005 us** — one 1 ms tick |

The last two rows are the whole diagnosis: the mechanism is a **rate**, not a
lost wakeup. A kernel trace (`uptime_us` stamps, not console prose) pinned the
rest: `pend -> helper actually running` **4405 us**, then `EINTR` decision ->
handler-frame setup **185 us**. The tick was 1000 us with no governor demotion,
so 4.4 ms is ~4 ticks, not one.

### Root cause: two costs, and the mask addressed neither

**1. Eligibility is not execution.** `pend_signal_for_thread` wakes the target
correctly (the `WAITING->READY` CAS fires, an SGI goes out), and then the thread
joins the **back of the round-robin queue** — so it waits ~`tick x
runnable-threads` before it can look at the bit the mask so carefully preserved.
This is the *same floor* `WAKE_DEADLINE_PREEMPT` was added for on 2026-08-18
(`SCHEDULING_AUDIT.md`), reached by a different path: that arm covers the
*deadline* wake-pass, and a signal wake never touches it.

**2. Delivery's priority over resuming the interrupted syscall was unbounded.**
This is the original document's own sentence — "delivery has strict priority over
resuming the interrupted syscall, and nothing bounds how long it keeps that
priority" — and the mask does not bound it. The `EINTR` is computed and stashed
in the trap frame's `x0`, then a handler frame is installed on top, and
`rt_sigreturn` takes the next pending signal **immediately** after restoring the
handler's context. Userspace therefore never executes the instruction after its
`svc` while signals keep arriving. Linux has the same re-check on its exit path
and does not starve, because a delivery round trip there costs microseconds;
Akuma's costs milliseconds, and against a 10 ms sender the chain refills faster
than it drains.

### The second fix

Both halves are needed; each was measured on its own.

- **`threading::SIGNAL_WAKE_PREEMPT`** — arm the existing one-slot
  `PREEMPT_WAKE_TID` hint from `pend_signal_for_thread`, so a signal-woken thread
  runs on the *next* switch. This is a third, independently gated arm on that
  hint, deliberately **not** the blanket `WAKEUP_LOCALITY_HINT` (still off: it
  preempts a producer with compute left and cost llama decode 2.5 -> 1.15 tok/s).
  A signal wake is not a compute handoff, it is rare, and the woken thread's next
  act is to return to userspace.
- **`threading::SIGFRAME_ACTIVE`** — bound delivery to **one handler per unit of
  userspace progress**. Set at the `try_deliver_signal` chokepoint; consulted by
  `rt_sigreturn`, which now returns to the restored context instead of
  re-delivering; cleared at syscall entry, with the **same `rt_sigreturn`
  exemption** the mask already needed and for the same reason. A signal that loses
  the race stays pending and is delivered at the next kernel exit — which is
  Linux's model minus the unbounded same-exit re-arm. All three delivery sites are
  syscall paths, so the flag cannot get stuck.

### Verification

| Tree | Result at SMP=1 |
|---|---|
| `649cdb38` (mask only) | **1 / 10** PASS, `handler ran 45-57 times`, failures never terminate |
| \+ `SIGNAL_WAKE_PREEMPT` | **9 / 10** PASS — better, still margin, still `51 / 38 / 66` handler runs |
| \+ `SIGFRAME_ACTIVE` | **15 / 15** PASS, and **`after 1 handler runs` on every single run** |

That last column is the point: the first signal interrupts, deterministically,
instead of the chain racing the sender's cadence. `98 [PASS]` boot suite with
failure set `{retired_reclaim_ab}` (the documented flake the runbook says to read
as equal to empty).

### Corrected lessons

1. **A margin fix reads exactly like a real fix at 1 sample.** The original
   verification ran the probe once, got PASS, and wrote FIXED. The distribution
   was 1-in-10. Run a probe that guards a *race* ten times, not once — the
   runbook already says a single SMP=1 exercise run proves nothing.
2. **"Deliberately not run" is where the bug was.** The one gate this document
   declined to run is the one that caught it, and the stated reason (someone
   else's QEMU) had expired long before the claim was written down.
3. **Lesson 3 needs a corollary.** "Instrumenting it makes it pass" rules out
   *console* instruments, not all instruments. A userspace probe that records to
   memory perturbs the loop by nothing and answered this in one build.
4. **Two mechanisms can wear one symptom.** `PENDING_SIGNALS` being cleared by
   delivery was real, and fixing it was necessary. It was not what made the
   probe fail.

## Lessons

1. **A performance fix can be a correctness change.** Nothing about the identity
   cache repair touched signals. It removed microseconds, and microseconds were
   what made this work.
2. **SMP=4 cannot see this class.** Both the regression and its baseline were
   green at 4 cores across every run. A single-core arm is not redundant.
3. **If instrumenting it makes it pass, stop reading code.** Three plausible
   wrong-branch hypotheses were investigated and disproved before the Heisenbug
   reading settled it. That reading was available in one build.

## Background

- [`IDENTITY_CACHE_LAZY_RESTAMP.md`](IDENTITY_CACHE_LAZY_RESTAMP.md) — the change
  that exposed this, and the hit-rate measurement
- [`IDENTITY_CACHE_SMP_REVIEW.md`](IDENTITY_CACHE_SMP_REVIEW.md) — Findings A and
  B, the safety windows in the same code
- `userspace/forktest/c_stress/pthread_kill_eintr.c` — the probe; PHASE2's
  `SA_RESTART` arm is the guard against over-reporting `EINTR`
