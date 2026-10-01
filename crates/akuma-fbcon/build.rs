//! Bake the console's fonts into coverage tables.
//!
//! Two fonts come out of here and they arrive in very different shapes:
//!
//! - **IBM Plex Mono** ([`PLEX_TTF`]) is a TrueType outline font. There is no
//!   bitmap in it at all — every glyph is a set of quadratic contours that have
//!   to be scaled, positioned and filled before there are any pixels.
//! - **Spleen** ([`SPLEEN_BDF`]) is already a bitmap, one bit per pixel.
//!
//! Both are emitted as the same thing: an **8-bit coverage value per pixel**,
//! row-major, `WIDTH * HEIGHT` bytes per cell, in code-point order from
//! [`FIRST`] to [`LAST`], with one extra cell on the end for the replacement
//! box. Spleen's bits become `0x00` or `0xFF` and lose nothing; IBM Plex Mono
//! keeps its partial coverage, which is the whole reason an outline font is
//! worth having on screen — a 1-bit rasterization of a face drawn for
//! anti-aliasing has visibly uneven stems.
//!
//! IBM Plex Mono is vendored as the single `IBMPlexMono-Regular.ttf` file (SIL
//! OFL 1.1, `vendor/ibm-plex-mono/LICENSE.txt`, `PROVENANCE.txt` records the
//! upstream commit) rather than as a submodule: `github.com/IBM/plex` is a
//! ~200 MB monorepo of every Plex family and there is no standalone Plex Mono
//! repo, so a submodule would cost far more than the one 170 KB file it exists
//! to reach. Spleen stays a submodule — its repo is small. Either way the table
//! is generated from the file by `build.rs`, never hand-written.
//!
//! Which code points are emitted is [`RANGES`]: printable ASCII, Latin-1,
//! Latin Extended-A, Greek, Cyrillic, general punctuation, arrows, geometric shapes and a few
//! symbols — enough for a TUI such as late.sh. **Box drawing, block elements and
//! Braille are deliberately not here**: the console draws those itself, per cell,
//! because a font's line glyphs leave gaps at the cell edge once they are scaled
//! and anti-aliased, and a box that does not close is worse than no box. A code
//! point in [`RANGES`] that a face has no glyph for gets the replacement box.

use std::collections::HashMap;
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use ab_glyph::{Font as _, FontRef, point};

/// First printable ASCII code point — the range the cell is *laid out* from.
const FIRST: u32 = 0x20;
/// Last printable ASCII code point.
const LAST: u32 = 0x7E;

/// Every range baked into the tables, in the order their cells are stored.
/// `font.rs` looks a code point up by walking exactly this list, so the order is
/// part of the format: the generated `.rs` carries a copy.
const RANGES: &[(u32, u32)] = &[
    (0x0020, 0x007E), // printable ASCII
    (0x00A0, 0x00FF), // Latin-1 Supplement
    (0x0100, 0x017F), // Latin Extended-A (Polish, Czech, Turkish, ...)
    (0x0370, 0x03FF), // Greek and Coptic
    (0x0400, 0x052F), // Cyrillic and Cyrillic Supplement
    (0x2010, 0x2027), // dashes, quotes, bullet, ellipsis
    (0x2030, 0x203A), // per mille, primes, guillemets
    (0x20AC, 0x20AC), // euro
    (0x2190, 0x21FF), // arrows
    (0x25A0, 0x25FF), // geometric shapes: squares, circles, triangles
    (0x2605, 0x2606), // stars
    (0x2660, 0x2667), // card suits
    (0x266A, 0x266B), // musical notes
    (0x2713, 0x2718), // check and cross marks
    (0x2726, 0x2727), // four-pointed stars
];

/// Cells in a table: one per code point in [`RANGES`], plus the replacement box
/// on the end.
fn cell_count() -> usize {
    RANGES.iter().map(|&(a, b)| (b - a + 1) as usize).sum::<usize>() + 1
}

/// Every baked code point, in table order.
fn code_points() -> impl Iterator<Item = u32> {
    RANGES.iter().flat_map(|&(a, b)| a..=b)
}

/// The vendored IBM Plex Mono file, and the one weight the console uses.
const PLEX_TTF: &str = "vendor/ibm-plex-mono/IBMPlexMono-Regular.ttf";
/// The Spleen submodule, and the one size the console uses.
const SPLEEN_BDF: &str = "vendor/spleen/spleen-8x16.bdf";

/// One rasterized font, ready to be written out.
struct Face {
    /// Name of the generated `static`.
    ident: &'static str,
    /// Basename of the generated `.rs` and `.bin`.
    stem: &'static str,
    /// What to call it in the docs and in `Font::name`.
    name: &'static str,
    /// Where it came from, for the generated doc comment.
    origin: &'static str,
    /// How it was fitted to the cell, for the generated doc comment.
    fit: String,
    /// Pixels across one cell.
    width: usize,
    /// Pixels down one cell.
    height: usize,
    /// One cell per code point in [`RANGES`] plus the replacement, each
    /// `width * height` coverage bytes, row-major.
    cells: Vec<u8>,
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));

    emit(&ibm_plex_mono(&PLEX_SD), &out);
    emit(&ibm_plex_mono(&PLEX_HD), &out);
    emit(&spleen(), &out);
}

// ---------------------------------------------------------------------------
// IBM Plex Mono
// ---------------------------------------------------------------------------

/// Pixels across one IBM Plex Mono cell.
///
/// The face advances 600 units per 1000-unit em, so a 12-wide cell is exactly
/// its drawn proportion against a 24-tall one. Squeezing it into the 8 columns
/// Spleen uses would narrow every glyph by a third and undo the reason for
/// preferring an outline font in the first place.
const PLEX_WIDTH: usize = 12;
/// The HD cut: the same face baked at exactly twice the size, for a screen where the
/// console would otherwise draw the 12x24 table pixel-doubled (see `Console::choose_font`).
/// Hinting-free outlines scale linearly, so this is the same fit with a bigger `PxScale`
/// -- stems are real 2-4 pixel strokes with their own anti-aliasing instead of a
/// half-resolution bitmap with every pixel repeated.
const PLEX_HD_WIDTH: usize = 24;
const PLEX_HD_HEIGHT: usize = 48;
/// Coverage exponent for the HD cut: `c' = 255 * (c/255)^HD_COVERAGE_GAMMA`.
/// The console blends in sRGB space, which renders light text on a dark ground a little
/// thin (the edge pixel is darker than the true linear mix would be). A mild boost to
/// partial coverage compensates, without touching solid (255) or empty (0) pixels.
/// 1.0 turns it off.
const HD_COVERAGE_GAMMA: f32 = 0.8;
/// Pixels down one IBM Plex Mono cell.
///
/// Shorter than the face's own line height, which is 1.32 em and would spend
/// four rows on leading and on accents no code point below `0x7F` reaches.
/// What fills the cell is the ink of printable ASCII, centred. See [`place`].
const PLEX_HEIGHT: usize = 24;

/// The least coverage that counts as ink, out of 255.
///
/// The rasterizer's bounds round outward, so a glyph whose outline stops a
/// hair short of a pixel boundary still reports that pixel and fills it with a
/// few percent of coverage. `&` does exactly this: it inks twelve columns and
/// reports thirteen, the last carrying 15/255 — around 6 %, which against this
/// console's colours is a foreground of `0x0C` on a `0x00` background and is
/// not visible on any monitor.
///
/// Laying the cell out from the reported bounds instead of from the ink would
/// shrink the whole alphabet by 8 % to make room for that. So the layout
/// ignores anything below this, and [`ibm_plex_mono`] refuses to discard
/// anything at or above it — a sliver is dropped, real ink outside the cell is
/// a build failure.
const INK_FLOOR: u8 = 16;

/// One glyph, rasterized: `(x, y, coverage)` in pen coordinates — `x` from the
/// left of the cell, `y` from the baseline, before the cell offset is applied.
type Drawn = Vec<(i32, i32, u8)>;

/// One size of IBM Plex Mono to bake.
struct PlexSize {
    ident: &'static str,
    stem: &'static str,
    name: &'static str,
    width: usize,
    height: usize,
    /// See [`HD_COVERAGE_GAMMA`]; 1.0 leaves coverage untouched.
    gamma: f32,
}

const PLEX_SD: PlexSize = PlexSize {
    ident: "IBM_PLEX_MONO",
    stem: "ibm_plex_mono",
    name: "IBM Plex Mono",
    width: PLEX_WIDTH,
    height: PLEX_HEIGHT,
    gamma: 1.0,
};

const PLEX_HD: PlexSize = PlexSize {
    ident: "IBM_PLEX_MONO_HD",
    stem: "ibm_plex_mono_hd",
    name: "IBM Plex Mono HD",
    width: PLEX_HD_WIDTH,
    height: PLEX_HD_HEIGHT,
    gamma: HD_COVERAGE_GAMMA,
};

/// Rasterize IBM Plex Mono into a cell table.
fn ibm_plex_mono(size: &PlexSize) -> Face {
    let (plex_width, plex_height) = (size.width, size.height);
    println!("cargo:rerun-if-changed={PLEX_TTF}");
    let data = fs::read(PLEX_TTF).unwrap_or_else(|e| {
        panic!(
            "cannot read {PLEX_TTF}: {e}\n\
             \n\
             IBM Plex Mono is vendored at crates/akuma-fbcon/vendor/ibm-plex-mono/;\n\
             if it is missing the checkout is incomplete.\n"
        )
    });
    let font = FontRef::try_from_slice(&data).expect("IBMPlexMono-Regular.ttf is not TrueType");

    // What `PxScale` scales is **not** the em square: ab_glyph divides it by
    // `ascent - descent`, so asking for the em size renders every glyph short
    // by the font's line-height ratio — 1.32 here, which is subtle enough to
    // look like a deliberately airy font rather than a bug. Solve for the
    // scale that puts the advance on exactly one cell width instead.
    let advance = monospace_advance(&font);
    let px = plex_width as f32 * (font.ascent_unscaled() - font.descent_unscaled()) / advance;

    // Rasterized once, up front, because placement has to know where the ink
    // landed before it can decide where the ink goes.
    //
    // `None` for a code point this face has no glyph for: it gets the replacement
    // box below. Placement is fitted from **ASCII alone** — what a cell is sized
    // for — so a tall accented capital or a wide arrow is clipped to the cell
    // rather than shrinking the alphabet to make room for it.
    let drawn: Vec<Option<Drawn>> = code_points().map(|c| rasterize(&font, c, px)).collect();
    let ascii: Vec<Drawn> = drawn
        .iter()
        .take((LAST - FIRST + 1) as usize)
        .map(|g| g.clone().expect("IBM Plex Mono lacks a printable ASCII glyph"))
        .collect();
    let (off_x, off_y, ink_w, ink_h) = place(&ascii, plex_width, plex_height);

    let cells_n = cell_count();
    let mut cells = vec![0u8; cells_n * plex_width * plex_height];
    let replace = replacement(plex_width, plex_height);
    for (index, glyph) in drawn.iter().enumerate() {
        let base = index * plex_width * plex_height;
        let Some(glyph) = glyph else {
            cells[base..base + plex_width * plex_height].copy_from_slice(&replace);
            continue;
        };
        let is_ascii = index < (LAST - FIRST + 1) as usize;
        for &(gx, gy, coverage) in glyph {
            let (x, y) = (gx + off_x, gy + off_y);
            let inside =
                x >= 0 && y >= 0 && (x as usize) < plex_width && (y as usize) < plex_height;
            // Strict for ASCII (a clipped letter is a font bug); lenient beyond it.
            assert!(
                inside || coverage < INK_FLOOR || !is_ascii,
                "code point index {index} puts {coverage}/255 of ink at ({x},{y}), outside its \
                 {plex_width}x{plex_height} cell; widen the cell rather than clipping a glyph"
            );
            if inside {
                cells[base + y as usize * plex_width + x as usize] = boost(coverage, size.gamma);
            }
        }
    }
    let last = (cells_n - 1) * plex_width * plex_height;
    cells[last..].copy_from_slice(&replace);

    Face {
        ident: size.ident,
        stem: size.stem,
        name: size.name,
        origin: "vendor/ibm-plex-mono/IBMPlexMono-Regular.ttf (SIL OFL 1.1)",
        fit: format!(
            "Rasterized at a {px:.1}-pixel `PxScale`, chosen so the face's {advance:.0}-unit \
             advance lands on exactly {plex_width} pixels. Printable ASCII inks \
             {ink_w}x{ink_h} of the cell and is centred in it. Coverage exponent {gamma}.",
            gamma = size.gamma
        ),
        width: plex_width,
        height: plex_height,
        cells,
    }
}

/// Apply the coverage exponent; solid and empty pixels are fixed points.
fn boost(coverage: u8, gamma: f32) -> u8 {
    if (gamma - 1.0).abs() < f32::EPSILON || coverage == 0 || coverage == 255 {
        return coverage;
    }
    ((f32::from(coverage) / 255.0).powf(gamma) * 255.0).round().clamp(1.0, 254.0) as u8
}

/// Rasterize one code point at the origin, in pen coordinates.
///
/// Empty for a code point the font draws nothing for — space has no contours,
/// so it has no outline and its cell stays blank.
fn rasterize(font: &FontRef<'_>, c: u32, px: f32) -> Option<Drawn> {
    let ch = char::from_u32(c)?;
    let id = font.glyph_id(ch);
    if id.0 == 0 {
        return None;
    }
    let Some(glyph) = font.outline_glyph(id.with_scale_and_position(px, point(0.0, 0.0))) else {
        return Some(Drawn::new());
    };

    // `draw` reports coordinates relative to the bounding box, which is where
    // the box's own origin has to be added back to get somewhere comparable
    // between one glyph and the next.
    let origin = glyph.px_bounds().min;
    let mut ink = Drawn::new();
    glyph.draw(|x, y, coverage| {
        let coverage = (coverage.clamp(0.0, 1.0) * 255.0).round() as u8;
        if coverage > 0 {
            // Glyph coordinates are a handful of pixels; the conversions are
            // spelled out only because a silent wrap here would place ink in
            // another cell and look like a font bug rather than a cast bug.
            let x = i32::try_from(x).expect("glyph column out of range");
            let y = i32::try_from(y).expect("glyph row out of range");
            ink.push((origin.x as i32 + x, origin.y as i32 + y, coverage));
        }
    });
    Some(ink)
}

/// The one advance width every emitted glyph shares.
///
/// Checked rather than assumed: the cell width *is* the advance here, so a
/// proportional font would silently produce a table where every glyph sits at a
/// slightly different place inside its cell and the text looked shaky for no
/// visible reason.
fn monospace_advance(font: &FontRef<'_>) -> f32 {
    let mut advance: Option<f32> = None;
    for c in FIRST..=LAST {
        let this = font.h_advance_unscaled(font.glyph_id(char::from_u32(c).expect("ASCII")));
        match advance {
            None => advance = Some(this),
            Some(first) => assert!(
                (this - first).abs() < 0.5,
                "code point {c:#04x} advances {this} where the rest advance {first}; \
                 this generator needs a monospaced font"
            ),
        }
    }
    advance.expect("the emitted range is not empty")
}

/// Where to put the ink so the whole alphabet is centred in the cell.
///
/// Returns the offset to add to every glyph's pen coordinates, and the size of
/// the ink block that was fitted. Positioning each glyph independently would be
/// wrong — it would put `.` and `M` at the same height. What is centred is the
/// union over every glyph, so the alphabet keeps its own baseline, ascenders
/// and descenders and simply sits in the middle of the cell as a block.
///
/// The font's own ascent and descent are not used for this, on purpose: they
/// cover accents and diacritics that no code point below `0x7F` reaches, and
/// laying the cell out from them spends a sixth of it on rows that are always
/// blank.
fn place(drawn: &[Drawn], cell_w: usize, cell_h: usize) -> (i32, i32, usize, usize) {
    let (mut min_x, mut min_y) = (i32::MAX, i32::MAX);
    let (mut max_x, mut max_y) = (i32::MIN, i32::MIN);
    for &(x, y, coverage) in drawn.iter().flatten() {
        if coverage >= INK_FLOOR {
            min_x = min_x.min(x);
            max_x = max_x.max(x);
            min_y = min_y.min(y);
            max_y = max_y.max(y);
        }
    }
    assert!(min_x <= max_x, "the font drew nothing at all");

    let (ink_w, ink_h) = ((max_x - min_x + 1) as usize, (max_y - min_y + 1) as usize);
    assert!(
        ink_w <= cell_w && ink_h <= cell_h,
        "printable ASCII inks {ink_w}x{ink_h} pixels, which does not fit a \
         {cell_w}x{cell_h} cell; widen the cell rather than clipping a glyph"
    );

    let slack_x = i32::try_from(cell_w - ink_w).expect("cell wider than an i32");
    let slack_y = i32::try_from(cell_h - ink_h).expect("cell taller than an i32");
    (slack_x / 2 - min_x, slack_y / 2 - min_y, ink_w, ink_h)
}

// ---------------------------------------------------------------------------
// Spleen
// ---------------------------------------------------------------------------

/// Widen Spleen's 8x16 bitmap into the same coverage table.
///
/// A bit becomes `0x00` or `0xFF`, so nothing about how the font looks changes
/// — this costs eight times the bytes and buys one drawing path in the console
/// instead of two.
fn spleen() -> Face {
    println!("cargo:rerun-if-changed={SPLEEN_BDF}");
    let text = fs::read_to_string(SPLEEN_BDF).unwrap_or_else(|e| {
        panic!(
            "cannot read {SPLEEN_BDF}: {e}\n\
             \n\
             Spleen is a git submodule. Fetch it with:\n\
             \n\
             \tgit submodule update --init crates/akuma-fbcon/vendor/spleen\n"
        )
    });

    let (width, height, glyphs) = parse_bdf(&text);
    assert_eq!(width, 8, "spleen-8x16.bdf is meant to be eight pixels wide");

    let cells_n = cell_count();
    let mut cells = vec![0u8; cells_n * width * height];
    let replace = replacement(width, height);
    for (index, c) in code_points().enumerate() {
        let base = index * width * height;
        if let Some(rows) = glyphs.get(&c) {
            assert_eq!(rows.len(), height, "glyph {c:#04x} is not {height} rows");
            for (y, bits) in rows.iter().enumerate() {
                for x in 0..width {
                    cells[base + y * width + x] = if bits & (0x80 >> x) != 0 { 0xFF } else { 0x00 };
                }
            }
        } else {
            assert!(c > LAST, "spleen-8x16.bdf has no glyph for ASCII {c:#04x}");
            cells[base..base + width * height].copy_from_slice(&replace);
        }
    }
    let last = (cells_n - 1) * width * height;
    cells[last..].copy_from_slice(&replace);

    Face {
        ident: "SPLEEN",
        stem: "spleen",
        name: "Spleen",
        origin: "vendor/spleen/spleen-8x16.bdf (BSD-2-Clause, Frederic Cambus)",
        fit: String::from(
            "Already a bitmap at this size, so every coverage value here is 0x00 or 0xFF.",
        ),
        width,
        height,
        cells,
    }
}

/// Pull the bounding box and every glyph bitmap out of a BDF.
///
/// Spleen's glyphs all carry a `BBX` identical to the font bounding box, so no
/// per-glyph placement arithmetic is needed — but that is checked rather than
/// assumed, because a font where it is not true would otherwise produce a table
/// of subtly shifted glyphs and no error.
fn parse_bdf(text: &str) -> (usize, usize, HashMap<u32, Vec<u8>>) {
    let mut fbb: Option<(usize, usize, i32, i32)> = None;
    let mut glyphs = HashMap::new();

    let mut encoding: Option<u32> = None;
    let mut bbx: Option<(usize, usize, i32, i32)> = None;
    let mut rows: Vec<u8> = Vec::new();
    let mut in_bitmap = false;

    for line in text.lines() {
        let line = line.trim_end();
        if let Some(rest) = line.strip_prefix("FONTBOUNDINGBOX ") {
            fbb = Some(quad(rest));
        } else if let Some(rest) = line.strip_prefix("ENCODING ") {
            encoding = rest.trim().parse().ok();
        } else if let Some(rest) = line.strip_prefix("BBX ") {
            bbx = Some(quad(rest));
        } else if line == "BITMAP" {
            in_bitmap = true;
            rows = Vec::new();
        } else if line == "ENDCHAR" {
            in_bitmap = false;
            if let (Some(enc), Some(b), Some(f)) = (encoding, bbx, fbb) {
                assert_eq!(
                    b, f,
                    "glyph {enc:#x} has a bounding box unlike the font's; this \
                     generator assumes a fixed-cell font"
                );
                glyphs.insert(enc, std::mem::take(&mut rows));
            }
            encoding = None;
            bbx = None;
        } else if in_bitmap && !line.is_empty() {
            let byte = u8::from_str_radix(line.trim(), 16)
                .unwrap_or_else(|e| panic!("bad bitmap row {line:?}: {e}"));
            rows.push(byte);
        }
    }

    let (w, h, _, _) = fbb.expect("BDF has no FONTBOUNDINGBOX");
    (w, h, glyphs)
}

/// Parse the four whitespace-separated numbers a `BBX`/`FONTBOUNDINGBOX` carries.
fn quad(text: &str) -> (usize, usize, i32, i32) {
    let mut fields = text.split_whitespace();
    let mut next_i = || fields.next().expect("short BBX").parse::<i32>().expect("bad BBX");
    let w = next_i();
    let h = next_i();
    let x = next_i();
    let y = next_i();
    (w as usize, h as usize, x, y)
}

// ---------------------------------------------------------------------------
// Shared
// ---------------------------------------------------------------------------

/// The cell drawn for a code point no table holds: a hollow box.
///
/// Drawn here rather than typed out as bits so it comes out at whatever size
/// the font is, and so that no glyph anywhere in this crate is a hand-written
/// table. A console that silently draws garbage for a stray byte is worse than
/// one that draws a visible box.
fn replacement(width: usize, height: usize) -> Vec<u8> {
    let mut cell = vec![0u8; width * height];
    let (x0, x1) = (width / 6, width - width / 6);
    let (y0, y1) = (height / 6, height - height / 6);
    for y in y0..y1 {
        for x in x0..x1 {
            if x == x0 || x + 1 == x1 || y == y0 || y + 1 == y1 {
                cell[y * width + x] = 0xFF;
            }
        }
    }
    cell
}

/// Write one face as a `.bin` blob and the `static` that points at it.
///
/// The coverage goes in a separate file and reaches the crate through
/// `include_bytes!` rather than as a Rust array literal: IBM Plex Mono is
/// 27 KB of it, and 27 000 comma-separated integers is a source file `rustc`
/// spends real time parsing on every build for no benefit.
fn emit(face: &Face, out: &Path) {
    assert_eq!(
        face.cells.len(),
        cell_count() * face.width * face.height,
        "{} produced a table of the wrong size",
        face.name
    );
    fs::write(out.join(format!("{}.bin", face.stem)), &face.cells)
        .expect("writing the font table");

    let (ident, name, origin, fit) = (face.ident, face.name, face.origin, &face.fit);
    let (width, height, stem) = (face.width, face.height, face.stem);
    let mut ranges = String::new();
    for &(a, b) in RANGES {
        let _ = write!(ranges, "(0x{a:04X}, 0x{b:04X}), ");
    }
    let mut rs = String::new();
    let _ = write!(
        rs,
        "// Generated by build.rs. Do not edit.\n\
         /// {name}, {width}x{height}, one 8-bit coverage value per pixel.\n\
         ///\n\
         /// Baked from `{origin}`.\n\
         ///\n\
         /// {fit}\n\
         pub static {ident}: Font = Font::new(\n\
         \x20   \"{name}\",\n\
         \x20   {width},\n\
         \x20   {height},\n\
         \x20   &[{ranges}],\n\
         \x20   include_bytes!(concat!(env!(\"OUT_DIR\"), \"/{stem}.bin\")),\n\
         );\n"
    );
    fs::write(out.join(format!("{stem}.rs")), rs).expect("writing the generated font");
}
