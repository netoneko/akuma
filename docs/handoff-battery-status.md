# Handoff prompt: battery status on Akuma/ryzen (for Kimi, on the box)

Paste everything below the line into goose (`goose-kimi session`) on ryzen, or
read it as `/root/HANDOFF-battery.md`. Written 2026-10-06.

---

You are on Akuma/amd64 running on bare metal on a Lenovo IdeaPad 5 2-in-1 16AHP9
(Ryzen 7 8845HS). The repo is at `/src/github.com/netoneko/akuma` (branch
`ryzen-wifi`; **the user drives all commits — never `git commit`/`push`**).
Environment: `. /etc/akuma-dev.env`; build the kernel with `kbuild --online`
(first run; `-j 1`, SMP builds are fragile), install with `kinstall`, reboot with
`/bin/busybox reboot -f`. Read `CLAUDE.md` and `docs/runbooks/amd64-bare-metal-loop.md`
first. There is no `/sys` and no `/proc/acpi`; this kernel is not Linux.

**Goal:** report the battery's state — percentage, charging/discharging,
voltage, rate, time remaining — from Akuma, then a small applet (text first,
framebuffer/rio second) that shows it. A first target is a `/dev/battery`-style
or `/proc/...` text file the applet can read.

## What exists

- `crates/akuma-ryzen-amd64/src/acpi.rs`: RSDP scan, XSDT/RSDT walk, table
  lookup by signature, MADT parse. `amd64/src/machine.rs` prints the table list
  at boot (`acpi: tables:` in `dmesg`). **No AML interpreter**, and nothing reads
  the embedded controller (EC) yet.
- The battery on this class of laptop is not a table of values: it is AML
  control methods (`_BIF`/`_BIX` static info, `_BST` state) in the DSDT/SSDTs
  that read **EC registers** through an `EmbeddedControl` operation region (EC
  command port `0x66`, data port `0x62`). The `ECDT` table, if present, names
  the EC and its ports.

## Two ways in — pick after looking at the tables

1. **Read the EC directly.** Find the battery's EC offsets in the decompiled
   DSDT (`OperationRegion (ECOR, EmbeddedControl, …)` fields used by `_BST`/`_BIF`
   — remaining capacity, full-charge capacity, present rate, voltage, status
   bits), then implement the EC read protocol (`RD_EC` = command `0x80`, wait for
   IBF clear / OBF set on port `0x66`) in a small host-testable crate behind a
   `forbid(unsafe_code)` seam (port I/O stays in the kernel; see how
   `amd64/src/` does port access). Cheapest, brittle to this one model.
2. **An AML interpreter** for `_BST`/`_BIF`: heavier, general, many ACPI
   namespace dependencies (`_STA`, `_HID` PNP0C0A, mutexes). Only if (1) is
   unworkable.

## Ground truth and the data

The dump is on disk (2026-10-07): `/root/acpi/` — `tables/` (45 raw tables:
`DSDT`, `SSDT1..26`, `FACP`, `BATB`, no `ECDT`), `asl/` (decompiled `.dsl`;
start with `grep -n "_BIX\|_BST\|PNP0C0A\|EmbeddedControl" asl/DSDT.dsl asl/SSDT*.dsl`),
`linux/power_supply-BAT0.uevent` (Linux's reading: 87 % charging, 12.973 V,
20.964 W, 47.2 / 54.42 Wh — compare yours against a fresh reading, which will
differ; ask the user to run `cat /sys/class/power_supply/BAT0/uevent` in Pop if
a same-moment comparison is needed). `linux/ec0-io.bin` is **absent** (debugfs EC
file not available), so find the EC offsets from the ASL (`OperationRegion
(…, EmbeddedControl, …)` and the `Field` blocks used by `_BIX`/`_BST`).
Related graphics findings (for rio, not this task): `/root/gfx/gfx.txt`,
`docs/archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md` § 5.8. To refresh either dump the
user boots Pop and runs `sh overlays/ryzen/acpi-dump.sh` / `gfx-dump.sh`.

## Rules that bite here

- Kernel code: justify every allocation; console output only via
  `safe_print!`/`tprint!`; keep new pure logic in a host-tested crate.
- Run clippy and the host tests (`cargo test --target <host>`) for any crate you
  touch; kernel builds: `--features no-tests` is the metal build.
- Never print, log or commit the wifi network's name/passphrase, the BSSID or
  the card's real MAC. Never print the Kimi token.
- `goose` runs through `goose-kimi`; the model budget is finite (about 100 units
  per 5 h), so batch tool calls and do not poll.
- Report: what the EC/AML showed, the numbers Akuma read next to Linux's, and
  the next command.
