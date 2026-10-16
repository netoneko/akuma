# Touchpad and pointer (amd64): `/dev/input/mouse0` and the console bridge

Stability grade: **B** — metal-verified on the ryzen laptop (IdeaPad 16AHP9,
Goodix `GXTP5100`) on 2026-10-16: bring-up, one- and two-finger contacts,
click, right-click, two-finger scroll (natural direction by default), and the
pointer in herdr on the framebuffer console (relative motion, click and
scroll). Only that one machine has been seen; every other box skips all of it.
The run predates tap-to-click: the tap limits below are host-tested
(`crates/akuma-touchpad` tests), not yet metal-verified.

The pad is an I²C-HID device behind an AMD DesignWare I²C controller. There is
no interrupt line in use: it is **polled**.

## What runs where

| piece | where | job |
|---|---|---|
| `akuma-dwi2c` | `crates/` | controller register map, timings, abort decode, the AOAC wake (`aoac`) |
| `akuma-i2c-hid` | `crates/` | HID-over-I²C framing: descriptor, commands, input-report length word |
| `akuma-hid` | `crates/` | report-descriptor parser (OpenBSD `hid.c` port) |
| `akuma-touchpad` | `crates/` | `ptp` (report layout + id-keyed finger slots + wire format + control lines), `Clickpad` policy (click, right-click, scroll, natural direction, tap-to-click, `Lift`), `tty` (pointer events → xterm mouse reports) |
| `amd64/src/i2c.rs` | kernel | AOAC wake, MMIO mapping, polled transactions — every wait is TSC-bounded |
| `amd64/src/touchpad.rs` | kernel | discovery, bring-up, polling, `/dev/input/mouse0`, the console bridge |
| `akuma-fbcon` | `crates/` | records the mouse modes a program sets; draws the pointer cell |

All four `crates/` pieces forbid `unsafe`. Decisions live there and are host
tested; the kernel files are sequencing and state.

## Discovery and bring-up

The pad is only brought up when the DSDT/SSDT names `GXTP5100`, or the kernel
command line says `touchpad=0x<mmio>:0x<addr>` (any other machine). Otherwise
`dmesg` shows one line, `[touchpad] not found (…)`.

Bring-up log, in order (a good boot ends at `up:`):

```
[touchpad] GXTP5100 @0x5d on dw_i2c 0xfedc2000
[touchpad] dw_i2c v=… fs=99:214 (kept) fifo=256/256
[touchpad] hid 27c6:01e0 rdesc=665 maxin=32
[touchpad] ptp report 4 contacts=5 range=3751x2327 -> 1920x1200
[touchpad] up: /dev/input/mouse0
```

A runtime-suspended `AMDI0010` reads `0xFFFFFFFF` everywhere; `i2c::wake` runs
the DSDT's `_PS0` (a plain MMIO sequence on the AOAC block) first. After
`RESET` the driver reads the report descriptor and puts the pad in Precision
Touchpad mode with **one `SET_REPORT` per feature report**: Surface Switch and
Button Switch share a report and `SET_REPORT` replaces all of it, so two writes
leave the pad reporting the button but no contacts (the first-metal-run bug).

## Polling

The console pump (`console::pump_once`) calls `touchpad::poll()` every lap:
16 ms apart, 8 ms while a finger is down or within 2 s of one. The pad holds
only its **latest** report, so slow idle polling sees a tap's lift-off frame
and nothing else. A NACK on a poll means "nothing to report" and is not a
failure; eight consecutive real failures disable the pad. The GPIO interrupt
(pin 8) would remove the polling and is not used.

## `/dev/input/mouse0`

Absent (`ENOENT`) unless the pad came up. Reads return whole 8-byte records or
`EAGAIN` — never block. No `poll`/`epoll` readiness yet: readers loop.

| byte 0 | meaning | other bytes |
|---|---|---|
| 1 | move | x, y as u16 LE at 4–7, panel pixels (absolute) |
| 2 | button | byte 1: 0 left, 1 right, 2 middle; byte 2: down |
| 3 | wheel | dx, dy as i16 LE at 4–7; `+dy` = up |
| 4 | lift | the pointer stroke ended; the next move starts a new one |

Writes take control lines (all parsed before any applies, `/dev/power`'s rule):

| line | effect |
|---|---|
| `natural` | toggle natural scrolling |
| `natural=1` / `natural=0` (also `on off true false yes no`, `natural 1`) | set it |
| `tap` | toggle tap-to-click |
| `tap=1` / `tap=0` (same value spellings) | set it |

```
echo natural=0 > /dev/input/mouse0     # traditional wheel direction
echo natural=1 > /dev/input/mouse0     # MacBook direction (the default)
echo tap=0 > /dev/input/mouse0         # only the physical button clicks
echo tap=1 > /dev/input/mouse0         # tap-to-click (the default)
```

Kernel command line: `touchpad.natural=0` makes traditional the boot default,
`touchpad.tap=0` disables tap-to-click at boot. **Natural** means the content
follows the fingers: two fingers moving up scroll the view down (negative
wheel).

## Tap-to-click

On by default, as on a MacBook: a quick, still one-finger touch is a left
click (press + release) without pressing the pad down, and two quick taps are
the two click pairs a consumer calls a double click. There is deliberately no
double-tap state machine — each tap fires its own pair as it lifts, so no tap
ever waits to find out whether it was alone.

The limits are libinput's: the touch must end within 200 ms of landing
(`TAP_MAX_US`, against the kernel's uptime µs clock) and stay within 3 % of
each sensor axis span (`TAP_SLOP_PCT`) of where it landed — beyond either, the
touch was a rest or a drag, and lifting must not click. The tap is cancelled
the moment the gesture becomes something else: the physical button comes down
(a click-and-drag), a second finger lands (scroll arming), or a scroll
engages. The click pair is queued *after* the stroke's `Lift`, so a consumer
sees the stroke end before the click. A two-finger tap stays a no-op.

## The console bridge: the pointer inside terminal programs

`touchpad::console_bridge`, called by the console pump, types **xterm mouse
reports** into the console's input queue beside the keyboard — only while the
program on the console has turned reporting on:

- `CSI ? 1000 h` presses and wheel, `1002` plus drag motion, `1003` all motion;
  `1006` selects SGR reports (`ESC [ < b ; col ; row M|m`), otherwise the legacy
  form (coordinates stop at 223). `akuma-fbcon` records the modes
  (`Console::mouse_mode`); the console itself has no mouse.
- The pointer moves **relatively**: the first move of a stroke only anchors,
  later moves add the finger's delta at 3/2 gain, clamped to the text grid; it
  starts in the middle of the screen. (`Move` is an absolute pad position; an
  absolute pointer follows wherever a finger rests, which put it on herdr's
  right pane during two-finger scrolls.)
- The pointer's cell is drawn inverted (`Console::set_pointer`) when output goes
  idle and taken down by the next write, like the text cursor.
- When reporting is off the events stay for `/dev/input/mouse0` readers, and
  the first enable discards what piled up.

## Symptoms

| you see | read |
|---|---|
| no `[touchpad]` line | the DSDT scan did not find the pad; `touchpad=0xfedc2000:0x5d` |
| `… would not power up (AOAC)` | the `_PS0` wake failed; AOAC bytes `0xFED81E4A/4B`, awake = `0x7C/0x27` |
| `up:` but no records, `reports=0` in `polls=` | pad never left its idle mode: check the one-write-per-report path in `enable_ptp_mode` |
| records but no fingers, button only | the Surface/Button Switch overwrite again |
| taps missed, drags fine | idle poll period too long |
| taps move the pointer but never click | `echo tap=1 > /dev/input/mouse0`, or drop `touchpad.tap=0` from the cmdline |
| pointer jumps to where fingers rest | something is using `Move` as absolute: use the relative bridge / honour `Lift` |
| `echo natural=… > mouse0` says "No such device" | the pad is not up |

`dmesg | grep touchpad` also carries raw dumps (`empty read:`, `frame:`, a
`polls= nack= empty= reports= other= down=` summary per 4096 polls) kept on
purpose for first-boot diagnosis on new hardware.

## Verify

1. `dmesg | grep touchpad` ends in `up: /dev/input/mouse0`.
2. On the machine's own console (not ssh — the bridge feeds the console tty):
   `printf '\033[?1000h\033[?1006h'; cat -v`. Dragging moves a marked cell;
   a click types `^[[<0;C;RM` then `…m` — and a light tap (no press) must
   type the same pair; two fingers up type `^[[<65;…M`
   (natural) / `^[[<64;…M` (traditional). Ctrl-C, then
   `printf '\033[?1000l\033[?1006l'`.
3. Run `herdr` on the console: click panes, scroll history.
4. herdr's scrollback itself is checked with
   `scripts/herdr_scrollback.py [--cat PATH]` over an ssh PTY (SGR wheel
   reports; needs `pyte`), independent of the pad.

## Background

- [`archive/AKUMA_AMD64_TOUCHPAD_I2C_HID.md`](../../archive/AKUMA_AMD64_TOUCHPAD_I2C_HID.md) — research, the M0
  measurements, the build-up and the first-metal-run findings.
- [`proposals/AMD64_TOUCHPAD_I2C_HID.md`](../../../proposals/AMD64_TOUCHPAD_I2C_HID.md) — the original design.
- `overlays/ryzen/README.md` — the ryzen test loop (entry 12, ssh over wifi).
