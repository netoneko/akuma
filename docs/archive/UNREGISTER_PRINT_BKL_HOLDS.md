# A diagnostic print was every `[BKL] stuck` line under thread churn

Date: 2026-11-10. Branch `akuma-multiarch-attempt-0`, found while comparing the hooked and SGI
schedulers (`docs/archive/AKUMA_MULTIARCH_ATTEMPT_0.md`).

## Symptom

Under the hooked scheduler, the futex suite at SMP=4 logged about 30 `[BKL] stuck` lines (a
waiter at 8M spins); the SGI scheduler logged 1–2. All of them came from `futextest`, whose
phases spawn and join threads, about 1,200 thread exits per run. `futexops`, `futexkey` and
`futexkill` run alone produced none. With the same workload, SGI produced more of the shorter
`[bkls>]` waits (about 560 per run against hooked's 165). So the two schedulers did the same work
with the lock held for different stretches, and a 13–16 s `futextest` on SGI was just as slow.

## How it was found

1. **Tag the holds.** With `bkl-profile`, 44 of 66 stuck lines were on tag 511 ("unknown"). The
   hooked path set no tag in `irq_hooked` and none in its idle loop, and its switch did not reload
   the core's tag for the incoming thread. Once those were added, 50 of 64 stuck lines were blamed
   on `idle` (502).
2. **Time the idle hold.** A temporary probe around the hooked idle thread's
   `reclaim_terminated_slots` showed the reap itself was slow: one slot took 7.7 ms, 12 took
   76 ms and 15 took 91 ms, about 6 ms per slot, all inside one BKL hold.
3. **Remove the print.** Each reaped slot runs `unregister_process`. On AArch64 that printed
   `[unregister] pid=… tid=… has NO map owner — terminating anyway` for every thread, because the
   slot recycler has already evicted the map entry by then. Under KVM every serial byte is a trap.
   With the print suppressed, `futextest` took 2.5 s instead of about 12.8 s, with 0 stuck lines
   and 2 `[bkls>]` waits.

A first hypothesis was tested and rejected along the way: bounding the collector to 2 slots per
hold changed nothing (A/B/A/B: 30 and 28 stuck lines unbounded, 25 and 26 bounded). That fits the
cause: even one slot cost close to the stuck threshold.

## Cause

`akuma-exec/src/process/table.rs` added the print on 2026-09-18 as a TEMPORARY diagnostic. The
suspicion was that amd64 claims a slot (`x86_claim_slot`) before it publishes the map entry, so
`unregister` could terminate a live, freshly claimed thread. The amd64 self-host investigation
then saw it print zero times (`AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md`), but on AArch64 the
missing entry is the normal case, so it printed once per thread exit, forever.

On the SGI scheduler it was paid by whoever reaped: the async-main collector and thread 0's idle.
On hooked, every core's idle thread reaps, holding the BKL, so it showed as long waits on the
peers.

## Fix

The diagnostic stays but is rate-limited: the first 8 occurrences, then powers of two, each with
the running count (`(xN)`). It still names a killer if the amd64 window ever opens, without one
console line per thread exit.

Also kept: the hooked path's profiler tags. `irq_hooked` uses a transient `irq` tag, which it sets
again after a switch. The idle loop tags itself `idle`. `switch_to` reloads the core's tag for
the incoming thread. Each costs one relaxed load unless `bkl-profile` turns profiling on.

## Verification

`futextest`, SMP=4, Lima `fc`, two full runs per kernel, after the fix:

| kernel | wall time | `[BKL] stuck` | `[bkls>]` | `[unregister]` lines |
|---|---|---|---|---|
| SGI | 3.3 s, 3.1 s | 0, 0 | 0, 0 | 16, then 1 |
| hooked | 2.7 s, 2.5 s | 0, 0 | 12, 2 | 16, then 1 |

Before the fix, the same workload took 13–16 s on SGI and 11–14 s on hooked.

Also checked:

- the boot suite at SMP=1/2/4 on both schedulers. SGI's SMP=2 result comes from an alternating
  A/B against HEAD without the change, 3 boots each: HEAD 323/1, 323/1, 323/1; this change 322/2,
  324/0, 323/1. Every failure on both sides is one of the two known flakes (`orphan reparented to
  init`, `test_epoll_socket_waker`).
- the epoll and futex suites on hooked at SMP=2 and 4: all pass, 0 stuck lines.
- amd64 self-test under TCG: SMP=1 829/0, SMP=4 839/0.
- clippy: AArch64 with and without `hooked-sched`, and amd64.

## Open

- The `bkl-profile` dumps stop after about 30 s of uptime on both schedulers. The async-main
  loop that prints them stops reaching that point. Not investigated.

## Background

- `docs/archive/AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md`: the amd64 run where the print fired
  zero times.
- `docs/archive/CONSOLE_LOG_COST.md`: per-byte console cost; the same class as the gated
  `[Cleanup]` line beside the recycler.
