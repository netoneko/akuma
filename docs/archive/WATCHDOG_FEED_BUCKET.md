# An idle box reset itself: the watchdog was fed only on `tick % 100 == 0` (2026-10-16)

**Status: FIXED 2026-10-16** (`1e1b8138`). The current-state rule is in
[`docs/reference/subsystems/power.md`](../reference/subsystems/power.md) § "Watchdog feeding";
this file is the incident record.

## Symptom

Reports that a ryzen left alone went dark and then shut down. The hardware watchdog
(`wdt`, AMD FCH, default 60 s) had fired.

## Cause

Found auditing `powersaved`; the daemon was not the cause. With `idlesleep` the BSP's tick count
is credited in jumps (`lapic::credit_shot_fired`, `settle_idle_shot`), and `watchdog::pet` fed
the FCH watchdog only when `tick % 100 == 0`. A steady wake pattern — anything that wakes the
BSP every N ticks, `powersaved`'s own 1 s sleep included — could step over every multiple of 100
and never reload the watchdog. 60 s later the chipset reset a healthy machine.

## Fix

`pet` decides by which one-second *bucket* the count is in (`watchdog::pet_due`) instead of by
an exact multiple. A replay of seven wake patterns: a 600 s feed gap in five of them before,
~1 s in all after.

Two smaller dark-to-light gaps from the same audit were closed in the same commit:

- The touchpad did not count as activity: `idle_ms=` only saw the keyboard, so a finger on the
  pad did not undim the console. A finger down or a click now calls `input::note_input`.
- `powersaved` cached the brightness it last wrote, so a dim changed by anything else left its
  restore unwritten. It re-syncs the cache from the `dim=` in the `/proc/power` it already reads
  each lap.

## Background

- [`docs/reference/subsystems/power.md`](../reference/subsystems/power.md) — the idle staircase
  and `idlesleep` this interacts with.
- [`overlays/ryzen/README.md`](../../overlays/ryzen/README.md) — `wdt-probe.py` and the `wdt`
  command-line option.
