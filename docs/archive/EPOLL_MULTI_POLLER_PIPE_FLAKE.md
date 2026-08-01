# `test_epoll_multi_poller_pipe`: a boot-suite flake that is a test defect, not a kernel bug

**Status**: Investigated 2026-08-02 during Phase 7f tranche 2
([`BKL_PHASE7F_OPTOUT_LIST.md`](../archive/BKL_PHASE7F_OPTOUT_LIST.md) §5.4). **Fixed
2026-08-02** as its own change — both defects in §4 remedied exactly as §6 proposed;
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
inputs to the race were removed, not merely made less likely. Note also that §5's rule
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
