# `pthread_kill` could not interrupt a blocking syscall under a fast signal source

**Date:** 2026-08-28
**Scope:** exposed by [`IDENTITY_CACHE_LAZY_RESTAMP.md`](IDENTITY_CACHE_LAZY_RESTAMP.md),
which made the syscall path fast enough to lose a race it had always been running.
**Status:** **FIXED.** `pthread_kill_eintr` PHASE1 + PHASE2 pass; the guard is
that probe, which is a Tier 3 exercise rather than a boot test.

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
