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

## Why an idle box could reset itself (fixed 2026-10-16)

Found auditing `powersaved` after reports that a box left alone went dark and
then shut down. The daemon was not the cause. With `idlesleep` the BSP's tick
count is credited in jumps (`lapic::credit_shot_fired`,
`settle_idle_shot`), and `watchdog::pet` fed the FCH watchdog only when
`tick % 100 == 0` — so a steady wake pattern (anything that wakes the BSP every
N ticks, `powersaved`'s own 1 s sleep included) could step over every multiple
of 100 and never reload it. 60 s later (`wdt` default) the chipset reset the
machine. `pet` now decides by which one-second *bucket* the count is in
(`watchdog::pet_due`); a replay of seven wake patterns went from a 600 s gap
(five of them) to ~1 s in all. Two smaller dark-to-light gaps were closed at
the same time: a finger on the touchpad now counts as activity (`idle_ms=`
only saw the keyboard), and `powersaved` re-syncs its cached brightness from
the `dim=` in the `/proc/power` it already reads each second, so a dim changed
by anything else no longer leaves the restore unwritten.

## The power-saving daemon (`powersaved`)

A dedicated userspace binary (`userspace/powersaved/`, herd-supervised — the
"binaries, not scripts" rule of
[`../../userspace-layout.md`](../userspace-layout.md)) that closes the loop
between the meter and a lever:

- **Input**: `/proc/power` once a second — its `idle_ms=` fact (console-input
  inactivity; `amd64/src/input.rs` records the last keystroke across the
  i8042, USB HID and serial) and, on a metered machine, `power_mw=`. The
  daemon's 1 Hz read is also what keeps the energy ledger integrating.
- **Policy**: a staircase of idle thresholds and brightness steps, pure code
  in the `--lib` half, host-tested (`cargo test -p powersaved --lib
  --no-default-features`). Defaults: 60 s → 60 %, 180 s → 30 %, 600 s → 10 %;
  anything below the first step is 100 %. argv tunes it (`stage=ms:pct`,
  repeatable; `after_ms=` moves the first step); a malformed argv exits
  rather than silently dimming on defaults.
- **Effect**: only on a *change*, one `dim=<pct>` line to `/dev/power` — the
  kernel's control device (`amd64/src/power.rs`'s `dev_read`/`dev_write`,
  misc 10:243, the `wifi0` shape). The kernel clamps, applies the factor to
  the framebuffer console (`multiboot2::fb_set_dim` → `akuma-fbcon`'s global
  dim + `repaint()`), and reports it back as `dim=` in both files.
- **Restore**: a keystroke resets `idle_ms` in the kernel; the next
  observation asks for 100 % and one write puts the light back. The daemon
  re-reads `/dev/power`'s `dim=` at start, so a restart mid-dim continues the
  staircase instead of double-stepping.

The dim is the console's colour scaling: it lowers what the pixels emit but
does not touch the panel's backlight — that lever (EC/backlight register,
P3 in the demon proposal) gets its own `/dev/power` command when the kernel
has the register map. `dim=0` is refused (`EINVAL`) for exactly that reason:
software scaling cannot truly blank a panel, and pretending otherwise would
promise a watt the pixels do not deliver.

### The decision map

```
                POWER-SAVING DECISIONS (amd64, the ryzen laptop)

  boot cmdline                          idle core, every lap
  ------------                          --------------------
  idlesleep[=N]  idlemwait              x86_yield() found work? --yes--> run it
      |  (off by default)               |no; quiet laps++
      v                                 v
  idle policy on:                  budget = WakeBudget(quiet laps) <= 100 ticks
  one long shot may replace             |     (no budget unless idlesleep)
  a run of 10 ms ticks                  v
                                  arm_deadline (exact, always)
                                  + arm_idle_shot(budget)
                                        v
                                  sti; hlt|mwait; cli
                                        |
                     +------------------+------------------+
                     | timer fires                      | device IRQ first
                     v                                  v
               on_tick: credit armed-1          settle_idle_shot:
               (the clock never                 credit min(elapsed, armed-1),
                loses a tick)                   pet the watchdog
                     |                                  |
                     +------------------+------------------+
                                        v
             [IDLESTAT] halts= shots= shot_ticks_credited=     (every 30 s)

             known gap (P1): an allow_tick halt arms no budget yet --
             on a nosmp boot the BSP lives there, so the budget never
             engages; arm it from allow_tick (P1, first item)

  keystroke (i8042 / USB HID / serial)
        |  input::getb / has_byte
        v
  LAST_INPUT_US = now  ----->  /proc/power  idle_ms=<ms>
                                     (plus source/ac/battery/percent/
                                      power_mw=/energy_mwh=/dim=)
                                     | read at 1 Hz by powersaved
                                     | (this read integrates the ledger)
                                     v
                          powersaved::step  (pure, host-tested)
                            idle <   60 s -> 100   (restore: one write)
                            idle >=  60 s -> 60
                            idle >= 180 s -> 30
                            idle >= 600 s -> 10
                                     | write only on change
                                     v
                          /dev/power   dim=<pct>
                            (parse all lines, then apply; dim=0 is EINVAL
                             -- software dim cannot truly blank, P3 lever)
                                     v
                          multiboot2::fb_set_dim
                                     v
                          fbcon: dim_pct + repaint() -> the glass
```


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
