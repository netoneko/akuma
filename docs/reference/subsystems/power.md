# Power (amd64): `/proc/power`, the energy ledger, and the idle wake budget

Stability grade: **A-** — the battery decode is metal-verified against Linux
field-for-field (2026-10-15: `percent` 98 = 98, `voltage_mv` 12876 = 12876000,
`energy_now_mwh` 52480 = 52480000, `power_mw` 0 = 0, on AC); the ledger and
the idle knobs are QEMU-verified (2026-10-15). The metal A/B that decides the
`idlesleep` default has not run yet, and the budget does not yet reach
`allow_tick` — see the caveat at the end.

## `/proc/power`

`key=value` lines, units in the key, shaped like Linux's `power_supply`
uevent. Three sources, selected at boot:

| `source=` | selected by | what |
|---|---|---|
| `none` | default | no EC window found — a desktop or a VM. `ac=unknown`, `battery=0` |
| `ec` | the DSDT's `ERAM` region, or `ecram=0x<pa>` | the laptop's embedded controller, mapped uncached, read byte-wide |
| `sim` | `powersim` | `akuma_power::sim`: a 120 s discharge/charge cycle in real EC bytes, for machines with no battery |

Decode, render and integration live in `akuma-power`
(`#![forbid(unsafe_code)]`, no allocation, host-tested against bytes measured
on the real machine); `amd64/src/power.rs` is only the physmap scan, the
device mapping and the volatile reads. The field map and its Linux ground
truth: [`archive/AKUMA_ACPI_POWER.md`](../../archive/AKUMA_ACPI_POWER.md).

`energy_mwh=` is the running total the **ledger** integrates
(`crates/akuma-power/src/ledger.rs`). It accumulates between reads — the
renderer is the sampler, so an unread file costs and integrates nothing, and a
reader wanting resolution reads more often (the rio panel's once a second is
plenty). Only valid samples integrate; an EC glitch pauses the total instead
of inventing energy. On AC at BTST 0 the pack's own power is near zero and the
number honestly says so — it answers "how much came out of the pack", which is
what a before/after power comparison reads.

## The idle wake budget (`idlesleep`)

An idle amd64 core wakes every periodic tick (10 ms) and, finding nothing,
halts again. `idlesleep` lets `sched::idle_loop` replace a run of ticks with
one one-shot of up to **100 ticks (1 s)** — the budget ramping with
consecutive quiet laps (`akuma-kacho::WakeBudget`), floored again by the first
wake that finds work. `idlesleep=<ticks>` caps the ceiling lower.

Two invariants, both stated where they are enforced (`amd64/src/lapic.rs`):

1. **Deadline precision is not traded away.** A sub-tick futex deadline arms
   an exact one-shot (`arm_deadline`) and the long shot refuses to cover it;
   inside the budget the shot arms at the deadline's exact LAPIC counts, not
   rounded ticks.
2. **The clock never loses a tick.** `TICKS` *is* `uptime_us` and every
   smoltcp timeout rides on it. A long shot's skipped ticks are credited back
   — on the fire (`credit_shot_fired`) or on an early IRQ wake
   (`settle_idle_shot`, which also pets the watchdog). `[IDLESTAT]
   halts= shots= shot_ticks_credited=` in the 30 s dump is the A/B evidence.

**Off by default.** `idlemwait` swaps the idle bracket's wait instruction for
`mwait` (CPUID-gated, hint 0) as a third A/B arm. The decision procedure,
phases and the QEMU smoke evidence are in
[`../../../proposals/RYZEN_POWERSAVE_DEMON.md`](../../../proposals/RYZEN_POWERSAVE_DEMON.md).

**Known gap (measured on metal, 2026-10-15):** on a `nosmp` boot the idle
loop's 30 s block never runs in 11 minutes of a real workload (herd + sshd +
wifi, `[IDLESTAT]` absent from the whole `boot-118` dmesg) — every parker
takes the `allow_tick` halt, not the idle loop, so the budget never engages.
The fix is to arm the same budget from `allow_tick` (with `set_halted` around
its halt so peer wakes kick the core), and it is the first item of P1.

## Background

- [`archive/AKUMA_ACPI_POWER.md`](../../archive/AKUMA_ACPI_POWER.md) — the
  battery decode, the no-AML-interpreter decision, the field map.
- [`archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md`](../../archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md)
  §5.8 — the EC window at `0xFEEC2300`, verified to the milliwatt against
  Linux's `BAT0`.
- [`runbooks/amd64-bare-metal-loop.md`](../../runbooks/amd64-bare-metal-loop.md)
  — the command-line token table (`powersim`, `ecram=`, `idlesleep`,
  `idlemwait`).
