# Framebuffer console flicker: double buffer + synchronized output

Started 2026-11-10 on the ryzen (amd64 bare metal, UEFI GOP framebuffer, no GPU driver).
**Status: implemented; metal check pending the user's eyes** (flicker is a visual symptom — see
Verify).

## Symptom

The console flickered while Claude Code's TUI ran on it ("never seen before claude"), and once per
dim step (`powersaved`: 60 s, 180 s, 600 s of console idle).

## Cause

`amd64/src/multiboot2.rs`'s `Framebuffer` drew every pixel **straight into the scanned-out,
write-combining framebuffer** (`fill`/`put` on an `MmioWindow`); `flush()` was only an `sfence`.
So whatever order a program painted in was the order the panel showed:

- Claude Code repaints by erasing lines and rewriting them, wrapped in `CSI ? 2026 h … l`
  (synchronized output: "hold the picture until `l`"). The console ignored the mode, so every
  intermediate state was visible. The klog shows the frames (a 6 KB frame costs ~3 ms of drawing)
  and a spinner animation repainting continuously.
- `Console::repaint()` (every dim step, and unmute after `/dev/fb0`) fills the whole screen with the
  background and then redraws every cell: a full-screen flash.

## Fix

- `Framebuffer` gets a `Back` buffer (cached RAM, one `u32` per pixel) plus a dirty span per row.
  `fill`/`put` write there; **`flush()` is the present**: each dirty span is copied to the scanout
  with one `rep movsd` (`MmioWindow::copy32_from`, the copy twin of `fill32`), then `sfence`.
  Enabled by `fb_enable_double_buffer()` after the heap exists (console init runs before it), then a
  `repaint()` so the copy matches the glass. Fallible allocation: on failure, or for a mode that is
  not 32 bpp on a dword pitch, drawing stays direct (`[fb] double buffer on|off` in `dmesg`).
- `CSI ? 2026 h` / `l` is parsed (`Console::sync_update`). While set, `mirror_byte` does not present
  on newline; `l` presents. A frame whose `l` never arrives is presented by `cursor_idle` after
  150 ms, so a crashed program cannot freeze the screen.
- Crash dumps present every byte (`fb_force_unmute` sets `CRASHED`): nothing calls `cursor_idle`
  after a panic and the dump may be the last thing drawn.
- `repaint()` now draws into the back buffer and presents once, so dimming no longer flashes.

Costs: one screen of RAM (width x pitch: ~9 MB at 1920x1200, ~21 MB at 2880x1800 — the heap is 1 GiB
on the ryzen); one extra memory copy of the changed spans per present. There is still no vsync (no
GPU driver), so a present can tear for a few milliseconds instead of a frame being visibly painted
in pieces. The scanout is never read.

## Tests

`crates/akuma-fbcon/tests/sync_update.rs` (mode set/clear, mode lists, text inside a frame still
reaches the grid). The `Back` logic lives in the amd64 binary and is exercised on the machine.

## Verify

Boot the kernel, `dmesg | grep 'double buffer'` says `on`; run `claude` on the console and watch
the spinner and a long answer; idle past 60 s and watch the dim step. No flicker, text correct,
and `/dev/fb0` users (kami) still draw and hand the screen back.

## Related

- [`CLAUDE_COMPATIBILITY_ISSUES.md`](CLAUDE_COMPATIBILITY_ISSUES.md), the firmware battery splash
  that leaves the panel black (a separate display problem: the panel stops scanning our buffer).
