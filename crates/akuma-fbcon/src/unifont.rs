//! GNU Unifont glyphs: CJK (two columns) and the scripts the text fonts lack (one).
//!
//! The text fonts (IBM Plex Mono, Spleen) have no CJK, and a CJK font that fits a
//! 12x24 cell does not exist — these characters are two columns wide and want a
//! square. Unifont's 16x16 bitmaps are exactly that, and at the console's usual
//! scale a two-cell box is 48x48, which is **3x** 16: the glyph is drawn at an
//! integer multiple with no resampling, so strokes stay crisp. The same holds for
//! the 8x16 narrow glyphs against a 24x48 cell, which is why Unifont's *narrow*
//! glyphs are also here: IBM Plex Mono as vendored has one Greek letter and about
//! two thirds of Cyrillic, and Spleen less, so Greek, Cyrillic, Latin Extended-B,
//! IPA and Vietnamese fall back to these.
//!
//! # The data
//!
//! Two files under `vendor/unifont/`, from `scripts/bake_unifont.py` and committed:
//!
//! * `wide-16.bin` — for each code point in [`WIDE`], 32 bytes (16 rows of 16 pixels,
//!   two bytes a row). About 1 MB: the BMP's kana, ideographs, Hangul and fullwidth
//!   forms as bitmaps.
//! * `narrow-8.bin` — for each code point in [`NARROW`], 16 bytes (16 rows of 8
//!   pixels). 16 KB.
//!
//! Most significant bit leftmost, 1 = ink. Unifont is used under the SIL OFL 1.1
//! (`vendor/unifont/LICENSE-OFL-1.1.txt`).

/// The wide ranges baked into `wide-16.bin`, in storage order. `bake_unifont.py` has the same list.
pub const WIDE: &[(u32, u32)] = &[
    (0x3000, 0x30FF), // CJK symbols and punctuation, Hiragana, Katakana
    (0x4E00, 0x9FFF), // CJK Unified Ideographs
    (0xAC00, 0xD7A3), // Hangul syllables
    (0xFF01, 0xFF60), // fullwidth forms
];

/// The narrow ranges baked into `narrow-8.bin`.
pub const NARROW: &[(u32, u32)] = &[
    (0x0180, 0x02AF), // Latin Extended-B, IPA extensions
    (0x0370, 0x03FF), // Greek and Coptic
    (0x0400, 0x052F), // Cyrillic and Cyrillic Supplement
    (0x1E00, 0x1EFF), // Latin Extended Additional (Vietnamese)
];

const WIDE_BITS: &[u8] = include_bytes!("../vendor/unifont/wide-16.bin");
const NARROW_BITS: &[u8] = include_bytes!("../vendor/unifont/narrow-8.bin");

/// Bytes per wide glyph.
pub const GLYPH_BYTES: usize = 32;
/// Bytes per narrow glyph.
pub const NARROW_BYTES: usize = 16;
/// Height of every glyph, and the width of a wide one, in pixels.
pub const SIZE: usize = 16;
/// Width of a narrow glyph in pixels.
pub const NARROW_WIDTH: usize = 8;

const fn total(ranges: &[(u32, u32)]) -> usize {
    let mut n = 0;
    let mut i = 0;
    while i < ranges.len() {
        n += (ranges[i].1 - ranges[i].0 + 1) as usize;
        i += 1;
    }
    n
}

// A table that disagrees with its ranges would index past its end at draw time.
const _: () = assert!(WIDE_BITS.len() == total(WIDE) * GLYPH_BYTES, "wide-16.bin does not match WIDE");
const _: () = assert!(NARROW_BITS.len() == total(NARROW) * NARROW_BYTES, "narrow-8.bin does not match NARROW");

/// U+3000 IDEOGRAPHIC SPACE: legitimately blank, so not "missing".
const IDEOGRAPHIC_SPACE: u32 = 0x3000;

/// Find `cp` in `ranges` over `bits` of `bytes` a glyph; `None` if outside or the
/// glyph is empty (and not allowed to be).
fn lookup(
    ranges: &[(u32, u32)],
    bits: &'static [u8],
    bytes: usize,
    cp: u32,
    allow_blank: bool,
) -> Option<&'static [u8]> {
    let mut base = 0usize;
    for &(first, last) in ranges {
        if (first..=last).contains(&cp) {
            let i = (base + (cp - first) as usize) * bytes;
            let g = &bits[i..i + bytes];
            return (allow_blank || g.iter().any(|&b| b != 0)).then_some(g);
        }
        base += (last - first + 1) as usize;
    }
    None
}

/// The 16x16 bitmap for `cp`, or `None` if `cp` is outside the table or Unifont has
/// no glyph for it (an empty bitmap that is not the ideographic space).
///
/// 32 bytes: row `y` is `bytes[2y] << 8 | bytes[2y + 1]`, bit 15 leftmost.
#[must_use]
pub fn glyph(cp: u32) -> Option<&'static [u8]> {
    lookup(WIDE, WIDE_BITS, GLYPH_BYTES, cp, cp == IDEOGRAPHIC_SPACE)
}

/// The 8x16 bitmap for `cp` (Greek, Cyrillic, Latin Extended-B, IPA, Vietnamese), or
/// `None`. 16 bytes, one a row, bit 7 leftmost.
#[must_use]
pub fn narrow(cp: u32) -> Option<&'static [u8]> {
    lookup(NARROW, NARROW_BITS, NARROW_BYTES, cp, false)
}

/// Draw a narrow glyph at integer scale `f`, by horizontal runs, like [`paint`].
pub fn paint_narrow(g: &[u8], f: usize, fill: &mut impl FnMut(usize, usize, usize, usize)) {
    for (y, &row) in g.iter().enumerate().take(SIZE) {
        let mut x = 0;
        while x < NARROW_WIDTH {
            if row & (0x80 >> x) == 0 {
                x += 1;
                continue;
            }
            let start = x;
            while x < NARROW_WIDTH && row & (0x80 >> x) != 0 {
                x += 1;
            }
            fill(start * f, y * f, (x - start) * f, f);
        }
    }
}

/// Draw `g` at an integer scale `f` (`>= 1`) by calling `fill(x, y, w, h)` for each
/// horizontal run of ink, so a glyph costs tens of fills, not hundreds.
pub fn paint(g: &[u8], f: usize, fill: &mut impl FnMut(usize, usize, usize, usize)) {
    for y in 0..SIZE {
        let row = u16::from_be_bytes([g[2 * y], g[2 * y + 1]]);
        let mut x = 0;
        while x < SIZE {
            if row & (0x8000 >> x) == 0 {
                x += 1;
                continue;
            }
            let start = x;
            while x < SIZE && row & (0x8000 >> x) != 0 {
                x += 1;
            }
            fill(start * f, y * f, (x - start) * f, f);
        }
    }
}
