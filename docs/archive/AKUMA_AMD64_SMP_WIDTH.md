# amd64 SMP width: why 9+ cores hang, and dynamic cores for power

2026-10-07. **Stability: B** — the diagnosis is from code reading plus boots on
the ryzen laptop, and **the fix works on the metal: 9 and 16 cores boot and hold**
(see "Result"). Not yet soaked for hours.

Written in answer to "figure out the cores, and consider turning them on and off
dynamically — might help with power saving". The first half is a bug hunt, the
second a design that is not built.

## What was measured (ryzen laptop, 16 threads, `rtw89wifi`, quiet no-tests kernel unless noted)

| cores (`smp=N`) | result | where |
|---|---|---|
| 1 (`nosmp`) | clean | entry 12 |
| 2, 4, 8 | stable | `AKUMA_AMD64_ON_RYZEN_LAPTOP.md` §5.9 (tests kernel, `fbverbose`) |
| **8** | **boots, stays up, rio "super fast"** — the control run on the same entry/kernel as the failures | entry 14, 2026-10-07 |
| 9, 12, 14, 16 | **freeze** — no network, no Pop, frozen before `sshd` and `klog` start; a power cycle is the only way out (the watchdog does not catch it) | entry 14 |
| 11, 13, 15 | not tried (9 already freezes) | |

The 8-core control matters: it rules out "SMP with the quiet kernel" as the
cause. The hang starts at the **9th core**, and it is the first core whose index
is 8.

## Diagnosis: four tables sized for 8 cores on a 16-core target

`amd64/src/smp.rs` runs up to `MAX_CPUS = 16`. Shared crates written for
AArch64 (4–8 cores) carry per-core tables sized 8, and indexing past them does
one of three things:

| where | what it does at core index ≥ 8 |
|---|---|
| `akuma-bkl` `KernelLock::barged: [AtomicBool; BARGE_MAX_CORES = 8]` | `barged.get(core)` is `None`. **Release** of a ticket-free ("barge") hold then advances `now_serving`, consuming a waiting core's ticket; **acquire** falls back to the old compensating `next_ticket.fetch_add(1)`, which the code's own comment calls "a ticket value NO core will ever hold … a freeze". Both leak FIFO slots: `ticket=N serving=N-1` forever. That is the 16-core wedge signature in `klog` (`[bkls>] core=12 ticket=… serving=…-1 owner=15`) |
| `akuma-mmu` `ACTIVE_L0` / `PREV_L0: [_; TTBR_TRACK_CORES = 8]`, indexed `core % 8` | core 8 *overwrites core 0's* published page-table root, so the page-table free gate (`cores_on_l0_mask`) can see core 0 as not on a root it is running on. A table in use can be freed. (TLB shootdowns are safe: `TARGETED` requires `peers <= TTBR_TRACK_CORES` and broadcasts otherwise) |
| `akuma-threading` `VOLUNTARY_SCHEDULE: [_; MAX_CORES = 8]`, indexed `core % 8` | cores 8–15 share the "reschedule voluntarily" flag with 0–7 |
| `akuma-bkl` profiler `HOLDER_TAG: [_; PROFILE_MAX_CORES = 8]` | guarded with `if c < 8`: attribution is lost, nothing breaks |

`MAX_CORES = 8` in `akuma-exec-core` is what the third row (and a few
`if core < MAX_CORES` guards in `akuma-threading`) read.
**Not a cause**: `YIELD_TAG` in `amd64/src/sched.rs` (indexed with `.get()` /
`% 8`, a debug tag), and `shootdown.rs`'s `u64` masks (16 fits).

Why 2/4/8 were stable and 16 wedged: cores 0–7 are tracked everywhere; every
core above them leaks a slot or aliases a neighbour each time it takes a
ticket-free BKL hold, which the BKL-free scheduler and network paths do all the
time. 9 cores has one such core, so it should fail less often than 16 — it
froze anyway, at boot.

## The change (built, host-tested, **booted on the metal: works**)

```
crates/akuma-bkl/src/sync.rs       PROFILE_MAX_CORES 8 -> 16, BARGE_MAX_CORES 8 -> 16
crates/akuma-exec-core/src/thread.rs  MAX_CORES 8 -> 16
crates/akuma-mmu/src/lib.rs        TTBR_TRACK_CORES 8 -> 16
```

Costs a few dozen bytes of `.bss`, nothing at run time. It is shared with
AArch64, whose boards have ≤ 8 cores: the arrays only get longer. Host tests:
`akuma-bkl` 35, `akuma-exec-core` 39, `akuma-mmu` 22, all green.

## Result (2026-10-07, same day)

Kernel built from HEAD `76724bbc` plus the three constants (no-tests, `smp-shared`),
the same quiet command line as entry 12 with `smp=N` and `ecram=0xfeec2380`:

| cores | before the change | after |
|---|---|---|
| 9 | froze at boot (power cycle) | **up**, 9 online, stable |
| 16 | froze twice (the BKL wedge) | **up, 16 online**; held for 260 s including a stress run |

Stress on 16 cores: 16 busy shell loops plus file churn for 45 s, then 80 s of
quiet. Load average and uptime kept advancing, no freeze, no panic, `sshd` and
wifi stayed up. The BKL still logs `[bkls>] … serving=ticket-1` stalls (26 → 32
lines over the run, 1–2 M spins each) and a few early `[BKL] stuck: owner=… waiter=…`
lines; all of them cleared on their own. That is long BKL holds under contention
(the ~20 per boot already recorded at 2/4/8 cores), **not** the lost-ticket
wedge, which never recovers. Whether those holds are worth shortening is a
performance question, not a correctness one.

**Power, from `/proc/power` `power_mw` (battery draw):** ~9.2–9.5 W idle with all
16 cores online in `hlt`, the same as 1–8 cores (~8.5–9.5 W); **47 W** at the peak
of the 16-core stress. So idle cores cost almost nothing here and parking them
would save little; the cost is in *running* cores (frequency/voltage), which
points at P-states, not parking.

**Next:** soak (hours, rio running, a build), and the open question of why the
`[BKL] stuck` lines appear at all at boot. If a regression ever appears at 9+
cores, the first thing to rule out is another table sized for 8 — `grep -rn
'; 8\]\|% 8\b\|MAX_CORES' crates amd64/src`.

## Dynamic cores for power saving: a design, not built

**First, measure whether it is worth it — and the first measurement is in.** Cores
with nothing to run are already in `hlt` (`sched.rs` `idle_loop`), waking at each
LAPIC tick, and `/proc/power` `power_mw` (the battery's real draw) reads ~9.2–9.5 W
with 16 idle cores against ~9 W with one: **idle cores are nearly free**, so
parking is the weakest lever. Under load it is 47 W, which is what to attack. The
remaining check is a controlled `nosmp` vs `smp=16` idle comparison, a minute each.

**The levers, in the likely order of payoff on this CPU (Zen 4):**
1. *P-states* (frequency/voltage via CPPC MSRs): usually the largest by far. A
   core running rio at full clock is where the 9 → 20 W swing comes from.
2. *Tickless idle*: stop the LAPIC timer on an idle core so it sleeps until an
   IPI, instead of waking 1000×/s.
3. *Deeper C-states* (`mwait` with ACPI `_CST`, or the C2 I/O port): needs the
   ACPI tables this kernel already finds, plus an AML-free decode of `_CST`.
4. *Parking cores* (this task).

**What parking needs, from reading the code:**
- *An online mask, not a count.* `smp::online_cpus()` is a count and
  `shootdown.rs` builds `(1 << peers) - 1` from it, assuming the online cores are
  indices `0..n`. Parking core 3 of 8 breaks that; shootdown targets, `/proc/cores`
  and the idle accounting must read a mask.
- *The scheduler skips a parked core.* Placement is `!m.idle && (m.pinned ==
  NO_CPU || m.pinned == cpu)` (`sched.rs`); add "and the core is not parked", and
  refuse to park a core that has a thread pinned to it (or unpin first).
- *A parked core must not be in anyone's wait*: it must hold no BKL ticket and
  be absent from `shootdown::wait_for_acks`'s set *before* it stops answering IPIs.
- *Its TLB is stale on wake*: it missed every shootdown while parked, so resume
  with a full flush (reload `CR3`) before running anything.
- *Park state*: `hlt` with the LAPIC timer stopped (wake by IPI), or the real
  offline state (INIT → wait-for-SIPI, lowest power) and re-run the existing
  `start_secondaries` path to bring it back (~ms; the trampoline is reusable).
- *Policy*: the `akuma-kacho` observe/decide/hysteresis layer already hosts the
  other self-tuning policies; a "wanted cores" policy driven by runnable-thread
  count with hysteresis (park slowly, unpark fast) fits it, and
  `akuma-scheduler`'s simulator can rank candidates before a metal boot.

None of this should be started before the 9+ core freeze is fixed: parking is
built on the same BKL and shootdown machinery that is currently wrong above 8.

## Background

- `AKUMA_AMD64_ON_RYZEN_LAPTOP.md` §5.9 — the earlier SMP bisect (2/4/8 stable, 16 wedges).
- `AKUMA_AMD64_BKL_NETWORKING.md`, `GICD_*`-style per-core table bugs: this is the
  amd64 twin of "a constant that moves with the core count".
- `overlays/ryzen/README.md` — menu entries 12–17.
