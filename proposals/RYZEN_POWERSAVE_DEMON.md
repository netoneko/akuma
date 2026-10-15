# The Ryzen **power-saving demon** — what is built, and the road to the rest

> **Baselines to hold** (measured 2026-10-15, on the `ryzen-powersave`
> worktree): host tests **2013/0** (11 new), `akuma-amd64` compiles clean in
> both feature sets (`--features no-tests` and default), and QEMU `microvm`
> smoke boots with the full self-test suite green — `lapic: ticks per 50ms
> (expect 5) 5` with `idlesleep` on. Observed in those boots: across a guest
> `sleep 40` the kernel clock read 8.56 → 48.61 s (40.05 s, the catch-up
> holding), `/proc/power`'s `energy_mwh` went 0 → 77 (6.97 W × 40 s = 77.4
> mWh, the ledger integrating exactly), and an `SMP=2` boot whose secondary
> idles in `idle_loop` logged `[IDLESTAT] halts=23593 shots=22564` over 295 s
> with the box answering throughout. What QEMU **cannot** see: C-state
> behaviour, the BSP-idle credit path under a real workload, and long-run
> drift — all metal A/B questions (P0/P1 below).
>
> **Everything here is opt-in.** The default boot is byte-for-byte today's
> behaviour: `idlesleep` off means `arm_idle_shot` never runs, and the only
> change on the hot path is one atomic swap per wake. The metal A/B that
> decides the default has not run yet.

---

## The prompt

> Build the strategy session's conclusion: a power-saving demon for
> Akuma/amd64 on the Ryzen laptop. Not a daemon that spins — a *meter*, a
> *policy vocabulary*, and the first real mechanism, each in the house style:
> pure logic in a `forbid(unsafe_code)` crate, host-tested; kernel wiring that
> states its invariants; opt-in knobs so nothing changes for an existing boot
> until a measured A/B says so.

The strategy this implements (2026-10-15 session): on a laptop the SMU already
does DVFS better than we will, so the OS's levers are **idle residency, wake
frequency, and devices** — not frequency tables. And the project's unusual
advantage is that **the meter already exists**: `/proc/power`'s `power_mw` was
verified against Linux's `BAT0` to the milliwatt on this exact machine
(`docs/archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md` §5.8). Most hobby-OS power work
has no meter at all; this one has had one since the battery applet.

## What is built in this increment

| piece | where | what it is |
|---|---|---|
| **Energy ledger** | `crates/akuma-power/src/ledger.rs` | integrates `power_mw` between `/proc/power` reads; µWh internally (a 1 s step at 7 W keeps its 1.9 mWh instead of truncating to 0 in mWh), renders `energy_mwh=`; left-rectangle rule, saturating, backwards-clock safe. 7 host tests, sim-driven |
| **`energy_mwh=` line** | `render.rs`, `amd64/src/power.rs` | the kernel glue samples the ledger inside the renderer — the file already costs nothing when unread, and now it integrates nothing when unread either; only valid samples integrate, so an EC glitch pauses the total instead of inventing energy |
| **`WakeBudget`** | `crates/akuma-kacho/src/lib.rs` | the fourth `kacho` primitive: consecutive quiet laps → a sleep budget, `ramp`-shaped, one busy wake floors it. Pure, `const`, host-tested |
| **The idle long shot** | `amd64/src/lapic.rs` `arm_idle_shot` / `settle_idle_shot` / `credit_shot_fired` | one one-shot of up to 100 ticks (1 s) replaces a run of periodic ticks, waking at a parked thread's deadline inside the budget in **exact LAPIC counts, not rounded ticks** — so `arm_deadline`'s measured sub-tick futex win survives. Off unless `idlesleep` |
| **Clock catch-up** | same file | every skipped tick is credited back — `credit_shot_fired` when the shot itself fires, `settle_idle_shot` when a device IRQ wakes the core first. `TICKS` **is** the kernel clock (`net::uptime_us`, every smoltcp timeout, `clock_gettime`); a skipped tick is a clock that ran slow |
| **`idlemwait`** | `amd64/src/sched.rs` | `sti; mwait; cli` instead of `sti; hlt; cli`, CPUID-gated (MONITOR leaf 1 ECX bit 3), hints pinned to 0 because a garbage MWAIT hint is a firmware-defined C-state. An experiment lever for the metal A/B, not a win in evidence |
| **`[IDLESTAT]`** | the idle loop's 30 s dump | `halts= shots= shot_ticks_credited=` — the A/B numbers, one line per dump |

### The invariants the kernel change rests on

1. **Deadline precision is not traded away.** `arm_idle_shot` runs *after*
   `arm_deadline` and refuses when `ONESHOT` is set — a sub-tick deadline's
   exact one-shot wins over a long shot, always. Inside the budget the shot
   arms at the deadline's exact count (`left_us * per_tick / US_PER_TICK`,
   ceiling), which is what `timerlat` measured 1.0 ms waits landing at 10.0 ms
   before `arm_deadline` existed.
2. **The clock never loses a tick.** `ARMED_IDLE_TICKS` is consumed exactly
   once — by the fire (`on_tick`, credit `armed − 1`, the fire itself is the
   tick the handler adds) or by an early wake (`settle_idle_shot`, credit
   `min(elapsed, armed − 1)`, capped because periodic counting resumes at
   once). The atomic swap is what makes the two paths exclusive.
3. **The watchdog keeps its window.** `pet` fires from the tick path as
   before; `settle_idle_shot` pets when it credits, so a 1 s idle sleep never
   eats into the FCH countdown. The ceiling (`MAX_IDLE_SHOT_TICKS` = 100) is a
   tenth of the shortest `wdt=<s>` clamp (10 s).
4. **First idle halt is today's halt.** The budget is computed from the count
   of quiet laps *before* the current one (`quiet_before`), so lap zero ramps
   to 1 tick and the code falls through to `arm_deadline` alone. One busy
   wake (`x86_yield` switched a thread, or a peer's work was visible) stores
   0 back and the next halt is a one-tick halt again.
5. **Secondaries are safe by omission.** Only the BSP touches `TICKS`; a
   secondary's long shot clears its own armed state on both wake paths and
   credits nothing.

## Where the watts are, and the road (staged)

Rough anatomy of this laptop (Ryzen 7 8845HS, 16-thread) at light idle: CPU
package ~2–4 W, dominated by whether cores reach CC6 or sit in shallow halt;
panel backlight ~1–3 W; the RTL8852CE ~0.5–1.5 W; platform overhead the
remainder. Each stage below ends in a metal A/B through the existing reboot
loop (`overlays/ryzen/cycle.py`), scored by `energy_mwh` over a fixed idle
script, cross-checked once per stage against `turbostat` under Pop.

| # | stage | done when | sessions |
|---|---|---|---|
| P0 | **The scoreboard, on metal.** Boot ryzen, `cat /proc/power` over ssh after N minutes idle, compare against the EC readings Linux reports at the same moment; fix whatever the uncacheable-window mapping got wrong on real silicon | `energy_mwh` on the metal matches a turbostat-derived estimate within ~5 % over one hour | 1 |
| P1 | **The A/B this kernel change was built for.** Same boot, `idlesleep` on vs off, fixed idle script (sshd up, nothing else), 30 min each arm, swap order. `idlemwait` as a third arm | a measured delta in `energy_mwh` with `[IDLESTAT]` explaining it; a default flipped or the knob deleted, either honestly | 2–3 |
| P2 | **WiFi power.** The card already enters idle power-save under Linux between traffic; Akuma's station keeps it hot after `wifijoin`. PS between bursts (the PS-Poll template is already in the join segments), or a real `wifi down` that powers the card off (`shutdown_for_reset` exists). The one open hazard is documented: after idle power-save the chip refuses a second firmware download in one boot (`FWDL_SECURITY_FAIL`, §5.6) | `power_mw` during idle-with-wifi drops measurably; a reconnect still works | 2–4 |
| P3 | **The EC's other registers.** Brightness, lid, thermal — the same `OperationRegion` approach that cracked the battery, against the DSDT already dumped on p3 (`/root/acpi/asl/`). Lid → dim or blank is a laptop's cheapest watt | `/proc/power` grows lid/brightness facts; rio's panel config can dim | 2–4 |
| P4 | **The demon itself.** A thin `herd` service that reads `/proc/power` + `[IDLESTAT]` and applies tunables — policy staying in `kacho`-shaped pure code, the service only carrying effects. Only worth building once there are ≥2 knobs for it to reconcile | a `herd` service that survives `box` restarts and a reboot loop, adjusting from measurements | 1–2 |

> **Status (2026-10-15): P4 exists — `powersaved`**, a dedicated binary
> (`userspace/powersaved/`, the "binaries, not scripts" rule of
> `docs/reference/userspace-layout.md`), supervised by herd. Its first policy
> is the idle-dim staircase (60 s → 60 %, 180 s → 30 %, 600 s → 10 %, a
> keystroke restores 100 %), applied through the kernel's `/dev/power`
> control device (`dim=<pct>`) onto a global `akuma-fbcon` dim factor — the
> console's colour scaling, not the backlight, which stays P3's lever. The
> policy is pure in the `--lib` half and host-tested; the metal run that
> day watched the staircase step 100 → 60 → 30 on the ryzen laptop with the
> facts (`idle_ms=`, `dim=`) honest in `/proc/power` throughout. What the
> daemon does **not** yet do is reconcile: one policy, one lever — the
> "adjusting from measurements" half of this row waits for the second knob.
| P5 | **Stretch: s2idle.** The ACPI `SLP_TYP` values sit in the dumped DSDT. Real suspend is the endgame for battery minutes and a project of its own (device quiescing on a live bus; the rtw89 warm-reset lessons apply) | the box sleeps and wakes | 6–12 |

**Deliberately out of scope, and why:** OS-driven P-states/CPPC. The SMU does
autonomous frequency selection on this part; reading current frequency for the
scoreboard (APERF/MPERF, Pstate MSRs) is cheap and can join P0, but writing
desired-perf fights firmware that has more context than the kernel does. If
the P1 A/B shows shallow idle dominating the residual, revisit — with
measurements, not hope.

## Risks, and what the smoke runs already said

- **A long sleep starves tick-driven work.** Everything on this target that
  needs a timed wake either parks with a deadline (`block_until_deadline`,
  futex, netpoll laps — all served exactly by the deadline arm) or is woken by
  IPI (remote kicks). The residual risk is a *poller with no deadline* that
  relied on periodic ticks; the metal A/B's 30-minute arm is what would catch
  it, which is why the default stays off until then.
- **The clock is the thing that breaks first.** QEMU under TCG cannot see
  C-state behaviour at all, but it *can* see the clock arithmetic — the smoke
  boots run the full self-test suite, and the tick-rate check (`ticks per
  50ms (expect 5) 5`) passing with `idlesleep` on is the cheap half of the
  catch-up proof. The expensive half (long-run drift against wall time) is a
  metal measurement.
- **QEMU/devbox needs no battery.** `powersim` plus the ledger is the whole
  development loop; nothing in this increment requires the laptop.

## Background

- `docs/archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md` — the machine, the reboot
  loop, the battery field map (§5.8), and the rtw89 idle-power-save notes
  (§5.2, §5.6) P2 builds on.
- `docs/archive/AKUMA_ACPI_POWER.md` — the battery decode and `/proc/power`'s
  shape; §Open lists brightness/lid/thermal as the known-not-exposed set (P3).
- `crates/akuma-kacho/src/lib.rs` — the observe/decide/hysteresis discipline
  this policy joins; `WakeBudget` is its fourth consumer-shaped primitive.
- `amd64/src/lapic.rs` — `arm_deadline`'s comment is the sub-tick-latency
  measurement the budget must not regress; `on_tick`'s is why periodic mode
  returns inside the handler.
