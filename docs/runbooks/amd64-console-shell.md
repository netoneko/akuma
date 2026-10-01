# A shell on the trashcan's TV — `/bin/sh` on the console, no ssh

**Stability: B.** Verified on the real box 2026-10-01: the USB keyboard types into
the shell on the TV (the boot log shows the real key reports decoding). `^C` and
`exit` from the *physical* keyboard are covered by QEMU only. See
[Verify](#verify), which says which claim each check settles.

The HP box's only console is the framebuffer GRUB hands the kernel, and its only
local input is a USB keyboard that **the kernel drives natively over xHCI**
(`amd64/src/xhci.rs`). Since 2026-10-01 a login shell runs on that console
beside `sshd`.

> **Do not rely on the firmware's PS/2 emulation (`kbd.rs`).** It was the first
> design and it does not work on this box: the emulation belongs to the
> firmware's USB stack and ends at the xHCI bring-up's `BIOS handoff` + `HCRST`
> (the keyboard's lights go out there), and the i8042 status byte then sits at a
> constant `0x7c` with no data. See "The keyboard" below.

## What is running

`init=/bin/herd` supervises services, and **herd's services get pipes, not the
console** — so for the first month of this boot path nothing read the keyboard
and the TV showed the boot log and then nothing. The fix is one more service:

```
/etc/herd/enabled/console.conf
    command = /bin/sh
    console = true
    restart = true
    restart_delay = 1000
    env = TERM=xterm-256color
    env = ENV=/etc/console.rc
```

| piece | where | what it does |
|---|---|---|
| `console = true` | `userspace/herd/src/main.rs` | spawns the service with `SPAWN_FLAG_CONSOLE` and a full environment (`PATH`, `HOME`, `TERM`) |
| `SPAWN_FLAG_CONSOLE` (2) | `amd64/src/usermode.rs` `sys_spawn` | the child gets **no** channel of its own, so `register_exec_process` attaches it to the console channel and the shared `TerminalState` — exactly what `init` gets. Refused (`EPERM`) unless the **caller** is itself console-attached, so an ssh session cannot take the keyboard |
| the pump | `amd64/src/console.rs` | keyboard -> line discipline (`^C` -> `SIGINT`) -> the shell; the shell's echo -> screen |
| native USB keyboard | `amd64/src/xhci.rs` `init_keyboard` / `keyboard_poll` | a **second xHCI slot** beside the disk's: enumerate the boot-keyboard interface, `SET_CONFIGURATION`, Configure Endpoint (interrupt IN), `SET_PROTOCOL(boot)`, keep one 8-byte report in flight; decode with `akuma-usb`'s `BootKeyboardDecoder` into a key FIFO that `input::getb` drains. Reports are taken in `Xhci::next_event`, the one place events are consumed, so a disk read cannot swallow one |
| Backspace = `0x7f` | `amd64/src/kbd.rs`, `akuma-usb` keymap | was `0x08`, which cooked-mode `read(0)` does not treat as erase |
| arrows + key repeat | `crates/akuma-usb` keymap/decoder, `xhci::keyboard_poll` | navigation keys emit Linux-console escape sequences; a USB boot keyboard never repeats a held key, so the decoder remembers the held key and the pump re-emits it after 400 ms every 35 ms. Backspace is `0x7f` here too (the USB keymap still said `0x08` until 2026-10-01) |
| quiet framebuffer | `amd64/src/serial.rs` `set_fb_quiet` | once the console shell is spawned the TV shows the shell and its echo only; `[probe]`/`[herd]`/`[BKL]`/`[PSTATS]` stay in `dmesg`; a panic or fatal exception reopens it; boot flag `fbverbose` disables it. Console traffic uses `putb_tty`; pid 1's (`herd`'s) writes do not |
| ANSI subset | `crates/akuma-fbcon/src/console.rs` | busybox's line editor emits `\b`, `ESC[nD`, `ESC[J`, `ESC[K`; they used to be drawn as glyphs |
| cursor | same, + `multiboot2::cursor_idle` | a block, drawn when output goes quiet, removed by the next byte |

It restarts on exit, so `exit` gives a fresh prompt instead of a dead screen.
The shell's output is on the screen and **not** in `/var/log/herd/console.log`
(herd's drain reads the exit-status handle, which carries nothing).

## Turn it on or off

```sh
ssh akuma 'cp /etc/herd/available/console.conf /etc/herd/enabled/'   # herd reloads within 20 s
ssh akuma 'rm /etc/herd/enabled/console.conf; herd stop console'
```

`mkdisk.sh` stages and enables it on a fresh image. On the live partition it is a
file you write; **install the kernel and `/bin/herd` first** — an old herd ignores
`console =` and would run an ordinary piped `/bin/sh` that reads nothing.

## Test it off the metal

```sh
python3 scripts/utils/amd64_console_probe.py                  # INIT=/bin/herd (default)
INIT=/bin/busybox python3 scripts/utils/amd64_console_probe.py   # control: init is the shell
```

It boots `amd64/run.sh`, types through the 16550, and requires: a command's
output, `$0` naming the shell, a pipeline, **line editing with DEL**, and **`^C`
killing a `sleep 100` with the shell surviving**. Each marker is something the
typed line does not contain, so a reflected echo cannot pass. Run the **control**
once when you change the harness: a probe that has never been seen to pass proves
nothing about a silent boot.

Three rigs, each proving a different link:

| rig | command | proves | does not prove |
|---|---|---|---|
| serial | `amd64_console_probe.py` | spawn attach, pump, line discipline, shell, echo, the TV quiet policy (via `fbtrace`) | any keyboard, any pixel |
| emulated i8042 | `amd64_console_probe.py --kbd` | `kbd.rs` decode (needs the controller's translation on) | firmware emulation |
| **USB** | `amd64_console_probe.py --usb` | the xHCI keyboard driver end to end: `q35` + `qemu-xhci` + USB disk (root) + `usb-kbd`, typed with `sendkey` — the same boot shape as the box | the real keyboard's descriptors, the firmware handoff, the real screen |

The framebuffer console's parsing is host-tested (`cargo test -p akuma-fbcon`,
which renders into memory and checks pixels).

## What the TV console can draw (2026-10-01)

`crates/akuma-fbcon` is a small terminal emulator, enough to run a TUI such as
late.sh over `ssh` from the console shell. Before this the console was an ASCII-only
teletype: every byte of a multi-byte character was drawn as its own box, and
anything beyond cursor-left/erase was drawn as text.

| area | what it does |
|---|---|
| UTF-8 | decoded (overlong, surrogate and truncated sequences show one box each); characters are one, two (CJK, emoji) or zero (combining, joiners) columns wide, so a line with an emoji stays aligned |
| fonts | IBM Plex Mono and Spleen baked for printable ASCII, Latin-1, Latin Extended-A (Polish, Czech...), punctuation, a few symbols — `build.rs` `RANGES` |
| drawn, not fonted | box drawing `U+2500..257F`, block elements `2580..259F` and Braille `2800..28FF` are painted per cell from the cell size, so frames close at any scale; arrows, `● ○ ■ □ ▲ ▶ ▼ ◀ ◆ ✓ ✗` likewise (`glyph.rs`) |
| colour | SGR: 16, 256-colour and truecolor (stored as the nearest palette entry), bold, dim, underline, reverse; erase keeps the background |
| editing | cursor addressing, erase line/screen/characters, insert/delete characters and lines, scroll regions, scroll up/down, repeat, save/restore cursor, the alternate screen (cleared on entry and exit — the old screen is not kept) |
| replies | cursor position (`CSI 6 n`), device attributes (`CSI c`), foreground/background colour queries — typed back into the program by the console pump, so busybox no longer waits at every prompt and a TUI learns the colours |
| not drawn | colour emoji (a two-column outlined box), Cyrillic/Greek/CJK glyphs (width is right, the glyph is a box), combining accents |

**Crates.** Two crates do the parts that should not be hand-written (adopted
2026-10-01, replacing a first hand-rolled parser and width table):

- **`vte` 0.15** — the escape-sequence parser (Alacritty's): UTF-8 decoding and every
  CSI/OSC/ESC/DCS state. Built with `default-features = false`, which makes it
  `no_std` and allocation-free; its default `std` feature would pull `memchr/std`,
  and `std` does not exist for the kernel's `x86_64-unknown-none` (so `vte` 0.14,
  whose `memchr` is not optional, **cannot** be used here).
- **`unicode-width` 0.2** — 0/1/2 column widths from the Unicode tables.

Their licences (`Apache-2.0 OR MIT`, `MIT OR Apache-2.0`, and the `arrayvec` and
`memchr` they pull in) are permissive and compatible with this repository's
BSD-2-Clause; they are used under MIT and their notices are in
[`crates/akuma-fbcon/THIRD_PARTY_LICENSES.md`](../../crates/akuma-fbcon/THIRD_PARTY_LICENSES.md)
(regenerate it when a version changes). **Cost to know about:** the box builds
offline from a primed cargo cache, so a kernel build on the box needs these four
crates fetched into `/root/.cargo` first (`cargo fetch` from `/src/.../akuma`, from
Ubuntu or with network). `cargo build` prints a harmless future-incompatibility note
for `memchr` on the nightly toolchain (both 2.8.0 and 2.8.3). No crate provides a
no-alloc *screen model* — `vt100`, `alacritty_terminal` and `termwiz` all need
`std`/`alloc` — so the grid, colours, scrolling and drawing are `akuma-fbcon`'s own.

## Screen size: what `stty size` says, and what you can set

The framebuffer's grid is fixed at boot: the console picks a font and an integer
scale so the screen holds about 48 text rows, then insets every edge by 1/24 of
the screen (overscan). Before 2026-10-01 the console's terminal state said **24x80
whatever the grid was**, so `stty size`, `ls`'s columns and the line editor's
wrapping described a terminal that was not on the glass. Now:

- `run_init` copies the real grid into the console's terminal state, so
  **`stty size`** prints the framebuffer's `rows cols`.
- A **`[fb]` line** is printed at the end of the boot log (and so is in `dmesg`):
  `[fb] 1920x1080 pitch 7680 (= width*bpp) bpp 32 cell 12x24 grid 150x42 margin 80,45`.
  A pitch that is `NOT width*bpp` explains sheared lines; a grid taller than the
  visible area explains a blank band.
- **`stty cols N rows M` sets the printing area** (it used to be accepted and
  dropped). Text wraps and scrolls inside the top-left N x M cells, **nothing is
  drawn outside it** (the rest is blanked and stays blank), and programs lay out for
  it — so late.sh can run in the left part of the screen with the rest left alone.
  Values larger than the screen are clamped and `stty size` reports what is in
  force. Put it in `/etc/console.rc` (sourced by the console shell via `$ENV`) to
  make it stick per shell, or use `/etc/console.conf` (below) for the boot itself. Reset with the full size from `[fb]`.
  The anchor is the top left; a window elsewhere is not supported.

### Per-machine setup: `/etc/console.conf` (read at boot)

```text
# /etc/console.conf on the trashcan: the left half of the screen, no dead border.
margin = 0
cols = 50%
```

| key | meaning | forms |
|---|---|---|
| `margin` | pixels between the text and the screen edge (the default is about 0.8 %) | `0`, `24`, `24,12` |
| `cols` / `rows` | the printing area, anchored top-left | `73`, `50%` |

Read by `run_init` after the root filesystem mounts and before the console's size is
reported, so `stty size`, the shell and everything it runs see the area in force; the
boot log's last line says what happened (`[fb] ... margin 0,0 view 78x41`). A
percentage is of what the screen holds *after* the margin. Unknown keys, bad values
and comments are ignored — a typo costs a setting, never a boot. **It is a per-machine
file**: `mkdisk.sh` does not stage one, so QEMU images keep the full screen; edit it on
the box (`vi /etc/console.conf`, then `reboot`). A margin change clears the screen.

Live, without a reboot: `stty cols 73 rows 41` (area), or `printf '\033[?9001;0;0h'`
(margin in pixels; `\033[?9001l` restores the default — it changes the grid, and
`stty size` follows within a lap).

## The keyboard

**Where it must be plugged in.** The driver sees only a keyboard on a port the
**xHCI** controller owns. In Akuma's boot the front keyboard port is
`[xhci] port 8 USB2` (full speed); a port the firmware leaves on an EHCI
controller keeps the keyboard lit but is invisible to this driver. The test is
the log, not the lights: `dmesg | grep xhci` must show `[xhci] keyboard up on
port N`.

```
[xhci] port 8 USB2 connected PORTSC=0x000206e1 PLS=7 not-enabled   <- before reset
[xhci] .. find USB keyboard
[xhci] kbd slot 2 port 8 speed 1
[xhci] kbd interface 0 ep 0x81 mps 64 bInterval 1
[xhci] keyboard up on port 8
[xhci] kbd report len=8 mod=0x00 keys=0b 00 00 00 00 00   <- first 24 reports, per key edge
```

| what the log says | what it means |
|---|---|
| no `[xhci] port N USB2 connected` besides the disk | the keyboard is on a port the xHCI does not own — move it |
| `no USB keyboard: …` after `[init]` | enumeration failed; the line names the step (`Address Device`, `Configure Endpoint`, `no HID boot keyboard interface`) |
| `kbd control cc=…` | a control transfer was refused; `cc` is the xHCI completion code |
| `kbd interrupt-IN cc=…`, then `giving up` | the endpoint halted and 4 reset attempts did not bring it back |
| `keyboard up` but no `kbd report` lines while you type | the endpoint is armed and the keyboard is silent: wrong interface (a gaming keyboard exposes several), or it ignored `SET_PROTOCOL(boot)` and sends a non-boot layout |

**Why this is not `kbd.rs`.** Measured 2026-10-01 on the box: the lights went out
"after a brief moment on boot" (the BIOS handoff + `HCRST`), and with the
keyboard on a port the firmware kept, the i8042 status byte was a constant `0x7c`
over minutes of typing (`[kbd] polls=… st=0x7c or=0x7c scancodes=0`). QEMU's
emulated i8042 decodes correctly (and showed that `kbd.rs` assumed translated
set-1 scancodes; a raw set-2 controller needs the translation bit set). The
heartbeat and the first-48-scancodes log in `kbd.rs` stay, so "is the emulation
there?" is a `dmesg` away.

## Verify

On the **TV**, after a boot (an ssh answer says nothing about this):

0. **Keyboard port** — `ssh akuma 'dmesg' | grep xhci` shows `keyboard up on port N` (and the keyboard's lights are back on after the boot-time dip). *(Settles: the driver found and configured the keyboard.)*
1. **A prompt** — `/ #` appears after the `[herd] Started console` line, with a
   solid block cursor after it. *(Settles: the service started and the screen shows it.)*
2. **Keys** — type `echo hello` and Enter: the characters appear as you type and
   `hello` follows. *(Settles: the i8042 emulation delivers keystrokes through the
   pump. If nothing appears, the gap is input, not the shell — ssh in and run
   `herd status`; the shell being up while the keys are dead points at `kbd.rs`.)*
3. **Erase** — type `abc`, press Backspace twice: only `a` remains, no `^H`, no
   `[D` debris.
4. **`^C`** — `sleep 100`, then Ctrl-C: the prompt returns at once.
5. **Restart** — `exit`: a new prompt within ~1–2 s.
6. **ssh still works** — `ssh akuma 'echo ok'`, and an interactive ssh session is
   unaffected by what you typed on the TV.

To tell a *false* pass from a real one: the screen **is** the evidence for 1–5;
`dmesg` only shows what the kernel believes. For 2, a prompt you did not type
into proves nothing — look at your own keystrokes appearing.

If the box does not come back: pick **`Akuma/amd64 (known good)`** at the GRUB
menu (10 s). `/boot/akuma-amd64.prev` holds the kernel that was installed before
this one.

## Known limits

- No function keys (F1..F12) and no modified cursor keys (Ctrl-Left etc.).
  Arrows, Home/End, Insert/Delete and PageUp/PageDown work (2026-10-01) as the
  Linux-console escape sequences, so ash has history (Up/Down) and mid-line editing.
- A keyboard on an EHCI-routed port is not driven (no EHCI controller driver;
  the split-transaction builders exist in `akuma-usb::ehci`, unwired).
- No hot-plug: unplugging halts the interrupt endpoint (four recovery attempts,
  then it gives up); replug needs a reboot.
- The key FIFO is 64 bytes and drops on overflow, like a tty input queue.
- (Cursor-position queries are answered now — see "What the TV console can draw".)
- One console shell. Two programs reading the console share one input queue.

## Background

- [`../archive/AKUMA_TEAHOUSE_CONSOLE_EXPERIMENT.md`](../archive/AKUMA_TEAHOUSE_CONSOLE_EXPERIMENT.md) — the experiment, the paused swarm attempt, and the 2026-10-01 log of this work.
- [`amd64-bare-metal-loop.md`](amd64-bare-metal-loop.md) — the box, its two personalities and the GRUB entries; the `tail -f ignores ^C` row there is the shared-`TerminalState` bug the console-attachment rule comes from.
