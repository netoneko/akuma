//! The console, rendered into an in-memory surface and inspected pixel by
//! pixel.
//!
//! Everything a framebuffer console can get wrong is visible here: glyphs in
//! the wrong place, a scale that stretches unevenly, text drawn into the
//! overscan margin, a scroll that loses or duplicates a line, and writes past
//! the end of the surface — which on real hardware is not a wrong pixel but a
//! corrupted page.

use akuma_fbcon::console::{MAX_COLS, MAX_ROWS};
use akuma_fbcon::{Console, Rgb, Surface, font};

/// A surface that records what was written and, crucially, **notices writes
/// that fall outside it** instead of quietly clipping them. On hardware those
/// land on whatever follows the framebuffer.
struct MemSurface {
    w: usize,
    h: usize,
    px: Vec<Rgb>,
    out_of_bounds: usize,
    /// Every accepted pixel write, so a test can measure drawing cost.
    writes: usize,
}

impl MemSurface {
    fn new(w: usize, h: usize) -> Self {
        Self { w, h, px: vec![Rgb::BLACK; w * h], out_of_bounds: 0, writes: 0 }
    }
    fn at(&self, x: usize, y: usize) -> Rgb {
        self.px[y * self.w + x]
    }
    fn count(&self, c: Rgb) -> usize {
        self.px.iter().filter(|p| **p == c).count()
    }
}

impl Surface for MemSurface {
    fn width(&self) -> usize {
        self.w
    }
    fn height(&self) -> usize {
        self.h
    }
    fn put(&mut self, x: usize, y: usize, color: Rgb) {
        if x >= self.w || y >= self.h {
            self.out_of_bounds += 1;
            return;
        }
        self.px[y * self.w + x] = color;
        self.writes += 1;
    }
}

/// Render the text area as ASCII art, for eyeballing a failure.
fn art(s: &MemSurface, x0: usize, y0: usize, w: usize, h: usize) -> String {
    let mut out = String::new();
    for y in y0..(y0 + h).min(s.h) {
        for x in x0..(x0 + w).min(s.w) {
            out.push(if s.at(x, y) == Rgb::BLACK { '.' } else { '#' });
        }
        out.push('\n');
    }
    out
}

#[test]
fn a_glyph_lands_where_the_font_says() {
    let mut con = Console::with_scale(MemSurface::new(320, 200), 1).unwrap();
    con.set_fg(Rgb::WHITE);
    con.clear();
    con.write_str_bytes("A");

    let (mx, my) = Console::<MemSurface>::auto_margin(320, 200);
    let s = con.into_surface();
    let glyph = font::glyph(b'A');
    for (gy, bits) in glyph.iter().enumerate() {
        for gx in 0..font::WIDTH {
            let expect_lit = bits & (0x80 >> gx) != 0;
            let got = s.at(mx + gx, my + gy);
            assert_eq!(
                got == Rgb::WHITE,
                expect_lit,
                "pixel ({gx},{gy}) of 'A' is wrong\n{}",
                art(&s, mx, my, 8, 8)
            );
        }
    }
}

/// Scaling must be square. A scale applied on one axis only is legible enough
/// to look like a success and wrong enough to waste an afternoon.
#[test]
fn scaling_expands_each_font_pixel_into_a_square() {
    let scale = 3;
    let mut con = Console::with_scale(MemSurface::new(320, 200), scale).unwrap();
    con.set_fg(Rgb::WHITE);
    con.clear();
    con.write_str_bytes("A");

    let (mx, my) = Console::<MemSurface>::auto_margin(320, 200);
    let s = con.into_surface();
    let glyph = font::glyph(b'A');
    for (gy, bits) in glyph.iter().enumerate() {
        for gx in 0..font::WIDTH {
            let expect_lit = bits & (0x80 >> gx) != 0;
            for dy in 0..scale {
                for dx in 0..scale {
                    let got = s.at(mx + gx * scale + dx, my + gy * scale + dy);
                    assert_eq!(
                        got == Rgb::WHITE,
                        expect_lit,
                        "block ({gx},{gy}) sub-pixel ({dx},{dy}) wrong at scale {scale}"
                    );
                }
            }
        }
    }
}

/// Televisions crop the edges. Text drawn there is not "slightly off" — it is
/// invisible, and indistinguishable from a kernel that printed nothing.
#[test]
fn nothing_is_drawn_inside_the_overscan_margin() {
    let (w, h) = (1024, 768);
    let mut con = Console::new(MemSurface::new(w, h)).unwrap();
    con.set_fg(Rgb::WHITE);
    con.set_bg(Rgb::BLACK);
    con.clear();
    for _ in 0..MAX_ROWS * 2 {
        con.write_str_bytes("MMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMM\n");
    }

    let (mx, my) = Console::<MemSurface>::auto_margin(w, h);
    assert!(mx > 0 && my > 0, "a television needs a real margin");
    let s = con.into_surface();
    for y in 0..h {
        for x in 0..w {
            let inside = x >= mx && y >= my && x < w - mx && y < h - my;
            if !inside {
                assert_eq!(s.at(x, y), Rgb::BLACK, "pixel ({x},{y}) is in the margin");
            }
        }
    }
}

#[test]
fn text_never_escapes_the_surface() {
    let mut con = Console::new(MemSurface::new(640, 480)).unwrap();
    con.clear();
    for i in 0..200 {
        con.write_str_bytes("the quick brown fox jumps over the lazy dog 0123456789 ");
        if i % 3 == 0 {
            con.write_byte(b'\n');
        }
    }
    let s = con.into_surface();
    assert_eq!(s.out_of_bounds, 0, "wrote outside the framebuffer");
}

/// Scrolling is the operation that would tempt a reader of video memory. What
/// it must do is keep the last N lines and drop the first.
#[test]
fn scrolling_keeps_the_newest_lines() {
    let mut con = Console::with_scale(MemSurface::new(320, 200), 1).unwrap();
    con.set_fg(Rgb::WHITE);
    con.clear();
    let rows = con.rows();

    // One distinct character per line, more lines than fit. The newline goes
    // *between* lines, never after the last one: a trailing newline opens a
    // further empty line and shifts what "the top line" means by one, which is
    // exactly the off-by-one this test exists to pin down.
    for i in 0..(rows + 5) {
        if i > 0 {
            con.write_byte(b'\n');
        }
        con.write_byte(b'a' + (i % 26) as u8);
    }

    let (mx, my) = Console::<MemSurface>::auto_margin(320, 200);
    let s = con.into_surface();

    // `rows + 5` lines were written and `rows` fit, so the first five scrolled
    // off and line 5 is now at the top.
    let expected_top = b'a' + 5;
    let glyph = font::glyph(expected_top);
    for (gy, bits) in glyph.iter().enumerate() {
        for gx in 0..font::WIDTH {
            let expect_lit = bits & (0x80 >> gx) != 0;
            assert_eq!(
                s.at(mx + gx, my + gy) == Rgb::WHITE,
                expect_lit,
                "after scrolling, the top line is not the {}th line written\n{}",
                5,
                art(&s, mx, my, 8, 8)
            );
        }
    }
}

/// A line longer than the grid wraps rather than running off the right edge.
#[test]
fn long_lines_wrap() {
    let mut con = Console::with_scale(MemSurface::new(320, 200), 1).unwrap();
    con.clear();
    let cols = con.cols();
    let long: String = core::iter::repeat_n('X', cols + 5).collect();
    con.write_str_bytes(&long);

    let (mx, my) = Console::<MemSurface>::auto_margin(320, 200);
    let s = con.into_surface();
    assert_eq!(s.out_of_bounds, 0);
    // Something was drawn on the second row: the overflow went down, not away.
    let second_row_lit = (0..8)
        .flat_map(|y| (0..8).map(move |x| (x, y)))
        .any(|(x, y)| s.at(mx + x, my + font::HEIGHT + y) != Rgb::BLACK);
    assert!(second_row_lit, "the wrapped text did not appear on the next row");
}

/// The scale is chosen from the resolution so one font serves a monitor and a
/// 4K television. These are the two machines this actually runs on.
#[test]
fn the_scale_suits_the_screen() {
    type C = Console<MemSurface>;
    // With a 16-pixel font these are the scales that land near 48 rows.
    assert_eq!(C::auto_scale(768), 1, "1024x768 monitor");
    assert_eq!(C::auto_scale(1080), 1, "1080p");
    assert_eq!(C::auto_scale(2160), 2, "4K television");
    assert_eq!(C::auto_scale(480), 1, "640x480 fallback mode");
    assert_ne!(C::auto_scale(64), 0, "a tiny framebuffer still gets scale 1");
}

/// Scrolling must cost work proportional to the **text**, not to the screen.
///
/// The naive version redraws every cell, which at 4K is seven million uncached
/// writes per line of output. This pins the cheap behaviour: a screenful of
/// short lines scrolls for a small fraction of a full redraw.
#[test]
fn scrolling_sparse_text_does_not_redraw_the_screen() {
    let mut con = Console::new(MemSurface::new(1920, 1080)).unwrap();
    con.clear();
    let (cols, rows) = (con.cols(), con.rows());

    // Fill the screen with short lines, then measure one more line of output.
    for _ in 0..rows {
        con.write_str_bytes("short line\n");
    }
    let before = con.surface_mut().writes;
    con.write_str_bytes("one more\n");
    let cost = con.surface_mut().writes - before;

    let full_redraw = cols * rows * font::WIDTH * font::HEIGHT;
    assert!(
        cost * 8 < full_redraw,
        "a scroll cost {cost} pixel writes, within 8x of a full redraw \
         ({full_redraw}) -- the changed-cell optimisation is not working"
    );
}

/// Roughly 40 to 60 rows at every resolution — that is what the scale is for.
#[test]
fn every_real_resolution_gives_a_readable_grid() {
    for (w, h) in [(640, 480), (800, 600), (1024, 768), (1920, 1080), (3840, 2160)] {
        let con = Console::new(MemSurface::new(w, h)).unwrap();
        assert!(
            (20..=MAX_ROWS).contains(&con.rows()),
            "{w}x{h} gave {} rows at scale {}",
            con.rows(),
            con.scale()
        );
        assert!(con.cols() >= 40, "{w}x{h} gave only {} columns", con.cols());
        assert!(con.cols() <= MAX_COLS && con.rows() <= MAX_ROWS);
    }
}

/// A framebuffer too small for one character is a mis-parsed tag, not a
/// console. Say so instead of dividing by zero.
#[test]
fn an_impossible_surface_is_refused() {
    assert!(Console::new(MemSurface::new(0, 0)).is_none());
    assert!(Console::new(MemSurface::new(4, 4)).is_none());
    assert!(Console::new(MemSurface::new(320, 4)).is_none());
}

/// Exactly one glyph still counts as a console. The boundary is worth pinning:
/// it is where the "too small" check has to stop refusing.
#[test]
fn a_surface_holding_exactly_one_glyph_is_accepted() {
    let con = Console::new(MemSurface::new(font::WIDTH, font::HEIGHT)).unwrap();
    assert_eq!((con.cols(), con.rows()), (1, 1));
}

/// `flood` is the first thing a bring-up does: it proves the address, the pitch
/// and the pixel format at once, before any font is involved.
#[test]
fn flood_paints_every_pixel_including_the_margin() {
    let (w, h) = (320, 200);
    let mut con = Console::new(MemSurface::new(w, h)).unwrap();
    con.flood(Rgb::GOOD);
    let s = con.into_surface();
    assert_eq!(s.count(Rgb::GOOD), w * h);
    assert_eq!(s.out_of_bounds, 0);
}

#[test]
fn write_macro_works() {
    use core::fmt::Write;
    let mut con = Console::new(MemSurface::new(640, 480)).unwrap();
    con.clear();
    write!(con, "fb {}x{} @ {:#x}", 1024, 768, 0xe000_0000u64).unwrap();
    let s = con.into_surface();
    assert!(s.count(Rgb::TEXT) > 0, "formatted output drew nothing");
}
