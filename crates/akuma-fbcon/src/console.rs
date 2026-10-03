//! The text console: a character grid in RAM, drawn onto a [`Surface`].
//!
//! The grid is the point. Video memory is write-only here (see the crate
//! header), so the console cannot scroll by copying pixels — it holds the
//! characters, shifts *those*, and re-draws. That makes scrolling cost a full
//! screen of glyph blits, which on a 4K framebuffer is real work, and is still
//! the cheaper of the two options by a wide margin.

use core::fmt;

use crate::font::{self, Font};
use crate::emoji;
use crate::glyph;
use crate::unifont;
use crate::{Rgb, Surface};

/// The font a [`Console`] uses when the framebuffer can afford it.
pub const DEFAULT_FONT: &Font = &font::IBM_PLEX_MONO;

/// [`DEFAULT_FONT`] baked at twice the size (24x48), for a screen where the console
/// would otherwise draw the 12x24 table at scale 2. Same cell on the glass, but the
/// glyphs are rasterized from the outlines at that size rather than being a
/// half-resolution bitmap with each pixel doubled -- smooth curves, real stems.
pub const HD_FONT: &Font = &font::IBM_PLEX_MONO_HD;

/// The same face at 20x40 — what a screen that would draw [`DEFAULT_FONT`] at
/// scale 2 gets **by default** since 2026-10-03: 15 % smaller than
/// [`HD_FONT`], for more text on the 4K television (192x54 cells, not 160x45).
pub const FONT_40: &Font = &font::IBM_PLEX_MONO_40;

/// The same face at 16x32, for `font = 32` in `/etc/console.conf`.
pub const FONT_32: &Font = &font::IBM_PLEX_MONO_32;

/// Every baked cut of the default face, by cell height — what
/// `/etc/console.conf`'s `font =` chooses among ([`Console::font_by_height`]).
pub const PLEX_CUTS: [&Font; 4] = [DEFAULT_FONT, FONT_32, FONT_40, HD_FONT];

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
///
/// 240x68 since 2026-10-03, so the 16x32 cut fills a 3840x2160 screen (the
/// old 160x56 was exactly 3840/24 and would have clamped every smaller font
/// back to 24-pixel-wide columns' worth of text). 98 KiB of cells.
pub const MAX_COLS: usize = 240;
/// Tallest grid the console will use.
pub const MAX_ROWS: usize = 68;

/// Target number of text rows [`Console::auto_scale`] aims for.
///
/// Not a hard bound — the scale is an integer, so the result lands near this
/// rather than on it. Chosen so a full screen of boot output is readable across
/// a room, which is the actual use: the machine this exists for is wired to a
/// television.
const TARGET_ROWS: usize = 48;

/// Fraction of each dimension left blank at the edges, as a divisor.
///
/// `1/128` is under 1 %: a hair of border so text never touches the bezel. It was
/// `1/24` (about 4 %) until 2026-10-01, which is what a television that still
/// overscans needs and what a monitor or a modern TV in PC mode only wastes — on a
/// 4K screen that is 160 pixels of dead space down the left edge. The margin is
/// adjustable at run time (`CSI ? 9001 ; x ; y h`, [`Console::set_margin`]), so a
/// screen that does crop its edges can have the old inset back.
const MARGIN_DIVISOR: usize = 128;

/// How many `;`-separated numbers a CSI sequence keeps. A colour change such as
/// `ESC [ 1 ; 38 ; 2 ; r ; g ; b ; 48 ; 2 ; r ; g ; b m` carries twelve; the rest
/// are counted and dropped.
const CSI_PARAMS: usize = 16;

/// Bytes of an `ESC ]` string the parser keeps. Enough for the colour queries
/// (`10;?` / `11;?`) a TUI sends at start-up; a window title is longer and is
/// simply truncated, which is harmless.
const PARSER_OSC: usize = 64;

/// Bytes of pending terminal reply (cursor position, device attributes, colour
/// query answers) the console holds for its owner to take. A reply that does not
/// fit is dropped whole: half an escape sequence typed into a shell is worse.
const REPLY_CAP: usize = 48;

// ---------------------------------------------------------------------------
// Cells and colour
// ---------------------------------------------------------------------------

/// Cell flag bits.
const F_BOLD: u8 = 1;
const F_DIM: u8 = 2;
const F_UNDER: u8 = 4;
const F_REVERSE: u8 = 8;
/// `fg` / `bg` hold a palette index (otherwise: the console's default colours).
const F_FG: u8 = 16;
const F_BG: u8 = 32;

/// Code-point markers a cell can hold instead of a character.
const CONT: u16 = 0xFFFF; // the right half of a two-column character
const TOFU: u16 = 0xFFFD; // a character that cannot be stored (invalid UTF-8)
const TOFU_WIDE: u16 = 0xFFFE; // one beyond the BMP and two columns wide (emoji)
/// A cell holding emoji picture `n` stores `EMOJI_BASE + n` as its code point. The
/// surrogate range is used because it can never be a real character (the parser
/// never yields one), so no text can collide with it; there is room for 2048.
const EMOJI_BASE: u16 = 0xD800;
const _: () = assert!(emoji::count() <= 0x800, "more emoji than the surrogate range holds");

/// One screen cell: what is shown, and the colours and flags it is shown in.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Cell {
    cp: u16,
    fg: u8,
    bg: u8,
    flags: u8,
}

const BLANK: Cell = Cell { cp: 0x20, fg: 0, bg: 0, flags: 0 };

/// The colours and flags the next character will be drawn with.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Pen {
    fg: u8,
    bg: u8,
    flags: u8,
}

const PEN_DEFAULT: Pen = Pen { fg: 0, bg: 0, flags: 0 };

/// The 16 ANSI colours, chosen to sit with this crate's own palette
/// ([`Rgb::GOOD`], [`Rgb::WARN`], [`Rgb::BAD`], [`Rgb::ACCENT`]).
const ANSI16: [(u8, u8, u8); 16] = [
    (0x1C, 0x20, 0x28), // 0 black
    (0xE0, 0x50, 0x50), // 1 red
    (0x50, 0xD0, 0x60), // 2 green
    (0xE0, 0xC0, 0x40), // 3 yellow
    (0x60, 0xA0, 0xE0), // 4 blue
    (0xC0, 0x70, 0xD0), // 5 magenta
    (0x50, 0xC8, 0xD0), // 6 cyan
    (0xC8, 0xD0, 0xD8), // 7 white
    (0x60, 0x68, 0x70), // 8 bright black
    (0xF0, 0x80, 0x80), // 9
    (0x80, 0xF0, 0x90), // 10
    (0xF0, 0xD8, 0x70), // 11
    (0x90, 0xC0, 0xF0), // 12
    (0xE0, 0xA0, 0xF0), // 13
    (0x90, 0xE0, 0xE8), // 14
    (0xEE, 0xEE, 0xEE), // 15
];

/// A level of the xterm 6x6x6 colour cube.
const fn cube_level(n: u8) -> u8 {
    if n == 0 { 0 } else { 55 + 40 * n }
}

/// An xterm-256 palette index as a colour.
fn palette(idx: u8) -> Rgb {
    match idx {
        0..=15 => {
            let (r, g, b) = ANSI16[usize::from(idx)];
            Rgb::new(r, g, b)
        }
        16..=231 => {
            let i = idx - 16;
            Rgb::new(cube_level(i / 36), cube_level((i / 6) % 6), cube_level(i % 6))
        }
        _ => {
            let v = 8 + 10 * (idx - 232);
            Rgb::new(v, v, v)
        }
    }
}

/// The nearest xterm-256 index to an RGB colour: how a truecolor request
/// (`38;2;r;g;b`) is stored in a one-byte cell. Nearest of the colour cube and the
/// grey ramp, by squared distance.
fn rgb_to_palette(r: u8, g: u8, b: u8) -> u8 {
    let level = |v: u8| -> u8 {
        // Nearest of 0, 95, 135, 175, 215, 255.
        let mut best = 0u8;
        let mut best_d = u32::MAX;
        for n in 0..6u8 {
            let d = u32::from(v.abs_diff(cube_level(n)));
            if d < best_d {
                best_d = d;
                best = n;
            }
        }
        best
    };
    let (ri, gi, bi) = (level(r), level(g), level(b));
    let cube = 16 + 36 * ri + 6 * gi + bi;
    let dist = |c: Rgb| {
        let d = |a: u8, b: u8| u32::from(a.abs_diff(b)).pow(2);
        d(r, c.r) + d(g, c.g) + d(b, c.b)
    };
    let avg = ((u32::from(r) + u32::from(g) + u32::from(b)) / 3) as u8;
    let gi = if avg < 8 { 0 } else { ((u32::from(avg) - 8 + 5) / 10).min(23) as u8 };
    let gray = 232 + gi;
    if dist(palette(gray)) < dist(palette(cube)) { gray } else { cube }
}

/// A scrolling text console.
// Four independent terminal modes (autowrap, cursor visibility, alternate screen,
// pending geometry report); a bitset would only hide them.
#[allow(clippy::struct_excessive_bools)]
pub struct Console<S: Surface> {
    surface: S,
    font: &'static Font,
    grid: [[Cell; MAX_COLS]; MAX_ROWS],
    /// The **physical** grid the screen holds.
    cols: usize,
    rows: usize,
    /// The **printing area**: the part of the grid text wraps and scrolls inside,
    /// anchored at the top left. Equal to the physical grid until
    /// [`Console::set_view`] narrows it — which is how a part of the screen that
    /// does not work, or a part reserved for something else, is left alone.
    vcols: usize,
    vrows: usize,
    col: usize,
    row: usize,
    scale: usize,
    origin_x: usize,
    origin_y: usize,
    fg: Rgb,
    bg: Rgb,
    /// The escape-sequence parser (`vte`): UTF-8 decoding and every CSI/OSC/ESC state.
    /// An `Option` only so [`Console::write_byte`] can take it out while the
    /// console it drives is borrowed.
    parser: Option<vte::Parser<PARSER_OSC>>,
    pen: Pen,
    /// Cursor saved by `ESC 7` / `CSI s`.
    saved: (usize, usize, Pen),
    /// Scroll region, inclusive row numbers within the printing area.
    top: usize,
    bot: usize,
    autowrap: bool,
    cursor_hidden: bool,
    /// An application is using the "alternate screen" (`CSI ? 1049 h`): it owns
    /// the whole screen, so scrolling is one line at a time, as it expects.
    alt: bool,
    /// The last character printed, for `CSI n b` (repeat).
    last_cp: u32,
    /// The cell the cursor is drawn on right now, if it is drawn at all. See
    /// [`Console::show_cursor`].
    cursor: Option<(usize, usize)>,
    reply: [u8; REPLY_CAP],
    reply_len: usize,
    /// The grid changed size under the program (a new margin); see
    /// [`Console::take_geometry`].
    geometry_dirty: bool,
    /// Where the last character went, and whether it was an emoji picture — what an
    /// emoji *sequence* (VS16, ZWJ, a skin-tone modifier) is glued to.
    last_cell: Option<(usize, usize)>,
    last_emoji: bool,
    /// A zero-width joiner was just seen: the emoji after it joins the previous one
    /// and is not drawn.
    swallow: bool,
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
        // never read back), and it is built once, at boot, on a stack of 256 KiB
        // (`boot.s`): about 54 KiB of cells here, so a handful of moves is
        // survivable and a hundred would not be.
        #[allow(clippy::large_stack_arrays)]
        Some(Self {
            surface,
            font,
            grid: [[BLANK; MAX_COLS]; MAX_ROWS],
            cols,
            rows,
            vcols: cols,
            vrows: rows,
            col: 0,
            row: 0,
            scale,
            origin_x: mx,
            origin_y: my,
            fg: Rgb::TEXT,
            bg: Rgb::BLACK,
            parser: Some(vte::Parser::new_with_size()),
            pen: PEN_DEFAULT,
            saved: (0, 0, PEN_DEFAULT),
            top: 0,
            bot: rows - 1,
            autowrap: true,
            cursor_hidden: false,
            alt: false,
            last_cp: 0x20,
            cursor: None,
            reply: [0; REPLY_CAP],
            reply_len: 0,
            geometry_dirty: false,
            last_cell: None,
            last_emoji: false,
            swallow: false,
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
        // The default font, drawn at its own pixel size when the scale would be 2:
        // [`HD_FONT`] is the same face and the same cell on the glass, with every
        // glyph rasterized at that size instead of doubled. Its own `auto_scale` is
        // 1 across the whole range where the default's is 2 (heights 1728..3455),
        // so the grid is unchanged.
        //
        // Since 2026-10-03 that screen gets [`FONT_40`] instead: 15 % smaller
        // than the HD cut, more text per screen. `font = 48` in
        // `/etc/console.conf` brings the HD cut back.
        let default = if Self::auto_scale(DEFAULT_FONT, height) == 2 { FONT_40 } else { DEFAULT_FONT };
        match (grid(default), grid(FALLBACK_FONT)) {
            (Some((cols, rows)), _) if cols >= MIN_COLS && rows >= MIN_ROWS => default,
            // Nothing to fall back to, including the case where neither font
            // fits at all -- `new` then returns `None`, which is the honest
            // answer and the one the caller can act on.
            (_, None) => default,
            (None, Some(_)) => FALLBACK_FONT,
            (Some((dc, dr)), Some((fc, fr))) => {
                if fc * fr > dc * dr { FALLBACK_FONT } else { default }
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

    /// Draw `text` (ASCII) at grid position `(row, col)` in the colours given,
    /// **without touching the grid** — a decoration that is not part of the
    /// terminal's contents, so it does not scroll, is not erased by an escape
    /// sequence, and is gone on the next [`Console::clear`]. What the boot splash
    /// is drawn with. Clipped to the physical grid.
    pub fn draw_text_rgb(&mut self, row: usize, col: usize, text: &str, fg: Rgb, bg: Rgb) {
        if row >= self.rows {
            return;
        }
        for (i, b) in text.bytes().enumerate() {
            if col + i >= self.cols {
                break;
            }
            self.draw_glyph(row, col + i, u32::from(b), false, fg, bg);
        }
    }

    /// The background colour (what [`Console::clear`] fills with).
    #[must_use]
    pub const fn background(&self) -> Rgb {
        self.bg
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

    /// The overscan inset in pixels, `(x, y)`: where the text area starts.
    #[must_use]
    pub const fn margin(&self) -> (usize, usize) {
        (self.origin_x, self.origin_y)
    }

    /// Columns of text in the **printing area** — what a program should lay out
    /// for. Equal to [`Console::max_cols`] until [`Console::set_view`] narrows it.
    #[must_use]
    #[allow(clippy::misnamed_getters)] // the *view*, deliberately: see `set_view`
    pub const fn cols(&self) -> usize {
        self.vcols
    }

    /// Rows of text in the printing area.
    #[must_use]
    #[allow(clippy::misnamed_getters)] // the *view*, deliberately: see `set_view`
    pub const fn rows(&self) -> usize {
        self.vrows
    }

    /// Columns the screen physically holds.
    #[must_use]
    pub const fn max_cols(&self) -> usize {
        self.cols
    }

    /// Rows the screen physically holds.
    #[must_use]
    pub const fn max_rows(&self) -> usize {
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

    /// Restrict printing to the top-left `cols` x `rows` of the screen.
    ///
    /// Text wraps at `cols` and scrolls at `rows`, and every cell outside is
    /// blanked and stays blank — so what a program sees as the terminal is exactly
    /// the part of the screen that is used, and the rest can be dead, covered, or
    /// kept for something else. Clamped to `1..=` the physical grid. Content inside
    /// the new area is kept; the cursor and any scroll region are pulled back
    /// inside it.
    pub fn set_view(&mut self, cols: usize, rows: usize) {
        self.hide_cursor();
        let (nc, nr) = (cols.clamp(1, self.cols), rows.clamp(1, self.rows));
        for r in 0..self.rows {
            for c in 0..self.cols {
                if (r >= nr || c >= nc) && self.grid[r][c] != BLANK {
                    self.grid[r][c] = BLANK;
                    self.paint(r, c, false);
                }
            }
        }
        self.vcols = nc;
        self.vrows = nr;
        self.row = self.row.min(nr - 1);
        self.col = self.col.min(nc);
        self.top = 0;
        self.bot = nr - 1;
    }

    /// The baked cut of the default face whose cell is `height` pixels tall
    /// (24, 32, 40 or 48), or `None`.
    #[must_use]
    pub fn font_by_height(height: usize) -> Option<&'static Font> {
        PLEX_CUTS.iter().copied().find(|f| f.height() == height)
    }

    /// Switch to `font` at scale 1 — `/etc/console.conf`'s `font =`.
    ///
    /// Like [`Console::set_margin`]: the grid is recomputed for the current
    /// margin, the screen is cleared (every cell moved), the printing area goes
    /// back to the whole grid, and the new size is reported once through
    /// [`Console::take_geometry`]. `false`, with nothing changed, when the font
    /// would not fit a single cell.
    pub fn set_font(&mut self, font: &'static Font) -> bool {
        let (w, h) = (self.surface.width(), self.surface.height());
        let (mx, my) = (self.origin_x, self.origin_y);
        let (cw, ch) = (font.width(), font.height());
        if cw == 0 || ch == 0 || w.saturating_sub(2 * mx) < cw || h.saturating_sub(2 * my) < ch {
            return false;
        }
        self.hide_cursor();
        self.font = font;
        self.scale = 1;
        self.cols = (w.saturating_sub(2 * mx) / cw).clamp(1, MAX_COLS);
        self.rows = (h.saturating_sub(2 * my) / ch).clamp(1, MAX_ROWS);
        self.vcols = self.cols;
        self.vrows = self.rows;
        self.clear_screen();
        self.geometry_dirty = true;
        true
    }

    /// Move the text area in from the screen edges by `mx` pixels left and right and
    /// `my` top and bottom.
    ///
    /// The grid is **recomputed** — a smaller margin is more columns and rows — and
    /// the screen is cleared, because every cell moved. The printing area goes back
    /// to the whole grid. Clamped to a quarter of each dimension. The new size is
    /// reported once through [`Console::take_geometry`] so the program on the
    /// console can be told (`stty size`).
    pub fn set_margin(&mut self, mx: usize, my: usize) {
        self.hide_cursor();
        let (w, h) = (self.surface.width(), self.surface.height());
        let (mx, my) = (mx.min(w / 4), my.min(h / 4));
        let (cw, ch) = (self.font.width() * self.scale, self.font.height() * self.scale);
        self.cols = (w.saturating_sub(2 * mx) / cw).clamp(1, MAX_COLS);
        self.rows = (h.saturating_sub(2 * my) / ch).clamp(1, MAX_ROWS);
        self.vcols = self.cols;
        self.vrows = self.rows;
        self.origin_x = mx;
        self.origin_y = my;
        self.clear_screen();
        self.geometry_dirty = true;
    }

    /// The printing area as `(rows, columns)` if it changed since the last call
    /// (a new margin), `None` otherwise. The console's owner polls this and tells
    /// the program on the console its new size.
    pub fn take_geometry(&mut self) -> Option<(usize, usize)> {
        core::mem::take(&mut self.geometry_dirty).then_some((self.vrows, self.vcols))
    }

    /// Paint the whole surface — not just the text area — and reset the cursor.
    ///
    /// The whole surface on purpose: the margin is part of what proves the
    /// framebuffer is being written at all, and on a first bring-up "the screen
    /// changed colour" is the entire signal.
    pub fn clear(&mut self) {
        self.clear_screen();
        self.parser = Some(vte::Parser::new_with_size());
    }

    /// [`Console::clear`] without touching the parser — what a sequence handled
    /// *by* the parser has to use.
    fn clear_screen(&mut self) {
        let (w, h) = (self.surface.width(), self.surface.height());
        self.surface.fill(0, 0, w, h, self.bg);
        // Filled in place rather than assigned from a fresh array: the array
        // literal is a temporary the size of the whole grid, and this runs on a
        // boot stack that has no guard page beneath it.
        for row in &mut self.grid {
            row.fill(BLANK);
        }
        self.col = 0;
        self.row = 0;
        self.pen = PEN_DEFAULT;
        self.top = 0;
        self.bot = self.vrows - 1;
        // The fill above painted over it; there is nothing left to erase.
        self.cursor = None;
    }

    /// Redraw the whole screen from the grid: background, then every cell.
    ///
    /// For a surface that **stopped showing** for a while and kept recording —
    /// the amd64 kernel mutes its framebuffer while a program owns `/dev/fb0`
    /// and keeps feeding this console, so the text that arrived meanwhile is in
    /// the grid and nowhere on the glass. Handing the screen back is this call.
    /// The cursor is dropped rather than redrawn; the next idle tick
    /// ([`Console::show_cursor`]) puts it back where the grid says it is.
    pub fn repaint(&mut self) {
        let (w, h) = (self.surface.width(), self.surface.height());
        self.surface.fill(0, 0, w, h, self.bg);
        for row in 0..self.rows {
            for col in 0..self.cols {
                self.paint(row, col, false);
            }
        }
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

    /// Take the bytes the console wants sent **back to the program**: the answer
    /// to a cursor-position request (`CSI 6 n`), a device-attributes query
    /// (`CSI c`) or a colour query (`OSC 10;?` / `OSC 11;?`).
    ///
    /// A terminal answers these on the same line the program reads its keys from,
    /// so the owner of the console must type the bytes into the program's input.
    /// Without it busybox's line editor waits out a timeout at every prompt asking
    /// where the cursor is, and a TUI waits to learn the terminal's colours.
    /// Returns how many bytes were copied into `out` (0 if none pending).
    pub fn take_reply(&mut self, out: &mut [u8]) -> usize {
        let n = self.reply_len.min(out.len());
        out[..n].copy_from_slice(&self.reply[..n]);
        self.reply.copy_within(n..self.reply_len, 0);
        self.reply_len -= n;
        n
    }

    fn reply_push(&mut self, bytes: &[u8]) {
        if self.reply_len + bytes.len() <= REPLY_CAP {
            self.reply[self.reply_len..self.reply_len + bytes.len()].copy_from_slice(bytes);
            self.reply_len += bytes.len();
        }
    }

    /// Append a decimal number to the reply being built in `buf`.
    fn push_dec(buf: &mut [u8], len: &mut usize, mut v: usize) {
        let mut tmp = [0u8; 8];
        let mut n = 0;
        loop {
            tmp[n] = b'0' + (v % 10) as u8;
            n += 1;
            v /= 10;
            if v == 0 || n == tmp.len() {
                break;
            }
        }
        while n > 0 {
            n -= 1;
            buf[*len] = tmp[n];
            *len += 1;
        }
    }

    /// Write one byte: UTF-8 text, `\n \r \t \b`, and the escape sequences a shell
    /// or a TUI uses.
    ///
    /// The bytes go through `vte`'s parser, which decodes UTF-8 and recognises every
    /// CSI/OSC/ESC/DCS sequence; the console only acts on what it dispatches (see
    /// the [`vte::Perform`] impl below). Plain ASCII output — which is all the
    /// kernel's own messages are — comes out exactly as before.
    pub fn write_byte(&mut self, b: u8) {
        self.hide_cursor();
        let mut parser = self.parser.take().unwrap_or_else(vte::Parser::new_with_size);
        parser.advance(&mut Performer(self), &[b]);
        self.parser = Some(parser);
    }

    /// An `ESC <byte>` sequence with no intermediates.
    fn escape(&mut self, b: u8) {
        match b {
            // DECSC / DECRC.
            b'7' => self.saved = (self.row, self.col, self.pen),
            b'8' => self.restore_cursor(),
            // IND, NEL, RI.
            b'D' => self.index(),
            b'E' => {
                self.col = 0;
                self.index();
            }
            b'M' => self.reverse_index(),
            // RIS: full reset.
            b'c' => {
                self.clear();
                self.autowrap = true;
                self.cursor_hidden = false;
                self.alt = false;
            }
            // Keypad modes and the rest: no meaning here, and none may be drawn.
            _ => {}
        }
    }

    /// An `ESC ]` command ended: answer the two colour queries, ignore the rest
    /// (window titles, hyperlinks, ...).
    fn osc(&mut self, params: &[&[u8]]) {
        let (which, c) = match params {
            [b"11", b"?"] => (b'1', self.bg),
            [b"10", b"?"] => (b'0', self.fg),
            _ => return,
        };
        // OSC 1x ; rgb:RRRR/GGGG/BBBB ST — each channel as 16 bits, 8 repeated.
        let mut out = [0u8; 40];
        out[..3].copy_from_slice(b"\x1b]1");
        out[3] = which;
        out[4..9].copy_from_slice(b";rgb:");
        let mut n = 9;
        for (i, v) in [c.r, c.g, c.b].into_iter().enumerate() {
            for _ in 0..2 {
                for nib in [v >> 4, v & 15] {
                    out[n] = b"0123456789abcdef"[usize::from(nib)];
                    n += 1;
                }
            }
            if i < 2 {
                out[n] = b'/';
                n += 1;
            }
        }
        out[n] = 0x1b;
        out[n + 1] = b'\\';
        n += 2;
        self.reply_push(&out[..n]);
    }

    /// A C0 control byte.
    fn control(&mut self, b: u8) {
        match b {
            // LF, VT and FF all move down (and, as always on this console, home
            // the column: the kernel's own messages end in a bare `\n`).
            b'\n' | 0x0B | 0x0C => self.newline(),
            b'\r' => self.col = 0,
            b'\t' => {
                let next = ((self.col / 8 + 1) * 8).min(self.vcols);
                while self.col < next {
                    self.put_cp(0x20);
                }
            }
            // Non-destructive, as on a terminal: the erase is the space the
            // line discipline writes next (`\b \b`), not the backspace.
            0x08 => self.col = self.col.saturating_sub(1),
            // BEL, SO/SI, NUL and the rest: nothing to draw.
            _ => {}
        }
    }

    /// Execute one finished `ESC [ params final` sequence.
    ///
    /// Positions are clamped, never wrapped or scrolled: a program that moves
    /// the cursor off the grid gets the nearest edge, as on a terminal.
    fn csi(&mut self, final_byte: u8, params: &[u16; CSI_PARAMS], count: usize, private: u8) {
        if private == b'?' {
            return self.private_mode(final_byte, params, count);
        }
        if private != 0 {
            // `CSI > c` and friends: parsed, no answer.
            return;
        }
        // A cursor sitting one past the last column is a deferred wrap, not a
        // position; every movement below starts from the last real column.
        let last_col = self.vcols - 1;
        let last_row = self.vrows - 1;
        let col = self.col.min(last_col);
        let p = |i: usize| usize::from(params[i]);
        // Movement counts treat 0 like "absent" and mean 1, per ECMA-48.
        let n = |i: usize| p(i).max(1);
        match final_byte {
            b'A' => {
                self.row = self.row.saturating_sub(n(0));
                self.col = col;
            }
            b'B' | b'e' => {
                self.row = (self.row + n(0)).min(last_row);
                self.col = col;
            }
            b'C' | b'a' => self.col = (col + n(0)).min(last_col),
            b'D' => self.col = col.saturating_sub(n(0)),
            b'E' => {
                self.row = (self.row + n(0)).min(last_row);
                self.col = 0;
            }
            b'F' => {
                self.row = self.row.saturating_sub(n(0));
                self.col = 0;
            }
            b'G' | b'`' => self.col = (n(0) - 1).min(last_col),
            b'd' => self.row = (n(0) - 1).min(last_row),
            b'H' | b'f' => {
                self.row = (n(0) - 1).min(last_row);
                self.col = (n(1) - 1).min(last_col);
            }
            b'J' => match p(0) {
                0 => {
                    self.blank(self.row, col, self.vcols);
                    for r in self.row + 1..self.vrows {
                        self.blank(r, 0, self.vcols);
                    }
                }
                1 => {
                    for r in 0..self.row {
                        self.blank(r, 0, self.vcols);
                    }
                    self.blank(self.row, 0, col + 1);
                }
                2 | 3 => {
                    for r in 0..self.vrows {
                        self.blank(r, 0, self.vcols);
                    }
                }
                _ => {}
            },
            b'K' => match p(0) {
                0 => self.blank(self.row, col, self.vcols),
                1 => self.blank(self.row, 0, col + 1),
                2 => self.blank(self.row, 0, self.vcols),
                _ => {}
            },
            // ECH: erase n characters, in place.
            b'X' => self.blank(self.row, col, (col + n(0)).min(self.vcols)),
            // ICH / DCH: shift the rest of the line right / left.
            b'@' => self.shift_cells(col, n(0), true),
            b'P' => self.shift_cells(col, n(0), false),
            // IL / DL: insert / delete lines within the scroll region.
            b'L' if (self.top..=self.bot).contains(&self.row) => {
                self.scroll_down(self.row, self.bot, n(0));
                self.col = 0;
            }
            b'M' if (self.top..=self.bot).contains(&self.row) => {
                self.scroll_up(self.row, self.bot, n(0));
                self.col = 0;
            }
            // SU / SD.
            b'S' => self.scroll_up(self.top, self.bot, n(0)),
            b'T' => self.scroll_down(self.top, self.bot, n(0)),
            // REP: repeat the last character.
            b'b' => {
                let cp = self.last_cp;
                for _ in 0..n(0).min(self.vcols * 2) {
                    self.put_cp(cp);
                }
            }
            // DECSTBM: set the scroll region, home the cursor.
            b'r' => {
                let top = n(0) - 1;
                let bot = if p(1) == 0 { last_row } else { (p(1) - 1).min(last_row) };
                if top < bot {
                    self.top = top;
                    self.bot = bot;
                    self.row = 0;
                    self.col = 0;
                }
            }
            b's' => self.saved = (self.row, self.col, self.pen),
            b'u' => self.restore_cursor(),
            b'm' => self.sgr(params, count),
            // DSR and DA: the answers go to the program that asked.
            b'n' => match p(0) {
                5 => self.reply_push(b"\x1b[0n"),
                6 => {
                    let mut out = [0u8; 24];
                    out[..2].copy_from_slice(b"\x1b[");
                    let mut len = 2;
                    Self::push_dec(&mut out, &mut len, self.row + 1);
                    out[len] = b';';
                    len += 1;
                    Self::push_dec(&mut out, &mut len, col + 1);
                    out[len] = b'R';
                    len += 1;
                    self.reply_push(&out[..len]);
                }
                _ => {}
            },
            b'c' if p(0) == 0 => self.reply_push(b"\x1b[?1;2c"),
            // Every sequence this console has no use for.
            _ => {}
        }
    }

    /// `CSI ? Pm h` / `l` — DEC private modes. Only the few that change what is
    /// drawn are acted on; mouse, bracketed paste and the rest are accepted and
    /// ignored.
    fn private_mode(&mut self, final_byte: u8, params: &[u16; CSI_PARAMS], count: usize) {
        let on = match final_byte {
            b'h' => true,
            b'l' => false,
            _ => return,
        };
        // `CSI ? 9001 ; x ; y h` — this console's own: set the screen margin to `x`
        // pixels left/right and `y` top/bottom (`y` defaults to `x`); `l` restores
        // the default. Private modes take their parameters as a mode list, so this
        // one is read before the loop and ends it.
        if params[0] == 9001 {
            if on {
                let x = usize::from(params[1]);
                let y = if count >= 2 { usize::from(params[2]) } else { x };
                self.set_margin(x, y);
            } else {
                let (w, h) = (self.surface.width(), self.surface.height());
                let (mx, my) = Self::auto_margin(w, h);
                self.set_margin(mx, my);
            }
            return;
        }
        for &mode in &params[..=count.min(CSI_PARAMS - 1)] {
            match mode {
                7 => self.autowrap = on,
                25 => self.cursor_hidden = !on,
                // The alternate screen. Entering clears and homes (the previous
                // screen cannot be kept — that would cost a second grid); leaving
                // clears again and gives back the saved cursor.
                47 | 1047 | 1049 => {
                    if on {
                        self.saved = (self.row, self.col, self.pen);
                    }
                    self.alt = on;
                    for r in 0..self.vrows {
                        self.blank(r, 0, self.vcols);
                    }
                    self.top = 0;
                    self.bot = self.vrows - 1;
                    if on {
                        self.row = 0;
                        self.col = 0;
                    } else {
                        self.restore_cursor();
                    }
                }
                _ => {}
            }
        }
    }

    fn restore_cursor(&mut self) {
        let (r, c, pen) = self.saved;
        self.row = r.min(self.vrows - 1);
        self.col = c.min(self.vcols);
        self.pen = pen;
    }

    /// `CSI Pm m` — Select Graphic Rendition.
    fn sgr(&mut self, params: &[u16; CSI_PARAMS], count: usize) {
        let n = count.min(CSI_PARAMS - 1) + 1;
        let mut i = 0;
        while i < n {
            let code = params[i];
            i += 1;
            match code {
                0 => self.pen = PEN_DEFAULT,
                1 => self.pen.flags |= F_BOLD,
                2 => self.pen.flags |= F_DIM,
                4 => self.pen.flags |= F_UNDER,
                7 => self.pen.flags |= F_REVERSE,
                21 | 22 => self.pen.flags &= !(F_BOLD | F_DIM),
                24 => self.pen.flags &= !F_UNDER,
                27 => self.pen.flags &= !F_REVERSE,
                30..=37 => {
                    self.pen.fg = (code - 30) as u8;
                    self.pen.flags |= F_FG;
                }
                90..=97 => {
                    self.pen.fg = (code - 90) as u8 + 8;
                    self.pen.flags |= F_FG;
                }
                39 => self.pen.flags &= !F_FG,
                40..=47 => {
                    self.pen.bg = (code - 40) as u8;
                    self.pen.flags |= F_BG;
                }
                100..=107 => {
                    self.pen.bg = (code - 100) as u8 + 8;
                    self.pen.flags |= F_BG;
                }
                49 => self.pen.flags &= !F_BG,
                38 | 48 => {
                    // `5;n` (palette) or `2;r;g;b` (truecolor, stored as the nearest
                    // palette entry).
                    let idx = match params.get(i).copied() {
                        Some(5) if i + 1 < n => {
                            let v = params[i + 1].min(255) as u8;
                            i += 2;
                            Some(v)
                        }
                        Some(2) if i + 3 < n => {
                            let v = rgb_to_palette(
                                params[i + 1].min(255) as u8,
                                params[i + 2].min(255) as u8,
                                params[i + 3].min(255) as u8,
                            );
                            i += 4;
                            Some(v)
                        }
                        _ => None,
                    };
                    if let Some(v) = idx {
                        if code == 38 {
                            self.pen.fg = v;
                            self.pen.flags |= F_FG;
                        } else {
                            self.pen.bg = v;
                            self.pen.flags |= F_BG;
                        }
                    }
                }
                _ => {}
            }
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
        if self.cursor_hidden {
            return;
        }
        // A deferred wrap parks `col` one past the end; the cursor sits on the
        // last real cell, which is where a terminal draws it too.
        let at = (self.row, self.col.min(self.vcols - 1));
        if self.cursor == Some(at) {
            return;
        }
        self.hide_cursor();
        self.paint(at.0, at.1, true);
        self.cursor = Some(at);
    }

    /// Take the cursor down, restoring the cell it covered. A no-op if it is not
    /// drawn. Called first by [`Console::write_byte`], so nothing ever draws
    /// over — or scrolls — a stale cursor.
    pub fn hide_cursor(&mut self) {
        if let Some((r, c)) = self.cursor.take() {
            self.paint(r, c, false);
        }
    }

    /// The cell erase operations leave behind: blank, in the pen's background
    /// (so a coloured bar erased to end of line stays a coloured bar).
    fn blank_cell(&self) -> Cell {
        Cell { cp: 0x20, fg: 0, bg: self.pen.bg, flags: self.pen.flags & F_BG }
    }

    /// Blank `row`'s cells `from..to`, redrawing only those that were not
    /// already blank — the same economy [`Console::scroll_up`] has, for the same
    /// reason: every pixel is an uncached write to video memory.
    fn blank(&mut self, row: usize, mut from: usize, mut to: usize) {
        to = to.min(self.vcols);
        // Never leave half a two-column character behind.
        if from > 0 && from < self.cols && self.grid[row][from].cp == CONT {
            from -= 1;
        }
        if to < self.cols && self.grid[row][to].cp == CONT {
            to += 1;
        }
        let blank = self.blank_cell();
        for c in from..to.min(self.cols) {
            if self.grid[row][c] != blank {
                self.grid[row][c] = blank;
                self.paint(row, c, false);
            }
        }
    }

    /// ICH (`right`) / DCH: move the cells from `col` to the end of the line by
    /// `n`, filling the gap with blanks.
    fn shift_cells(&mut self, col: usize, n: usize, right: bool) {
        let w = self.vcols;
        if col >= w {
            return;
        }
        let n = n.min(w - col);
        let blank = self.blank_cell();
        let row = self.row;
        let old = self.grid[row];
        for c in col..w {
            let next = if right {
                if c >= col + n { old[c - n] } else { blank }
            } else if c + n < w {
                old[c + n]
            } else {
                blank
            };
            self.grid[row][c] = next;
        }
        // A two-column character cut in half by the shift is blanked.
        for c in 0..w {
            if self.grid[row][c].cp == CONT && (c == 0 || self.grid[row][c - 1].cp == CONT) {
                self.grid[row][c] = BLANK;
            }
        }
        for c in col..w {
            if self.grid[row][c] != old[c] {
                self.paint(row, c, false);
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
    /// to know what is on the glass, can ask without reading video memory. A
    /// character beyond the BMP shows as U+FFFD, and the right half of a
    /// two-column character as U+FFFF.
    #[must_use]
    pub fn cell_at(&self, row: usize, col: usize) -> char {
        let cp = self.grid[row][col].cp;
        // An emoji picture reads back as the emoji it is.
        let n = usize::from(cp.wrapping_sub(EMOJI_BASE));
        let real = if n < emoji::count() { emoji::codepoint(n) } else { u32::from(cp) };
        char::from_u32(real).unwrap_or('\u{FFFD}')
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
        self.index();
    }

    /// Move down a row, scrolling the region if the cursor is on its last row.
    fn index(&mut self) {
        if self.row == self.bot {
            // The whole screen scrolling under a log is jumped several rows at a
            // time (see [`Console::SCROLL_ROWS`]); anything that has set a scroll
            // region, or owns the alternate screen, is scrolled one row, which is
            // what such a program counts on.
            let whole = self.top == 0 && self.bot == self.vrows - 1 && !self.alt;
            if whole {
                let shift = Self::SCROLL_ROWS.min(self.vrows - 1);
                self.scroll_up(0, self.vrows - 1, shift);
                // The line that triggered this scroll moved up by `shift`; the
                // next goes just below it, leaving `shift` blank rows to fill
                // before the next scroll.
                self.row = self.vrows - shift;
            } else {
                self.scroll_up(self.top, self.bot, 1);
            }
        } else if self.row + 1 < self.vrows {
            self.row += 1;
        }
    }

    /// RI: up a row, scrolling the region down if the cursor is on its first.
    fn reverse_index(&mut self) {
        if self.row == self.top {
            self.scroll_down(self.top, self.bot, 1);
        } else {
            self.row = self.row.saturating_sub(1);
        }
    }

    /// How many rows a single implicit scroll of the whole screen advances by.
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

    /// Put one character at the cursor and advance it.
    ///
    /// Two-column characters take two cells (the second marked [`CONT`]) and wrap
    /// early if only one is left; zero-width ones are not drawn at all. A
    /// character the console cannot store (beyond the BMP) is kept as a marker so
    /// it still occupies the right number of columns.
    fn put_cp(&mut self, cp: u32) {
        // Emoji sequences are one picture two columns wide, however many code
        // points spell them: a program lays its frame out for that, so the console
        // must take the same space — and not draw the pieces separately.
        match cp {
            // ZWJ: the emoji after it is part of the one before.
            0x200D => {
                self.swallow = self.last_emoji;
                return;
            }
            // VS16: the character before it asks for its emoji form (`❤` -> `❤️`).
            0xFE0F => return self.emoji_presentation(),
            // A skin-tone modifier is part of the emoji before it.
            0x1F3FB..=0x1F3FF if self.last_emoji => return,
            _ => {}
        }
        if core::mem::take(&mut self.swallow)
            && (emoji::index_of(cp).is_some() || glyph::width(cp) == 2)
        {
            return;
        }
        let w = glyph::width(cp);
        if w == 0 {
            return;
        }
        if self.col + w > self.vcols {
            if self.autowrap {
                self.col = 0;
                self.index();
            } else {
                self.col = self.vcols - w;
            }
        }
        let (row, col) = (self.row, self.col);
        // Writing into one half of a two-column character blanks the other half.
        self.detach_wide(row, col);
        if w == 2 {
            self.detach_wide(row, col + 1);
        }
        // A two-column character with a baked picture is stored as that picture.
        let picture = emoji::index_of(cp).filter(|_| w == 2);
        let stored = if let Some(i) = picture {
            EMOJI_BASE + i as u16
        } else if cp > 0xFFFE {
            if w == 2 { TOFU_WIDE } else { TOFU }
        } else {
            cp as u16
        };
        let mut cell = Cell { cp: stored, fg: self.pen.fg, bg: self.pen.bg, flags: self.pen.flags };
        self.grid[row][col] = cell;
        if w == 2 {
            cell.cp = CONT;
            self.grid[row][col + 1] = cell;
        }
        self.paint(row, col, false);
        self.col += w;
        self.last_cp = cp;
        self.last_cell = Some((row, col));
        self.last_emoji = picture.is_some();
    }

    /// VS16 after a narrow character that has an emoji picture: widen it into the
    /// picture, two columns, and move the cursor past it. Anything else (no picture,
    /// already wide, the cursor moved since, no room) is left alone.
    fn emoji_presentation(&mut self) {
        let Some((row, col)) = self.last_cell else { return };
        let cell = self.grid[row][col];
        let Some(i) = emoji::index_of(u32::from(cell.cp)) else { return };
        if self.row != row || self.col != col + 1 || col + 1 >= self.vcols {
            return;
        }
        self.detach_wide(row, col + 1);
        self.grid[row][col].cp = EMOJI_BASE + i as u16;
        let mut right = self.grid[row][col];
        right.cp = CONT;
        self.grid[row][col + 1] = right;
        self.paint(row, col, false);
        self.col = col + 2;
        self.last_emoji = true;
    }

    /// Before overwriting `(row, col)`: if it is half of a two-column character,
    /// blank the other half.
    fn detach_wide(&mut self, row: usize, col: usize) {
        if col >= self.cols {
            return;
        }
        if self.grid[row][col].cp == CONT && col > 0 {
            self.grid[row][col - 1] = BLANK;
            self.paint(row, col - 1, false);
        } else if col + 1 < self.cols && self.grid[row][col + 1].cp == CONT {
            self.grid[row][col + 1] = BLANK;
            self.paint(row, col + 1, false);
        }
    }

    /// Scroll rows `top..=bot` up by `n`, blanking the vacated rows, re-drawing
    /// only the cells that changed.
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
    fn scroll_up(&mut self, top: usize, bot: usize, n: usize) {
        let n = n.min(bot + 1 - top);
        let blank = self.blank_cell();
        for r in top..=bot {
            self.replace_row(r, (r + n <= bot).then_some(r + n), blank);
        }
    }

    /// Scroll rows `top..=bot` down by `n`: the mirror of [`Console::scroll_up`].
    fn scroll_down(&mut self, top: usize, bot: usize, n: usize) {
        let n = n.min(bot + 1 - top);
        let blank = self.blank_cell();
        for r in (top..=bot).rev() {
            self.replace_row(r, (r >= top + n).then(|| r - n), blank);
        }
    }

    /// Rewrite row `r` of the grid from row `source` (or all `blank` if none), then
    /// draw the cells that changed. Two passes: the width of a character is read
    /// from its right neighbour, so the whole row has to be in place before any
    /// cell is drawn.
    fn replace_row(&mut self, r: usize, source: Option<usize>, blank: Cell) {
        let mut changed = [false; MAX_COLS];
        for c in 0..self.vcols {
            let next = source.map_or(blank, |src| self.grid[src][c]);
            changed[c] = self.grid[r][c] != next;
            self.grid[r][c] = next;
        }
        for (c, &ch) in changed.iter().enumerate().take(self.vcols) {
            if ch {
                self.paint(r, c, false);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Drawing
    // -----------------------------------------------------------------------

    /// The colours a cell is drawn in: its own where it sets them, the console's
    /// defaults otherwise, then bold, dim and reverse applied.
    fn resolve(&self, cell: Cell) -> (Rgb, Rgb) {
        let mut fg = if cell.flags & F_FG != 0 { palette(cell.fg) } else { self.fg };
        let bg = if cell.flags & F_BG != 0 { palette(cell.bg) } else { self.bg };
        if cell.flags & F_BOLD != 0 {
            fg = if cell.flags & F_FG == 0 {
                Rgb::WHITE
            } else if cell.fg < 8 {
                palette(cell.fg + 8)
            } else {
                fg
            };
        }
        if cell.flags & F_DIM != 0 {
            fg = bg.blend(fg, 140);
        }
        if cell.flags & F_REVERSE != 0 { (bg, fg) } else { (fg, bg) }
    }

    /// Draw the cell at `(row, col)`; `invert` swaps its colours (the cursor).
    ///
    /// The right half of a two-column character draws nothing: the left half
    /// painted both.
    fn paint(&mut self, row: usize, col: usize, invert: bool) {
        let cell = self.grid[row][col];
        if cell.cp == CONT {
            return;
        }
        let (mut fg, mut bg) = self.resolve(cell);
        if invert {
            core::mem::swap(&mut fg, &mut bg);
        }
        let wide = col + 1 < self.cols && self.grid[row][col + 1].cp == CONT;
        self.draw_glyph(row, col, u32::from(cell.cp), wide, fg, bg);
        if cell.flags & F_UNDER != 0 {
            let cw = self.font.width() * self.scale;
            let ch = self.font.height() * self.scale;
            let x0 = self.origin_x + col * cw;
            let y0 = self.origin_y + row * ch;
            let span = if wide { 2 * cw } else { cw };
            self.surface.fill(x0, y0 + ch - 2 * self.scale, span, self.scale, fg);
        }
    }

    /// Blit one glyph, background included, so a redraw needs no prior clear.
    ///
    /// Characters come from three places, in this order: [`glyph`]'s procedural
    /// drawing (box, blocks, Braille, shapes — exact at any scale), the font, and
    /// the replacement box. Each font pixel carries a coverage value, and a
    /// partly-covered one is drawn as a mix of the two colours (see
    /// [`Rgb::blend`]); the two ends of that range are the overwhelming majority
    /// of pixels and are taken without arithmetic — every pixel here is a write to
    /// uncached video memory, so a multiply that only matters on an edge should
    /// not be paid for the interior.
    fn draw_glyph(&mut self, row: usize, col: usize, cp: u32, wide: bool, fg: Rgb, bg: Rgb) {
        let (fw, fh, sc) = (self.font.width(), self.font.height(), self.scale);
        let (cw, ch) = (fw * sc, fh * sc);
        let x0 = self.origin_x + col * cw;
        let y0 = self.origin_y + row * ch;
        let span = if wide { 2 * cw } else { cw };

        // An emoji picture, two cells wide.
        let marker = cp.wrapping_sub(u32::from(EMOJI_BASE));
        if wide && (marker as usize) < emoji::count() {
            self.draw_emoji(x0, y0, span, ch, marker as usize, bg);
            return;
        }

        // A CJK character: Unifont's 16x16 bitmap, at the largest whole multiple
        // that fits the two-cell box (3x on the usual 48x48), centred.
        if wide && let Some(g) = unifont::glyph(cp) {
            self.surface.fill(x0, y0, span, ch, bg);
            let f = (span / unifont::SIZE).min(ch / unifont::SIZE).max(1);
            let side = unifont::SIZE * f;
            let (ox, oy) = (x0 + span.saturating_sub(side) / 2, y0 + ch.saturating_sub(side) / 2);
            let surface = &mut self.surface;
            unifont::paint(g, f, &mut |x, y, w, h| surface.fill(ox + x, oy + y, w, h, fg));
            return;
        }

        // A character wider than one cell has no glyph here: an outlined box over
        // both cells, so the layout stays right and the gap is visible.
        if wide || cp == u32::from(TOFU_WIDE) {
            self.surface.fill(x0, y0, span, ch, bg);
            let (ix, iy) = (cw / 6, ch / 6);
            let (bx, by, bw, bh) = (x0 + ix, y0 + iy, span - 2 * ix, ch - 2 * iy);
            let t = sc.max(1);
            self.surface.fill(bx, by, bw, t, fg);
            self.surface.fill(bx, by + bh - t, bw, t, fg);
            self.surface.fill(bx, by, t, bh, fg);
            self.surface.fill(bx + bw - t, by, t, bh, fg);
            return;
        }

        match glyph::kind(cp) {
            Some(glyph::Kind::Shape) => {
                for gy in 0..ch {
                    for gx in 0..cw {
                        let cov = glyph::shape_coverage(cp, gx, gy, cw, ch).unwrap_or(0);
                        let color = match cov {
                            0 => bg,
                            255 => fg,
                            c => bg.blend(fg, c),
                        };
                        self.surface.put(x0 + gx, y0 + gy, color);
                    }
                }
            }
            Some(kind) => {
                self.surface.fill(x0, y0, cw, ch, bg);
                let surface = &mut self.surface;
                match kind {
                    glyph::Kind::Box => glyph::paint_box(cp, cw, ch, &mut |x, y, w, h| {
                        surface.fill(x0 + x, y0 + y, w, h, fg);
                    }),
                    glyph::Kind::Block => glyph::paint_block(cp, cw, ch, &mut |x, y, w, h, shade| {
                        let c = if shade == 255 { fg } else { bg.blend(fg, shade) };
                        surface.fill(x0 + x, y0 + y, w, h, c);
                    }),
                    _ => glyph::paint_braille(cp, cw, ch, &mut |x, y, w, h| {
                        surface.fill(x0 + x, y0 + y, w, h, fg);
                    }),
                }
            }
            None => {
                // A script the text font lacks (Greek, Cyrillic, Vietnamese...): Unifont's
                // 8x16 bitmap at a whole multiple of its size — 3x in the usual 24x48 cell.
                if cp >= 0x180
                    && !self.font.draws(cp)
                    && let Some(g) = unifont::narrow(cp)
                {
                    self.surface.fill(x0, y0, cw, ch, bg);
                    let f = (cw / unifont::NARROW_WIDTH).min(ch / unifont::SIZE).max(1);
                    let (gw, gh) = (unifont::NARROW_WIDTH * f, unifont::SIZE * f);
                    let (ox, oy) = (x0 + cw.saturating_sub(gw) / 2, y0 + ch.saturating_sub(gh) / 2);
                    let surface = &mut self.surface;
                    unifont::paint_narrow(g, f, &mut |x, y, w, h| surface.fill(ox + x, oy + y, w, h, fg));
                    return;
                }
                let cell = self.font.cell_cp(cp);
                for gy in 0..fh {
                    for gx in 0..fw {
                        let color = match cell[gy * fw + gx] {
                            0x00 => bg,
                            0xFF => fg,
                            coverage => bg.blend(fg, coverage),
                        };
                        let px = x0 + gx * sc;
                        let py = y0 + gy * sc;
                        if sc == 1 {
                            self.surface.put(px, py, color);
                        } else {
                            self.surface.fill(px, py, sc, sc, color);
                        }
                    }
                }
            }
        }
    }

    /// Draw emoji picture `idx` into the `span` x `ch` box at `(x0, y0)`: the
    /// background, then the picture, square and centred, smoothly scaled and blended
    /// onto it.
    fn draw_emoji(&mut self, x0: usize, y0: usize, span: usize, ch: usize, idx: usize, bg: Rgb) {
        self.surface.fill(x0, y0, span, ch, bg);
        let side = span.min(ch);
        let (ox, oy) = (x0 + (span - side) / 2, y0 + (ch - side) / 2);
        let surface = &mut self.surface;
        emoji::paint(idx, side, side, &mut |x, y, [r, g, b], a| {
            if a > 0 {
                let c = Rgb::new(r, g, b);
                surface.put(ox + x, oy + y, if a == 255 { c } else { bg.blend(c, a) });
            }
        });
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

/// The `vte` side: the parser calls these as it recognises text and sequences.
struct Performer<'a, S: Surface>(&'a mut Console<S>);

impl<S: Surface> vte::Perform for Performer<'_, S> {
    fn print(&mut self, c: char) {
        self.0.put_cp(u32::from(c));
    }

    fn execute(&mut self, byte: u8) {
        self.0.control(byte);
    }

    fn csi_dispatch(&mut self, params: &vte::Params, intermediates: &[u8], ignore: bool, action: char) {
        // A sequence the parser had to truncate, or an action beyond ASCII, is
        // not one this console knows.
        let Ok(action) = u8::try_from(action) else { return };
        if ignore {
            return;
        }
        // `?`, `<`, `=` and `>` arrive as the first intermediate (the private
        // marker). Any other intermediate (`CSI ! p`, `CSI SP q`) makes it a
        // different sequence from the one the final byte names, so it is dropped.
        let private = match intermediates {
            [] => 0,
            [m @ (b'?' | b'<' | b'=' | b'>')] => *m,
            _ => return,
        };
        // Flatten sub-parameters into one list (`38:2::r:g:b` reads as `38;2;;r;g;b`).
        let mut flat = [0u16; CSI_PARAMS];
        let mut n = 0;
        for sub in params {
            for &v in sub {
                if n < CSI_PARAMS {
                    flat[n] = v;
                    n += 1;
                }
            }
        }
        self.0.csi(action, &flat, n.saturating_sub(1), private);
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        // `ESC ( B` and friends select character sets: no meaning here.
        if intermediates.is_empty() && !ignore {
            self.0.escape(byte);
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        self.0.osc(params);
    }
}

impl<S: Surface> fmt::Write for Console<S> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.write_str_bytes(s);
        Ok(())
    }
}
