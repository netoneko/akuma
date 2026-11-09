//! Smooth glyphs for what IBM Plex Mono does not draw.
//!
//! CJK, and the symbols, Greek and Latin Extended that Plex lacks, anti-aliased like the
//! Plex text. Unifont's bitmaps ([`crate::unifont`]) are 1 bit a pixel and stay as the
//! last fallback.
//!
//! # The data
//!
//! Four files under `vendor/hires/`, from `tests/bake_hires.rs` and committed (the source
//! fonts are not: Noto Sans CJK is 16 MB). Sources and licences: `PROVENANCE.txt` there.
//!
//! * `sym-24x48.{idx,bin}` — DejaVu Sans Mono, 24x48 cells of 8-bit coverage on the same
//!   baseline as Plex's 24x48 cut. [`sym`] returns one; [`resample`] fits it to any cell.
//! * `cjk.idx`, `cjk-22.bin`, `cjk-40.bin` — Noto Sans CJK SC, the em square in a 22x22 and
//!   a 40x40 box (the two-column cell of the 11x22 and 20x40 defaults), **4-bit** coverage,
//!   two pixels a byte, high nibble first. [`cjk`] picks the nearer size to draw from.
//!
//! The `.idx` files are sorted little-endian `u16` code points; a glyph's number in the
//! `.bin` is its position there, so a lookup is a binary search over a few thousand
//! entries and nothing is allocated. All of it is `static` data in the kernel image.
//!
//! Only the common set is here (GB2312, JIS X 0208 and KS X 1001 Hangul, kana, fullwidth
//! forms): any other code point is `None` and the caller falls through to Unifont.

const SYM_IDX: &[u8] = include_bytes!("../vendor/hires/sym-24x48.idx");
const SYM_BIN: &[u8] = include_bytes!("../vendor/hires/sym-24x48.bin");
const CJK_IDX: &[u8] = include_bytes!("../vendor/hires/cjk.idx");
const CJK_22: &[u8] = include_bytes!("../vendor/hires/cjk-22.bin");
const CJK_40: &[u8] = include_bytes!("../vendor/hires/cjk-40.bin");

/// Width and height of a symbol cell, in pixels.
pub const SYM_W: usize = 24;
/// See [`SYM_W`].
pub const SYM_H: usize = 48;
/// Bytes of one symbol cell (8-bit coverage).
const SYM_BYTES: usize = SYM_W * SYM_H;
/// The two baked CJK box sides.
pub const CJK_SMALL: usize = 22;
/// See [`CJK_SMALL`].
pub const CJK_LARGE: usize = 40;

const fn cjk_bytes(side: usize) -> usize {
    (side * side).div_ceil(2)
}

// A table that disagrees with its index would read past its end at draw time.
const _: () = assert!(SYM_IDX.len().is_multiple_of(2) && CJK_IDX.len().is_multiple_of(2), "an index is whole u16s");
const _: () = assert!(SYM_BIN.len() == SYM_IDX.len() / 2 * SYM_BYTES, "sym-24x48.bin does not match its index");
const _: () = assert!(CJK_22.len() == CJK_IDX.len() / 2 * cjk_bytes(CJK_SMALL), "cjk-22.bin does not match cjk.idx");
const _: () = assert!(CJK_40.len() == CJK_IDX.len() / 2 * cjk_bytes(CJK_LARGE), "cjk-40.bin does not match cjk.idx");

/// Position of `cp` in a sorted `u16` index, or `None`.
fn find(idx: &[u8], cp: u32) -> Option<usize> {
    let cp = u16::try_from(cp).ok()?;
    let (mut lo, mut hi) = (0usize, idx.len() / 2);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let v = u16::from_le_bytes([idx[2 * mid], idx[2 * mid + 1]]);
        match v.cmp(&cp) {
            core::cmp::Ordering::Equal => return Some(mid),
            core::cmp::Ordering::Less => lo = mid + 1,
            core::cmp::Ordering::Greater => hi = mid,
        }
    }
    None
}

/// The 24x48 coverage cell for a symbol, Greek or Latin Extended letter, or `None`.
/// `SYM_W * SYM_H` bytes, row-major, 0 = empty, 255 = solid.
#[must_use]
pub fn sym(cp: u32) -> Option<&'static [u8]> {
    let i = find(SYM_IDX, cp)?;
    Some(&SYM_BIN[i * SYM_BYTES..(i + 1) * SYM_BYTES])
}

/// A CJK glyph: 4-bit coverage in a square box of `side` pixels.
#[derive(Clone, Copy, Debug)]
pub struct Cjk {
    bits: &'static [u8],
    side: usize,
}

impl Cjk {
    /// Edge of the box this glyph was baked in.
    #[must_use]
    pub fn side(&self) -> usize {
        self.side
    }

    /// Coverage at `(x, y)`, 0..=255 (the 4-bit value times 17).
    #[must_use]
    pub fn at(&self, x: usize, y: usize) -> u8 {
        let i = y * self.side + x;
        let byte = self.bits[i / 2];
        let nibble = if i.is_multiple_of(2) { byte >> 4 } else { byte & 15 };
        nibble * 17
    }
}

/// The CJK glyph for `cp`, from the baked size nearer to `target` pixels a side, or `None`
/// if `cp` is not in the common set.
#[must_use]
pub fn cjk(cp: u32, target: usize) -> Option<Cjk> {
    let i = find(CJK_IDX, cp)?;
    // The geometric middle of 22 and 40 is ~30: below it the small box is closer in
    // scale, above it the large one is, and shrinking beats stretching.
    let (table, side) = if target <= 30 { (CJK_22, CJK_SMALL) } else { (CJK_40, CJK_LARGE) };
    let n = cjk_bytes(side);
    Some(Cjk { bits: &table[i * n..(i + 1) * n], side })
}

/// Fit an `sw` x `sh` coverage image into `dw` x `dh` pixels by exact area averaging, and
/// report every destination pixel with any ink as `put(x, y, coverage)`.
///
/// The same method as [`crate::unifont::paint_symbol`] but on 8-bit input and with the two
/// axes independent, so a 24x48 symbol can go into any cell. Source pixel `s` spans
/// `[s * dw, (s + 1) * dw)` and destination pixel `d` spans `[d * sw, (d + 1) * sw)` on a
/// common axis; a destination pixel's coverage is the overlap-weighted mean of the
/// source pixels it touches. Integer arithmetic, no allocation. At equal sizes it is a copy.
pub fn resample(
    (sw, sh): (usize, usize),
    (dw, dh): (usize, usize),
    at: impl Fn(usize, usize) -> u8,
    put: &mut impl FnMut(usize, usize, u8),
) {
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
        return;
    }
    let overlap = |d: usize, s: usize, sn: usize, dn: usize| -> usize {
        let (d0, d1) = (d * sn, (d + 1) * sn);
        let (s0, s1) = (s * dn, (s + 1) * dn);
        d1.min(s1).saturating_sub(d0.max(s0))
    };
    let area = sw * sh;
    for dy in 0..dh {
        let (sy0, sy1) = (dy * sh / dh, ((dy + 1) * sh).div_ceil(dh).min(sh));
        for dx in 0..dw {
            let (sx0, sx1) = (dx * sw / dw, ((dx + 1) * sw).div_ceil(dw).min(sw));
            let mut acc = 0usize;
            for sy in sy0..sy1 {
                let oy = overlap(dy, sy, sh, dh);
                for sx in sx0..sx1 {
                    let c = at(sx, sy);
                    if c != 0 {
                        acc += oy * overlap(dx, sx, sw, dw) * usize::from(c);
                    }
                }
            }
            if acc > 0 {
                put(dx, dy, ((acc + area / 2) / area).clamp(1, 255) as u8);
            }
        }
    }
}
