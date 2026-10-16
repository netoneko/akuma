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

/// The symbol ranges: arrows, math operators, technical, shapes, dingbats.
///
/// Baked into **both** `narrow-8.bin` (the glyphs Unifont draws 8 pixels wide) and
/// `symbol-16.bin` (the ones it draws 16 wide — Claude Code's `⏺ ✻ ✽ ✶ ✳ ✢ ⏵ ➜`).
/// A code point is empty in the table that does not have it.
pub const SYMBOLS: &[(u32, u32)] = &[
    (0x2040, 0x2070), // general punctuation: low asterisk...
    (0x2100, 0x218F), // letterlike symbols, number forms
    (0x2190, 0x21FF), // arrows
    (0x2200, 0x23FF), // mathematical operators, miscellaneous technical
    (0x25A0, 0x25FF), // geometric shapes
    (0x2600, 0x27BF), // miscellaneous symbols, dingbats
    (0x27F0, 0x27FF), // supplemental arrows-A
    (0x2900, 0x297F), // supplemental arrows-B
    (0x2B00, 0x2BFF), // miscellaneous symbols and arrows
];

/// The narrow ranges baked into `narrow-8.bin`: the scripts the text fonts lack,
/// then [`SYMBOLS`] — `bake_unifont.py` builds the same list.
pub const NARROW: &[(u32, u32)] = &[
    (0x0180, 0x02AF), // Latin Extended-B, IPA extensions
    (0x0370, 0x03FF), // Greek and Coptic
    (0x0400, 0x052F), // Cyrillic and Cyrillic Supplement
    (0x1E00, 0x1EFF), // Latin Extended Additional (Vietnamese)
    (0x2040, 0x2070),
    (0x2100, 0x218F),
    (0x2190, 0x21FF),
    (0x2200, 0x23FF),
    (0x25A0, 0x25FF),
    (0x2600, 0x27BF),
    (0x27F0, 0x27FF),
    (0x2900, 0x297F),
    (0x2B00, 0x2BFF),
];

const WIDE_BITS: &[u8] = include_bytes!("../vendor/unifont/wide-16.bin");
const NARROW_BITS: &[u8] = include_bytes!("../vendor/unifont/narrow-8.bin");
const SYMBOL_BITS: &[u8] = include_bytes!("../vendor/unifont/symbol-16.bin");

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
const _: () = assert!(SYMBOL_BITS.len() == total(SYMBOLS) * GLYPH_BYTES, "symbol-16.bin does not match SYMBOLS");

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

/// The 16x16 bitmap for a symbol Unifont draws wide (`⏺ ✻ ✽ ➜ ⚠`...).
///
/// `None` if `cp` is not a symbol in the table or Unifont draws it narrow (see
/// [`narrow`]). 32 bytes, laid out like [`glyph`]. A symbol is one terminal column wide, so it
/// is meant to be scaled into the cell: see [`paint_symbol`].
#[must_use]
pub fn symbol(cp: u32) -> Option<&'static [u8]> {
    lookup(SYMBOLS, SYMBOL_BITS, GLYPH_BYTES, cp, false)
}

/// Scale a 16x16 glyph down to `side` x `side` pixels and report each pixel with
/// any ink as `put(x, y, coverage)`, coverage 1..=255, by exact area averaging.
///
/// Integer scaling cannot do this: a symbol is one column wide, which is under 16
/// pixels at every console size but the biggest, and halving a 1-pixel stroke
/// without averaging deletes it. Each destination pixel covers a `16 / side` square
/// of the source; its coverage is the inked fraction of that square. Integer
/// arithmetic only (source pixel `s` spans `[s * side, (s + 1) * side)` and
/// destination pixel `d` spans `[d * 16, (d + 1) * 16)` on a common axis).
pub fn paint_symbol(g: &[u8], side: usize, put: &mut impl FnMut(usize, usize, u8)) {
    if side == 0 {
        return;
    }
    let overlap = |d: usize, s: usize| -> usize {
        let (d0, d1) = (d * SIZE, (d + 1) * SIZE);
        let (s0, s1) = (s * side, (s + 1) * side);
        d1.min(s1).saturating_sub(d0.max(s0))
    };
    for dy in 0..side {
        let (sy0, sy1) = (dy * SIZE / side, ((dy + 1) * SIZE).div_ceil(side).min(SIZE));
        for dx in 0..side {
            let (sx0, sx1) = (dx * SIZE / side, ((dx + 1) * SIZE).div_ceil(side).min(SIZE));
            let mut ink = 0usize;
            for sy in sy0..sy1 {
                let row = u16::from_be_bytes([g[2 * sy], g[2 * sy + 1]]);
                let oy = overlap(dy, sy);
                for sx in sx0..sx1 {
                    if row & (0x8000 >> sx) != 0 {
                        ink += oy * overlap(dx, sx);
                    }
                }
            }
            if ink > 0 {
                // The destination pixel's area is `SIZE * SIZE` source-units.
                put(dx, dy, ((ink * 255 + SIZE * SIZE / 2) / (SIZE * SIZE)).clamp(1, 255) as u8);
            }
        }
    }
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
