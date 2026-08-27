# The identity cache ran at 0.1% hit rate under thread churn

**Date:** 2026-08-28
**Scope:** follow-up to
[`IDENTITY_CACHE_SMP_REVIEW.md`](IDENTITY_CACHE_SMP_REVIEW.md), which went
looking for two *safety* windows and found a *performance* one instead.
**Status:** **FIXED** and measured. Boot test
`identity_lazy_restamp` (`src/process_tests.rs`) is the regression guard.

## Summary

`c2a0e630` took `getpid` from 410 ns to 150 ns by replacing up to nine "who am
I" resolutions per syscall with one cached identity
([`AKUMA_SYSCALL_PERFORMANCE_AUDIT.md`](AKUMA_SYSCALL_PERFORMANCE_AUDIT.md)).

The cache was **stamped exactly once per thread and never repaired**, so a
thread that lost a startup race ran its whole life on the slow path. Under
thread churn that was most of them:

| | hits | misses | hit rate |
|---|---|---|---|
| before | 617,271 | 556,569,997 | **0.11 %** |
| after | 529,496,761 | 5,734 | **99.999 %** |

Same load, same instrumentation (`IDENTITY_AUDIT=true`), same `SMP=4
MEMORY=2048` box. **~556 million slow-path table scans per run**, each a lock +
map walk + IRQ-masked scan of the whole process table — the exact work the
identity cache was built to delete. The win was real at idle and absent from the
multithreaded workload it was built for.

## How it was found

Not by inspection. `IDENTITY_CACHE_SMP_REVIEW.md` recorded two use-after-free
windows found by reading the code, and the question put to this session was
whether to fix them or build something else. The answer was **measure first**:
flip `config::IDENTITY_AUDIT` to `true` and see whether the windows are live.

They were not — `epi_stale=0 epi_moved=0` on every heartbeat, through a load of
6 × (`bssfork 20 8 1` + `forkprobe` + `pthread_kill_eintr` + `stackstress`)
alongside `forktest_parent -duration=45s`.

But the **fallback counters in the same `[IDENT]` line are not gated on
`IDENTITY_AUDIT`, and nobody had ever read them under load.** They said this:

| phase | hits | miss / cleared |
|---|---|---|
| early boot | 264,574 | 6,817 |
| during load | 617,271 | 63,872,533 |
| idle after, 4 consecutive heartbeats | 1.33M → 2.41M, climbing | **556,569,997 — frozen, identical all four** |

The frozen-at-idle reading is what made it diagnosable rather than alarming.
The cache is *perfect* in steady state: across four 30-second heartbeats with
the box idle, `miss` does not move at all while `hits` climbs. So this is not
a broken cache — it is a cache that a specific event knocks out permanently,
and the event is thread creation.

It was present on the shipping config too (`audit=0`), just smaller because the
gate's boot is not thread-heavy: `cleared` climbing 42,450 → 67,978 → 110,597
across three heartbeats at SMP=4, and the same shape at SMP=1.

## Root cause

`identity_store_locked` has exactly one caller — `thread_pid_map_insert`, at
thread-map insert time — and `identity_get`'s miss arm returned `None` **without
ever re-stamping**. So `INVALID_SLOT` once was `INVALID_SLOT` for the rest of
that tid's life.

The way in is documented, and dismissed, in `identity_store_locked`'s own
header:

> An unresolvable pid (insert raced ahead of `register_process`) stores the
> invalid marker: fast paths miss, slow paths answer, **nothing is wrong**.

Nothing is wrong *for correctness* — every reader falls back and gets the right
answer. But the marker is permanent, so losing that race once at thread creation
disables the cache for that thread forever, and under churn a large fraction of
new threads lose it. `thread_pid_map_insert`'s own doc already names the failure
mode — "or none, so the thread runs on the slow path forever" — but attributes
it to callers bypassing the wrapper, when the sanctioned wrapper reaches it too.

### The counter naming hid it for a month

`own_pid` was stored even when slot resolution failed, and the miss arm
classified on `own_pid != 0`:

```rust
if e.own_pid.load(Relaxed) == 0 { IDENTITY_FB_UNSTAMPED } else { IDENTITY_FB_CLEARED }
```

So a thread that **never resolved** was counted as `CLEARED` — documented as "the
entry *was* stamped and is now invalid. A genuine cache loss: the thread had an
identity and lost it." That is a description of a different event. It is why the
numbers read `cleared=556,569,833` against `unstamped=164` and looked like
catastrophic cache eviction rather than a stamp that never landed.

The classification was also indifferent to which half was asked for: a miss on
the **tgid** half — which is what the syscall prologue uses — was classified by
looking at `own_pid`.

## The fix

**Lazy re-stamp on miss, bounded.** `crates/akuma-exec/src/process/table.rs`.

1. `identity_get`, on a miss that carries a stamped pid, re-runs
   `identity_store_locked` under an IRQ mask and retries the read once. Nesting
   is safe (`IrqGuard` saves and restores DAIF) and nothing on this path takes
   `THREAD_PID_MAP`, so it cannot deadlock against the insert that calls the
   same function with the lock held.
2. `repair_attempts: AtomicU8` per entry, capped at `MAX_REPAIR_ATTEMPTS = 4`.
   **The bound is as load-bearing as the repair.** Without it, an entry whose pid
   never registers trades one permanent slow path for a permanent *table scan*,
   which is strictly worse than what was there before.
3. `identity_clear_locked` now zeroes `own_pid`/`tgid` too. This is not
   tidiness: `thread_pid_map_remove` is the one sanctioned invalidation in the
   cache, and leaving the pid set would let the repair path resolve the process
   again and hand back an identity that call just revoked. Zeroing makes a
   cleared entry read as `UNSTAMPED` — unrepairable by construction, and the
   honest description.
4. Because of (3), `CLEARED` now means only "a pid is stamped and does not
   resolve", which is what its docs always claimed.

The hit path is untouched — no extra load on a hit, one extra `AtomicU8` per
entry.

### The budget reset was got wrong twice, the same way

Both times the bug was the same shape — the reset makes the bound unreachable,
so an entry that can never resolve re-scans the whole table on *every syscall*,
which is strictly worse than the permanent slow path this set out to fix.

**Attempt 1 — reset unconditionally.** Every failed repair zeroes the counter it
is about to increment, so the entry sits at 1 attempt forever. The boot test
caught it on its first run:

```
[Test] identity_lazy_restamp FAILED: ... bounded=false (attempts=1 failed_delta=7 budget=4)
```

**Attempt 2 — reset when the OWN half resolved.** This passed the boot test and
the whole SMP=4 gate, and still shipped a hang, because it missed a state that is
completely ordinary: a `CLONE_THREAD` sibling whose own pid resolves and whose
**group leader has exited**. The syscall prologue asks for the *tgid* half, so
every syscall repaired → reset → repaired again, at two table scans each
(`identity_store_locked` makes a second pass for the tgid half).

What caught it was `pthread_kill_eintr`, which is a Tier 3 exercise and not a
boot test:

| | SMP=1 | SMP=4 |
|---|---|---|
| baseline (fix stashed) | `ok` | `ok` |
| attempt 2 | **TIMEOUT (>420 s), 2/2 runs** | `ok` |

`host_timejumps: 0` on every one of those runs, so it was not host starvation.
**SMP=4 stayed green throughout** — the other cores absorbed the scans — which is
the part worth remembering: a single-core regression that the multi-core arm
cannot see.

**Final rule: reset only when BOTH halves resolved.** A half that cannot resolve
must be allowed to exhaust the budget and stop; the other half is unaffected,
since once `own_slot` is valid `identity_get` hits on it and never reaches the
repair path. The boot test now covers the half-resolvable case directly
(`half_bounded`), by registering a process whose `tgid` names a leader that was
never registered.

## Verification

`repairs=421 repair_failed=4` after the load. The 4 failures are the boot test's
own ghost pid exhausting its budget — **zero unexplained repair failures in real
operation**, so nothing in this workload has a pid that never registers.

Boot suite unchanged: **99 `[PASS]`, empty failure set, `host_timejumps: 0`** at
SMP=4, identical to the pre-fix baseline. All four clippy configurations clean;
host tests **858 / 0 failed**, unchanged.

> **Status of the final revision:** the numbers above were measured on attempt 2,
> whose *repair* behaviour is identical — the fix that followed changes only when
> the budget resets, which cannot affect a resolution that succeeds. The hit-rate
> result therefore stands. **What has not been re-run on the final revision is
> the boot suite and Tier 3**, including the `pthread_kill_eintr` case that
> caught the regression. Re-run `scripts/verify_trim.py --tier all` before
> treating this as closed.

### What was NOT measured

**No nanosecond-level latency measurement was taken.** The evidence here is
counter-based: 556M slow paths became 5.7K, and the ~420 repair scans that
replaced them are a rounding error against that. That is decisive for the
*direction*, and it is not a substitute for
`userspace/ext2probe/c/read_syscall_cost.c` on the dispatch, which should be run
before quoting a new `getpid` figure. See
[`../runbooks/verify-trim-fat-change.md`](../runbooks/verify-trim-fat-change.md)
§"Performance guards".

Note also that the before/after runs were on the same host in different power
states, so **do not read wall-clock or per-second rates across them** — the
comparison that holds is hits-vs-misses within each run.

## What this does not fix

Findings A and B of [`IDENTITY_CACHE_SMP_REVIEW.md`](IDENTITY_CACHE_SMP_REVIEW.md)
are **still open**. They are safety defects (entries that resolve to something
freed); this was a performance defect (entries that resolve to nothing). They are
independent, and Finding A not reproducing under this load is not a clearance —
the audit only samples excursions that happened.

One thing this fix does change for them: with the cache now actually hitting,
Finding B's window — a recycled slot reading ACTIVE while the cached pointer
dangles — is reached ~1000x more often than it was, because before this fix
almost every resolution took the slow path that re-validates. **Fixing the
generation check (Finding B) is now more urgent than it was this morning, not
less.**

## Background

- [`IDENTITY_CACHE_SMP_REVIEW.md`](IDENTITY_CACHE_SMP_REVIEW.md) — the two safety
  windows, and §"Measured 2026-08-28" for the run that turned this up
- [`AKUMA_SYSCALL_PERFORMANCE_AUDIT.md`](AKUMA_SYSCALL_PERFORMANCE_AUDIT.md) —
  the 410 ns → 150 ns work whose win this restores under load
- [`AKUMA_EXTRACT_SYSCALLS.md`](AKUMA_EXTRACT_SYSCALLS.md) §7 — the shape crate
  that was proposed instead, and why it was not built
