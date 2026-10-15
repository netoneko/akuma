# Touchpad on the ryzen panel — research: I²C-HID clickpad, polled, into rio

> **Built in the first increment (2026-10-16, same day):** the four pure
> crates of the stack — [`crates/akuma-hid`](../crates/akuma-hid) (the
> OpenBSD `hid.c` port, attribution preserved in its `NOTICE`),
> [`crates/akuma-i2c-hid`](../crates/akuma-i2c-hid),
> [`crates/akuma-dwi2c`](../crates/akuma-dwi2c) and
> [`crates/akuma-touchpad`](../crates/akuma-touchpad) — 50 host tests, all
> `#![forbid(unsafe_code)]`, building for both kernel targets. The full
> suite read 2076/0. What remains needs hardware facts: **M0 below**
> (one Linux session), then the `amd64/src/` glue and the rio fork's reader.
> Session record: [`docs/archive/AKUMA_AMD64_TOUCHPAD_I2C_HID.md`](../docs/archive/AKUMA_AMD64_TOUCHPAD_I2C_HID.md).

2026-10-16. Research, nothing built. Written in answer to: *"can you do some
research on making touchpad work in framebuffer on akuma? i'd like to be able
to click/right click/scroll stuff on ryzen."*

"Framebuffer" here means the rio panel (`/dev/fb0`, 1920×1200 GOP framebuffer)
on the **Lenovo IdeaPad 5 2-in-1 16AHP9 (83DS)** — the machine of
`docs/archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md`. Its internal touchpad is the
only pointer device it has, and rio's known-gaps list has said since the first
session: *"Input is keyboard-only (console tty). Mouse would need evdev; there
is no pointer on the panel anyway"* (`docs/archive/AKUMA_AMD64_RIO_FBDEV_BUILD.md`
§ "Known gaps").

Verdict up front: **the touchpad is an HID-over-I²C device on an AMD DesignWare
I²C controller, and everything akuma already does polled (xHCI keyboard, i8042,
NVMe) covers the shape of this driver.** No interrupts, no AML interpreter and
no evdev are required for click / right-click / scroll. The work is five small
layers, three of them pure host-tested crates in the house style, and one
consumers-side change in the rio fork.

---

## 0. The machine's touch, measured and re-measured

| fact | value | source |
|---|---|---|
| touchpad | **I²C HID** ("Goodix touchpad, Wacom pen" — see below) | `AKUMA_AMD64_ON_RYZEN_LAPTOP.md` §1 |
| keyboard | real i8042, PS/2 behind the EC — already works (`amd64/src/kbd.rs`) | same |
| panel | eDP 1920×1200 via GOP framebuffer | same |
| linux-hardware probe of the *same model* (83DS) | touchpad `SYNA2BA6:00 06CB:CEFE` (Synaptics), pen `Wacom HID 53C2 Finger/Pen`, fingerprint `Goodix 27c6:650a` (USB) | linux-hardware.org probe `26d850d9b3` |
| I²C controllers | AMD FCH DesignWare instances, ACPI `_HID` `AMDI0010` class | Linux `i2c-designware-platdrv.c` ACPI table |

**Open question 1 — which vendor's pad is on *this unit*.** Our doc says
Goodix; the same-model probe says Synaptics `06cb:cefe` with the Goodix part
being only the USB fingerprint reader. It does not matter for the plan: both
speak the same i²c-hid transport, and the driver learns vendor/product from the
HID descriptor at probe time (`wVendorID`/`wProductID`) and logs them. Only
quirk timing differs (§3). The Linux-side measurement in M0 settles it.

---

## 1. The stack, bottom up

```
finger ─ Goodix/Synaptics clickpad (i2c-hid device, addr typ. 0x14/0x2c/0x38/0x5d)
       ─ AMD FCH DesignWare I²C master (ACPI AMDI0010, MMIO from _CRS)   ← §2
       ─ akuma polled I²C driver (no IRQ — everything here is polled)    ← §2
       ─ i²c-hid transport (HID desc → report desc → input reports)      ← §3
       ─ HID report decode (report items → touch contacts + button)      ← §4
       ─ clickpad policy (click / right-click / two-finger scroll)       ← §5
       ─ /dev/input/mouse0-style node → rio fb platform → winit events   ← §6
```

### 1.1 Discovery — what to measure on Linux first (M0)

One Linux session on the box (`sudo` on Pop) pins every constant. The
`ecram=0x<pa>` override precedent (`amd64/src/power.rs`) and the wifi
`w0-trace.sh` recipe are the models: measure on Linux, hardcode + cmdline
override in akuma, generalize later.

```sh
# which controller, which bus, which address
acpidump > /tmp/acpi.out && acpixtract -a /tmp/acpi.out && iasl -d /tmp/dsdt.dat
grep -nE "AMDI0010|PNP0C50|SYNA|GDX|GXTP|ELAN" /tmp/dsdt.dsl
#   touchpad device _HID (PNP0C50 = i2c-hid marker) sits under an AMDI0010
#   controller; its I2cSerialBusV2 resource carries the slave address + speed;
#   the controller's _CRS carries the MMIO base/len (+ GSI we will ignore)
dmesg | grep -E "i2c_designware|i2c-(SYNA|GDX|GXTP|ELAN)"
ls -l /sys/bus/i2c/devices/          # i2c-N that owns the touchpad
#   _CRS MMIO base of that controller:
cat /sys/bus/acpi/devices/AMDI0010:0N/physical_node/*/resource 2>/dev/null || \
  grep -A3 "Device (I2C.)" /tmp/dsdt.dsl
# timing registers the DSDT supplies (Linux prefers these over computing):
grep -B2 -A6 "Method (.*FMCN|SSCN)" /tmp/dsdt.dsl   # packages [hcnt, lcnt, sda_hold]
# the raw HID report descriptor, for pinning test fixtures:
cat /sys/kernel/debug/hid/*/rdesc | xxd
```

Record: controller MMIO base, touchpad 7-bit address, bus speed (400 kHz
typical), FMCN/SSCN values, and the report-descriptor bytes. If the DSDT route
is annoying, `mmiotrace` the Linux probe (the wifi recipe) — it shows the same
facts as register writes.

---

## 2. The DesignWare I²C controller (AMD FCH)

AMD's internal I²C is the Synopsys DW_apb_i2c IP, enumerated by ACPI as
`AMD0010` / `AMDI0010` / `AMDI0019` / `AMDI0510` (Linux
`i2c-designware-platdrv.c` `dw_i2c_acpi_match`; `AMDI0019` adds an arbitration
semaphore — if the box has one on the touchpad's bus, M0 will show it). Linux
supports these controllers **with no IRQ at all** (`ACCESS_POLLING`), which is
exactly this kernel's style.

### 2.1 Register map (32-bit MMIO registers, from `include/linux/designware_i2c.h`)

| offset | name | use |
|---|---|---|
| 0x00 | `IC_CON` | mode: `MASTER`(bit0) \| `SPEED`(bits2:1, `10`=400 kHz) \| `RESTART_EN`(5) \| `SLAVE_DISABLE`(6) \| `BUS_CLEAR_CTRL`(11, AMD) |
| 0x04 | `IC_TAR` | target address, 7-bit |
| 0x10 | `IC_DATA_CMD` | write byte (bits7:0), or read-command `0x100`; bit8 = RESTART, bit9 = STOP (set on the last byte), bit10 = STOP_ON_EMPTY |
| 0x14–0x20 | `IC_SS/FS_SCL_HCNT/LCNT` | SCL high/low counts, standard (100k) and fast (400k) |
| 0x38/0x3c | `IC_RX_TL`/`IC_TX_TL` | FIFO thresholds (RX_TL=0 is what we want) |
| 0x34 | `IC_RAW_INTR_STAT` | polled status: `RX_FULL`(2), `TX_EMPTY`(4), `TX_ABRT`(6), `STOP_DET`(9) |
| 0x40/0x54 | `IC_CLR_INTR`/`IC_CLR_TX_ABRT` | write-1-to-clear (read-to-clear) side effects |
| 0x6c | `IC_ENABLE` | bit0 enable, bit1 ABORT |
| 0x70 | `IC_STATUS` | `ACTIVITY`(0), `TFE`(2), `RFNE`(3), `MST_ACTIVITY`(5) |
| 0x74/0x78 | `IC_TXFLR`/`IC_RXFLR` | FIFO fill levels |
| 0x80 | `IC_TX_ABRT_SOURCE` | why the last transfer aborted — bit0 `7B_ADDR_NOACK` is the "nothing at this address" answer |
| 0x9c | `IC_ENABLE_STATUS` | bit0 = actually disabled (disabling takes effect after the current byte) |
| 0xf4–0xfc | `IC_COMP_PARAM_1` / `_VERSION` / `_TYPE` | FIFO depths bits[23:16]/[15:8]; `_TYPE` = `0x44570140` ("DW") — the cheap alive-check |

### 2.2 Init

1. Read `IC_COMP_TYPE` — must be `0x44570140`, else the MMIO window is wrong
   (this is the probe's first gate, and what QEMU can never show).
2. Disable (`IC_ENABLE=0`), poll `IC_ENABLE_STATUS` bit0.
3. Write `FS_SCL_HCNT/LCNT` (+ `SS_*`), from the DSDT's **`FMCN`/`SSCN`**
   packages `[hcnt, lcnt, sda_hold]` — this is why we never need to know the
   controller's input clock: Linux takes the counts straight from ACPI when
   present (`i2c_dw_acpi_configure`), which is the honest thing to copy. If the
   DSDT has no methods, compute (Linux `i2c_dw_scl_hcnt/lcnt`: FM tHIGH=0.6 µs,
   tLOW=1.3 µs, tf=0.3 µs) from a guessed 100 MHz clock **and say so in dmesg**.
4. `IC_SDA_HOLD` from FMCN's third element (write only if `IC_COMP_VERSION ≥
   0x3131312A`), and force a non-zero RX hold (bit16) as Linux does.
5. `IC_TX_TL = depth/2 - 1` (or small), `IC_RX_TL = 0`, then
   `IC_CON = MASTER | SLAVE_DISABLE | RESTART_EN | SPEED_FAST`.
   **AMD caveat, load-bearing:** read `IC_CON` *first* and preserve
   `BUS_CLEAR_CTRL` (bit 11) if firmware set it — on AMD platforms the SMU
   firmware owns SCL/SDA stuck-low recovery, and clearing the bit forfeits it
   (`i2c_dw_probe_master`: *"On AMD platforms BIOS advertises the bus clear
   feature… Driver should not ignore this"*).

### 2.3 One polled transaction (the shape akuma wants)

A write-then-read (`i²c-hid` register read) is one controller transaction with
a RESTART between the halves:

1. Wait `IC_STATUS.ACTIVITY == 0` (bus idle).
2. `IC_ENABLE=0`; set `IC_TAR` = touchpad address; `IC_ENABLE=1`.
3. Write phase: push each byte into `IC_DATA_CMD` (plain); first byte of the
   *second* (read) half instead carries RESTART(bit10).
4. Read phase: push N read-commands `0x100` (+STOP bit9 on the last), draining
   `IC_RXFLR` bytes from `IC_DATA_CMD` (bits7:0) as `RFNE`/`RXFLR` allows —
   respect FIFO depth from `IC_COMP_PARAM_1` to avoid RX overrun.
5. Poll `RAW_INTR_STAT` for `STOP_DET` (success) or `TX_ABRT` (failure). On
   `TX_ABRT`: read `IC_TX_ABRT_SOURCE` **before** clearing; bit0 (address
   NACK) means "no device at this address", exactly the diagnostic Linux's
   `i2c_hid_probe_address` produces. Then `IC_ENABLE=0`, wait
   `IC_ENABLE_STATUS`, clear `CLR_TX_ABRT`.
6. Timeout (say 25 ms for a report read, per-byte pacing not needed at 400 kHz
   for ≤ 64 bytes): disable + re-init the controller (Linux's recovery without
   GPIO), report EIO upward.

FreeBSD's `iichid` and Linux's polled `ACCESS_POLLING` path both prove this
shape; U-Boot's `drivers/i2c/designware_i2c.c` is a third readable reference
that does the whole thing synchronously.

---

## 3. The i²c-hid transport

Spec: **"HID Over I²C" v1.0** (Microsoft/Intel; sections cited below are the
ones FreeBSD's `iichid` names). Behavioral references, all read for this doc:
Linux `drivers/hid/i2c-hid/i2c-hid-core.c` + `i2c-hid-acpi.c`, FreeBSD
`sys/dev/iicbus/iichid.c`, OpenBSD `ihidev(4)`. BSD-licensed sources may be
consulted freely; implement from the spec + these, do not paste code.

### 3.1 Finding the device

- ACPI marker: the touchpad device's `_HID` is `PNP0C50`/`ACPI0C50` (or a
  vendor string like `SYNA2BA6`/`GDX…` with `_CID PNP0C50`). Its
  `I2cSerialBusV2` resource names the controller + slave address.
- HID descriptor register address: from `_DSM` (GUID
  `3cdff6f7-4267-4555-ad05-b30a3d8938de`, rev 1, func 1 → u16). Every device in
  practice answers at **0x0001**; FreeBSD falls back to `ELAN=0x0001`, Linux
  hardcodes it. akuma: try 0x0001, cmdline override if the box ever disagrees.

### 3.2 HID descriptor (spec §5.1.1; 30 bytes, read = write reg-addr LE then read)

| off | field | expect |
|---|---|---|
| 0 | `wHIDDescLength` | **30** |
| 2 | `bcdVersion` | **0x0100** |
| 4 | `wReportDescLength` | e.g. 200–400 |
| 6 | `wReportDescRegister` | 0x0002 |
| 8 | `wInputRegister` | informational |
| 10 | `wMaxInputLength` | read cap for reports |
| 12/16 | `wOutputRegister`/`wMaxOutputLength` | 0 = no output reports |
| 18 | `wCommandRegister` | typ. 0x0003 |
| 20 | `wDataRegister` | typ. 0x0004 |
| 22/24/26 | `wVendorID`/`wProductID`/`wVersionID` | e.g. 0x06CB / 0xCEFE |

Validate `bcdVersion == 0x0100` and length == 30 (both drivers do); log
vendor:product — this is also where the Goodix-vs-Synaptics question answers
itself.

### 3.3 Bring-up sequence (the part with the magic delays)

From `i2c_hid_core_probe`, with the delays Linux's comments justify:

1. *address probe*: 1-byte read; if NACK, wait 400 µs, retry (STM/Weida quirk).
2. `SET_POWER ON` (command, below). If it fails, retry once after 400 µs.
   **Sleep 60 ms after power-on** — the spec lets the device clock-stretch,
   but Linux observed Windows sleeping 1 ms here and *60 ms for Goodix*, and
   "several devices will not work properly" without it. Cheap insurance.
3. `RESET` command. The device answers not with a response on the command
   channel but with an **input report of length 0** (a 2-byte `0x0000` read).
   In polled mode: sleep 100 ms then do one length-2 read; a `0x0000`
   acknowledges (FreeBSD's `reset_acked` shape; retry ×3 on failure — Linux
   tries the whole power+reset dance three times).
4. Read the report descriptor: write `wReportDescRegister` LE, read
   `wReportDescLength` bytes.
5. Later `SET_POWER SLEEP` when no consumer has the device open (spec §8.2);
   **reads from a SLEEP device return garbage** (FreeBSD comment), so the poll
   loop must power on before its first read after any sleep.

### 3.4 Command encoding (spec §7.2; byte-exact, host-test it)

```
byte0 = report_type << 4 | report_id        (input=0x1, output=0x2, feature=0x3)
byte1 = opcode                              byte2 (only) if report_id ≥ 0x0F
written as: [wCommandRegister LE16] [byte0] [byte1] ([byte2])
```

Opcodes: RESET 0x01, GET_REPORT 0x02, SET_REPORT 0x03, SET_IDLE 0x05,
SET_POWER 0x08. `SET_POWER` sends `power_state` (0=ON, 1=SLEEP) as the
report_id field.

### 3.5 Reading input reports, polled (the design decision)

Two proven patterns:

- **Plain read**: read `wMaxInputLength` bytes straight from the device
  (no register write). First 2 bytes = report length LE; 0 = nothing pending
  (and this doubles as the RESET ack, see above). This is what both Linux
  (`i2c_master_recv`) and FreeBSD (`iichid_cmd_read`) do on the interrupt;
  FreeBSD *also* runs it from a **sampling callout when there is no IRQ**:
  80 Hz while active, 10 Hz when idle, with a 16-sample hysteresis between
  rates, deduplicating identical consecutive reports.
- **GET_REPORT command** (opcode 0x02, type input): write command, then read
  `[len16][report...]` from `wDataRegister`. Response `len ≤ 2` or
  `0xFFFF` = nothing (FreeBSD `ENOMSG`).

akuma takes the **plain read** on a poll step (§6): fewer bus turns, one
dedup memcmp, and the identical code path reads the RESET ack. Failure
(NACK / `TX_ABRT`) is silent-tolerated — the touchpad may be powered down and
must not spam the console (match `input.rs`'s "absent device costs nothing"
contract).

Quirk notes worth carrying: ALPS/Synaptics-SYNA2393 want RESET on resume
(irrelevant for us — no suspend yet); Goodix `27c6:0d42` wants 1.5 s extra
after power-on before it will interrupt (our 60 ms + first-touch retry covers
the polled case); ELAN sends bogus `0xFFFF` lengths — clamp on
`ret_size > wMaxInputLength` like Linux does, drop the report.

---

## 4. HID report layer

The report descriptor is item stream per USB HID §6.2.2 — same grammar as the
USB side `crates/akuma-usb` already lives next to, but general (we must *parse*,
not assume boot-protocol layout; touchpads do not do boot protocol).

Prior art of exactly the right size: OpenBSD `sys/dev/hid/hid.c` (NetBSD
lineage, ~450 lines, BSD) — `hid_get_item` walks main/global/local items with
Push/Pop and per-report-ID bit positions; `hid_locate` finds a field by
(usage, report ID, kind); `hid_get_data` extracts sign-extended bitfields.
This ports cleanly to a `#![forbid(unsafe_code)]` crate: fixed-size state
(`MAXUSAGE 64`, `MAXPUSH 4`, `MAXID 16` — all consts), no allocation, borrow
over `&[u8]`.

Fields akuma needs, by usage page/usage, discovered at probe and cached in
five `hid_location`s + the report ID:

| field | usage page | usage |
|---|---|---|
| tip switch (contact) | Digitizers (0x0D) | 0x42 (array/bitfield of contact max) |
| contact count | Digitizers | 0x54 |
| X | Digitizers or Desktop (0x01) | 0x30 |
| Y | same | 0x31 |
| physical click button | Button (0x09) | 0x01 |
| (wheel, if the pad reports one) | Desktop | 0x38 |

`logical_min/max` of X/Y gives the sensor range used by §5's geometry;
`contact count max` says whether two-finger scroll is real multitouch or
semi-MT (semi-MT pads: libinput refuses two-finger scroll — degraded mode:
single-finger edge scroll, say so in dmesg). Host-test the parser against the
**real descriptor dumped from the box** pinned as a fixture byte array, the way
`akuma-usb`'s tests are built against bytes read off the HP box.

---

## 5. Clickpad policy (the layer that makes click / right-click / scroll)

This pad is a **clickpad**: one physical button under the whole surface; the
button bit is button 0 regardless of where the finger was. Everything else is
synthesized. libinput's model (doc "Clickpad software button behavior" +
`evdev-mt-touchpad-buttons.c`, and OpenBSD `wstpad(4)`) is the reference; all
of it is pure state-machine logic — ideal `kacho`-style crate material.

**Click → left / right:**

- Soft button areas: bottom strip, height = min(10 mm, 15% of pad height);
  right half of the strip = right button, left half = left (libinput default).
  With the pad's mm size unknown, use the fraction form: bottom **15%** band,
  right **50%** split. No middle button (there is no use for it on this panel;
  add later only if something needs paste).
- The decision uses where the click finger **started**: a finger that set down
  in the right area and never left it makes the click a right-click; fingers
  that wandered into the strip from the main area never become button-area
  fingers; fingers inside the strip don't move the pointer while they stay
  there (all libinput rules, listed because they are the difference between a
  clickpad that feels native and one that fights you).
- Physical click with no positional data (finger past sensor edge) = left,
  dropped into motion state as-is.
- Clickfinger alternative (1 finger = left, 2 = right) is a config bit, not a
  second code path: the same state machine with the area check replaced by a
  finger-count check.

**Scroll → wheel:**

- Two fingers down + movement = scroll; the accumulated Y delta of the two
  contacts becomes wheel ticks, X delta becomes horizontal scroll (rio can
  ignore it at first). Libinput requires a start threshold before scrolling
  engages; use ~8 px of combined movement, then 1:1. Wheel tick = ~50 px of
  finger travel (tunable const; rio maps LineDelta anyway).
- While scrolling, suppress pointer motion and button-area interpretation.
- A physical click ends scroll mode (libinput rule).
- Contact lift of either finger ends scroll mode; the next contact starts
  clean.

**Pointer motion:** absolute — contacts map sensor range → panel 1920×1200
1:1, no acceleration (v1). Absolute is both simpler and nicer on a terminal
(accuracy beats feel); it also means the kernel never needs to know DPI.

**Tap-to-click**: deliberately phase 2 (needs a hold-time state and causes
more false positives than it solves on the first day). The physical click +
button areas cover the ask.

Output of the policy crate is a small event enum — `Move(x,y)`,
`Button(Left/Right, down/up)`, `Wheel(dx,dy)` — the entire API surface the
kernel wiring consumes.

---

## 6. Wiring into akuma, and the consumer

### 6.1 Kernel modules (proposed)

| piece | where | what it is |
|---|---|---|
| DW-I²C mechanism | `amd64/src/i2c.rs` | MMIO (`writel`-shaped like the existing BAR mapping in `pci.rs`), the polled transaction of §2.3, `dwi2c=0x<pa>` cmdline override beside `ecram=` |
| DW-I²C decisions | `crates/akuma-dwi2c` (new, `forbid(unsafe_code)`) | FMCN/SSCN decode, hcnt/lcnt math, TX_ABRT source → error enum, the timeout/recovery decision — host-tested |
| i²c-hid transport | `crates/akuma-i2c-hid` (new, `forbid(unsafe_code)`) | descriptor structs + validation, §3.4 command encoder (byte-exact tests), report framing + dedup + quirk timing table — host-tested |
| HID report parse | `crates/akuma-hid` (new, `forbid(unsafe_code)`) | the `hid.c` port: items → locations, locate-by-usage, `get_data`; fixtures = real rdesc bytes — host-tested |
| clickpad policy | `crates/akuma-touchpad` (new, `forbid(unsafe_code)`) | §5 state machine over contacts+button → events — host-tested exhaustively (this is where the correctness lives) |
| glue + probe | `amd64/src/touchpad.rs` | probe sequence with the §3.3 delays (runs once, at boot, under the BKL), owns the `static` state, one `poll()` step, dmesg gates |
| poll cadence | the console pump that already calls `xhci::keyboard_poll` | one `touchpad::poll()` beside it: plain read → dedup (memcmp vs last) → policy → queue. Free-running at the pump's cadence; no IRQ, no timer of its own — the identical pattern that made the xHCI keyboard work |

That mirrors the tree's split exactly: mechanism in `amd64/` where the
`unsafe` lives, every decision in a host-tested `forbid` crate.

### 6.2 The event path to rio

`fd.rs` grows one device node beside `/dev/fb0`: **`/dev/input/mouse0`**, a
fixed 16-byte record per event — `{kind, buttons, dx, dy, wheel_dx, wheel_dy,
abs_x, abs_y, seq}` (`kind`: MOVE/BUTTON/WHEEL; O_NONBLOCK read, poll/epoll
readiness reusing the fd-layer machinery — the ptys slice proved the shape).
Not Linux evdev (pinned divergence, same as `ifreq`/`/proc/net/dev` were);
it is one `read(2)` loop, testable with `dd`/`od` from busybox before rio
cares.

The rio fork's fb platform (`rio-window/src/platform_impl/fb/event_loop.rs`)
reads the node each loop iteration and feeds winit's real pointer events —
`CursorMoved`, `MouseInput`, `MouseScrollDelta` — which the rest of rio
already speaks, plus a cursor sprite drawn on the fb surface (kernel renders
nothing; rio owns `/dev/fb0` exclusively when it runs).

Not chosen for v1, noted for later: synthesizing xterm SGR mouse sequences
onto the console tty gated by a `\e[?1000h`-style mode (that would give every
console TUI mouse without rio changes, and `kbd.rs`'s escape-sequence tables
are the precedent — but it couples the console tty and needs mode tracking;
the device node is the smaller first slice).

### 6.3 Bring-up gates (each its own boot; ssh is up via menu entry 12, so every gate is verifiable from the dev host)

- **M0** (Linux): constants recorded (§1.1). Gate: this table filled in.
- **M1 controller alive**: dmesg `IC_COMP_TYPE = 0x44570140`, FMCN applied.
  Failure = wrong base or clocks gated → mmiotrace what Linux does at probe.
- **M2 device alive**: `i2c-hid: desc 30B bcd 0x0100 vendor=xxxx product=xxxx`
  then rdesc length. Failure = power (§7 R1) or wrong address.
- **M3 reports**: debug tap prints raw reports while a finger moves; then the
  five located fields print as contacts.
- **M4 policy**: `/dev/input/mouse0` read from a shell shows MOVE / BUTTON
  (Left on a plain press, Right when pressed in the bottom-right band) /
  WHEEL under two fingers.
- **M5 rio**: pointer visible, clicks land in rio, scrollback scrolls. Gate:
  the same session driven via `/tmp/rio-expect.sh` plus a scripted touch, then
  by hand.

---

## 7. Risks

| | risk | mitigation |
|---|---|---|
| R1 | pad in D3 / reset-GPIO-gated at handoff (Goodix GT7375P-class pads need a real reset GPIO driven) | firmware POST usually leaves it on (GRUB reaches the panel, so devices are alive); if M2 NACKs, mmiotrace Linux's probe — `pinctrl-amd` writes will show the enable/reset GPIO; worst case a two-write GPIO hack in `touchpad.rs` with the measured address, like `ecram=` |
| R2 | wrong bus/address hardcoded (unit variance: Goodix vs Synaptics lot) | M0 measures *this* box; cmdline override; M2's vendor:product log catches drift |
| R3 | controller wedged by firmware state (AMD SMU owns BUS_CLEAR) | preserve `IC_CON` bit 11; on timeout re-init (Linux's no-GPIO recovery); SMU does deep recovery, we never fight it |
| R4 | latency: pump cadence bounds pointer smoothness | FreeBSD's measured answer: 80 Hz active / 10 Hz idle + dedup; the pump already runs per lap, raise only this consumer's share if it feels sluggish |
| R5 | bus shared with Wacom pen / touchscreen | we only ever address the touchpad's address; RESTART-only transactions; TX_ABRT on other addresses is impossible by construction |
| R6 | the other half of the ask — "click stuff" needs rio work regardless | rio fork change is small (§6.2) but real; until it lands, M4's shell-level gate still proves the kernel half |

What this deliberately is not: no evdev, no AML interpreter, no ACPI power
methods, no interrupts, no acceleration curves, no libinput port. Every one of
those has a cheap first slice waiting behind the same five layers.

---

## 8. Tests

- `akuma-dwi2c`: hcnt/lcnt math golden values (FM/SS, with and without FMCN),
  FMCN package decode, TX_ABRT source → error table.
- `akuma-i2c-hid`: command encoder byte-exact vs §3.4 (incl. report_id ≥ 15
  third-byte case), descriptor validation rejections, framing: reset-ack via
  zero-length, bogus `0xFFFF` clamp, dedup memcmp decision.
- `akuma-hid`: parser against pinned fixtures (a boot mouse descriptor and the
  box's real rdesc once M0/M2 capture it), locate/get_data vectors, semi-MT
  detection.
- `akuma-touchpad`: the state machine, exhaustively — soft-area membership and
  the wander rules, click left/right, two-finger scroll engage/threshold/end,
  click-ends-scroll, scroll-suppresses-motion, contact tracking across lifts.
- Metal: §6.3 gates; host tests + clippy per `CLAUDE.md` for every crate.

---

## 9. References

- HID over I²C spec v1.0 (Microsoft/Intel) — protocol of record; §refs above.
- Linux: `drivers/hid/i2c-hid/i2c-hid-core.c`, `i2c-hid-acpi.c`;
  `drivers/i2c/busses/i2c-designware-{core,common,master,platdrv}.c`;
  `include/linux/designware_i2c.h` — behavioral reference, GPL.
- FreeBSD `sys/dev/iicbus/iichid.c` — polling/sampling mode, reset-ack shape
  (BSD; directly consultable).
- OpenBSD `sys/dev/hid/hid.c` (parser prior art), `ihidev(4)`, `wstpad(4)`,
  `dwiic(4)` (BSD).
- libinput docs: "Clickpad software button behavior", "Scrolling";
  `src/evdev-mt-touchpad-buttons.c` — the gesture semantics of §5.
- linux-hardware.org probe `26d850d9b3` (same model 83DS) — vendor identity
  cross-check.
- In-tree: `AKUMA_AMD64_ON_RYZEN_LAPTOP.md` (machine), `amd64/src/xhci.rs`
  (polled input precedent), `amd64/src/input.rs`, `amd64/src/kbd.rs`,
  `amd64/src/power.rs` (`ecram=` override precedent),
  `AKUMA_AMD64_RIO_FBDEV_BUILD.md` (rio input path, known gaps),
  `AKUMA_AMD64_WGPU_KERNEL_WORK.md` (`/dev/fb0` slices).
