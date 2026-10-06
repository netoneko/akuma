# ACPI power on Akuma/amd64: battery and AC for the status bar

2026-10-07. **Stability: B** — the plumbing is verified on QEMU and on the
trashcan; the battery decode is verified against Linux on the Ryzen laptop's
*measurements* but has **not yet been read live by Akuma on that laptop**.

Written because the plan is to turn rio on the panel into a full computing
experience with a status bar (clock, battery, wifi, bell —
`AKUMA_AMD64_RIO_FBDEV_BUILD.md` § "Direction"), and the bar can only show what
the kernel exposes. This is the kernel half. Prior art, all of it used here:
`AKUMA_AMD64_ON_RYZEN_LAPTOP.md` §5.8 (the EC field map, measured from Linux),
`docs/handoff-battery-status.md` (the original brief; in git at `db751e53`).

## What exists now

| piece | where | state |
|---|---|---|
| ACPI table walk (RSDP, XSDT, MADT) | `crates/akuma-ryzen-amd64/src/acpi.rs` | existed |
| **DSDT lookup** through the FADT | same file, `acpi::dsdt` | new — no root table lists the DSDT, and it is where the battery's AML lives |
| **`akuma-power`** crate: AML `OperationRegion` finder, EC block decode, `/proc/power` text, simulator | `crates/akuma-power/` | new, 13 host tests, `forbid(unsafe_code)`, no allocation |
| kernel glue: scan tables, map the window, register the file | `amd64/src/power.rs` | new |
| **`/proc/power`** | `akuma-vfs-glue` `set_power_renderer` + five sites in `proc.rs` | new; absent on kernels that never register a renderer (every AArch64 target) |
| `KCMDLINE="…" sh amd64/run.sh` | `amd64/run.sh` | new — appends bare flags to the QEMU command line |

## Why there is no AML interpreter

A laptop battery is not a table of values. It is control methods (`_BIX`,
`_BST`) that read embedded-controller registers, and running them takes an AML
interpreter plus the ACPI namespace under it: thousands of lines, with failures
that are silent wrong numbers. The registers themselves are plain bytes. On the
Ryzen laptop the EC mirrors them into a memory window that the DSDT names:

```
OperationRegion (ERAM, SystemMemory, 0xFEEC2380, …)
```

So the subsystem does two small things instead: `aml::find_region` pulls that one
declaration out of the table bytes (`5B 80 <name> <space> <offset> <len>`, with
the name-string prefixes handled), and `ec::Reading::decode` turns the window
into numbers. **All per-machine knowledge is the field layout in `ec.rs`**; the
discovery is generic.

The interpreter is the right answer when a second machine needs a different
layout. It is not the right first answer, and `docs/handoff-battery-status.md`
reached the same verdict.

## `/proc/power`

`key=value` lines, units in the key, shaped like Linux's `power_supply` uevent so
a reader can ask for one field. Always present on amd64 (a bar must tell "no
battery here" from "kernel too old to say"); an AArch64 kernel does not have it.

```
source=ec            ec | sim | none
ac=0                 0 | 1 | unknown
battery=1
valid=1              0 = a window that decoded to nonsense; the fields below are omitted
status=Discharging   Discharging | Charging | Not charging | Unknown
percent=97
voltage_mv=13028
current_ma=536       a magnitude; the sign is `status`
power_mw=6983
energy_now_mwh=52940
energy_full_mwh=54420
energy_design_mwh=57000
minutes=454          to empty (discharging) or to full (charging); absent otherwise
btst=1               the raw status code, for the codes not yet decoded
ec_raw=0600…         the 20 undecoded bytes, to check the decode against another OS
```

`valid=0` exists because a window read from the wrong address — or before the EC
has filled it — is all `0x00` or all `0xFF`, and that must never render as a
charge. (`ecram=0xfeb00000` on QEMU, a device window that is not an EC, exercises
exactly this.)

## Sources and how to select them

| `source=` | selected by | for |
|---|---|---|
| `ec` | the DSDT or an SSDT declares an `ERAM` `SystemMemory` region; or `ecram=0x<pa>` on the command line | the Ryzen laptop |
| `sim` | `powersim` | a battery that does not exist: QEMU has no battery device and the trashcan is a desktop. A 120 s cycle (discharge 100→40 % on battery, then charge back on AC) generated as **raw EC bytes**, so the real decoder runs |
| `none` | default | desktops, VMs |

Boot log line: `[power] source=…`. The window is mapped uncached in the device
window (`DEVMAP_BASE`), read a byte at a time into a 20-byte stack array, and
rendered into the caller's buffer — **no allocation anywhere**, init included
(the table scan uses a 512-byte stack chunk with a 48-byte overlap so a
declaration across a chunk edge is still seen).

Init runs **after** `mem::init`, on both boot paths (PVH in `main.rs`,
multiboot2 in `multiboot2.rs`), because mapping the window allocates page-table
frames. Putting it next to `machine::report` — the obvious place — would have
run it before the PMM exists.

## Verification (2026-10-07)

**Host:** `cargo test -p akuma-power -p akuma-ryzen-amd64` — decode of the exact
block measured on the laptop (13028 mV, 5294/5442/5700, RSOC 97 %, 6983 mW,
status `0b110`), garbage rejection for all-`00` and all-`FF`, the AML finder
against every name-prefix form, truncations and non-memory spaces, the DSDT
lookup through both FADT pointers.

**QEMU microvm** (`KCMDLINE=… INIT=/bin/busybox INITARGS=cat,/proc/power sh amd64/run.sh`):

| flags | result |
|---|---|
| `powersim` | `source=sim`, 96 %, Discharging, 6966 mW, `minutes=449` — kernel → procfs → `cat` |
| (none) | `source=none ac=unknown battery=0` |
| `ecram=0xfeb00000` | `source=ec`, mapped, `valid=0`, all-zero `ec_raw` — the mapping + volatile read path without a crash |

**Trashcan** (HP 500-502nj, metal, kernel `e52e33cb` + this work, `no-tests`):
`/proc/power` appears in `ls /proc`; `[power] source=none (no ERAM region in the
DSDT/SSDTs)` — the real DSDT and SSDTs were scanned without incident and the
machine, a desktop, is reported as having no battery. That is the correct answer
and the first live check that the scan is safe on firmware nobody tuned it for.

## Open

1. **Read it live on the laptop.** Everything the decode depends on was measured
   from Linux through `/dev/mem`; Akuma has not yet read the window on that
   machine. Boot ryzen menu entry 12, `cat /proc/power`, and compare the
   `ec_raw` bytes and the numbers against `/sys/class/power_supply/BAT0/uevent`
   read in Pop at about the same time. If the layout is off by a byte the
   `valid` check will say so rather than show a wrong percentage.
2. **BTST codes.** `1` = discharging is observed. `2` = charging is **inferred**
   from the ACPI `_BST` convention and unobserved on this part; `0` on AC is
   rendered `Not charging`. The codes for *full* and *no battery* are open. Plug
   it in and read `btst=`; until then an unseen code renders `Unknown`, never a
   guess.
3. **The `ERAM` name and the `+0x80` base are this laptop's.** A second machine
   with a different region name or layout needs either a second table entry or
   the AML interpreter. `ecram=` is the escape hatch for investigating one.
4. **Not exposed yet:** thermal zones, lid, power button, brightness, CPU
   frequency — the same ACPI-region approach may cover some; none is needed for
   the first bar. Time is `clock_gettime` (UTC; there is no timezone database),
   wifi is `/dev/wifi0`, and the bell is rio's own — none needs `/proc/power`.
5. **The SSID.** `/dev/wifi0` carries it; whether the bar shows it is a rio
   decision (project rule: never *log* the network name — a status bar is the
   user's own screen, but make it a config switch).
6. **ECDT** is absent on this machine; a machine that has one names the EC's
   ports and would let the scan skip the DSDT.

## Next: the rio status bar

Not started. Plan: reserve one row in the fb platform of `netoneko/rio`, shrink
the pty by a row, and poll `/proc/power`, `/dev/wifi0` and the clock every second
or two from a thread; draw damaged cells only (the repaint costs in
`AKUMA_AMD64_ON_RYZEN_LAPTOP.md` §5.8 apply); set a bell flag when `BEL` arrives
on the pty and clear it on the next keypress.

## Background

- `AKUMA_AMD64_ON_RYZEN_LAPTOP.md` §5.8 — the EC field map and the Linux ground
  truth; `overlays/ryzen/ec-sample.sh`, `overlays/ryzen/acpi-dump.sh`.
- `AKUMA_AMD64_RIO_FBDEV_BUILD.md` — rio on the panel.
- `AKUMA_WIFI_CONTROL.md` (in `proposals/`) — `/dev/wifi0`.
- `docs/handoff-battery-status.md` at `db751e53` — the original brief.
