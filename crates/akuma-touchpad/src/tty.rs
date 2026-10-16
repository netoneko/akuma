//! Pointer events -> xterm mouse reports, for a program on the console tty.
//!
//! A terminal program (herdr, vim, a pager) turns mouse reporting on with
//! `CSI ? 1000|1002|1003 h` (what to report) and `CSI ? 1006 h` (SGR encoding),
//! and then expects the *terminal* to type the reports in. The console has no
//! mouse of its own; this is the translation from [`PointerEvent`]s (panel
//! pixels, buttons, wheel ticks) to the bytes such a program reads. Pure
//! arithmetic over a caller-supplied buffer: no allocation, host-tested.
//!
//! | event | report |
//! |---|---|
//! | button down / up | `ESC [ < b ; col ; row M` / `m` (b = 0 left, 1 middle, 2 right) |
//! | wheel | b = 64 up, 65 down, 66 left, 67 right; one report per tick, `M` |
//! | move, button held, mode >= 1002 | b = 32 + button |
//! | move, no button, mode 1003 | b = 35 |
//!
//! Moves are reported only when the pointer enters a different *cell*; a
//! program cannot see finer than that and a 125 Hz pad would otherwise flood
//! its input queue. Without SGR (`1006`) the legacy `ESC [ M` form is used,
//! whose coordinates stop at 223.

use crate::{Button, PointerEvent};

/// How much the program asked to hear about (the highest of 1000/1002/1003).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Tracking {
    /// Mouse reporting off: report nothing.
    #[default]
    Off,
    /// 1000: presses, releases and the wheel.
    Press,
    /// 1002: plus motion while a button is held.
    Drag,
    /// 1003: plus all motion.
    Any,
}

/// The console's text grid in panel pixels, for pixel -> cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grid {
    pub origin_x: u32,
    pub origin_y: u32,
    pub cell_w: u32,
    pub cell_h: u32,
    pub cols: u32,
    pub rows: u32,
}

impl Grid {
    /// 1-based `(col, row)` of the cell under pixel `(x, y)`, clamped to the
    /// grid. A degenerate grid (zero cell or size) answers `(1, 1)`.
    #[must_use]
    pub fn cell(&self, x: u32, y: u32) -> (u32, u32) {
        if self.cell_w == 0 || self.cell_h == 0 || self.cols == 0 || self.rows == 0 {
            return (1, 1);
        }
        let col = x.saturating_sub(self.origin_x) / self.cell_w;
        let row = y.saturating_sub(self.origin_y) / self.cell_h;
        (col.min(self.cols - 1) + 1, row.min(self.rows - 1) + 1)
    }
}

/// Longest single report: `ESC [ < 35 ; 65535 ; 65535 M` is 17 bytes.
pub const MAX_REPORT: usize = 20;

const HELD_LEFT: u8 = 1;
const HELD_MIDDLE: u8 = 2;
const HELD_RIGHT: u8 = 4;
/// Wheel ticks emitted per event, however large `dy`: a fling must not queue
/// hundreds of reports ahead of the next keystroke.
const MAX_TICKS: i32 = 8;

/// Translation state: which buttons are held and the cell last reported.
#[derive(Clone, Copy, Debug, Default)]
pub struct MouseEncoder {
    held: u8,
    pos: (u32, u32),
    reported: (u32, u32),
}

impl MouseEncoder {
    #[must_use]
    pub const fn new() -> Self {
        Self { held: 0, pos: (1, 1), reported: (0, 0) }
    }

    /// The cell the pointer is in (1-based), for drawing a marker.
    #[must_use]
    pub const fn cell(&self) -> (u32, u32) {
        self.pos
    }

    /// Forget held buttons — the program turned reporting off or exited, so a
    /// later release must not be reported for a press it never saw.
    pub fn reset(&mut self) {
        self.held = 0;
        self.reported = (0, 0);
    }

    fn button_code(b: Button) -> u8 {
        match b {
            Button::Left => 0,
            Button::Middle => 1,
            Button::Right => 2,
        }
    }

    fn held_code(&self) -> Option<u8> {
        if self.held & HELD_LEFT != 0 {
            Some(0)
        } else if self.held & HELD_MIDDLE != 0 {
            Some(1)
        } else if self.held & HELD_RIGHT != 0 {
            Some(2)
        } else {
            None
        }
    }

    /// Translate `ev`, appending zero or more reports to `out`; returns the
    /// bytes written. `out` must have room for [`MAX_REPORT`] bytes per report
    /// (a wheel event can write up to [`MAX_TICKS`]); what does not fit is
    /// dropped whole, never cut mid-sequence.
    pub fn feed(&mut self, ev: &PointerEvent, mode: Tracking, sgr: bool, grid: &Grid, out: &mut [u8]) -> usize {
        let mut n = 0;
        match *ev {
            PointerEvent::Move { x, y } => {
                let cell = grid.cell(x, y);
                self.pos = cell;
                if mode == Tracking::Off || cell == self.reported {
                    return 0;
                }
                let held = self.held_code();
                let b = match (mode, held) {
                    (Tracking::Any, None) => Some(35),
                    (Tracking::Drag | Tracking::Any, Some(h)) => Some(32 + h),
                    _ => None,
                };
                if let Some(b) = b {
                    self.reported = cell;
                    n += report(&mut out[n..], b, cell, true, sgr);
                }
            }
            PointerEvent::Button { button, down } => {
                if mode == Tracking::Off {
                    return 0;
                }
                let bit = match button {
                    Button::Left => HELD_LEFT,
                    Button::Middle => HELD_MIDDLE,
                    Button::Right => HELD_RIGHT,
                };
                if down {
                    self.held |= bit;
                } else if self.held & bit == 0 {
                    return 0; // a release for a press the program never saw
                } else {
                    self.held &= !bit;
                }
                self.reported = self.pos;
                n += report(&mut out[n..], Self::button_code(button), self.pos, down, sgr);
            }
            PointerEvent::Wheel { dx, dy } => {
                if mode == Tracking::Off {
                    return 0;
                }
                // REL_WHEEL: +dy = up (64), -dy = down (65); +dx = right (67),
                // -dx = left (66).
                for (d, positive, negative) in [(dy, 64u8, 65u8), (dx, 67, 66)] {
                    let (b, ticks) = if d > 0 { (positive, d) } else { (negative, d.saturating_neg()) };
                    for _ in 0..ticks.clamp(0, MAX_TICKS) {
                        n += report(&mut out[n..], b, self.pos, true, sgr);
                    }
                }
            }
        }
        n
    }
}

/// One report; 0 if it does not fit.
fn report(out: &mut [u8], b: u8, (col, row): (u32, u32), press: bool, sgr: bool) -> usize {
    if out.len() < MAX_REPORT {
        return 0;
    }
    if sgr {
        let mut n = 0;
        for &c in b"\x1b[<" {
            out[n] = c;
            n += 1;
        }
        n += dec(&mut out[n..], u32::from(b));
        out[n] = b';';
        n += 1;
        n += dec(&mut out[n..], col);
        out[n] = b';';
        n += 1;
        n += dec(&mut out[n..], row);
        out[n] = if press { b'M' } else { b'm' };
        n + 1
    } else {
        // Legacy: release carries button 3; every field is offset by 32 and
        // stops at 255, i.e. coordinate 223.
        let code = if press { b } else { 3 };
        out[0] = 0x1b;
        out[1] = b'[';
        out[2] = b'M';
        out[3] = 32 + code;
        out[4] = 32 + col.min(223) as u8;
        out[5] = 32 + row.min(223) as u8;
        6
    }
}

fn dec(out: &mut [u8], mut v: u32) -> usize {
    let mut tmp = [0u8; 10];
    let mut i = tmp.len();
    loop {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    let len = tmp.len() - i;
    out[..len].copy_from_slice(&tmp[i..]);
    len
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::vec::Vec;

    use super::*;

    const GRID: Grid = Grid { origin_x: 8, origin_y: 4, cell_w: 11, cell_h: 22, cols: 100, rows: 50 };

    fn run(e: &mut MouseEncoder, ev: PointerEvent, mode: Tracking, sgr: bool) -> Vec<u8> {
        let mut buf = [0u8; 200];
        let n = e.feed(&ev, mode, sgr, &GRID, &mut buf);
        buf[..n].to_vec()
    }
    fn mv(x: u32, y: u32) -> PointerEvent {
        PointerEvent::Move { x, y }
    }
    fn btn(button: Button, down: bool) -> PointerEvent {
        PointerEvent::Button { button, down }
    }

    #[test]
    fn pixel_to_cell_is_one_based_and_clamped() {
        assert_eq!(GRID.cell(0, 0), (1, 1)); // inside the margin
        assert_eq!(GRID.cell(8, 4), (1, 1));
        assert_eq!(GRID.cell(8 + 11, 4 + 22), (2, 2));
        assert_eq!(GRID.cell(99_999, 99_999), (100, 50));
        let dead = Grid { cell_w: 0, ..GRID };
        assert_eq!(dead.cell(500, 500), (1, 1));
    }

    #[test]
    fn off_reports_nothing_at_all() {
        let mut e = MouseEncoder::new();
        assert!(run(&mut e, mv(300, 300), Tracking::Off, true).is_empty());
        assert!(run(&mut e, btn(Button::Left, true), Tracking::Off, true).is_empty());
        assert!(run(&mut e, PointerEvent::Wheel { dx: 0, dy: 3 }, Tracking::Off, true).is_empty());
    }

    #[test]
    fn click_is_press_then_release_at_the_pointer_cell() {
        let mut e = MouseEncoder::new();
        run(&mut e, mv(8 + 11 * 4, 4 + 22 * 9), Tracking::Press, true); // cell (5, 10)
        assert_eq!(run(&mut e, btn(Button::Left, true), Tracking::Press, true), b"\x1b[<0;5;10M");
        assert_eq!(run(&mut e, btn(Button::Left, false), Tracking::Press, true), b"\x1b[<0;5;10m");
        assert_eq!(run(&mut e, btn(Button::Right, true), Tracking::Press, true), b"\x1b[<2;5;10M");
    }

    #[test]
    fn a_release_without_its_press_is_swallowed() {
        let mut e = MouseEncoder::new();
        assert!(run(&mut e, btn(Button::Left, false), Tracking::Press, true).is_empty());
    }

    #[test]
    fn wheel_is_one_report_per_tick_and_capped() {
        let mut e = MouseEncoder::new();
        assert_eq!(run(&mut e, PointerEvent::Wheel { dx: 0, dy: 1 }, Tracking::Press, true), b"\x1b[<64;1;1M");
        assert_eq!(
            run(&mut e, PointerEvent::Wheel { dx: 0, dy: -2 }, Tracking::Press, true),
            b"\x1b[<65;1;1M\x1b[<65;1;1M"
        );
        assert_eq!(run(&mut e, PointerEvent::Wheel { dx: 1, dy: 0 }, Tracking::Press, true), b"\x1b[<67;1;1M");
        assert_eq!(run(&mut e, PointerEvent::Wheel { dx: -1, dy: 0 }, Tracking::Press, true), b"\x1b[<66;1;1M");
        let fling = run(&mut e, PointerEvent::Wheel { dx: 0, dy: 500 }, Tracking::Press, true);
        assert_eq!(fling.len(), 8 * b"\x1b[<64;1;1M".len());
    }

    #[test]
    fn motion_only_when_the_mode_asks_and_the_cell_changes() {
        let mut e = MouseEncoder::new();
        // 1000: no motion.
        assert!(run(&mut e, mv(100, 100), Tracking::Press, true).is_empty());
        // 1003: motion with no button is 35, once per cell.
        assert_eq!(run(&mut e, mv(8 + 11 * 2, 4), Tracking::Any, true), b"\x1b[<35;3;1M");
        assert!(run(&mut e, mv(8 + 11 * 2 + 3, 5), Tracking::Any, true).is_empty()); // same cell
        // 1002: motion only while a button is held, as 32 + button.
        let mut e = MouseEncoder::new();
        assert!(run(&mut e, mv(8 + 11 * 2, 4), Tracking::Drag, true).is_empty());
        run(&mut e, btn(Button::Left, true), Tracking::Drag, true);
        assert_eq!(run(&mut e, mv(8 + 11 * 3, 4), Tracking::Drag, true), b"\x1b[<32;4;1M");
    }

    #[test]
    fn legacy_encoding_when_sgr_is_off() {
        let mut e = MouseEncoder::new();
        run(&mut e, mv(8 + 11 * 4, 4 + 22 * 9), Tracking::Press, false);
        assert_eq!(run(&mut e, btn(Button::Left, true), Tracking::Press, false), [0x1b, b'[', b'M', 32, 32 + 5, 32 + 10]);
        // Release is button 3 in the legacy form.
        assert_eq!(run(&mut e, btn(Button::Left, false), Tracking::Press, false), [0x1b, b'[', b'M', 32 + 3, 32 + 5, 32 + 10]);
    }

    #[test]
    fn a_report_that_does_not_fit_is_dropped_whole() {
        let mut e = MouseEncoder::new();
        let mut tiny = [0u8; 5];
        assert_eq!(e.feed(&btn(Button::Left, true), Tracking::Press, true, &GRID, &mut tiny), 0);
    }

    #[test]
    fn reset_forgets_held_buttons() {
        let mut e = MouseEncoder::new();
        run(&mut e, btn(Button::Left, true), Tracking::Press, true);
        e.reset();
        assert!(run(&mut e, btn(Button::Left, false), Tracking::Press, true).is_empty());
    }
}
