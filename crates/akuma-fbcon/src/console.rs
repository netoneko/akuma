//! The text console: a character grid in RAM, drawn onto a [`Surface`].
//!
//! The grid is the point. Video memory is write-only here (see the crate
//! header), so the console cannot scroll by copying pixels — it holds the
//! characters, shifts *those*, and re-draws. That makes scrolling cost a full
//! screen of glyph blits, which on a 4K framebuffer is real work, and is still
//! the cheaper of the two options by a wide margin.

use core::fmt;

use crate::font;
use crate::{Rgb, Surface};

/// Widest grid the console will use.
///
/// A 4K screen at the smallest scale this crate will choose is under this; the
/// grid is a fixed array because a kernel console must not depend on an
/// allocator that may be what broke.
pub const MAX_COLS: usize = 128;
/// Tallest grid the console will use.
pub const MAX_ROWS: usize = 64;

/// Target number of text rows [`Console::auto_scale`] aims for.
///
/// Not a hard bound — the scale is an integer, so the result lands near this
/// rather than on it. Chosen so a full screen of boot output is readable across
/// a room, which is the actual use: the machine this exists for is wired to a
/// television.
const TARGET_ROWS: usize = 48;

/// Fraction of each dimension left blank at the edges, as a divisor.
///
/// `1/24` is a little over 4 %, which covers the overscan of every television
/// that still does it. On a monitor it costs a small border and nothing else.
const MARGIN_DIVISOR: usize = 24;

/// A scrolling text console.
pub struct Console<S: Surface> {
    surface: S,
    grid: [[u8; MAX_COLS]; MAX_ROWS],
    cols: usize,
    rows: usize,
    col: usize,
    row: usize,
    scale: usize,
    origin_x: usize,
    origin_y: usize,
    fg: Rgb,
    bg: Rgb,
}

impl<S: Surface> Console<S> {
    /// A console sized to the surface, with the scale and margin chosen for it.
    ///
    /// Returns `None` when the surface cannot hold a single character even at
    /// scale 1 — a firmware that reported a 40-pixel-wide framebuffer, or a
    /// mis-parsed tag. Better a caller that knows than a console that divides
    /// by zero.
    pub fn new(surface: S) -> Option<Self> {
        let scale = Self::auto_scale(surface.height());
        Self::with_scale(surface, scale)
    }

    /// As [`Console::new`], with the glyph scale chosen by the caller.
    pub fn with_scale(surface: S, scale: usize) -> Option<Self> {
        let scale = scale.max(1);
        let (w, h) = (surface.width(), surface.height());
        let (mx, my) = Self::auto_margin(w, h);

        let cell_w = font::WIDTH * scale;
        let cell_h = font::HEIGHT * scale;
        let cols = w.saturating_sub(mx * 2) / cell_w;
        let rows = h.saturating_sub(my * 2) / cell_h;
        if cols == 0 || rows == 0 {
            return None;
        }

        Some(Self {
            surface,
            grid: [[b' '; MAX_COLS]; MAX_ROWS],
            cols: cols.min(MAX_COLS),
            rows: rows.min(MAX_ROWS),
            col: 0,
            row: 0,
            scale,
            origin_x: mx,
            origin_y: my,
            fg: Rgb::TEXT,
            bg: Rgb::BLACK,
        })
    }

    /// The integer glyph scale for a framebuffer of this height.
    ///
    /// Never zero: a very small framebuffer gets scale 1 and as many rows as it
    /// can hold.
    #[must_use]
    pub const fn auto_scale(height: usize) -> usize {
        let s = height / (font::HEIGHT * TARGET_ROWS);
        if s == 0 { 1 } else { s }
    }

    /// The overscan inset for a framebuffer of this size.
    #[must_use]
    pub const fn auto_margin(width: usize, height: usize) -> (usize, usize) {
        (width / MARGIN_DIVISOR, height / MARGIN_DIVISOR)
    }

    /// Columns of text.
    #[must_use]
    pub const fn cols(&self) -> usize {
        self.cols
    }

    /// Rows of text.
    #[must_use]
    pub const fn rows(&self) -> usize {
        self.rows
    }

    /// The glyph scale in use.
    #[must_use]
    pub const fn scale(&self) -> usize {
        self.scale
    }

    /// The colour subsequent text is drawn in.
    pub const fn set_fg(&mut self, fg: Rgb) {
        self.fg = fg;
    }

    /// The background colour, used by [`Console::clear`] and behind glyphs.
    pub const fn set_bg(&mut self, bg: Rgb) {
        self.bg = bg;
    }

    /// Paint the whole surface — not just the text area — and reset the cursor.
    ///
    /// The whole surface on purpose: the margin is part of what proves the
    /// framebuffer is being written at all, and on a first bring-up "the screen
    /// changed colour" is the entire signal.
    pub fn clear(&mut self) {
        let (w, h) = (self.surface.width(), self.surface.height());
        self.surface.fill(0, 0, w, h, self.bg);
        // Filled in place rather than assigned from a fresh array: the array
        // literal is a temporary the size of the whole grid, and this runs on a
        // boot stack that has no guard page beneath it.
        for row in &mut self.grid {
            row.fill(b' ');
        }
        self.col = 0;
        self.row = 0;
    }

    /// Fill the entire surface with one colour, leaving the grid alone.
    ///
    /// For bring-up signalling before there is anything to say: a screen that
    /// turns a known colour proves the address, the pitch and the pixel format
    /// in one step, with no font involved.
    pub fn flood(&mut self, color: Rgb) {
        let (w, h) = (self.surface.width(), self.surface.height());
        self.surface.fill(0, 0, w, h, color);
    }

    /// Write one byte, honouring `\n`, `\r` and `\t`.
    pub fn write_byte(&mut self, b: u8) {
        match b {
            b'\n' => self.newline(),
            b'\r' => self.col = 0,
            b'\t' => {
                let next = (self.col / 8 + 1) * 8;
                while self.col < next.min(self.cols) {
                    self.put_char(b' ');
                }
            }
            _ => self.put_char(b),
        }
    }

    /// Write a string.
    pub fn write_str_bytes(&mut self, s: &str) {
        for b in s.bytes() {
            self.write_byte(b);
        }
    }

    /// End the current line.
    pub fn newline(&mut self) {
        self.col = 0;
        if self.row + 1 == self.rows {
            self.scroll();
        } else {
            self.row += 1;
        }
    }

    fn put_char(&mut self, b: u8) {
        if self.col == self.cols {
            self.newline();
        }
        self.grid[self.row][self.col] = b;
        self.draw_cell(self.row, self.col, b);
        self.col += 1;
    }

    /// Shift the grid up one row and re-draw every cell.
    fn scroll(&mut self) {
        for r in 1..self.rows {
            self.grid[r - 1] = self.grid[r];
        }
        self.grid[self.rows - 1].fill(b' ');

        for r in 0..self.rows {
            for c in 0..self.cols {
                self.draw_cell(r, c, self.grid[r][c]);
            }
        }
    }

    /// Blit one glyph, background included, so a redraw needs no prior clear.
    fn draw_cell(&mut self, row: usize, col: usize, byte: u8) {
        let cell_w = font::WIDTH * self.scale;
        let cell_h = font::HEIGHT * self.scale;
        let x0 = self.origin_x + col * cell_w;
        let y0 = self.origin_y + row * cell_h;
        let glyph = font::glyph(byte);

        for (gy, bits) in glyph.iter().enumerate() {
            for gx in 0..font::WIDTH {
                let lit = bits & (0x80 >> gx) != 0;
                let color = if lit { self.fg } else { self.bg };
                let px = x0 + gx * self.scale;
                let py = y0 + gy * self.scale;
                if self.scale == 1 {
                    self.surface.put(px, py, color);
                } else {
                    self.surface.fill(px, py, self.scale, self.scale, color);
                }
            }
        }
    }

    /// Give the surface back.
    pub fn into_surface(self) -> S {
        self.surface
    }

    /// The surface, for a caller that wants to draw around the text.
    pub const fn surface_mut(&mut self) -> &mut S {
        &mut self.surface
    }
}

impl<S: Surface> fmt::Write for Console<S> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.write_str_bytes(s);
        Ok(())
    }
}
