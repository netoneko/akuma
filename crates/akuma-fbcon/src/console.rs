//! The text console: a character grid in RAM, drawn onto a [`Surface`].
//!
//! The grid is the point. Video memory is write-only here (see the crate
//! header), so the console cannot scroll by copying pixels — it holds the
//! characters, shifts *those*, and re-draws. That makes scrolling cost a full
//! screen of glyph blits, which on a 4K framebuffer is real work, and is still
//! the cheaper of the two options by a wide margin.

use core::fmt;

use crate::font::{self, Font};
use crate::{Rgb, Surface};

/// The font a [`Console`] uses when the framebuffer can afford it.
pub const DEFAULT_FONT: &Font = &font::IBM_PLEX_MONO;

/// The font used instead when [`DEFAULT_FONT`]'s cell is too big for the screen.
///
/// Half the height of the default, so it buys back rows on a framebuffer where
/// the scale has nothing left to give — the scale is an integer and does not go
/// below 1, which makes the cell size the only remaining lever.
pub const FALLBACK_FONT: &Font = &font::SPLEEN;

/// The grid [`Console::choose_font`] insists on before it keeps [`DEFAULT_FONT`].
///
/// Eighty columns is the width kernel log lines have been written for since
/// teletypes, and it is a real threshold rather than a matter of taste: below
/// it, lines wrap, and a wrapped line in a scrolling boot log is not "slightly
/// cramped" — it is a second line that looks like a separate message. The
/// 800x600 capture in `logs/font-shots/` shows one hex dump taking four.
///
/// Twenty-four rows is the other half of the same convention, and is what makes
/// the last screenful of output before a hang readable.
const MIN_COLS: usize = 80;
/// Rows [`Console::choose_font`] insists on. See [`MIN_COLS`].
const MIN_ROWS: usize = 24;

/// Widest grid the console will use.
///
/// A 4K screen at the smallest scale this crate will choose is under this; the
/// grid is a fixed array because a kernel console must not depend on an
/// allocator that may be what broke.
pub const MAX_COLS: usize = 256;
/// Tallest grid the console will use.
pub const MAX_ROWS: usize = 72;

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

/// How many `;`-separated numbers a CSI sequence keeps. The sequences a shell's
/// line editor writes carry at most two (`ESC [ row ; col H`); the rest are
/// counted and dropped.
const CSI_PARAMS: usize = 4;

/// Where [`Console::write_byte`]'s escape-sequence parser is.
///
/// A shell on this console is an ordinary Unix program and writes to it as to a
/// terminal: busybox's line editor moves the cursor with `ESC [ n D`, clears
/// with `ESC [ J`, and backspaces with `\b`. Without this every one of those
/// bytes was drawn as a glyph, which is the difference between a prompt and a
/// prompt that fills with `[D[D[J` the first time a key is pressed.
///
/// A fixed-size enum and no buffer: this runs per byte, under the console lock,
/// on the path that has to keep working when the allocator does not.
#[derive(Clone, Copy)]
enum Esc {
    Ground,
    /// Saw `ESC`; the next byte says what kind of sequence this is.
    Escape,
    /// Inside `ESC [`. `count` is the number of `;` seen, `private` is set by a
    /// `?`/`<`/`=`/`>` prefix (`ESC [ ? 25 h` and friends — mode switches this
    /// console has no modes for, so they are parsed and ignored).
    Csi { params: [u16; CSI_PARAMS], count: usize, private: bool },
    /// Inside an `ESC ]`/`ESC P`/... string, swallowed up to `BEL` or `ESC \`.
    Str,
}

/// A scrolling text console.
pub struct Console<S: Surface> {
    surface: S,
    font: &'static Font,
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
    esc: Esc,
    /// The cell the cursor is drawn on right now, if it is drawn at all. See
    /// [`Console::show_cursor`].
    cursor: Option<(usize, usize)>,
}

impl<S: Surface> Console<S> {
    /// A console sized to the surface, with the font, scale and margin all
    /// chosen for it.
    ///
    /// The font is [`Console::choose_font`]'s: [`DEFAULT_FONT`] on a screen that
    /// can afford its cell, [`FALLBACK_FONT`] on one that cannot. Every other
    /// constructor names a font, so this is the only one that decides.
    ///
    /// Returns `None` when the surface cannot hold a single character even at
    /// scale 1 — a firmware that reported a 40-pixel-wide framebuffer, or a
    /// mis-parsed tag. Better a caller that knows than a console that divides
    /// by zero.
    pub fn new(surface: S) -> Option<Self> {
        let font = Self::choose_font(surface.width(), surface.height());
        Self::with_font(surface, font)
    }

    /// As [`Console::new`], in a font the caller names. No fallback.
    pub fn with_font(surface: S, font: &'static Font) -> Option<Self> {
        let scale = Self::auto_scale(font, surface.height());
        Self::with_font_and_scale(surface, font, scale)
    }

    /// As [`Console::new`], in [`DEFAULT_FONT`] at a scale the caller names.
    ///
    /// No fallback: a caller naming a scale has already taken the decision away
    /// from the console, and silently swapping the font underneath that would
    /// be the surprising half of an override.
    pub fn with_scale(surface: S, scale: usize) -> Option<Self> {
        Self::with_font_and_scale(surface, DEFAULT_FONT, scale)
    }

    /// As [`Console::new`], with both the font and the scale chosen by the caller.
    pub fn with_font_and_scale(surface: S, font: &'static Font, scale: usize) -> Option<Self> {
        let scale = scale.max(1);
        let (w, h) = (surface.width(), surface.height());
        let (mx, my) = Self::auto_margin(w, h);
        let (cols, rows) = Self::grid_for(font, w, h, scale)?;

        // The grid is the console's whole reason to exist (video memory is
        // never read back), and it is built once, at boot, on the boot stack.
        #[allow(clippy::large_stack_arrays)]
        Some(Self {
            surface,
            font,
            grid: [[b' '; MAX_COLS]; MAX_ROWS],
            cols,
            rows,
            col: 0,
            row: 0,
            scale,
            origin_x: mx,
            origin_y: my,
            fg: Rgb::TEXT,
            bg: Rgb::BLACK,
            esc: Esc::Ground,
            cursor: None,
        })
    }

    /// The grid `font` fills on a framebuffer this size, or `None` if it cannot
    /// place a single cell.
    ///
    /// The one place this arithmetic lives. [`Console::choose_font`] asks it
    /// which font fits and [`Console::with_font_and_scale`] asks it how big the
    /// grid is, so the font that was picked and the grid that gets built cannot
    /// disagree — which is the whole failure mode a separate "will it fit?"
    /// calculation would introduce.
    #[must_use]
    pub fn grid_for(font: &Font, width: usize, height: usize, scale: usize) -> Option<(usize, usize)> {
        let (mx, my) = Self::auto_margin(width, height);
        let cols = width.saturating_sub(mx * 2) / (font.width() * scale);
        let rows = height.saturating_sub(my * 2) / (font.height() * scale);
        if cols == 0 || rows == 0 {
            return None;
        }
        Some((cols.min(MAX_COLS), rows.min(MAX_ROWS)))
    }

    /// Which font to draw a framebuffer this size in.
    ///
    /// [`DEFAULT_FONT`] whenever it reaches [`MIN_COLS`] by [`MIN_ROWS`]. Below
    /// that the choice is whichever font yields more cells, which is not always
    /// the smaller one: both fonts are scaled up independently, and at 1920x1200
    /// the default runs at scale 1 while the fallback rounds to 2 — a 16x32 cell
    /// against a 12x24 one, so the "small" font is the bigger of the two there.
    /// Comparing the grids the two actually produce is the only way to get that
    /// right; comparing their cell sizes is not.
    #[must_use]
    pub fn choose_font(width: usize, height: usize) -> &'static Font {
        let grid = |f: &'static Font| {
            Self::grid_for(f, width, height, Self::auto_scale(f, height))
        };
        match (grid(DEFAULT_FONT), grid(FALLBACK_FONT)) {
            (Some((cols, rows)), _) if cols >= MIN_COLS && rows >= MIN_ROWS => DEFAULT_FONT,
            // Nothing to fall back to, including the case where neither font
            // fits at all -- `new` then returns `None`, which is the honest
            // answer and the one the caller can act on.
            (_, None) => DEFAULT_FONT,
            (None, Some(_)) => FALLBACK_FONT,
            (Some((dc, dr)), Some((fc, fr))) => {
                if fc * fr > dc * dr { FALLBACK_FONT } else { DEFAULT_FONT }
            }
        }
    }

    /// The integer glyph scale for `font` on a framebuffer of this height.
    ///
    /// Rounded to nearest, not truncated. Truncating looks equivalent and is
    /// not: it can only ever under-scale, and it does so by a whole step. A 4K
    /// screen wants 1.875 cells' worth of a 24-pixel font, and truncation
    /// answers 1 — ninety rows of 24-pixel text on a television across a room,
    /// which is precisely the outcome [`TARGET_ROWS`] exists to prevent.
    ///
    /// Never zero: a very small framebuffer gets scale 1 and as many rows as it
    /// can hold.
    #[must_use]
    pub const fn auto_scale(font: &Font, height: usize) -> usize {
        let want = font.height() * TARGET_ROWS;
        let s = (height + want / 2) / want;
        if s == 0 { 1 } else { s }
    }

    /// The font this console draws in.
    #[must_use]
    pub const fn font(&self) -> &'static Font {
        self.font
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
        self.esc = Esc::Ground;
        // The fill above painted over it; there is nothing left to erase.
        self.cursor = None;
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

    /// Write one byte, honouring `\n`, `\r`, `\t`, `\b` and the small subset of
    /// ANSI escape sequences a shell's line editor uses (see [`Esc`]).
    ///
    /// Everything else — including control bytes the font draws as a box — is
    /// drawn as before, so the kernel's own output, which never contains an
    /// escape, is byte-for-byte what it was.
    pub fn write_byte(&mut self, b: u8) {
        self.hide_cursor();
        match self.esc {
            Esc::Ground => self.ground(b),
            Esc::Escape => {
                self.esc = match b {
                    b'[' => Esc::Csi { params: [0; CSI_PARAMS], count: 0, private: false },
                    b']' | b'P' | b'_' | b'^' | b'X' => Esc::Str,
                    // A two-byte sequence (`ESC c`, `ESC 7`, `ESC =`, ...): none
                    // has a meaning here, and none may be drawn.
                    _ => Esc::Ground,
                };
            }
            Esc::Str => match b {
                0x07 => self.esc = Esc::Ground,
                // `ESC \` is the string terminator; going through `Escape`
                // consumes the `\` as the two-byte sequence it is.
                0x1b => self.esc = Esc::Escape,
                _ => {}
            },
            Esc::Csi { mut params, mut count, mut private } => {
                match b {
                    b'0'..=b'9' => {
                        let p = &mut params[count.min(CSI_PARAMS - 1)];
                        *p = p.saturating_mul(10).saturating_add(u16::from(b - b'0')).min(9999);
                    }
                    b';' => count = (count + 1).min(CSI_PARAMS - 1),
                    b'<'..=b'?' => private = true,
                    // Intermediates (`ESC [ ! p`): part of the sequence, no meaning.
                    0x20..=0x2f => {}
                    0x40..=0x7e => {
                        self.esc = Esc::Ground;
                        if !private {
                            self.csi(b, &params);
                        }
                        return;
                    }
                    // CAN and SUB abort a sequence; any other control byte is
                    // executed by a real terminal mid-sequence and ignored here.
                    0x18 | 0x1a => {
                        self.esc = Esc::Ground;
                        return;
                    }
                    _ => {}
                }
                self.esc = Esc::Csi { params, count, private };
            }
        }
    }

    fn ground(&mut self, b: u8) {
        match b {
            b'\n' => self.newline(),
            b'\r' => self.col = 0,
            b'\t' => {
                let next = (self.col / 8 + 1) * 8;
                while self.col < next.min(self.cols) {
                    self.put_char(b' ');
                }
            }
            // Non-destructive, as on a terminal: the erase is the space the
            // line discipline writes next (`\b \b`), not the backspace.
            0x08 => self.col = self.col.saturating_sub(1),
            0x1b => self.esc = Esc::Escape,
            // BEL has no sound to make and DEL no glyph; the font would draw
            // both as the replacement box.
            0x07 | 0x7f => {}
            _ => self.put_char(b),
        }
    }

    /// Execute one finished `ESC [ params final` sequence.
    ///
    /// Positions are clamped, never wrapped or scrolled: a program that moves
    /// the cursor off the grid gets the nearest edge, as on a terminal.
    fn csi(&mut self, final_byte: u8, params: &[u16; CSI_PARAMS]) {
        // A cursor sitting one past the last column is a deferred wrap, not a
        // position; every movement below starts from the last real column.
        let last_col = self.cols - 1;
        let last_row = self.rows - 1;
        let col = self.col.min(last_col);
        let p = |i: usize| usize::from(params[i]);
        // Movement counts treat 0 like "absent" and mean 1, per ECMA-48.
        let n = |i: usize| p(i).max(1);
        match final_byte {
            b'A' => {
                self.row = self.row.saturating_sub(n(0));
                self.col = col;
            }
            b'B' => {
                self.row = (self.row + n(0)).min(last_row);
                self.col = col;
            }
            b'C' => self.col = (col + n(0)).min(last_col),
            b'D' => self.col = col.saturating_sub(n(0)),
            b'G' => self.col = (n(0) - 1).min(last_col),
            b'd' => self.row = (n(0) - 1).min(last_row),
            b'H' | b'f' => {
                self.row = (n(0) - 1).min(last_row);
                self.col = (n(1) - 1).min(last_col);
            }
            b'J' => match p(0) {
                0 => {
                    self.blank(self.row, col, self.cols);
                    for r in self.row + 1..self.rows {
                        self.blank(r, 0, self.cols);
                    }
                }
                1 => {
                    for r in 0..self.row {
                        self.blank(r, 0, self.cols);
                    }
                    self.blank(self.row, 0, col + 1);
                }
                2 | 3 => {
                    for r in 0..self.rows {
                        self.blank(r, 0, self.cols);
                    }
                }
                _ => {}
            },
            b'K' => match p(0) {
                0 => self.blank(self.row, col, self.cols),
                1 => self.blank(self.row, 0, col + 1),
                2 => self.blank(self.row, 0, self.cols),
                _ => {}
            },
            // `m` (colours) and every sequence this console has no use for.
            _ => {}
        }
    }

    /// Draw the cursor: the cell under it, colours swapped.
    ///
    /// **Not drawn by [`Console::write_byte`], on purpose.** A block cursor
    /// that follows every byte is two extra cell blits per character — one to
    /// erase, one to draw — on a surface where each pixel is an uncached write,
    /// and the boot log is thousands of lines. So the caller says when output
    /// has gone quiet (the console pump does, on an idle lap) and the cursor
    /// appears then; the next byte written takes it down again before touching
    /// anything. Idempotent: a cursor already drawn where it should be costs one
    /// comparison, which is what lets that idle call run every lap.
    ///
    /// On the television this is what says *where typing will land* — without it
    /// a prompt is indistinguishable from output, and a screen that has stopped
    /// scrolling cannot be told apart from one that is waiting for you.
    pub fn show_cursor(&mut self) {
        // A deferred wrap parks `col` one past the end; the cursor sits on the
        // last real cell, which is where a terminal draws it too.
        let at = (self.row, self.col.min(self.cols - 1));
        if self.cursor == Some(at) {
            return;
        }
        self.hide_cursor();
        let byte = self.grid[at.0][at.1];
        self.draw_cell_in(at.0, at.1, byte, self.bg, self.fg);
        self.cursor = Some(at);
    }

    /// Take the cursor down, restoring the cell it covered. A no-op if it is not
    /// drawn. Called first by [`Console::write_byte`], so nothing ever draws
    /// over — or scrolls — a stale cursor.
    pub fn hide_cursor(&mut self) {
        if let Some((r, c)) = self.cursor.take() {
            let byte = self.grid[r][c];
            self.draw_cell_in(r, c, byte, self.fg, self.bg);
        }
    }

    /// Blank `row`'s cells `from..to`, redrawing only those that were not
    /// already blank — the same economy [`Console::scroll`] has, for the same
    /// reason: every pixel is an uncached write to video memory.
    fn blank(&mut self, row: usize, from: usize, to: usize) {
        for c in from..to.min(self.cols) {
            if self.grid[row][c] != b' ' {
                self.grid[row][c] = b' ';
                self.draw_cell(row, c, b' ');
            }
        }
    }

    /// Where the next character lands, as `(row, column)`.
    #[must_use]
    pub const fn cursor(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    /// The character the console believes is at `(row, col)`.
    ///
    /// The grid is what scrolling and erasing work from, so this is the
    /// console's own account of the screen — what a test, or a caller that wants
    /// to know what is on the glass, can ask without reading video memory.
    #[must_use]
    pub const fn cell_at(&self, row: usize, col: usize) -> u8 {
        self.grid[row][col]
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

    /// How many rows a single [`scroll`](Self::scroll) advances by.
    ///
    /// One row per scroll means a scroll on *every* line once output reaches the
    /// bottom, and each scroll rewrites almost the whole screen cell by cell,
    /// top to bottom — which on a television reads as a tear sweeping down the
    /// picture on every printed line. Advancing several rows at once makes that
    /// redraw happen once per `SCROLL_ROWS` lines instead: the cursor lands a few
    /// rows up from the bottom with blank space below it, and that space fills in
    /// before the next scroll. Eight is enough to make the sweep occasional
    /// rather than constant without leaving a distractingly large gap.
    pub const SCROLL_ROWS: usize = 8;

    fn put_char(&mut self, b: u8) {
        if self.col == self.cols {
            self.newline();
        }
        self.grid[self.row][self.col] = b;
        self.draw_cell(self.row, self.col, b);
        self.col += 1;
    }

    /// Shift the grid up one row, re-drawing only the cells that changed.
    ///
    /// The obvious implementation shifts the grid and then redraws every cell,
    /// and on a large screen that is ruinous: at 3840x2160 the grid is over
    /// 13000 cells and each is 512 pixels, so one scroll is nearly seven
    /// million uncached writes. Boot output that scrolls a hundred times would
    /// take minutes, and the machine would look hung.
    ///
    /// Comparing each cell against what will replace it turns that into work
    /// proportional to the text rather than to the screen. Console output is
    /// mostly short lines on a wide grid, so the great majority of cells are
    /// blank both before and after and need no writes at all.
    fn scroll(&mut self) {
        let shift = Self::SCROLL_ROWS.min(self.rows - 1);
        for r in 0..self.rows {
            for c in 0..self.cols {
                let next = if r + shift < self.rows {
                    self.grid[r + shift][c]
                } else {
                    b' '
                };
                if self.grid[r][c] != next {
                    self.grid[r][c] = next;
                    self.draw_cell(r, c, next);
                }
            }
        }
        // The line that triggered this scroll sat on the last row and has moved
        // up to `rows - 1 - shift`; the next line goes just below it, leaving
        // `shift` blank rows at the bottom to fill before the next scroll.
        self.row = self.rows - shift;
    }

    /// Blit one glyph, background included, so a redraw needs no prior clear.
    ///
    /// Each font pixel carries a coverage value, and a partly-covered one is
    /// drawn as a mix of the two colours (see [`Rgb::blend`]). The two ends of
    /// that range are the overwhelming majority of pixels in any glyph and are
    /// taken without arithmetic — every pixel here is a write to uncached video
    /// memory, so a multiply that only matters on an edge should not be paid
    /// for the interior.
    fn draw_cell(&mut self, row: usize, col: usize, byte: u8) {
        self.draw_cell_in(row, col, byte, self.fg, self.bg);
    }

    /// [`Console::draw_cell`] in colours the caller names — the cursor is the
    /// one cell drawn with them swapped.
    fn draw_cell_in(&mut self, row: usize, col: usize, byte: u8, fg: Rgb, bg: Rgb) {
        let (fw, fh) = (self.font.width(), self.font.height());
        let cell_w = fw * self.scale;
        let cell_h = fh * self.scale;
        let x0 = self.origin_x + col * cell_w;
        let y0 = self.origin_y + row * cell_h;
        let cell = self.font.cell(byte);

        for gy in 0..fh {
            for gx in 0..fw {
                let color = match cell[gy * fw + gx] {
                    0x00 => bg,
                    0xFF => fg,
                    coverage => bg.blend(fg, coverage),
                };
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
