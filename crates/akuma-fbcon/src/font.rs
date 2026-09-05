//! The console fonts, as tables of per-pixel coverage.
//!
//! Two are baked in and the console picks one at construction:
//!
//! - [`JETBRAINS_MONO`] — 12x24, the default. An outline font rasterized with
//!   anti-aliasing, which is what a face drawn for screens needs to look like
//!   itself. Copyright the JetBrains Mono project, SIL OFL 1.1; full text in
//!   `vendor/jetbrains-mono/OFL.txt`.
//! - [`SPLEEN`] — 8x16, by Frederic Cambus. A monospaced bitmap font designed
//!   for consoles and shipped in OpenBSD base; BSD-2-Clause, full text in
//!   `vendor/spleen/LICENSE`. Half the cell of the default, so it is what to
//!   reach for on a small framebuffer where 24 pixels of height costs rows that
//!   matter.
//!
//! Neither is checked in. Both are git submodules and both tables are generated
//! from the upstream file by `build.rs`, so the submodule stays the source of
//! truth and updating a font is a submodule bump rather than a regenerated blob
//! in a diff.
//!
//! # The table
//!
//! One byte per pixel, `0x00` for untouched and `0xFF` for solid, row-major
//! within a cell, in code-point order from `FIRST` through `LAST`. A byte
//! outside that range renders as the replacement box on the end of the table
//! rather than as whatever follows it — a console that silently draws garbage
//! for a stray byte is worse than one that draws a visible box.
//!
//! Coverage rather than bits costs eight times the bytes (27 KB for JetBrains
//! Mono, 12 KB for Spleen) and buys two things: an outline font that does not
//! have visibly uneven stems, and one drawing path in [`crate::Console`] rather
//! than one per font. Only the font the kernel actually names is linked.
//!
//! Larger Spleen sizes exist upstream (12x24, 16x32, 32x64) and JetBrains Mono
//! will rasterize at any size at all; adding one is a change to `build.rs` and
//! nothing else, since nothing here or in [`crate::Console`] assumes a width.

include!(concat!(env!("OUT_DIR"), "/jetbrains_mono.rs"));
include!(concat!(env!("OUT_DIR"), "/spleen.rs"));

/// A fixed-cell font: one coverage value per pixel, one cell per code point.
pub struct Font {
    name: &'static str,
    width: usize,
    height: usize,
    first: u8,
    last: u8,
    /// `(last - first + 2)` cells of `width * height` bytes. The extra cell on
    /// the end is the replacement box, so an out-of-range byte is an index
    /// rather than a branch into a separate array.
    cells: &'static [u8],
}

impl Font {
    /// Wrap a generated table. Called only by the generated code.
    ///
    /// # Panics
    ///
    /// At compile time, if the table is not one cell per code point plus a
    /// replacement. The generator and this constructor have to agree about the
    /// layout and there is no way to check it later — a short table would read
    /// past its end at runtime, in a kernel, on the path that reports failures.
    #[must_use]
    pub const fn new(
        name: &'static str,
        width: usize,
        height: usize,
        first: u8,
        last: u8,
        cells: &'static [u8],
    ) -> Self {
        assert!(width > 0 && height > 0, "a font with no pixels in a cell");
        assert!(first <= last, "a font whose range runs backwards");
        assert!(
            cells.len() == (last as usize - first as usize + 2) * width * height,
            "the generated table is not one cell per code point plus a replacement"
        );
        Self { name, width, height, first, last, cells }
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

    /// One glyph's coverage, row-major, [`Font::width`] * [`Font::height`] bytes.
    ///
    /// A byte outside the font's range gives the replacement box.
    #[must_use]
    pub fn cell(&self, byte: u8) -> &'static [u8] {
        let index = if byte < self.first || byte > self.last {
            (self.last - self.first + 1) as usize
        } else {
            (byte - self.first) as usize
        };
        let size = self.width * self.height;
        let start = index * size;
        &self.cells[start..start + size]
    }

    /// How much of pixel `(x, y)` of `byte`'s glyph is ink, from 0 to 255.
    ///
    /// Out-of-cell coordinates read as untouched rather than panicking: this is
    /// on the console's drawing path, and a kernel that panics while printing
    /// has nothing left to print with.
    #[must_use]
    pub fn coverage(&self, byte: u8, x: usize, y: usize) -> u8 {
        if x >= self.width || y >= self.height {
            return 0;
        }
        self.cell(byte)[y * self.width + x]
    }
}
