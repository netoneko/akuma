# Cold-start idle CPU regression on `nca-terminal-fixes` (2026-08-18)

**Status: root cause identified, mechanism still being characterised.**
Measurements below are all on `devbox-smoltcp`, QEMU with `-accel hvf` on
darwin/arm64, host otherwise idle.

## The symptom

An idle, freshly-booted devbox-smoltcp VM burns **~330% host CPU** on this
branch. `main` sits at ~4%. Nothing is wrong in the boot log — no `[BKL] stuck`,
no time jumps, sshd comes up in ~2 s, the suite is unaffected. The kernel is not
wedged; it is *idle and expensive*.

## Measurement harness

[`scripts/measure_idle_cpu.py`](../../scripts/measure_idle_cpu.py) — boots the
VM, waits for the sshd marker, then samples QEMU's cumulative CPU time
(`ps -o time=`, all vCPU threads) across a window that starts *after* boot.

**`ps -o %cpu` cannot be used for this.** On macOS it is an average over the
whole process lifetime, so a VM that spun during boot and then went quiet reads
the same as one still spinning. Differencing `ps -o time=` over a known window
is the only reading that means anything.

## Bisect: one constant

| Arm | SMP=1 | SMP=2 | SMP=4 |
|---|---|---|---|
| branch as-is (`TIMER_INTERVAL_US = 1_000`) | 100.0% | 170.8% | 330.2% |
| same tree, `TIMER_INTERVAL_US = 10_000` | — | — | **3.8%** |
| 1 ms tick + `WAKE_DEADLINE_PREEMPT = false` | 100.0% | — | 330.3% |

The regression is entirely
[`src/config.rs`](../../src/config.rs)'s scheduler tick going
**10 ms → 1 ms**. `WAKE_DEADLINE_PREEMPT` (the other scheduling change in the
same commit range) is **not** implicated: turning it off changes nothing on
either core count. The epoll/pipe/fd changes are not implicated either — this is
an *idle* VM, none of those paths run.

Cost scales at roughly **one saturated host core per guest core**: a single
guest core at a 1 ms tick already pegs a full host core.

## What it is NOT

Instrumented `timer_irq_handler` directly (temporary `[TICKPROBE]`, measuring
handler body via `cntvct_el0` and the inter-tick period):

```
[TICKPROBE] n=40000 body_us=0 period_us=1002 wfi_per_2000t=750
```

- **Handler body: <1 µs.** The tick handler itself — alarm queue,
  `check_itimers` over `MAX_THREADS`, the watchdog, the SGI kick — is free.
- **Period: exactly 1002 µs, stable over 40 000 ticks.** No overrun, no
  re-entry death spiral. The next `cntv_cval_el0` is computed from a counter
  read at handler *entry*, so a handler that overran its own interval would show
  up as a collapsed period; it does not.

So the guest is doing precisely the 1000 ticks/s it was asked for, and each one
is nearly free. **The cost is not the tick handler.**

### The superlinearity is the finding

At a 10 ms tick, SMP=4 costs 3.8% total = 400 ticks/s across 4 cores =
**~95 µs of host CPU per tick**. At a 1 ms tick, SMP=1 costs 100% = 1000
ticks/s = **~1000 µs of host CPU per tick**. Ten times the tick rate costs a
hundred times the CPU. A fixed per-tick cost would predict ~9.5%, not 100%.

Whatever is expensive gets *more expensive per event* as the interval shrinks —
that is a threshold/quantisation shape, not a throughput shape. Characterising
that is the open item (see below).

## Correction to an in-flight inference

The `wfi_per_2000t=750` figure above was initially read as "the idle core only
reaches WFI a third of the time, so it must be spinning". **That reading is
wrong.** The probe counts entries to
`akuma_exec::threading::idle_halt()` only, and on a
`kernel_smp_shared` + `smoltcp` build the network-poll loop
([`src/main.rs`](../../src/main.rs), the `#[cfg(all(kernel_smp_shared, feature = "smoltcp"))]`
block) does **not** call `idle_halt` — it drops the BKL and executes a *raw*
`wfi` inline. Those halts are invisible to the counter. At SMP=1 the counted
halts are essentially just the boot/async-main idle thread at `main.rs:1213`.
The netpoll loop's own halt rate has not been measured yet.

## Where the two idle loops are

Both matter, and they have different shapes:

| Loop | Halt mechanism |
|---|---|
| async-main / boot idle, `src/main.rs:1213` | `yield_now()` then `threading::idle_halt()` (counted by the probe) |
| netpoll, `src/main.rs` `#[cfg(all(kernel_smp_shared, feature = "smoltcp"))]` | `bkl::leave_kernel()` → raw `wfi` → `bkl::enter_kernel()` (**not** counted) |
| secondary cores, `src/smp_shared.rs` | per-core idle thread → `idle_halt()` |

The netpoll loop also runs `akuma_net::smoltcp_net::poll()` up to 64 times per
iteration before halting. Under HVF every virtio MMIO touch in there is a
vmexit, so its per-iteration cost is real and it runs once per wake — i.e. 10x
more often at a 1 ms tick.

## Open questions

1. **Is the 1 ms WFI actually sleeping on the host?** The threshold-shaped cost
   curve is consistent with the host declining to sleep for short deadlines and
   spinning instead, which would make the guest's WFI a no-op and both idle
   loops pure busy-polls. Test: sweep the tick at 1/2/3/5/10 ms at SMP=1 and see
   whether the curve is a cliff or a slope.
2. **Is it HVF-specific?** Re-run the 1 ms arm with `HVF=0` (TCG).
3. **How often does the netpoll loop actually halt?** Instrument the raw `wfi`
   in `main.rs` the same way `idle_halt` was instrumented.

## Why this does not get fixed by just reverting

The 1 ms tick was adopted for measured wins recorded in
[`SCHEDULING_INVESTIGATION.md`](SCHEDULING_INVESTIGATION.md) (Matrix A): sleep
and poll floors ~35–40 ms → ~1 ms, pipe round-trip 10.4 → 3.2 µs/iter, terminal
stalls ~1000/1500 writes → 0, a 128 MB download 6.3 s → 3.4 s. Reverting to
10 ms buys back the idle CPU and gives all of that away.

The shape of a real fix is to stop conflating *preemption granularity* with
*wake resolution*: keep a coarse periodic tick (or none at all when idle) and
arm a one-shot timer at the earliest pending wake deadline, so an idle box takes
the interrupts it needs and no more. Confirm the mechanism in "Open questions"
before committing to a design.

## Gate

A cold-start idle-CPU comparison is being added to
[`../runbooks/verify-trim-fat-change.md`](../runbooks/verify-trim-fat-change.md)
so a change of this class cannot land green again: every functional gate in that
runbook passes on this branch.
