# Splitting the async-main loop: a network thread and a maintenance thread (2026-08-20)

**Status: designed, half-built, NOT measured end-to-end.** `netpoll_drain_step()`
is extracted and default-on (behaviour-identical). The two-thread arrangement is
specified here and deliberately not built yet — §4 is the reason to be careful,
and §5 is the measurement that must come first.

Companion to [`AKUMA_NET_ISSUES.md`](AKUMA_NET_ISSUES.md), which holds the NIC-path
numbers this design came out of.

---

## 1. The problem

`run_async_main()` (`src/main.rs`) is one loop that does two unrelated jobs:

```
loop {
    maintenance      // heartbeat, reclaim_terminated_slots (100 ms), pstats (30 s),
                     // profile dumps (5 s)               <- BKL-HELD
    drain            // while smoltcp_net::poll()          <- BKL-DROPPED (no-bkl-network)
    mem monitor      // off by default                     <- BKL-HELD
    herd output      // channel.try_read() -> console      <- BKL-HELD
    leave_kernel(); wfi; enter_kernel();
}
```

One loop means **one wake rate**, and it is set by whichever job needs it most —
the network. Everything else is dragged along.

That was harmless when the only thing that could end the `wfi` was the 3 ms tick
(~333 laps/s). It stopped being harmless on 2026-08-19, when the virtio-net SPI
was registered, and again on 2026-08-20 when the doorbell re-arm race was fixed
(`AKUMA_NET_ISSUES.md` §9). Measured now, under HTTP load:

```
laps = 56,312 per 5 s window           = 11,262 laps/s   (idle: 2,281/s)
```

(An earlier revision of this doc said 15,412 laps/s, derived as
`poll_calls - poll_progress`. That is wrong — most `poll()` calls come from
`wait_until`, epoll and the send/recv flush, not from this drain. `[NICSTAT]
laps=` now counts the loop directly.)

`BKL_PHASE7_AUDIT.md` §2.6 considered carving `netpoll_maint` and declined, on
this basis:

> The loop iterates once per IRQ (it WFIs at the bottom), so it re-acquires the
> BKL **≥100×/s**. §19.5 already recommended against carving it… it is
> process/thread-table code with no fine-grained lock underneath.

**That premise is 150x stale.** It was written 2026-08-01, before the NIC had an
interrupt at all. The reasoning was correct for its numbers; the numbers moved.

---

## 2. Why the audit's objection does not block this

The audit rejected **carving `netpoll_maint` out from under the BKL** — correctly.
The process table's only cross-core guard *is* the BKL (`locking.md`, "What the
BKL is still the only lock for": `with_irqs_disabled` gives mutual exclusion on
one core only), and the console UART has no guard at all. Both rows name this
loop explicitly (`netpoll_maint`, `netpoll_herd`).

Splitting threads is a **different move**. Nothing gets carved out:

- **maintenance thread** — keeps the BKL, exactly as the audit requires. Wakes at
  its own cadence (~100/s covers everything it does).
- **network thread** — runs `netpoll_drain_step()`, whose body is already inside a
  `no-bkl-network` dropped window. It never touches the process table or the
  console.

The BKL-held work stays BKL-held. It just stops being re-entered at packet rate.

As the design's author put it: *networking and the BKL are separate things.* The
extraction is right on separation-of-concerns grounds whatever the benchmark says.

---

## 3. The design

Two `#[inline]` halves of the current loop body:

```rust
#[inline] fn netpoll_drain_step()            // LANDED — the smoltcp drain + doorbell re-arm
#[inline] fn netpoll_maint_step(..)          // NOT YET — heartbeat, reclaim, dumps, herd, memmon
```

and two arrangements over them:

| build | arrangement |
|---|---|
| `kernel_smp_shared` **and** `probed_core_count() > 1` | two threads: drain thread calls `netpoll_drain_step()` at packet rate; async-main calls `netpoll_maint_step()` on a timed park |
| everything else — `extreme-size` (no `smp-shared`), or `SMP=1` | **one** thread calling both halves, i.e. today's behaviour byte-for-byte |

The single-thread fallback is what makes this safe to default on: no second
256 KB kernel stack on the 4 MB profile, and no two-threads-timesharing-one-core
inversion at `SMP=1`. `probed_core_count()` (`src/smp_shared.rs`) is the runtime
check; `SMP=N` is a runtime value, so a `cfg` alone cannot decide this.

**The doorbell re-arm must stay welded to the drain.** `NIC_WAKE_PENDING` is
cleared immediately *before* `while poll()`, and that ordering is the fix in
`AKUMA_NET_ISSUES.md` §9 — worth +65 % throughput. Splitting the loop is exactly
the refactor that would quietly separate them. It is inside
`netpoll_drain_step()` for that reason.

**Herd output moves to the maintenance side.** It is a log drain for herd-managed
processes' stdout, not an interactive path — SSH keystrokes go through sshd and
never touch it. See §4 for why this is not the free win it looks like.

---

## 4. The warning: contention share is not throughput

`bkl-profile` under HTTP load, `SMP=4`, three consecutive windows:

| tag | w2 | w3 | w4 |
|---|---:|---:|---:|
| irq/sched | 30.4 % | 32.7 % | 48.5 % |
| idle | 11.9 % | 19.9 % | 36.9 % |
| **netpoll_herd** | — | **14.3 %** | **5.0 %** |
| **netpoll_maint** | **15.2 %** | **10.6 %** | — |
| netpoll_drain | — | — | 2.2 % |

Maintenance + herd is **15-25 % of contended BKL time**; the already-carved drain
is 2.2 %. That looks like the split's payoff sitting in a table.

**It was tested and it is not.** Rate-limiting herd polling from every lap to
every 100 ms — a ~1,500x reduction, targeting the largest single row — measured
neutral-to-worse on the same machine state:

| | herd every lap | herd every 100 ms |
|---|---:|---:|
| req/s | **1,040** | 1,002 |
| p90 | **2,453 us** | 2,727 us |

Reverted. Two reasons, both of which apply to the full split:

1. **`spins` measures other cores *waiting*, and those cores are idle anyway.**
   `idle` + `irq/sched` is 85 % of contended BKL time in this workload. Returning
   the lock sooner to a core with nothing to do buys nothing.
2. **Less work per lap means more laps.** Reaching `wfi` sooner raises the lap
   rate and therefore the BKL acquire/release churn — the same shape as
   `AKUMA_NET_ISSUES.md` §8, where a plentiful-but-imprecise wake beat a precise
   one.

So the split may still be right — the *maintenance thread parking on a timer* is
a different lever than *doing less per lap*, because it genuinely reduces the
number of BKL entries rather than shortening each one. But the 15-25 % must not
be quoted as its expected value.

---

## 5. Measure this first

Three negative results preceded this design (`AKUMA_NET_ISSUES.md` §7, §8, §10),
all from mechanisms that looked airtight. The order that has worked:

1. Build the split behind the `probed_core_count() > 1` branch.
2. A/B end-to-end with `bench_nic_rtt.py --mode http -n 2000`, **five runs per
   arm**. `-n 400` cannot resolve p90 here (the baseline ranged 1,143-5,048 us
   across runs at that size).
3. **Measure the control in the same session.** The same build measured 1,108
   req/s and 1,040 req/s a few hours apart — ~6 % machine drift, enough to
   manufacture or hide a result this size.
4. Confirm `SMP=1` and `extreme-size` took the single-thread path and are
   unchanged.

---

## 6. What the split will NOT fix

Measured 2026-08-20 by decomposing `poll_us` into lock-wait, lock-hold and the
post-drop `wake_all` pass:

| per 5 s window | w3 | w4 | w5 |
|---|---:|---:|---:|
| `poll` total | 958 ms | 1,044 ms | 589 ms |
| …waiting for `NETWORK` | 89 ms (9 %) | 84 ms (8 %) | 37 ms (6 %) |
| …`wake_all` pass | 14 ms (1.5 %) | 18 ms (1.7 %) | 12 ms (2 %) |
| **…inside `iface.poll()`** | **855 ms (89 %)** | **942 ms (90 %)** | **540 ms (92 %)** |
| `poll` max | 3,693 us | 6,137 us | 6,143 us |
| `poll_wait` max | 1,336 us | 679 us | 776 us |

Two hypotheses die here:

- **The unfair `NETWORK` spinlock is not the bottleneck.** `spinning_top::Spinlock`
  has no fairness guarantee and a starved waiter was a live theory, but the wait
  is 6-9 % of poll time and its worst case (679-1,336 us) is far below the poll
  worst case (3.7-6.1 ms). Replacing it with a ticket lock could recover ~1.7 % of
  the window at best.
- **The `wake_all` pass is not the bottleneck** either, at 1.5-2 %, despite walking
  all 128 socket slots (`MAX_SOCKETS` = 128 on `devbox-smoltcp`: `no-tests` →
  `small-sockets` plus `many-sessions`).

**~90 % of poll time is inside `iface.poll()` itself, under `NETWORK`, with IRQs
masked** (`PreemptGuard`). And `poll max − poll_wait max` ≈ 4.8 ms means smoltcp
occasionally runs for **milliseconds with local interrupts masked** — long enough
to swallow a tick. That is the next thing to chase, and no amount of thread
splitting touches it. The obvious lead is that `iface.poll()` walks the whole
`SocketSet` on every call, and the set has 128 slots.

---

## Background

- [`AKUMA_NET_ISSUES.md`](AKUMA_NET_ISSUES.md) — the NIC-path investigation this
  came out of: §7 rings (off), §8 waker parking (off), §9 the doorbell race
  (fixed, the session's win), §10 the herd-gate negative result.
- [`BKL_PHASE7_AUDIT.md`](BKL_PHASE7_AUDIT.md) §2.6 — the original decision not to
  carve `netpoll_maint`, and the ≥100×/s premise that has since gone stale.
- [`BKL_FINE_GRAINED_LOCKING_PLAN.md`](BKL_FINE_GRAINED_LOCKING_PLAN.md) — the
  carve-out programme; `no-bkl-network` and the `netpoll_drain` carve both landed.
- [`../reference/subsystems/locking.md`](../reference/subsystems/locking.md) —
  "What the BKL is still the only lock for", the table §2 rests on.
