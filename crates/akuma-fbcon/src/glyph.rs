//! The characters the console draws itself, and how wide each character is.
//!
//! # Why not the font
//!
//! A TUI draws its frames, bars, graphs and bullets with characters a console
//! font is bad at. **Box drawing, block elements and Braille** are geometry, not
//! letterforms: a font's `─` rasterized and anti-aliased at one scale leaves
//! half-pixel gaps where two cells meet, so a frame that should close shows
//! seams — and IBM Plex Mono has no Braille at all. Drawn from the cell size
//! instead, every line reaches the cell edge exactly and joins its neighbour at
//! any scale. The same goes for the **arrows, circles, triangles and check marks**
//! a chat client uses for bullets and status dots, which neither face carries
//! (`tests/glyph_coverage.rs` prints the gaps).
//!
//! Everything here is integer arithmetic and takes closures for painting, so it
//! allocates nothing, needs no float library, and is tested on the host without a
//! framebuffer.
//!
//! # Width
//!
//! A terminal cell is one, two or — for combining marks and joiners — zero
//! columns wide, and a program laying out a screen assumes the terminal agrees.
//! [`width`] is the console's answer (from `unicode-width`): East Asian wide and
//! emoji are 2, so a line containing an emoji stays aligned with the rest of the
//! frame even though the console cannot draw a colour emoji and shows a box in its
//! place.

/// How many columns `cp` occupies: 0 (combining mark, joiner, variation
/// selector), 1, or 2 (East Asian wide, emoji).
///
/// From the `unicode-width` crate's Unicode tables — a hand-written range list
/// was the first version and was never going to track Unicode. A control
/// character (which the parser never prints) and anything the tables do not know
/// count as one column.
#[must_use]
pub fn width(cp: u32) -> usize {
    char::from_u32(cp).and_then(unicode_width::UnicodeWidthChar::width).unwrap_or(1)
}

/// What a code point is drawn with, when not from the font.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `U+2500..=U+257F`: lines and corners.
    Box,
    /// `U+2580..=U+259F`: halves, eighths, shades, quadrants.
    Block,
    /// `U+2800..=U+28FF`: eight-dot cells.
    Braille,
    /// A bullet, arrow, check mark, triangle...: see [`shape_coverage`].
    Shape,
}

/// Is `cp` drawn by this module (and so never looked up in the font)?
#[must_use]
pub fn kind(cp: u32) -> Option<Kind> {
    match cp {
        0x2500..=0x257F => Some(Kind::Box),
        0x2580..=0x259F => Some(Kind::Block),
        0x2800..=0x28FF => Some(Kind::Braille),
        _ if shape_of(cp).is_some() => Some(Kind::Shape),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Box drawing
// ---------------------------------------------------------------------------

/// Arms of each `U+2500..=U+257F`: `[up, down, left, right]`, each 0 (none),
/// 1 (light), 2 (heavy) or 3 (double). Dashed forms are drawn solid and the
/// rounded corners as square ones; the three diagonals have no arms.
static BOX: [[u8; 4]; 128] = [
    // 0x2500
    [0, 0, 1, 1], [0, 0, 2, 2], [1, 1, 0, 0], [2, 2, 0, 0], [0, 0, 1, 1], [0, 0, 2, 2], [1, 1, 0, 0], [2, 2, 0, 0],
    // 0x2508
    [0, 0, 1, 1], [0, 0, 2, 2], [1, 1, 0, 0], [2, 2, 0, 0], [0, 1, 0, 1], [0, 1, 0, 2], [0, 2, 0, 1], [0, 2, 0, 2],
    // 0x2510
    [0, 1, 1, 0], [0, 1, 2, 0], [0, 2, 1, 0], [0, 2, 2, 0], [1, 0, 0, 1], [1, 0, 0, 2], [2, 0, 0, 1], [2, 0, 0, 2],
    // 0x2518
    [1, 0, 1, 0], [1, 0, 2, 0], [2, 0, 1, 0], [2, 0, 2, 0], [1, 1, 0, 1], [1, 1, 0, 2], [2, 1, 0, 1], [1, 2, 0, 1],
    // 0x2520
    [2, 2, 0, 1], [2, 1, 0, 2], [1, 2, 0, 2], [2, 2, 0, 2], [1, 1, 1, 0], [1, 1, 2, 0], [2, 1, 1, 0], [1, 2, 1, 0],
    // 0x2528
    [2, 2, 1, 0], [2, 1, 2, 0], [1, 2, 2, 0], [2, 2, 2, 0], [0, 1, 1, 1], [0, 1, 2, 1], [0, 1, 1, 2], [0, 1, 2, 2],
    // 0x2530
    [0, 2, 1, 1], [0, 2, 2, 1], [0, 2, 1, 2], [0, 2, 2, 2], [1, 0, 1, 1], [1, 0, 2, 1], [1, 0, 1, 2], [1, 0, 2, 2],
    // 0x2538
    [2, 0, 1, 1], [2, 0, 2, 1], [2, 0, 1, 2], [2, 0, 2, 2], [1, 1, 1, 1], [1, 1, 2, 1], [1, 1, 1, 2], [1, 1, 2, 2],
    // 0x2540
    [2, 1, 1, 1], [1, 2, 1, 1], [2, 2, 1, 1], [2, 1, 2, 1], [2, 1, 1, 2], [1, 2, 2, 1], [1, 2, 1, 2], [2, 1, 2, 2],
    // 0x2548
    [1, 2, 2, 2], [2, 2, 1, 2], [2, 2, 2, 1], [2, 2, 2, 2], [0, 0, 1, 1], [0, 0, 2, 2], [1, 1, 0, 0], [2, 2, 0, 0],
    // 0x2550
    [0, 0, 3, 3], [3, 3, 0, 0], [0, 1, 0, 3], [0, 3, 0, 1], [0, 3, 0, 3], [0, 1, 3, 0], [0, 3, 1, 0], [0, 3, 3, 0],
    // 0x2558
    [1, 0, 0, 3], [3, 0, 0, 1], [3, 0, 0, 3], [1, 0, 3, 0], [3, 0, 1, 0], [3, 0, 3, 0], [1, 1, 0, 3], [3, 3, 0, 1],
    // 0x2560
    [3, 3, 0, 3], [1, 1, 3, 0], [3, 3, 1, 0], [3, 3, 3, 0], [0, 1, 3, 3], [0, 3, 1, 1], [0, 3, 3, 3], [1, 0, 3, 3],
    // 0x2568
    [3, 0, 1, 1], [3, 0, 3, 3], [1, 1, 3, 3], [3, 3, 1, 1], [3, 3, 3, 3], [0, 1, 0, 1], [0, 1, 1, 0], [1, 0, 1, 0],
    // 0x2570
    [1, 0, 0, 1], [0, 0, 0, 0], [0, 0, 0, 0], [0, 0, 0, 0], [0, 0, 1, 0], [1, 0, 0, 0], [0, 0, 0, 1], [0, 1, 0, 0],
    // 0x2578
    [0, 0, 2, 0], [2, 0, 0, 0], [0, 0, 0, 2], [0, 2, 0, 0], [0, 0, 1, 2], [1, 2, 0, 0], [0, 0, 2, 1], [2, 1, 0, 0],
];

/// Line thickness in pixels for a cell `cw` wide: light, and heavy = double the
/// light but at least one more.
const fn thickness(cw: usize) -> (usize, usize) {
    let light = if cw / 10 == 0 { 1 } else { cw / 10 };
    (light, if light * 2 > light { light * 2 } else { light + 1 })
}

/// Paint `cp` (`U+2500..=U+257F`) into a `cw` x `ch` cell, calling
/// `fill(x, y, w, h)` for every solid rectangle.
///
/// Arms run to the cell edge, so neighbouring cells join with no seam at any
/// scale; at the centre each arm reaches to the far edge of the thickest crossing
/// line, which fills the junction. Double lines are two light strokes with a gap
/// of two light widths.
#[allow(clippy::many_single_char_names)] // u/d/l/r: up, down, left, right
pub fn paint_box(cp: u32, cw: usize, ch: usize, fill: &mut impl FnMut(usize, usize, usize, usize)) {
    let idx = (cp - 0x2500) as usize;
    if matches!(cp, 0x2571..=0x2573) {
        let (light, _) = thickness(cw);
        for y in 0..ch {
            // Clamped so the stroke never overhangs the right edge.
            let x = (y * cw / ch.max(1)).min(cw.saturating_sub(light));
            if cp != 0x2572 {
                fill(cw - light - x, y, light, 1); // ╱ (and the ╱ of ╳)
            }
            if cp != 0x2571 {
                fill(x, y, light, 1); // ╲
            }
        }
        return;
    }
    let [u, d, l, r] = BOX[idx];
    let (light, heavy) = thickness(cw);
    // The band a horizontal arm occupies, as (offset from centre-line start, thickness).
    let span = |a: u8| match a {
        0 => 0,
        1 => light,
        2 => heavy,
        _ => 4 * light,
    };
    let (cx, cy) = (cw / 2, ch / 2);
    // Half the thickness of the widest line crossing each axis, for the reach.
    let vert_span = span(u).max(span(d));
    let horiz_span = span(l).max(span(r));
    let strokes = |a: u8| -> ([(usize, usize); 2], usize) {
        // (offset from the axis centre, thickness) for each stroke of an arm,
        // measured from `centre - span/2`.
        match a {
            1 => ([(0, light), (0, 0)], 1),
            2 => ([(0, heavy), (0, 0)], 1),
            3 => ([(0, light), (3 * light, light)], 2),
            _ => ([(0, 0), (0, 0)], 0),
        }
    };
    // Horizontal arms.
    for (arm, left) in [(l, true), (r, false)] {
        let (s, n) = strokes(arm);
        let total = span(arm);
        for &(off, t) in &s[..n] {
            let y = (cy + off).saturating_sub(total / 2);
            let (x0, x1) = if left { (0, cx + vert_span / 2) } else { (cx.saturating_sub(vert_span / 2), cw) };
            fill(x0, y, x1 - x0, t);
        }
    }
    // Vertical arms.
    for (arm, up) in [(u, true), (d, false)] {
        let (s, n) = strokes(arm);
        let total = span(arm);
        for &(off, t) in &s[..n] {
            let x = (cx + off).saturating_sub(total / 2);
            let (y0, y1) = if up { (0, cy + horiz_span / 2) } else { (cy.saturating_sub(horiz_span / 2), ch) };
            fill(x, y0, t, y1 - y0);
        }
    }
}

// ---------------------------------------------------------------------------
// Block elements
// ---------------------------------------------------------------------------

/// Paint `cp` (`U+2580..=U+259F`) into a `cw` x `ch` cell via
/// `fill(x, y, w, h, shade)`. `shade` is 255 for solid and 64/128/192 for the
/// three shade characters, which the caller blends rather than dithers.
pub fn paint_block(cp: u32, cw: usize, ch: usize, fill: &mut impl FnMut(usize, usize, usize, usize, u8)) {
    let (hw, hh) = (cw / 2, ch / 2);
    let eighth_h = |n: usize| ch * n / 8;
    let eighth_w = |n: usize| cw * n / 8;
    match cp {
        0x2580 => fill(0, 0, cw, hh, 255),
        0x2581..=0x2587 => {
            let h = eighth_h((cp - 0x2580) as usize);
            fill(0, ch - h, cw, h, 255);
        }
        0x2588 => fill(0, 0, cw, ch, 255),
        0x2589..=0x258F => {
            let w = eighth_w(8 - (cp - 0x2588) as usize);
            fill(0, 0, w, ch, 255);
        }
        0x2590 => fill(cw - hw, 0, hw, ch, 255),
        0x2591 => fill(0, 0, cw, ch, 64),
        0x2592 => fill(0, 0, cw, ch, 128),
        0x2593 => fill(0, 0, cw, ch, 192),
        0x2594 => fill(0, 0, cw, eighth_h(1), 255),
        0x2595 => {
            let w = eighth_w(1);
            fill(cw - w, 0, w, ch, 255);
        }
        0x2596..=0x259F => {
            // Quadrants as a bitmask: 1 = upper-left, 2 = upper-right,
            // 4 = lower-left, 8 = lower-right.
            let q = match cp {
                0x2596 => 4,
                0x2597 => 8,
                0x2598 => 1,
                0x2599 => 1 | 4 | 8,
                0x259A => 1 | 8,
                0x259B => 1 | 2 | 4,
                0x259C => 1 | 2 | 8,
                0x259D => 2,
                0x259E => 2 | 4,
                _ => 2 | 4 | 8, // 0x259F
            };
            if q & 1 != 0 {
                fill(0, 0, hw, hh, 255);
            }
            if q & 2 != 0 {
                fill(hw, 0, cw - hw, hh, 255);
            }
            if q & 4 != 0 {
                fill(0, hh, hw, ch - hh, 255);
            }
            if q & 8 != 0 {
                fill(hw, hh, cw - hw, ch - hh, 255);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Braille
// ---------------------------------------------------------------------------

/// Paint a Braille cell (`U+2800..=U+28FF`): up to eight square dots, two
/// columns by four rows, via `fill(x, y, w, h)`.
pub fn paint_braille(cp: u32, cw: usize, ch: usize, fill: &mut impl FnMut(usize, usize, usize, usize)) {
    let bits = (cp - 0x2800) as u8;
    // Bit -> (column, row) in Unicode's dot numbering: dots 1-3 are the left
    // column top to bottom, 4-6 the right column, 7 and 8 the bottom row.
    const POS: [(usize, usize); 8] = [(0, 0), (0, 1), (0, 2), (1, 0), (1, 1), (1, 2), (0, 3), (1, 3)];
    let dot = if cw / 6 == 0 { 1 } else { cw / 6 };
    for (i, &(c, r)) in POS.iter().enumerate() {
        if bits & (1 << i) != 0 {
            // Centre of a quarter-width / eighth-height lattice, then back off by
            // half a dot.
            let cx = cw * (2 * c + 1) / 4;
            let cy = ch * (2 * r + 1) / 8;
            fill(cx.saturating_sub(dot / 2), cy.saturating_sub(dot / 2), dot, dot);
        }
    }
}

// ---------------------------------------------------------------------------
// Shapes
// ---------------------------------------------------------------------------

/// What [`shape_coverage`] draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    Disc,
    Ring,
    Bullseye,
    SmallDisc,
    SmallRing,
    Square,
    SquareOutline,
    SmallSquare,
    SmallSquareOutline,
    Diamond,
    DiamondOutline,
    /// Triangle: direction 0 up, 1 right, 2 down, 3 left; `true` = filled.
    Triangle(u8, bool),
    /// Arrow: direction as above; `true` = a head at both ends (↔ ↕).
    Arrow(u8, bool),
    Check,
    HeavyCheck,
    Cross,
    HeavyCross,
}

fn shape_of(cp: u32) -> Option<Shape> {
    Some(match cp {
        0x25CF | 0x26AB => Shape::Disc,
        0x25CB | 0x26AA => Shape::Ring,
        0x25C9 | 0x25CE => Shape::Bullseye,
        // U+2022 (bullet) is left to the font, which has a good one.
        0x2219 | 0x2981 => Shape::SmallDisc,
        0x25E6 => Shape::SmallRing,
        0x25A0 | 0x25FC | 0x2B1B => Shape::Square,
        0x25A1 | 0x25FB | 0x2B1C => Shape::SquareOutline,
        0x25AA | 0x25FE => Shape::SmallSquare,
        0x25AB | 0x25FD => Shape::SmallSquareOutline,
        0x25C6 => Shape::Diamond,
        0x25C7 => Shape::DiamondOutline,
        0x25B2 | 0x25B4 => Shape::Triangle(0, true),
        0x25B3 | 0x25B5 => Shape::Triangle(0, false),
        0x25B6 | 0x25B8 | 0x25BA => Shape::Triangle(1, true),
        0x25B7 | 0x25B9 | 0x25BB => Shape::Triangle(1, false),
        0x25BC | 0x25BE => Shape::Triangle(2, true),
        0x25BD | 0x25BF => Shape::Triangle(2, false),
        0x25C0 | 0x25C2 | 0x25C4 => Shape::Triangle(3, true),
        0x25C1 | 0x25C3 | 0x25C5 => Shape::Triangle(3, false),
        0x2191 => Shape::Arrow(0, false),
        0x2192 => Shape::Arrow(1, false),
        0x2193 => Shape::Arrow(2, false),
        0x2190 => Shape::Arrow(3, false),
        0x2194 => Shape::Arrow(1, true),
        0x2195 => Shape::Arrow(0, true),
        0x2713 => Shape::Check,
        0x2714 => Shape::HeavyCheck,
        0x2717 => Shape::Cross,
        0x2718 => Shape::HeavyCross,
        _ => return None,
    })
}

/// Squared distance from `(px, py)` to the segment `(ax, ay)-(bx, by)`, in the
/// 0..=1000 unit square's own units.
fn seg_dist2(px: i64, py: i64, ax: i64, ay: i64, bx: i64, by: i64) -> i64 {
    let (dx, dy) = (bx - ax, by - ay);
    let len2 = dx * dx + dy * dy;
    let mut t = ((px - ax) * dx + (py - ay) * dy) * 1000;
    t = if len2 == 0 { 0 } else { (t / len2).clamp(0, 1000) };
    let (qx, qy) = (ax + dx * t / 1000, ay + dy * t / 1000);
    (px - qx) * (px - qx) + (py - qy) * (py - qy)
}

/// Is the point `(u, v)` — 0..=1000 across and down the glyph box — inside
/// `shape`?
fn inside(shape: Shape, u: i64, v: i64) -> bool {
    let (du, dv) = (u - 500, v - 500);
    match shape {
        Shape::Disc => du * du + dv * dv <= 420 * 420,
        Shape::Ring => {
            let r2 = du * du + dv * dv;
            (300 * 300..=420 * 420).contains(&r2)
        }
        Shape::Bullseye => {
            let r2 = du * du + dv * dv;
            (330 * 330..=420 * 420).contains(&r2) || r2 <= 190 * 190
        }
        Shape::SmallDisc => du * du + dv * dv <= 200 * 200,
        Shape::SmallRing => {
            let r2 = du * du + dv * dv;
            (120 * 120..=200 * 200).contains(&r2)
        }
        Shape::Square => du.abs() <= 360 && dv.abs() <= 360,
        Shape::SquareOutline => {
            du.abs() <= 360 && dv.abs() <= 360 && (du.abs() >= 270 || dv.abs() >= 270)
        }
        Shape::SmallSquare => du.abs() <= 200 && dv.abs() <= 200,
        Shape::SmallSquareOutline => {
            du.abs() <= 200 && dv.abs() <= 200 && (du.abs() >= 120 || dv.abs() >= 120)
        }
        Shape::Diamond => du.abs() + dv.abs() <= 420,
        Shape::DiamondOutline => {
            let m = du.abs() + dv.abs();
            (310..=420).contains(&m)
        }
        Shape::Triangle(dir, filled) => {
            // Rotate to the "up" frame: apex at the top, base at the bottom.
            // Each arm maps the apex to `y = -400`: up is `dv = -400`, right is
            // `du = +400`, down `dv = +400`, left `du = -400`.
            let (x, y) = match dir {
                0 => (du, dv),
                1 => (dv, -du),
                2 => (du, -dv),
                _ => (dv, du),
            };
            // Apex (0,-400), base y = +340, half-width 380 at the base.
            let tri = |x: i64, y: i64, scale: i64| {
                let y = y * 1000 / scale;
                let x = x * 1000 / scale;
                (-400..=340).contains(&y) && x.abs() * 740 <= 380 * (y + 400)
            };
            if filled {
                tri(x, y, 1000)
            } else {
                tri(x, y, 1000) && !tri(x, y + 40, 640)
            }
        }
        Shape::Arrow(dir, both) => {
            // Rotate so the arrow points right.
            let (x, y) = match dir {
                1 => (du, dv),
                2 => (dv, -du),
                3 => (-du, -dv),
                _ => (-dv, du),
            };
            let shaft = y.abs() <= 55 && x.abs() <= 400;
            let head = |x: i64| (150..=420).contains(&x) && y.abs() * 270 <= 230 * (420 - x);
            shaft || head(x) || (both && head(-x))
        }
        Shape::Check | Shape::HeavyCheck => {
            let t: i64 = if matches!(shape, Shape::Check) { 70 } else { 105 };
            seg_dist2(u, v, 220, 560, 400, 760) <= t * t
                || seg_dist2(u, v, 400, 760, 800, 240) <= t * t
        }
        Shape::Cross | Shape::HeavyCross => {
            let t: i64 = if matches!(shape, Shape::Cross) { 70 } else { 105 };
            seg_dist2(u, v, 240, 240, 760, 760) <= t * t
                || seg_dist2(u, v, 760, 240, 240, 760) <= t * t
        }
    }
}

/// Coverage (0..=255) of pixel `(x, y)` of a `cw` x `ch` cell for the shape drawn
/// by `cp`, or `None` if `cp` is not a shape.
///
/// The shape lives in a square about 80 % of the cell width, centred on the
/// cell's text line (a little above the middle, where a letter's body is), and is
/// sampled 4x4 per pixel — enough to anti-alias an edge without a float library.
#[must_use]
#[allow(clippy::cast_possible_wrap)] // cell sizes are a few hundred pixels
pub fn shape_coverage(cp: u32, x: usize, y: usize, cw: usize, ch: usize) -> Option<u8> {
    let shape = shape_of(cp)?;
    // Box side and top-left, in pixels.
    let side = (cw * 8 / 10).max(2) as i64;
    let left = ((cw as i64) - side) / 2;
    let top = (ch as i64 * 55 / 100 - side / 2).clamp(0, (ch as i64 - side).max(0));
    let mut hits = 0u32;
    for sy in 0..4i64 {
        for sx in 0..4i64 {
            // Sample position in 1/4-pixel steps, relative to the box, scaled to 0..=1000.
            let px = (x as i64 * 4 + sx) * 1000 / 4 - left * 1000;
            let py = (y as i64 * 4 + sy) * 1000 / 4 - top * 1000;
            let (u, v) = (px / side, py / side);
            // `px`/`py` are in 1/1000 pixel; `side` pixels span 1000 units.
            if (0..=1000).contains(&u) && (0..=1000).contains(&v) && inside(shape, u, v) {
                hits += 1;
            }
        }
    }
    Some((hits * 255 / 16) as u8)
}
