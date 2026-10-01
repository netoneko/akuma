//! The boot splash: the Akuma mark, glowing and shifting colour, with the
//! machine's identity beneath it.
//!
//! A quiet boot shows this instead of a scrolling log. Everything here is pure
//! except [`paint`], which draws through [`Console::draw_text_rgb`] — decoration
//! outside the terminal's grid, so there is nothing to scroll, erase or restore:
//! when the splash ends the console is simply cleared.
//!
//! # The glow
//!
//! The mark is ASCII art, and its characters are ordered by ink: `. : - = + * # % @`
//! runs from sparse to dense. Each cell's colour comes from three things:
//!
//! * **hue** — a wave that drifts diagonally across the art and round the colour
//!   wheel over time, so the colour is always moving and no two cells match;
//! * **value** — density, so the body of the mark is bright and its fringe soft,
//!   times a slow "breathing" swell;
//! * **saturation** — falling with density, so the densest cells wash toward white:
//!   the hot core of the glow.
//!
//! Only colours change between frames, never shapes, so a frame redraws just the
//! cells that have ink.

use crate::console::Console;
use crate::{Rgb, Surface};

/// Every info line is padded to this many columns, so a line that shrinks (an
/// uptime rolling from `up 99s` to `up 100s`' successor, a status that gets
/// shorter) overwrites all of what it replaces.
pub const INFO_WIDTH: usize = 72;

/// Characters of the art from sparse to dense. Anything else counts as dense.
const DENSITY: &str = " .:-=+*#%@";

/// Where the art's top-left cell goes, in a `rows` x `cols` printing area: on the
/// left, a little in from the edge, and vertically centred on the whole splash
/// (art plus `info_lines` of text and a gap).
#[must_use]
pub fn layout(art: &str, info_lines: usize, rows: usize, cols: usize) -> (usize, usize) {
    let art_rows = art.lines().count();
    let art_cols = art.lines().map(str::len).max().unwrap_or(0);
    let total = art_rows + 2 + info_lines;
    let top = rows.saturating_sub(total) / 2;
    // Two columns in, unless the screen is too narrow for the art at all.
    let left = if cols > art_cols + 4 { 2 } else { 0 };
    (top, left)
}

/// `sin(deg)` in thousandths, by Bhaskara I's approximation: integer-only and
/// within 0.2 % — plenty for a swell in brightness.
#[must_use]
pub fn sin_permille(deg: i64) -> i64 {
    let d = deg.rem_euclid(360);
    let (x, sign) = if d < 180 { (d, 1) } else { (d - 180, -1) };
    let num = 4 * x * (180 - x);
    let den = 40_500 - x * (180 - x);
    sign * 1000 * num / den
}

/// The colour of art cell `(x, y)` holding `ch` at time `t_ms`.
#[must_use]
pub fn art_color(ch: u8, x: usize, y: usize, t_ms: u64) -> Rgb {
    let density = DENSITY.bytes().position(|c| c == ch).unwrap_or(DENSITY.len() - 1);
    let d = density.min(DENSITY.len() - 1) as u32; // 0..=9
    // Hue: a wave across the art and round the wheel over time (one turn per ~14 s).
    let hue = (t_ms / 40) as u32 + x as u32 * 4 + y as u32 * 7;
    // Breathing: a slow swell between 75 % and 100 % brightness (a ~5 s period).
    let breath = 875 + sin_permille(i64::try_from(t_ms / 14).unwrap_or(0)) / 8; // 750..=1000
    let value = (90 + d * 18) * breath as u32 / 1000; // 90..=252, then breathed
    // Saturation falls with density: the densest cells go nearly white.
    let sat = 255 - d * 17;
    Rgb::from_hsv(hue, sat.min(255) as u8, value.min(255) as u8)
}

/// Draw one frame: the art in colour, then `info` (one line each) beneath it.
///
/// The first line of `info` is the title and takes the hue wave too; the rest are
/// plain text. Every info line is padded to [`INFO_WIDTH`] so a changing line (an
/// uptime) overwrites what it replaces. `t_ms` is the time since the splash began.
pub fn paint<S: Surface>(con: &mut Console<S>, art: &str, info: &[&str], t_ms: u64) {
    let (top, left) = layout(art, info.len(), con.max_rows(), con.max_cols());
    let bg = con.background();
    let mut cell = [0u8; 4];
    for (y, line) in art.lines().enumerate() {
        for (x, ch) in line.bytes().enumerate() {
            if ch == b' ' {
                continue;
            }
            let c = char::from(ch).encode_utf8(&mut cell);
            con.draw_text_rgb(top + y, left + x, c, art_color(ch, x, y, t_ms), bg);
        }
    }
    let first = top + art.lines().count() + 2;
    for (i, line) in info.iter().enumerate() {
        let color = if i == 0 {
            Rgb::from_hsv((t_ms / 40) as u32, 120, 255)
        } else {
            Rgb::TEXT
        };
        // Padded with spaces, drawn in one go.
        let mut padded = [b' '; 96];
        let n = line.len().min(padded.len());
        padded[..n].copy_from_slice(&line.as_bytes()[..n]);
        let w = INFO_WIDTH.min(padded.len());
        if let Ok(text) = core::str::from_utf8(&padded[..w]) {
            con.draw_text_rgb(first + i, left, text, color, bg);
        }
    }
}
