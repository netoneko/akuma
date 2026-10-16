# Akuma/amd64 touchpad — I²C-HID research and the pure half

2026-10-16. Written in answer to: *"can you do some research on making
touchpad work in framebuffer on akuma? i'd like to be able to click/right
click/scroll stuff on ryzen"*, then carried one increment: **the four pure
crates of the touchpad stack are built, host-tested and clippy-clean.**
Nothing is wired to hardware yet — that needs the Linux-side measurement
(M0 below) and the `amd64/src/` glue, in that order.

## Status

| | state |
|---|---|
| research (protocol, controller, clickpad semantics) | **done**, [`proposals/AMD64_TOUCHPAD_I2C_HID.md`](../../proposals/AMD64_TOUCHPAD_I2C_HID.md) |
| `akuma-hid` — report-descriptor parse + decode (OpenBSD `hid.c` port) | **done**: 13 host tests, `NOTICE` attribution |
| `akuma-i2c-hid` — transport decisions (descriptor, commands, framing) | **done**: 8 host tests |
| `akuma-dwi2c` — DesignWare I²C decisions (regs, timings, aborts) | **done**: 9 host tests |
| `akuma-touchpad` — clickpad policy (click/right/scroll state machine) | **done**: 20 host tests |
| M0: measure this unit's I²C controller + touchpad constants on Linux | **done 2026-10-16** — §4a below; rdesc pinned as `akuma-hid` fixture |
| kernel glue: `amd64/src/i2c.rs` + `touchpad.rs`, `/dev/input/mouse0` | **written 2026-10-16, builds, UNVERIFIED on metal** — §5; first boot is the next step |
| rio fork: read the node, winit pointer events, cursor sprite | **open** — kernel half first |

Full host suite at commit time: **2076 passed, 0 failed** (50 of them new);
the four crates build for `x86_64-unknown-none` and `aarch64-unknown-none`
and carry `#![forbid(unsafe_code)]` from their first commit
([`crate-safety.md`](../reference/crate-safety.md): 66 of 86).

## 1. The machine's touch

The ryzen box (Lenovo IdeaPad 5 2-in-1 16AHP9, `83DS`) carries its touchpad
as an **HID-over-I²C device on an AMD FCH DesignWare I²C controller**
(ACPI `_HID` `AMDI0010` class — `AMD0010`/`AMDI0010`/`AMDI0019` in Linux's
ACPI table). Its `_CRS` hangs off that controller; the touchpad's own ACPI
node is marked `PNP0C50`-shaped (an i2c-hid device). The keyboard is real
i8042 (works), the panel is the GOP framebuffer (works), there is no serial
port, and the touchpad is the machine's only pointer.

**Identity question — RESOLVED by M0 (§4a): Goodix `GXTP5100`, not Synaptics.** The original text: our machine doc says
*"I2C HID (Goodix touchpad, Wacom pen)"*; a linux-hardware.org probe of the
same model (`26d850d9b3`) says the touchpad is a **Synaptics `SYNA2BA6`
(06cb:cefe)** and the Goodix part is only the USB fingerprint reader. The
transport is identical either way, and the i2c-hid descriptor's
`wVendorID`/`wProductID` settles it in the first bring-up boot — only the
quirk timing differs (§3).

## 2. What the research established

The proposal carries the full stack with section references
(`proposals/AMD64_TOUCHPAD_I2C_HID.md`); the findings the crates encode:

1. **The controller** is the Synopsys `DW_apb_i2c` IP. Alive-check is one
   register: `IC_COMP_TYPE == 0x4457_0140`. Timing comes from the DSDT's
   `FMCN`/`SSCN` packages `[hcnt, lcnt, sda_hold]` — which means the OS
   never needs to know the controller's input clock. AMD-specific, and
   load-bearing: BIOS advertises the bus-clear feature in `IC_CON` bit 11
   and the **SMU firmware performs stuck-bus recovery** — a driver that
   clears that bit forfeits it (Linux `i2c_dw_probe_master`'s AMD comment).
   Linux supports these controllers with no IRQ at all (`ACCESS_POLLING`),
   which is this kernel's whole style.
2. **The transport** is HID-over-I²C v1.0. HID descriptor at register
   `0x0001` (30 bytes, `bcdVersion 0x0100`); bring-up is `SET_POWER ON` →
   **sleep 60 ms** (Goodix's Windows driver timing, and several devices do
   not work without it) → `RESET` → the ack arrives as a **zero-length
   input read** → read the report descriptor. `SET_POWER`'s power state
   rides in the id nibble with the **type nibble zeroed** (Linux passes
   report type 0; FreeBSD writes the byte raw) — found while writing the
   byte-exact encoder tests.
3. **Reading reports, polled:** a plain read returns `[len16][report…]`;
   length 0 = nothing pending (and doubles as the reset ack), `0xFFFF` =
   device garbage (ELAN). FreeBSD's `iichid` proves polling as a first-class
   mode: 80 Hz active / 10 Hz idle with a 16-sample hysteresis and whole-frame
   dedup. That is the shape the kernel pump will run.
4. **The pad is a clickpad**: one physical button under the whole surface.
   Right-click and scroll are synthesized — libinput's soft button areas
   (bottom min(10 mm, 15%) band, right half = right click), the wander rules
   (a main-area finger never becomes a button finger; a band finger that
   leaves it becomes a pointer finger permanently), the pending-click rule
   (click with no positional data waits for a finger, and is ignored if none
   comes — the "X1 Carbon rule"), and two-finger scroll with a start
   threshold and tick remainder carried.

## 3. What is built

Four crates under `crates/`, all `#![no_std]`, no allocation, fixed arrays,
host-tested. Mechanism will live in `amd64/` when the glue lands; every
*decision* already lives here and is tested.

| crate | lines¹ | tests | what it is |
|---|---|---|---|
| `akuma-hid` | 690 | 13 | the report-descriptor parser and report decoder — a structural Rust port of OpenBSD's `sys/dev/hid/c` (NetBSD lineage, Lennart Augustsson), with the copyright notice preserved in `src/lib.rs` **and** in a crate-level `NOTICE` |
| `akuma-i2c-hid` | 202 | 8 | descriptor validation (30 bytes, bcd 0x0100), the §7.2 command encoder, input framing (len-0 = reset ack, `0xFFFF` clamp), bring-up timing constants, the quirk subset that matters polled |
| `akuma-dwi2c` | 175 | 9 | the register map, `master_con` builder that preserves `BUS_CLEAR_CTRL`, the hcnt/lcnt arithmetic (golden values: 100 MHz → SS 427:499, FM 87:159), `FMCN` package apply, FIFO-depth and `TX_ABRT_SOURCE` decode |
| `akuma-touchpad` | 420 | 20 | the clickpad state machine: per-slot contacts, soft areas, clickfinger mode, two-finger scroll with banked engagement travel, the 16-slot event ring with moves-droppable overflow |

¹ production lines per `scripts/cloc_akuma.py`.

**Attribution** (the user's requirement): the NetBSD Foundation two-clause
notice ships verbatim at the top of `akuma-hid/src/lib.rs` and in
`crates/akuma-hid/NOTICE`, which also names the port's correspondences
(`hid_get_item` → `HidParser::next_item`, `hid_locate` → `locate`,
`hid_get_data` → `get_bits`) so upstream behaviour stays traceable. BSD-2
against the repo's BSD-2: compatible, attribution is the only condition.

### Two deliberate divergences from the C (documented in the port)

- **Collections always come out of the walk.** The C returns collection
  items unconditionally and lets the *caller* filter by kind; that return is
  what advances the state machine so the next call's reset clears the usage
  queue. The first port filtered inside the parser — and the boot-mouse test
  caught the button fields inheriting the *collection's* usages
  (`Usage(Mouse)`, `Usage(Pointer)` — pos 0 and 1 belonged to buttons).
- **`locate` walks the shared bit counter** (`Filter::All`, per-item kind
  check). The C's kind-filtered walk only advances positions for *matching*
  items, which makes `hid_locate(…, hid_feature)` on a PTP descriptor —
  whose Contact-Count-Maximum *feature* sits after all the *inputs* —
  return position 0. Wrong for exactly the field this port exists to read.

Both are in the crate header and doc comments; `report_size` keeps the C's
kind filter because only the requested kind advances there, which makes its
sizes per-kind correct.

### Bugs the tests found on the way (all fixed same-session)

- The collection-usage leak above — the reason the boot-mouse fixture
  exists as a test and not just a spec example.
- `get_bits`' byte assembly: the C's `data |= buf[rpos] << (8 * n)` uses the
  *post-decrement* `n` — highest byte shifted most. A first port with an
  ascending index read `X=1234` as `-11772`. The PTP fixture (a hand-built
  precision-touchpad descriptor, X/Y 16-bit at known positions) pinned it.
- `SET_POWER`'s type nibble (§2.2): the encoder initially wrote
  `ReportType::Input` — the byte-exact tests against Linux's
  `encode_command` call sites corrected it to type 0.

## 4. What the next increment needs (M0 first)

One Linux session on the box pins every constant (full recipe: proposal
§1.1): `acpidump`/`iasl` for the `AMDI0010` controller's MMIO base and the
touchpad's `I2cSerialBusV2` address, `dmesg | grep i2c` for the bus number,
and `/sys/kernel/debug/hid/*/rdesc` — which becomes a **second pinned
fixture** in `akuma-hid`'s tests, replacing the synthetic one for the
policy tests' geometry.

Then the kernel glue follows the wifi playbook (§6.3 of the proposal):
`amd64/src/i2c.rs` (MMIO, the polled transaction — `writel`-shaped like
`pci.rs`'s BAR mapping, `dwi2c=0x<pa>` cmdline override beside `ecram=`),
`amd64/src/touchpad.rs` (probe gates M1–M3 in dmesg), the pump hookup beside
`xhci::keyboard_poll`, and a `/dev/input/mouse0`-style node beside
`/dev/fb0` for the rio fork to read. QEMU cannot see any of this
(`microvm` has no I²C); every gate is verified over ssh via menu entry 12,
the same loop the wifi work ran.

## 4a. M0 results (measured on the box, Pop Linux 6.17, 2026-10-16)

| constant | value | source |
|---|---|---|
| touchpad | Goodix `GXTP5100` (ACPI `TPD3`, `_CID PNP0C50`), HID `27C6:01E0` | DSDT, dmesg |
| I²C slave address | **0x5D** (7-bit, `I2cSerialBusV2`) | DSDT. The `0018:` in the HID name is the bus type, **not** the address |
| bus speed | 400 kHz (0x61A80) | DSDT |
| controller | `AMDI0010:00`, MMIO **0xFEDC2000**, 4 KiB (siblings at FEDC3000/4000/5000) | `/proc/iomem`, DSDT `\_SB.I2CA` |
| interrupt | GpioInt level-low on `\_SB.GPIO` pin 8 — **ignored, we poll** | DSDT |
| FMCN/SSCN | **absent from the DSDT** | iasl; `akuma-dwi2c`'s package-apply path has nothing to apply here |
| other HID on the box | `0018:056A:53BC` (Wacom pen), `0020:1022:0001` (AMD sensor hub) | debugfs |

**Live registers** (read via `/dev/mem` after forcing the controller awake;
Linux had configured it): `IC_CON 0x65` (master, fast, restart-en),
`IC_FS_HCNT:LCNT 0x84:0xEF`, `IC_SS 0x282:0x2ED`, `IC_SDA_HOLD 0x10048`,
`IC_COMP_TYPE 0x44570140` (alive-check passes), `IC_COMP_VERSION 0x3230322a`.
Since no ACPI timing exists, the glue should **keep the HCNT/LCNT the
firmware/OS left** when it finds them non-zero (Linux does the same when it
has no clock), and fall back to computing from a clock only if zero.

**Power-gating trap (new risk):** with `power/control=auto` the controller is
`runtime_status: suspended` and **every register reads `0xFFFFFFFF`** — the
same signature as an unclocked block. Linux's ACPI PM brings it to D0 (and
the `AMDI0010` `_PS0`/clock gate). Akuma, which never ran that, must not
read `0xFFFFFFFF` as "no controller": M1 must treat it as *powered down*,
and the glue needs a way to wake it (ACPI `_PS0`, or the FCH clock-gate
register) before the `IC_COMP_TYPE` check. Same shape as the pad being
power-gated at handoff (R-table).

**Report descriptor** (665 bytes, ids 1,4,2,7,6,13,12,11,3,5,14) is now
`akuma-hid`'s `fixtures::GOODIX_GXTP5100`, with two pinned findings:
report **4** is the PTP input report — 29 payload bytes, 5 contacts × 40 bits
(tip bit 1, id 4 bits at 4, X 16 bits at 8, Y at 24), contact count at bit
216, clickpad button at bit 224; report 1 is a boot-mouse fallback. And
`FieldTable::collect` on the whole descriptor **truncates** (>64 fields across
11 report ids) — the glue must `locate()` per report id, not collect the lot.

## 5. The glue (2026-10-16 — builds; never run on the box)

Three more pure pieces landed first, because the first glue draft needed
decisions that had nowhere to live: `akuma-dwi2c::aoac` (the DSDT's `_PS0`
as functions — §4a), `akuma-touchpad::ptp` (report layout from the real
descriptor, **id-keyed finger slots**, the wire format) and the register
constants. Two things the real descriptor taught:
constant *padding* items keep the previous item's usage in the parser (the C
original does too), so `Tip` appeared at bits 1, 2 and 3 of contact 0 until
`PtpLayout` skipped `HIO_CONST` items; and the report's array order is not a
finger identity, which is why the decoder keys slots by contact id.

| file | what |
|---|---|
| `amd64/src/i2c.rs` | `wake` (AOAC D0, bounded), `map_mmio`, `Dw`: init keeping the firmware's divide counts, `xfer(write, read)` with FIFO-bounded command feeding and abort decode. All waits are TSC-bounded; no IRQs, no allocation |
| `amd64/src/touchpad.rs` | `discover` (DSDT contains `GXTP5100`, or `touchpad=0x<mmio>:0x<addr>`) → `init` (wake, DW init, HID descriptor @0x0001, `SET_POWER`, 60 ms, `RESET`, reset-ack, report descriptor, PTP layout, **Input Mode = 3 / Surface / Button switches** as `hid-multitouch` does, pointer geometry from the boot framebuffer) → `poll` from the console pump → `dev_read` |
| `FileDescriptor::DevMouse` | `open("/dev/input/mouse0")` → `ENOENT` unless the pad came up; `fstat` says char device (13,32) |

**Stream format** (8-byte records, `akuma_touchpad::ptp::encode`): byte 0 kind
(1 move, 2 button, 3 wheel); move: x,y as u16 LE at bytes 4–7 (panel pixels);
button: byte 1 = 0 left / 1 right / 2 middle, byte 2 = down; wheel: dx,dy as
i16 LE at 4–7 (`+dy` = up). `read` returns whole records or `EAGAIN` — it
never blocks.

**Known gaps, on purpose:**
- `poll`/`epoll` on the node: readiness is shared glue
  (`akuma-syscalls-glue::poll`), not touched here; readers loop on `EAGAIN`.
- The pump polls on its lap: 8 ms while a finger is down or within 2 s of one,
  100 ms idle, so the first touch after idle can wait up to 100 ms.
  The GPIO interrupt (pin 8) would remove that; polling was the design choice
  (§2.1 of the proposal).
- Eight consecutive failed polls disable the pad (`[touchpad] bus failing`),
  rather than stall the pump; a reboot revives it.
- The rio fork (read the node, winit pointer events, cursor sprite) is not
  started.

**First boot on the box — what to expect and what each failure means.** Build
`akuma-amd64` with `--features no-tests` (a plain build drops the quiet boot),
stage via menu entry 12 as the wifi work did, then look at `dmesg | grep
touchpad`:

| line | meaning if it is the last one |
|---|---|
| *(nothing)* | DSDT scan did not find `GXTP5100` — pass `touchpad=0xfedc2000:0x5d` |
| `GXTP5100 @0x5d on dw_i2c 0xfedc2000` then `controller would not power up (AOAC)` | the `_PS0` sequence did not turn the block on — compare AOAC bytes 0xFED81E4A/4B with §4a (`0x7C/0x27` awake) |
| `not a DesignWare controller (COMP_TYPE)` | still powered down or wrong window: print the register |
| `dw_i2c v=… fs=132:239 (kept)` | alive; those are Linux's counts. `(computed)` means the registers were zero and 150 MHz was assumed |
| `no ACK at the slave address` | address or timing wrong (`0x5d`, 400 kHz) |
| `hid 27c6:01e0 rdesc=665 maxin=32` | transport works end to end |
| `no reset acknowledgement (continuing)` | harmless if reports follow |
| `ptp report 4 contacts=5 range=3751x2327 -> WxH` | descriptor parsed, geometry chosen |
| `up: /dev/input/mouse0` | done |

Then, with a finger on the pad:
`while :; do head -c 8 /dev/input/mouse0 2>/dev/null | od -An -tu1; done`
prints `1 0 0 0 x x y y` records for movement, `2 0 1 0 …` on a click.

## Background

[`proposals/AMD64_TOUCHPAD_I2C_HID.md`](../../proposals/AMD64_TOUCHPAD_I2C_HID.md)
is the full research: register tables, protocol sections, the machine facts
([`AKUMA_AMD64_ON_RYZEN_LAPTOP.md`](AKUMA_AMD64_ON_RYZEN_LAPTOP.md) §1), the
risk table (power-gated pad at handoff, unit-to-unit touchpad vendor
variance), and the bring-up gates. External references: the HID-over-I²C
v1.0 spec; Linux `i2c-hid-core.c` and `i2c-designware-*.c`; FreeBSD
`iichid.c` (polled sampling); OpenBSD `hid.c`/`ihidev(4)`/`wstpad(4)`;
libinput's clickpad and scrolling docs.
