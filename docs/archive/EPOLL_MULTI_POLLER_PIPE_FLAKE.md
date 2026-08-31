# `test_epoll_multi_poller_pipe`: a boot-suite flake that is a test defect, not a kernel bug

**Status**: **ROOT-CAUSED and fixed 2026-09-01 — read §9 first.** The 2026-08-02
work (§1–§8) removed two of three mechanisms and the flake came back at ~12 % on
`SMP=4`; the one underneath it is that the *test's own wait loop* held the Big Kernel
Lock while spinning on `yield_now()`, freezing the peer core its second poller was
running on. §9 has the evidence, the fix (`blocking_relax()` everywhere in the test),
and the verification series. Still a test defect, still no kernel bug.

Original status line, kept: investigated 2026-08-02 during Phase 7f tranche 2
([`BKL_PHASE7F_OPTOUT_LIST.md`](../archive/BKL_PHASE7F_OPTOUT_LIST.md) §5.4), fixed
2026-08-02 as its own change — both defects in §4 remedied exactly as §6 proposed;
verification series in §8. Sections 1–5 are the original investigation, kept verbatim.
This doc exists because the flake **cost a conversion decision**:
it fired twice in a row immediately after a BKL opt-out entry was added, looked
exactly like a regression, and took six extra boots to exonerate.

Read this before investigating a `woken=1 (expected 2)` line, and before trusting any
single boot to accept or reject a change.

## 1. The symptom

```
[Test] test_epoll_multi_poller_pipe FAILED: woken=1 (expected 2)
```

Never `woken=0`. Never a hang, a panic, or a tripwire. The suite continues normally
and every other test passes. Prior documentation
([`BKL_PHASE7B_PPOLL_CARVE_OUT.md`](../archive/BKL_PHASE7B_PPOLL_CARVE_OUT.md) §5) recorded it as
"pre-existing SMP=4 scheduling-jitter flakiness", seen once in three boots. That
undersells both the rate and the core count.

## 2. Measured rate — 16 boots, one session

All boots `release-smp-shared --features devbox-smoltcp`, `DISK=devbox.img
MEMORY=4096 INSTANCE=60`, spread across six different kernel configurations during
the tranche-2 work.

| SMP | boots | failures | rate |
|---|---|---|---|
| 1 | 1 | 0 | — |
| 2 | 14 | 4 | **29%** |
| 4 | 1 | 1 | — |
| **total** | **16** | **5** | **31%** |

Grouped by the change under test, which is the point:

| configuration | boots | failures |
|---|---|---|
| HEAD baseline / pre-flight only (no new opt-outs) | 4 | 1 |
| `+ rt_sigprocmask` (135) | 5 | 2 |
| `+ rt_sigprocmask + nanosleep` (135, 101) | 7 | 2 |

**It flakes at SMP=2, not only SMP=4**, and at roughly a third of boots — high enough
that two consecutive failures are unremarkable (p ≈ 0.09 at the SMP=2 rate), which is
exactly the trap described in §5.

## 3. What the test does

`src/process_tests.rs`, `test_epoll_multi_poller_pipe`. Two `epoll` instances are
registered for `EPOLLIN` on the **same** pipe read-end, then two threads are spawned,
each blocking in `sys_epoll_pwait(..., maxevents=1, timeout=5000)` on one instance.
The main thread writes 4 bytes once and expects both threads to report the event.

```rust
// Small delay to ensure they are waiting
let wait_start = crate::timer::uptime_us();
while crate::timer::uptime_us() - wait_start < 2000 { yield_now(); }   //  2 ms

pipe_write(pipe_id, b"data").unwrap();

// Wait for both to be woken
let wait_start = crate::timer::uptime_us();
while WOKEN_COUNT.load(SeqCst) < 2 && (crate::timer::uptime_us() - wait_start < 10000) {
    yield_now();                                                        // 10 ms
}
```

Neither thread consumes the data (`epoll_pwait` reports readiness, it does not read),
so there is no "one thread stole the bytes" mechanism. The pipe stays readable for the
whole window.

## 4. Root cause: two test defects, no kernel defect

### 4.1 The wake path is correct — checked, not assumed

`epoll_check_fd_readiness`'s `PipeRead` arm (`src/syscall/poll.rs:464`) registers
**before** it checks:

```rust
if waker.is_some() { super::pipe::pipe_add_poller(pipe_id, tid); }
if super::pipe::pipe_can_read(pipe_id) { ready |= EPOLLIN; }
```

That is the lost-wakeup-free order, and it holds across all three interleavings with
`pipe_write` (which appends under the `PIPES` lock, then drains `pipe.pollers` with
`pop_first`, waking each tid):

- write **before** register → the `pipe_can_read` check sees the data;
- write **between** register and check → the poller is in the set and is woken (sticky
  `WOKEN_STATES`), *and* the check sees the data;
- write **after** check → the poller is registered and is woken.

Two separate `PIPES` acquisitions, but in the safe order. No window here. The failure
is not a lost wakeup, and `woken=1`-never-`0` is consistent with that: the mechanism
demonstrably works for one of the two threads every single time.

### 4.2 Defect 1 — the "ensure they are waiting" delay is an assumption, not a handshake

The 2 ms sleep is the only thing standing between spawn and `pipe_write`. Nothing
verifies that either spawned thread has been *scheduled at all*, let alone reached its
first readiness check. Under boot-suite load — ~250 tests in flight, the main thread
itself burning a core in a `yield_now()` spin — a freshly spawned thread waiting for a
run slot can easily miss that window.

A thread that registers *late* still passes (§4.1, case 1: its check sees the data).
So the failure needs the thread not to run at all inside the combined budget. Which
brings us to why that budget has no slack.

### 4.3 Defect 2 — the wake budget exactly equals the poll interval

```rust
const BLOCKING_POLL_INTERVAL_US: u64 = 10_000;   // src/syscall/poll.rs:50
```

`sys_epoll_pwait` sleeps `min(abs_deadline, now + effective_poll_interval_us(..))` per
iteration, so a poller that misses the push wake re-checks at **t ≈ 10 ms**. The
test's budget for both wakes is **10 ms**.

They are the same number. Any interleaving that forces one poller onto the interval
fallback lands its next readiness check precisely at the deadline, and the outcome is
decided by whichever of the two 10 ms clocks wins — scheduler jitter, nothing else.
There is no margin at all.

That is the whole flake: **defect 1 makes the fallback reachable, defect 2 makes the
fallback a coin flip.**

## 5. Why this matters beyond the test

During tranche 2 the test failed on two consecutive SMP=2 boots immediately after
`rt_sigprocmask` was added to the BKL opt-out list. With the workplan documenting the
flake only at SMP=4, that reads unambiguously as a regression, and the entry was
backed out. Re-running produced **1 failure in 4 boots without the change**; putting
it back produced three more clean boots (5 with-change boots, 2 failures). 2/5 versus
1/4 — the same population, no separable effect, and `rt_sigprocmask` touches neither
epoll nor pipes.

The generalisable rule, now recorded in the tranche-2 doc:

> At a ~30% failure rate, a single boot cannot distinguish a regression from this
> flake, and neither can two. Budget the extra boots, or the first suspicious result
> will either cost a good change or hide a bad one.

For a binary accept/reject on a change that plausibly touches scheduling, pipes, or
epoll, treat ≥4 clean boots as the bar and compare against a same-session
stash baseline, not against a count written in a doc.

## 6. The fix (applied 2026-08-02)

Both defects have a direct remedy, and the primitive for the first one **already
exists** — `pipe_pollers_count()` (`src/syscall/pipe.rs:326`), a test-only helper
gated on exactly the cfg the boot suite builds under. Its presence suggests the
handshake was intended and never wired up.

1. **Replace the 2 ms sleep with a real handshake**: spin until
   `pipe_pollers_count(pipe_id) == 2` (with a generous bounded timeout and an explicit
   FAIL message if it is not reached, so a genuine registration bug still surfaces
   rather than being masked). This removes the "thread never ran" case entirely.
2. **Raise the wake budget well above `BLOCKING_POLL_INTERVAL_US`** — 100 ms is still
   two orders of magnitude under the 5000 ms `epoll_pwait` timeout the threads
   themselves use, and restores margin for the interval fallback.

With both, the test asserts what it is named for — that *multiple* pollers on one pipe
are all woken — instead of asserting that two threads win a race against a 10 ms
timer. Failure would then mean a real multi-poller wake bug.

Deliberately **not** done during tranche 2: this is a test change in the middle of a
locking campaign that uses the boot suite's pass counts as its A/B instrument, and
changing a test's outcome distribution mid-campaign would invalidate the baseline
comparisons the tranche-2 verification rests on. It landed afterwards as its own
change, with its own boot series (§8).

### 6.1 What was applied

`src/process_tests.rs`, `test_epoll_multi_poller_pipe`. No kernel change — §4.1 stands,
the wake path was never at fault.

1. The 2 ms sleep is replaced by a spin on `pipe_pollers_count(pipe_id) >= 2`, bounded
   at 500 ms. `pollers` is only ever drained by a write or a close, none of which has
   happened at that point, so the count is monotonic up to 2 and the poll is race-free.
2. Missing the handshake is now its own FAIL line — `pollers=N (expected 2) … pollers
   never registered` — distinct from the wake failure, so a real registration bug
   surfaces instead of being masked as `woken=1`.
3. The wake budget is `WAKE_BUDGET_US = 100_000` (was 10 000, equal to
   `BLOCKING_POLL_INTERVAL_US`), still 50× under the threads' own 5000 ms
   `epoll_pwait` timeout.

## 7. Reproduction

```bash
cargo build --profile release-smp-shared --features devbox-smoltcp
for run in 1 2 3 4 5 6; do
  (DISK=devbox.img MEMORY=4096 SMP=2 INSTANCE=60 scripts/cargo_runner.sh \
     target/aarch64-unknown-none/release-smp-shared/akuma > /tmp/epollflake_$run.log 2>&1 &)
  sleep 3
  for i in $(seq 1 160); do
    LC_ALL=C grep -qa "Process Execution Tests Done" /tmp/epollflake_$run.log && break
    sleep 4
  done
  LC_ALL=C grep -a "epoll_multi_poller_pipe" /tmp/epollflake_$run.log
  pkill -9 -f qemu-system-aarch64; sleep 2
done
```

Expect roughly 1–2 failures in 6 **before** the §6.1 fix; 0 in 6 after (§8). Note the
NULs in the log (`grep -a`), and that nothing else may hold `devbox.img` open.

## 8. Verification of the fix

10 boots, `release-smp-shared --features devbox-smoltcp`, `DISK=devbox.img
MEMORY=4096 INSTANCE=60`, single session, all with the fix:

| SMP | boots | failures |
|---|---|---|
| 1 | 1 | 0 |
| 2 | 6 | 0 |
| 4 | 3 | 0 |
| **total** | **10** | **0** |

The SMP=2 leg is the §7 repro verbatim, which predicts 1–2 failures in 6 unfixed. All
10 suites ran to `Process Execution Tests Done`. Pass counts were identical across
boots at each core count (353 at SMP=2, 351 at SMP=4, 345 at SMP=1), with the same two
pre-existing unrelated failures in every boot (`PermissionDenied -> EPERM` errno
mapping, and `stp_xzr_ec15_handler_fires`, which depends on whether QEMU emits EC=0x15
or EC=0x25). Host tests and clippy clean.

Zero failures in 10 does not by itself prove the mechanism is gone — at the old 31%
rate it is p ≈ 0.02, strong but not conclusive. The reason to believe it is §4: both
inputs to the race were removed, not merely made less likely.

> **Correction, 2026-09-01.** This caveat was right and was read too generously: two
> of three mechanisms were removed, and the flake returned at ~12 % once measured
> again. §6.2's budget increase in particular made the surviving mechanism *worse*.
> See §9. Note also that §5's rule
still applies to *other* changes: this test getting quieter does not make a single boot
a valid A/B instrument.

---

## Background

- [`BKL_PHASE7F_OPTOUT_LIST.md`](../archive/BKL_PHASE7F_OPTOUT_LIST.md) §5.4 — the tranche-2
  conversion decision this flake nearly derailed, and the boot-count methodology it
  forced.
- [`BKL_PHASE7B_PPOLL_CARVE_OUT.md`](../archive/BKL_PHASE7B_PPOLL_CARVE_OUT.md) §5 — the earlier
  sighting ("once out of three SMP=4 boots"), recorded as SMP=4-only.
- [`../reference/subsystems/locking.md`](../reference/subsystems/locking.md) — the
  A/B playbook whose "re-measure before quoting" rule this is a concrete case of.
- `src/syscall/poll.rs` (`epoll_check_fd_readiness`, `BLOCKING_POLL_INTERVAL_US`),
  `src/syscall/pipe.rs` (`pipe_add_poller`, `pipe_write`, `pipe_pollers_count`).

---

## 9. It came back — and §6 could not have fixed it (2026-09-01)

§8's ten clean boots were real, but they were not proof: the §6 fix removed the
two mechanisms §4 identified, and a **third** one was underneath. Re-measured on
`afc251f6` (`cargo build --release`, i.e. `smp-shared`, `SMP=4`, `DISK=disk.img`
`MEMORY=4096 INSTANCE=60`), the same `woken=1 (expected 2)` line reappeared at
**2 failures in 17 boots (~12 %)**. Same signature, never `woken=0`, never a hang.

### 9.1 What the instrumentation showed

The test now records each poller's `epoll_pwait` return value, the time it landed,
and — on failure only — each poller's scheduler state, whether it is still
registered on the pipe, and a `[THR-DUMP]`. Two failing boots said the same thing:

```
FAILED: woken=1 (expected 2) late=1 pollers_now=1
  p0 tid=13 phase=3 rc=18446744073709551615 lat=0us st=2 reg=false
  p1 tid=14 phase=4 rc=1                    lat=16us st=3 reg=true
  tid=13 st=R pid=-1 ... last_core=2 cpu_us=1037
```

Read it in order:

- The winner returned in **16 µs**. The wake path is fine, again.
- The loser (`rc = u64::MAX`) **never returned at all** — not within the 100 ms
  budget, and not within the extra **2.1 s** diagnostic probe that follows it. So
  this was never a "late by a hair" problem, and no budget could have fixed it.
- The loser is `st=2` — **RUNNING** — with `cpu_us=1037`: about 1 ms of CPU across
  its whole life. A RUNNING thread that is not accumulating CPU is a thread
  spinning with IRQs masked.
- `reg=false`: the write popped it out of `pipe.pollers` and it has not re-lapped
  since. It is stuck *inside* one `epoll_pwait` lap, not cycling through the 10 ms
  fallback.
- `last_core=2`. The winner's `last_core` is 0 — **the main thread's core**.

And immediately above the FAILED line, ~45 consecutive lines of:

```
[BKL] stuck: owner=1 waiter=2 tag=511 (aff0+1)
[BKL] stuck: owner=1 waiter=3 tag=511 (aff0+1)
[BKL] stuck: owner=1 waiter=4 tag=511 (aff0+1)
```

Core 0 owns the Big Kernel Lock; cores 1, 2 and 3 are all spinning for it.

### 9.2 Root cause: the test's own wait loop was the starver

`test_epoll_multi_poller_pipe` waited with a bare `yield_now()` spin — in the
registration handshake, in the wake budget, and in both poller threads' terminal
`loop { yield_now() }` parks. Under `smp-shared` that is the documented
cross-core freeze shape, and `threading::blocking_relax`'s own doc comment states
it exactly:

> Without the drop the loop busy-spins holding the BKL (nothing else READY on the
> core → `yield_now` returns without switching), freezing every peer core.

The boot suite runs in the kernel with the BKL held. `yield_now()` triggers an SGI
and returns; when nothing else on this core is READY the scheduler switches
nothing, so the lock is never released and the loop re-enters holding it. Every
peer core is frozen out of the kernel for the entire duration of the wait.

That is the whole failure:

1. Both pollers register (the §6 handshake works — `pollers` reached 2).
2. `pipe_write` pops and wakes both. Both are correctly READY.
3. The main thread starts spinning on `yield_now()`, holding the BKL on core 0.
4. The poller that landed on core 0 gets scheduled locally and finishes: 16 µs.
5. The poller on a peer core is READY, gets picked, and immediately spins on the
   BKL acquire it needs to finish its `epoll_pwait` lap. It never gets it, because
   the thread that owns the BKL is the one waiting for it.

`woken=1` and never `woken=0` falls straight out of step 4: the co-located poller
always wins. So does the SMP≥2-only rate, and so does why §6 helped without
curing — the handshake removed one input to the race, but **raising the budget
from 10 ms to 100 ms made this worse, not better**: the budget *is* the starvation
window. Ten times the budget is ten times the freeze. §8's ten clean boots
measured a lower-probability version of a bug that was still there.

The correlator is free, and it is in every log: count `[BKL] stuck` lines. This
tree's boot-suite baseline is ~87 per boot (pre-existing, load-driven —
`BKL_TAG511_STORM.md`). Both failing boots read 129 and 132. The ~45-line excess
is this test.

### 9.3 The fix

`src/process_tests.rs`, `test_epoll_multi_poller_pipe`. Still no kernel change —
§4.1 stands and §9.1 re-confirms it, the wake path has never been at fault.

1. Every wait in the test is `blocking_relax()` instead of `yield_now()`: the
   registration handshake, the wake budget, the diagnostic probe, and both poller
   threads' terminal park loops. `blocking_relax` adds the `idle_halt` that
   **drops the BKL** around a WFI, so a peer core can enter the kernel and produce
   the progress being waited for. This is the same rule already written down for
   `wait_out_reclaim_cooldown` in this file, and enforced by
   `test_smp_shared_blocking_wait_peer_progress`; this test was simply never
   converted.
2. The failure line now carries the evidence that made §9.1 possible in two boots
   rather than six: per-poller return value, latency from the write, phase,
   scheduler state, pipe registration, and a `[THR-DUMP]`. A bounded 2 s probe
   past the budget distinguishes *late* from *lost* — the single most useful bit,
   and the one that ruled out every timing-tuning explanation immediately. The
   verdict is still scored at the 100 ms budget; the probe only annotates it.

### 9.4 The lesson that generalises

A kernel-thread test that waits for work on another core must not busy-wait. The
question to ask of any `while … { yield_now() }` in `src/process_tests.rs` is
**"can the thing I am waiting for only happen on a peer core?"** If yes, the loop
is a BKL freeze and the test is testing the scheduler, not its subject. There are
~75 `yield_now()` sites in the boot suite; most wait on same-core work and are
fine. This one was not, and it took three separate investigations to notice
because the failure it produces is indistinguishable from scheduler jitter.

Also: §8's "zero failures in 10" reasoning was sound about the mechanisms it had
identified and wrong about the conclusion. It said so itself — "zero failures in
10 does not by itself prove the mechanism is gone" — and then the doc got read as
if it had. When a fix removes causes rather than symptoms, the honest claim is
"these two are gone", not "the flake is gone".

### 9.5 Verification

Same configuration as §9's baseline (`SMP=4`, `disk.img`, `MEMORY=4096`,
`INSTANCE=60`, one session), with the §9.3 fix:

| arm | boots | `woken=1` failures | `[BKL] stuck` lines/boot |
|---|---|---|---|
| before (§9 baseline) | 17 | 2 | ~87, **129 / 132 on the two failures** |
| after (§9.3) | 12 | 0 | 85-87, no outliers |

Pass count 313/313 on every clean boot in both arms, so the change moves nothing
else. The reason to believe it beyond the boot count is §9.2: the starver was
identified, and it is gone by construction — the wait no longer holds the lock the
straggler needs.
