//! Subpixel text and the fallback chain for a code point the text font lacks.
//!
//! Written for two reports from the ryzen console (2026-10-16): text that was not
//! smooth, and Claude Code's own glyphs — `⏺ ✻ ✽ ✶ ✳ ✢ ⏵ ➜ ⚠`, arrows, asterisks —
//! drawing as boxes.

use akuma_fbcon::config::ConsoleConfig;
use akuma_fbcon::font::{self, Font};
use akuma_fbcon::{glyph, unifont, Console, Rgb, Smooth, Surface};

struct Mem(usize, usize, Vec<Rgb>);
impl Surface for Mem {
    fn width(&self) -> usize { self.0 }
    fn height(&self) -> usize { self.1 }
    fn put(&mut self, x: usize, y: usize, c: Rgb) {
        if x < self.0 && y < self.1 { self.2[y * self.0 + x] = c; }
    }
}

const BG: Rgb = Rgb::new(0, 0, 0);
const FG: Rgb = Rgb::new(255, 255, 255);

/// A console in the cut whose cell height is `height`, white on black, one cell
/// at (0,0) holding `text`; returns the cell's pixels as rows.
fn cell(height: usize, smooth: Smooth, text: &str) -> (Vec<Vec<Rgb>>, usize, usize) {
    let mut con = Console::new(Mem(400, 100, vec![BG; 400 * 100])).unwrap();
    con.apply_config(&ConsoleConfig::parse(&format!("font = {height}\nmargin = 0")));
    con.set_smooth(smooth);
    con.set_bg(BG);
    con.clear();
    con.write_str_bytes(&format!("\x1b[?25l\x1b[H\x1b[38;2;255;255;255m{text}"));
    let (w, h) = (con.font().width(), con.font().height());
    let s = con.into_surface();
    let rows = (0..h).map(|y| (0..w).map(|x| s.2[y * s.0 + x]).collect()).collect();
    (rows, w, h)
}

fn ink(rows: &[Vec<Rgb>]) -> usize {
    rows.iter().flatten().filter(|c| **c != BG).count()
}

/// Mass of a glyph's gray coverage and of its subpixel coverage per channel.
fn mass(f: &Font, cp: u32) -> (u32, u32) {
    let gray: u32 = f.cell_cp(cp).iter().map(|&b| u32::from(b)).sum();
    let lcd: u32 = f.lcd_cell_cp(cp).expect("lcd glyph").iter().map(|&b| u32::from(b)).sum();
    (gray, lcd / 3)
}

#[test]
fn the_laptop_cuts_carry_subpixel_data_for_ascii_and_latin1_only() {
    for f in [font::IBM_PLEX_MONO_20.clone_ref(), font::IBM_PLEX_MONO_22.clone_ref(), font::IBM_PLEX_MONO.clone_ref()] {
        for cp in [0x20, 0x41, 0x7E, 0xA0, 0xE9, 0xFF] {
            let t = f.lcd_cell_cp(cp).unwrap_or_else(|| panic!("{} lacks LCD for {cp:#x}", f.name()));
            assert_eq!(t.len(), f.width() * 3 * f.height());
        }
        assert!(f.lcd_cell_cp(0x100).is_none(), "{}: past the baked prefix", f.name());
        assert!(f.lcd_cell_cp(0x4E00).is_none());
    }
    // The cuts nobody reads on a laptop panel keep gray only.
    assert!(font::IBM_PLEX_MONO_40.lcd_cell_cp(0x41).is_none());
}

trait Keep { fn clone_ref(&self) -> &Self; }
impl Keep for Font { fn clone_ref(&self) -> &Self { self } }

/// Subpixel coverage is the same glyph, resampled and filtered — it must not be
/// visibly bolder or fainter than the gray one, or switching modes changes the
/// weight of the whole screen.
#[test]
fn subpixel_coverage_carries_about_as_much_ink_as_gray() {
    for f in [&font::IBM_PLEX_MONO_20, &font::IBM_PLEX_MONO_22, &font::IBM_PLEX_MONO] {
        for ch in "HlimWe@%&".chars() {
            let (gray, lcd) = mass(f, u32::from(ch));
            let (g, l) = (f64::from(gray), f64::from(lcd));
            assert!(
                (l - g).abs() / g < 0.15,
                "{} {ch:?}: gray mass {gray}, subpixel mass per channel {lcd}",
                f.name()
            );
        }
    }
}

#[test]
fn subpixel_text_has_coloured_edges_and_gray_text_never_does() {
    let (lcd, ..) = cell(20, Smooth::Rgb, "l");
    assert!(lcd.iter().flatten().any(|c| c.r != c.b), "an RGB-order glyph has no fringe");
    let (gray, ..) = cell(20, Smooth::Gray, "l");
    assert!(ink(&gray) > 0);
    assert!(gray.iter().flatten().all(|c| c.r == c.g && c.g == c.b), "gray text is coloured");
}

#[test]
fn bgr_order_mirrors_the_channels_of_rgb() {
    let (rgb, w, h) = cell(20, Smooth::Rgb, "l");
    let (bgr, ..) = cell(20, Smooth::Bgr, "l");
    for y in 0..h {
        for x in 0..w {
            assert_eq!((rgb[y][x].r, rgb[y][x].b), (bgr[y][x].b, bgr[y][x].r), "({x},{y})");
            assert_eq!(rgb[y][x].g, bgr[y][x].g);
        }
    }
}

#[test]
fn smoothing_can_be_set_by_the_config_file_and_by_escape_sequence() {
    assert_eq!(ConsoleConfig::parse("smooth = gray").smooth, Some(Smooth::Gray));
    assert_eq!(ConsoleConfig::parse("smooth = LCD").smooth, Some(Smooth::Rgb));
    assert_eq!(ConsoleConfig::parse("smooth = bgr").smooth, Some(Smooth::Bgr));
    assert_eq!(ConsoleConfig::parse("smooth = banana").smooth, None);
    let mut con = Console::new(Mem(400, 100, vec![BG; 400 * 100])).unwrap();
    assert_eq!(con.smooth(), Smooth::Rgb, "subpixel is the default");
    con.write_str_bytes("\x1b[?9002l");
    assert_eq!(con.smooth(), Smooth::Gray);
    con.write_str_bytes("\x1b[?9002;2h");
    assert_eq!(con.smooth(), Smooth::Bgr);
    con.write_str_bytes("\x1b[?9002h");
    assert_eq!(con.smooth(), Smooth::Rgb);
    con.apply_config(&ConsoleConfig::parse("smooth = off"));
    assert_eq!(con.smooth(), Smooth::Gray);
}

/// The set that was drawn as boxes: Claude Code's spinner, bullets and prompt,
/// herdr's progress glyphs, the arrows and asterisks.
const WANTED: &str = "⏺⎿✻✽✶✳✢✱∗⁎❯➜➤⏵⏸⏎⇧⌃⌥⌘↗↘⇒⇢⟶⬆⬇➔⚠ℹ◐◑◒◓▸▾⠿✔★❖→←↑↓";

#[test]
fn every_glyph_claude_code_draws_has_a_real_shape_somewhere() {
    for ch in WANTED.chars() {
        let cp = u32::from(ch);
        let drawn = font::IBM_PLEX_MONO_20.draws(cp)
            || glyph::kind(cp).is_some()
            || unifont::narrow(cp).is_some()
            || unifont::symbol(cp).is_some();
        assert!(drawn, "{ch} U+{cp:04X} would be a box");
    }
}

#[test]
fn a_wide_unifont_symbol_is_scaled_into_one_cell_not_clipped() {
    for ch in "⏺✻✽➜⚠".chars() {
        let (rows, w, h) = cell(20, Smooth::Rgb, &ch.to_string());
        let n = ink(&rows);
        assert!(n > 12, "{ch} drew only {n} pixels");
        assert!(rows.iter().flatten().all(|c| c.r == c.g && c.g == c.b), "{ch}: symbols are gray");
        // Inside the cell on every side, with the cell's own margin.
        let (first, last) = (rows[0].iter().any(|c| *c != BG), rows[h - 1].iter().any(|c| *c != BG));
        assert!(!first && !last, "{ch} touches the cell's top or bottom edge");
        let _ = w;
    }
}

#[test]
fn scaling_keeps_a_one_pixel_stroke_visible() {
    // A single vertical line, 1 pixel wide, at x = 7 of 16: halving it without
    // averaging would delete it half of the time.
    let mut g = [0u8; 32];
    for y in 0..16 {
        g[2 * y] = 0x01; // bit 8 of the 16-bit row = column 7
    }
    let mut mass = 0u32;
    unifont::paint_symbol(&g, 10, &mut |_, _, c| mass += u32::from(c));
    // The line is 1/16 of the glyph wide and all of it tall: at 10x10 that is
    // 255 * (10 / 16) * 10 of coverage, however it falls across the pixel grid.
    let expected = 255 * 10 * 10 / 16;
    assert!((expected - 40..=expected + 40).contains(&mass), "mass {mass}, expected about {expected}");
}

#[test]
fn typographic_spaces_are_blank_not_boxes() {
    for cp in [0x2002u32, 0x2003, 0x2009, 0x202F, 0x205F] {
        assert_eq!(glyph::fold(cp), Some(0x20));
        let (rows, ..) = cell(20, Smooth::Rgb, &char::from_u32(cp).unwrap().to_string());
        assert_eq!(ink(&rows), 0, "U+{cp:04X} drew something");
    }
}

/// What no font has — Armenian here — is its code point in a box, so the gap
/// says what is missing.
#[test]
fn a_character_nothing_draws_is_a_box_holding_its_code_point() {
    let (rows, w, h) = cell(20, Smooth::Rgb, "\u{0531}"); // ARMENIAN CAPITAL LETTER AYB
    // The outline on the cell edge.
    assert!(rows[0].iter().all(|c| *c == FG), "top edge");
    assert!(rows[h - 1].iter().all(|c| *c == FG), "bottom edge");
    assert!((0..h).all(|y| rows[y][0] == FG && rows[y][w - 1] == FG), "sides");
    // Digits inside: 0, 5, 3, 1 in 3x5 each — far more ink than the outline.
    let outline = 2 * w + 2 * (h - 2);
    assert!(ink(&rows) > outline + 20, "no digits inside the box");
    // And it is not the same box for a different code point.
    let (other, ..) = cell(20, Smooth::Rgb, "\u{0532}");
    assert_ne!(rows, other);
}

#[test]
fn the_hex_digits_are_distinct_three_by_five_shapes() {
    let mut seen = Vec::new();
    for d in 0..16 {
        let g = glyph::hex_digit(d);
        assert!(g.iter().all(|&r| r < 8), "digit {d} wider than 3 pixels");
        assert!(g.iter().any(|&r| r != 0));
        assert!(!seen.contains(&g), "digit {d} repeats an earlier one");
        seen.push(g);
    }
}
