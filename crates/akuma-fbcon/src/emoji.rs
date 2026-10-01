//! Colour emoji: a baked set of small images, and a smooth scaler to draw them.
//!
//! A text font cannot draw an emoji — they are pictures, in colour — so the console
//! carries a curated set of them as bitmaps: about 650 of the ones a chat client and
//! a shell prompt print (faces, hands, hearts, symbols, animals, food, objects,
//! coloured circles). Everything else still shows as an outlined box two cells wide,
//! which keeps the layout right even where the picture is missing.
//!
//! # The data
//!
//! Two files under `vendor/noto-emoji/`, produced by `scripts/bake_emoji.py` and
//! committed (so no build needs Pillow or the network):
//!
//! * `emoji-24.index` — the code points, ascending, `u32` little-endian;
//! * `emoji-24.rgba4444` — one 24x24 image per code point, 16 bits a pixel (4 bits
//!   each of R, G, B and straight alpha), row-major.
//!
//! The images are Google's Noto Emoji (Apache-2.0, see the licence files beside
//! them), downscaled with a premultiplied Lanczos filter. 24 pixels is the right
//! base: two cells of the default 12x24 font, so at the usual scale of 2 an emoji
//! is drawn 48x48 — scaled up smoothly, not replicated.
//!
//! # Drawing
//!
//! [`paint`] bilinearly resamples an image to any `w x h` and hands each pixel to a
//! callback as a straight colour and an alpha. Interpolation is on **premultiplied**
//! values, so a transparent texel's hidden colour never bleeds into an edge.

const INDEX: &[u8] = include_bytes!("../vendor/noto-emoji/emoji-24.index");
const PIXELS: &[u8] = include_bytes!("../vendor/noto-emoji/emoji-24.rgba4444");

/// Side of a baked image, in pixels.
pub const SIZE: usize = 24;
const IMAGE_BYTES: usize = SIZE * SIZE * 2;

// The two files must describe the same set: a short pixel file would read past its
// end at draw time, in a kernel, on the path that reports failures.
#[allow(clippy::manual_is_multiple_of)] // `is_multiple_of` is not const on stable
const _: () = assert!(INDEX.len() % 4 == 0, "emoji index is not whole u32s");
const _: () = assert!(PIXELS.len() == INDEX.len() / 4 * IMAGE_BYTES, "emoji index and pixels disagree");

/// How many emoji are baked.
#[must_use]
pub const fn count() -> usize {
    INDEX.len() / 4
}

/// The code point of image `i`.
#[must_use]
pub fn codepoint(i: usize) -> u32 {
    let b = &INDEX[i * 4..i * 4 + 4];
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// The image for `cp`, if one is baked.
#[must_use]
pub fn index_of(cp: u32) -> Option<usize> {
    // Binary search over the sorted index.
    let (mut lo, mut hi) = (0usize, count());
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        match codepoint(mid).cmp(&cp) {
            core::cmp::Ordering::Equal => return Some(mid),
            core::cmp::Ordering::Less => lo = mid + 1,
            core::cmp::Ordering::Greater => hi = mid,
        }
    }
    None
}

/// One texel of image `i` as straight `[r, g, b, a]`, each 0..=255.
#[must_use]
#[allow(clippy::many_single_char_names)]
pub fn texel(i: usize, x: usize, y: usize) -> [u8; 4] {
    let o = i * IMAGE_BYTES + (y.min(SIZE - 1) * SIZE + x.min(SIZE - 1)) * 2;
    let v = u16::from_le_bytes([PIXELS[o], PIXELS[o + 1]]);
    // A 4-bit channel `n` back to 8 bits is `n * 17` (0 -> 0, 15 -> 255).
    let ch = |shift: u32| (((v >> shift) & 0xF) * 17) as u8;
    [ch(12), ch(8), ch(4), ch(0)]
}

/// Resample image `i` to `w` x `h` pixels, calling `put(x, y, [r, g, b], alpha)` for
/// every one. Bilinear, centre-aligned, on premultiplied values.
#[allow(clippy::many_single_char_names, clippy::cast_possible_wrap)] // r/g/b/a; sizes are a few hundred
pub fn paint(i: usize, w: usize, h: usize, put: &mut impl FnMut(usize, usize, [u8; 3], u8)) {
    if w == 0 || h == 0 {
        return;
    }
    let last = SIZE as i64 - 1;
    // Where target pixel `p` of `n` falls in the source, in 1/256 texels, aligned
    // by pixel centres: `(p + 0.5) * SIZE / n - 0.5`.
    let src = |p: usize, n: usize| ((2 * p as i64 + 1) * SIZE as i64 * 256) / (2 * n as i64) - 128;
    for y in 0..h {
        let sy = src(y, h);
        let (y0, fy) = (sy.div_euclid(256), sy.rem_euclid(256));
        for x in 0..w {
            let sx = src(x, w);
            let (x0, fx) = (sx.div_euclid(256), sx.rem_euclid(256));
            let (mut r, mut g, mut b, mut a) = (0u64, 0u64, 0u64, 0u64);
            for (dy, wy) in [(0i64, 256 - fy), (1, fy)] {
                for (dx, wx) in [(0i64, 256 - fx), (1, fx)] {
                    let tx = (x0 + dx).clamp(0, last) as usize;
                    let ty = (y0 + dy).clamp(0, last) as usize;
                    let [tr, tg, tb, ta] = texel(i, tx, ty);
                    let wgt = (wx * wy) as u64;
                    let wa = wgt * u64::from(ta);
                    r += wa * u64::from(tr);
                    g += wa * u64::from(tg);
                    b += wa * u64::from(tb);
                    a += wa;
                }
            }
            // `a` is alpha * 65536; the colour sums are alpha-weighted, so dividing
            // by `a` un-premultiplies.
            let alpha = (a / 65536) as u8;
            let rgb = match (r.checked_div(a), g.checked_div(a), b.checked_div(a)) {
                (Some(r), Some(g), Some(b)) => [r as u8, g as u8, b as u8],
                _ => [0, 0, 0],
            };
            put(x, y, rgb, alpha);
        }
    }
}
