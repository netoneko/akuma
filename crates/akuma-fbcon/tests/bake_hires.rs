//! Bakes `vendor/hires/*`: the smooth (anti-aliased) glyph tables for CJK and for the
//! symbols/Greek/etc. that IBM Plex Mono lacks. Not an assertion: ignored by default.
//!
//!     python3 scripts/cjk_common_set.py > /tmp/cjk-set.txt
//!     AKUMA_FONT_DEJAVU=.../DejaVuSansMono.ttf \
//!     AKUMA_FONT_NOTO_CJK=.../NotoSansCJKsc-Regular.otf \
//!     AKUMA_CJK_SET=/tmp/cjk-set.txt \
//!       cargo test -p akuma-fbcon --test bake_hires -- --ignored
//!
//! The fonts are inputs, not vendored (the CJK one is 16 MB); the tables are the
//! output and are committed. `vendor/hires/PROVENANCE.txt` has the sources and licences.
//!
//! * `sym-24x48.{idx,bin}`: DejaVu Sans Mono at the 24x48 cut's size, 8-bit coverage, for
//!   every code point in `SYM` it has a glyph for. Laid on the **same baseline and left
//!   edge** as IBM Plex Mono's 24x48 cut (placement is recomputed from Plex here), so a
//!   symbol sits in the line with the text around it. The console area-resamples it to
//!   whatever cell it is drawing.
//! * `cjk.idx`, `cjk-22.bin`, `cjk-40.bin`: Noto Sans CJK SC, the em square scaled to a
//!   22x22 / 40x40 box (the two-column cell of the 11x22 and 20x40 defaults), **4-bit**
//!   coverage, two pixels a byte, high nibble first.
//!
//! Both gamma-boost partial coverage by the console's own exponent (0.6, see `build.rs`).

// An offline baker over glyph coordinates a few hundred pixels at most: the casts cannot wrap.
#![allow(clippy::cast_possible_wrap)]

use ab_glyph::{Font as _, FontRef, PxScale, point};
use std::fs;

const GAMMA: f32 = 0.6;
const INK_FLOOR: u8 = 16;

/// Same blocks as `unifont::NARROW` + `unifont::SYMBOLS`.
const SYM: &[(u32, u32)] = &[
    (0x0180, 0x02AF),
    (0x0370, 0x03FF),
    (0x0400, 0x052F),
    (0x1E00, 0x1EFF),
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

fn boost(coverage: u8) -> u8 {
    if coverage == 0 || coverage == 255 {
        return coverage;
    }
    ((f32::from(coverage) / 255.0).powf(GAMMA) * 255.0).round().clamp(1.0, 254.0) as u8
}

/// Pixels per `PxScale` so the font's advance is `width` pixels (`build.rs`'s fit).
fn px_for_advance(font: &FontRef<'_>, width: f32) -> f32 {
    let advance = font.h_advance_unscaled(font.glyph_id('0'));
    width * (font.ascent_unscaled() - font.descent_unscaled()) / advance
}

/// Rasterize `ch` into a `w`x`h` box: pen origin at column `ox`, baseline at row `base`.
/// `None` if the font has no glyph; `Some(false)` if it has one that draws nothing.
fn render(
    font: &FontRef<'_>, ch: char, px: f32, w: usize, h: usize, ox: i32, base: i32, out: &mut [u8],
) -> Option<bool> {
    let id = font.glyph_id(ch);
    if id.0 == 0 {
        return None;
    }
    let Some(glyph) = font.outline_glyph(id.with_scale_and_position(PxScale::from(px), point(0.0, 0.0)))
    else {
        return Some(false);
    };
    let origin = glyph.px_bounds().min;
    let mut any = false;
    glyph.draw(|x, y, c| {
        let v = (c.clamp(0.0, 1.0) * 255.0).round() as u8;
        let (gx, gy) = (ox + origin.x as i32 + x as i32, base + origin.y as i32 + y as i32);
        if v > 0 && gx >= 0 && gy >= 0 && (gx as usize) < w && (gy as usize) < h {
            out[gy as usize * w + gx as usize] = boost(v);
            any = true;
        }
    });
    Some(any)
}

/// Where IBM Plex Mono's 24x48 cut puts the pen: `build.rs`'s `place`, redone from the
/// vendored face, so the symbols share its baseline.
fn plex_pen(plex: &FontRef<'_>, w: usize, h: usize) -> (i32, i32) {
    let px = px_for_advance(plex, w as f32);
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
    for c in 0x20u32..=0x7E {
        let id = plex.glyph_id(char::from_u32(c).unwrap());
        let Some(g) = plex.outline_glyph(id.with_scale_and_position(PxScale::from(px), point(0.0, 0.0))) else {
            continue;
        };
        let o = g.px_bounds().min;
        g.draw(|x, y, cov| {
            if (cov * 255.0) as u8 >= INK_FLOOR {
                let (gx, gy) = (o.x as i32 + x as i32, o.y as i32 + y as i32);
                min_x = min_x.min(gx);
                max_x = max_x.max(gx);
                min_y = min_y.min(gy);
                max_y = max_y.max(gy);
            }
        });
    }
    let (ink_w, ink_h) = (max_x - min_x + 1, max_y - min_y + 1);
    ((w as i32 - ink_w) / 2 - min_x, (h as i32 - ink_h) / 2 - min_y)
}

fn pack4(cells: &[u8]) -> Vec<u8> {
    let q = |v: u8| ((u32::from(v) * 15 + 127) / 255) as u8;
    cells.chunks(2).map(|p| (q(p[0]) << 4) | p.get(1).map_or(0, |&b| q(b))).collect()
}

#[test]
#[ignore = "writes vendor/hires; needs the source fonts, see the file header"]
fn bake_hires() {
    let (Ok(dejavu), Ok(noto), Ok(set)) = (
        std::env::var("AKUMA_FONT_DEJAVU"),
        std::env::var("AKUMA_FONT_NOTO_CJK"),
        std::env::var("AKUMA_CJK_SET"),
    ) else {
        return;
    };
    let out = concat!(env!("CARGO_MANIFEST_DIR"), "/vendor/hires");
    fs::create_dir_all(out).unwrap();

    // ---- symbols, Greek, ...: DejaVu Sans Mono at 24x48 ----
    let plex_data = fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/vendor/ibm-plex-mono/IBMPlexMono-Regular.ttf")).unwrap();
    let plex = FontRef::try_from_slice(&plex_data).unwrap();
    let (w, h) = (24usize, 48usize);
    let (ox, base) = plex_pen(&plex, w, h);
    let dj_data = fs::read(dejavu).unwrap();
    let dj = FontRef::try_from_slice(&dj_data).unwrap();
    let px = px_for_advance(&dj, w as f32);
    let (mut idx, mut bin) = (Vec::new(), Vec::new());
    let mut cell = vec![0u8; w * h];
    for &(first, last) in SYM {
        for cp in first..=last {
            let Some(ch) = char::from_u32(cp) else { continue };
            cell.fill(0);
            if render(&dj, ch, px, w, h, ox, base, &mut cell) == Some(true) {
                idx.extend_from_slice(&(cp as u16).to_le_bytes());
                bin.extend_from_slice(&cell);
            }
        }
    }
    println!("sym: {} glyphs, {} bytes (pen x {ox}, baseline row {base})", idx.len() / 2, bin.len());
    fs::write(format!("{out}/sym-24x48.idx"), &idx).unwrap();
    fs::write(format!("{out}/sym-24x48.bin"), &bin).unwrap();

    // ---- CJK: Noto Sans CJK SC ----
    let noto_data = fs::read(noto).unwrap();
    let nf = FontRef::try_from_slice(&noto_data).unwrap();
    let upem = nf.units_per_em().unwrap_or(1000.0);
    let cps: Vec<u32> = fs::read_to_string(set)
        .unwrap()
        .lines()
        .filter_map(|l| u32::from_str_radix(l.trim(), 16).ok())
        .collect();
    let (mut idx, mut b22, mut b40) = (Vec::new(), Vec::new(), Vec::new());
    for &cp in &cps {
        let Some(ch) = char::from_u32(cp) else { continue };
        let mut cells: Vec<(usize, Vec<u8>)> = Vec::new();
        let mut ok = true;
        for side in [22usize, 40] {
            let s = side as f32;
            let px = s * (nf.ascent_unscaled() - nf.descent_unscaled()) / upem;
            // The ideographic em box has 0.88 em above the baseline and 0.12 below (CJK
            // fonts' own `hhea` ascent is 1.16 em, which would sink every glyph).
            let base = (s * 0.88).round() as i32;
            let mut cell = vec![0u8; side * side];
            ok &= render(&nf, ch, px, side, side, 0, base, &mut cell) == Some(true);
            cells.push((side, cell));
        }
        // A code point the font draws nothing for is left to Unifont (U+3000 is blank by design).
        if ok {
            idx.extend_from_slice(&(cp as u16).to_le_bytes());
            b22.extend_from_slice(&pack4(&cells[0].1));
            b40.extend_from_slice(&pack4(&cells[1].1));
        }
    }
    println!("cjk: {} of {} glyphs; 22 = {} bytes, 40 = {} bytes", idx.len() / 2, cps.len(), b22.len(), b40.len());
    fs::write(format!("{out}/cjk.idx"), &idx).unwrap();
    fs::write(format!("{out}/cjk-22.bin"), &b22).unwrap();
    fs::write(format!("{out}/cjk-40.bin"), &b40).unwrap();
}
