//! The console fonts, as tables of per-pixel coverage.
//!
//! Two are baked in and the console picks one at construction:
//!
//! - [`IBM_PLEX_MONO`] — 12x24, the default. An outline font rasterized with
//!   anti-aliasing, which is what a face drawn for screens needs to look like
//!   itself. © IBM Corp., SIL OFL 1.1; full text in
//!   `vendor/ibm-plex-mono/LICENSE.txt`.
//! - [`SPLEEN`] — 8x16, by Frederic Cambus. A monospaced bitmap font designed
//!   for consoles and shipped in OpenBSD base; BSD-2-Clause, full text in
//!   `vendor/spleen/LICENSE`. Half the cell of the default, so it is what to
//!   reach for on a small framebuffer where 24 pixels of height costs rows that
//!   matter.
//!
//! IBM Plex Mono is vendored as one `.ttf` file (`build.rs` explains why not a
//! submodule); Spleen is a submodule. Both tables are generated from the file
//! by `build.rs`, never hand-written.
//!
//! # The table
//!
//! One byte per pixel, `0x00` for untouched and `0xFF` for solid, row-major
//! within a cell, in the order of the generated range list (printable ASCII,
//! Latin-1, Latin Extended-A, punctuation, arrows, shapes, a few symbols — see
//! `build.rs`). A code point outside those ranges renders as the replacement box
//! on the end of the table rather than as whatever follows it — a console that
//! silently draws garbage for a stray character is worse than one that draws a
//! visible box. Box drawing, block elements and Braille are not in the tables:
//! the console draws those itself.
//!
//! Coverage rather than bits costs eight times the bytes (about 170 KB for IBM
//! Plex Mono and 75 KB for Spleen over the current ranges) and buys two things: an outline font that does not
//! have visibly uneven stems, and one drawing path in [`crate::Console`] rather
//! than one per font. Only the font the kernel actually names is linked.
//!
//! Larger Spleen sizes exist upstream (12x24, 16x32, 32x64) and IBM Plex Mono
//! will rasterize at any size at all; adding one is a change to `build.rs` and
//! nothing else, since nothing here or in [`crate::Console`] assumes a width.

include!(concat!(env!("OUT_DIR"), "/ibm_plex_mono.rs"));
include!(concat!(env!("OUT_DIR"), "/spleen.rs"));

/// A fixed-cell font: one coverage value per pixel, one cell per baked code point.
pub struct Font {
    name: &'static str,
    width: usize,
    height: usize,
    /// Inclusive `(first, last)` code-point ranges, in the order their cells are
    /// stored. The generator and this lookup walk the same list, so the order is
    /// part of the table's format.
    ranges: &'static [(u32, u32)],
    /// One cell of `width * height` bytes per code point in `ranges`, then the
    /// replacement box. An absent code point is an index rather than a branch
    /// into a separate array.
    cells: &'static [u8],
}

/// Cells a range list covers.
const fn cells_in(ranges: &[(u32, u32)]) -> usize {
    let mut n = 0;
    let mut i = 0;
    while i < ranges.len() {
        n += (ranges[i].1 - ranges[i].0 + 1) as usize;
        i += 1;
    }
    n
}

impl Font {
    /// Wrap a generated table. Called only by the generated code.
    ///
    /// # Panics
    ///
    /// At compile time, if the table is not one cell per code point in `ranges`
    /// plus a replacement. The generator and this constructor have to agree about
    /// the layout and there is no way to check it later — a short table would read
    /// past its end at runtime, in a kernel, on the path that reports failures.
    #[must_use]
    pub const fn new(
        name: &'static str,
        width: usize,
        height: usize,
        ranges: &'static [(u32, u32)],
        cells: &'static [u8],
    ) -> Self {
        assert!(width > 0 && height > 0, "a font with no pixels in a cell");
        assert!(
            cells.len() == (cells_in(ranges) + 1) * width * height,
            "the generated table is not one cell per code point plus a replacement"
        );
        Self { name, width, height, ranges, cells }
    }

    /// What to call this font in a boot message.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// Pixels across one cell.
    #[must_use]
    pub const fn width(&self) -> usize {
        self.width
    }

    /// Pixels down one cell.
    #[must_use]
    pub const fn height(&self) -> usize {
        self.height
    }

    /// The cell index of `cp`, or `None` if no table holds it.
    fn index_of(&self, cp: u32) -> Option<usize> {
        let mut base = 0usize;
        for &(first, last) in self.ranges {
            if cp >= first && cp <= last {
                return Some(base + (cp - first) as usize);
            }
            base += (last - first + 1) as usize;
        }
        None
    }

    /// Does this font carry a glyph for `cp`? (`false` means the replacement box.)
    #[must_use]
    pub fn has(&self, cp: u32) -> bool {
        self.index_of(cp).is_some()
    }

    /// Does this font really draw `cp` — it is in the table **and** is not the
    /// replacement box standing in for a glyph the face lacks? (The tables are
    /// generated with the box in every slot the typeface had no glyph for, so
    /// [`Font::has`] alone cannot say.)
    #[must_use]
    pub fn draws(&self, cp: u32) -> bool {
        self.has(cp) && self.cell_cp(cp) != self.cell_cp(u32::MAX)
    }

    /// One glyph's coverage, row-major, [`Font::width`] * [`Font::height`] bytes.
    ///
    /// A code point the font does not carry gives the replacement box.
    #[must_use]
    pub fn cell_cp(&self, cp: u32) -> &'static [u8] {
        let index = self.index_of(cp).unwrap_or_else(|| cells_in(self.ranges));
        let size = self.width * self.height;
        let start = index * size;
        &self.cells[start..start + size]
    }

    /// [`Font::cell_cp`] for a single byte, as Latin-1: `0x20..=0x7E` is ASCII,
    /// `0xA0..=0xFF` Latin-1, the rest the replacement box. Kept for callers that
    /// hold bytes, not code points.
    #[must_use]
    pub fn cell(&self, byte: u8) -> &'static [u8] {
        self.cell_cp(u32::from(byte))
    }

    /// How much of pixel `(x, y)` of `byte`'s glyph is ink, from 0 to 255.
    ///
    /// Out-of-cell coordinates read as untouched rather than panicking: this is
    /// on the console's drawing path, and a kernel that panics while printing
    /// has nothing left to print with.
    #[must_use]
    pub fn coverage(&self, byte: u8, x: usize, y: usize) -> u8 {
        self.coverage_cp(u32::from(byte), x, y)
    }

    /// [`Font::coverage`] by code point.
    #[must_use]
    pub fn coverage_cp(&self, cp: u32, x: usize, y: usize) -> u8 {
        if x >= self.width || y >= self.height {
            return 0;
        }
        self.cell_cp(cp)[y * self.width + x]
    }
}
