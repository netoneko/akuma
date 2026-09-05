//! The console, rendered into an in-memory surface and inspected pixel by
//! pixel.
//!
//! Everything a framebuffer console can get wrong is visible here: glyphs in
//! the wrong place, a scale that stretches unevenly, text drawn into the
//! overscan margin, a scroll that loses or duplicates a line, and writes past
//! the end of the surface — which on real hardware is not a wrong pixel but a
//! corrupted page.

use akuma_fbcon::console::{DEFAULT_FONT, FALLBACK_FONT, MAX_COLS, MAX_ROWS};
use akuma_fbcon::font::{self, Font};
use akuma_fbcon::{Console, Rgb, Surface};

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

/// Assert that `byte`'s glyph was drawn with its top-left at `(x0, y0)`.
///
/// Every pixel is checked against the colour the font's coverage calls for, not
/// merely against "is it lit". With an anti-aliased font most edge pixels are
/// neither the foreground nor the background, and a test that only asked
/// whether a pixel differed from black would pass on a glyph rendered at the
/// wrong weight, the wrong scale, or with the blend inverted.
fn assert_glyph_at(s: &MemSurface, font: &Font, byte: u8, x0: usize, y0: usize, fg: Rgb, bg: Rgb) {
    for gy in 0..font.height() {
        for gx in 0..font.width() {
            let want = bg.blend(fg, font.coverage(byte, gx, gy));
            let got = s.at(x0 + gx, y0 + gy);
            assert_eq!(
                got,
                want,
                "pixel ({gx},{gy}) of {:?}\n{}",
                char::from(byte),
                art(s, x0, y0, font.width(), font.height())
            );
        }
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
    assert_glyph_at(&s, DEFAULT_FONT, b'A', mx, my, Rgb::WHITE, Rgb::BLACK);
}

/// The other font has to render too, and at its own cell size. Spleen is half
/// the height of the default, so a console built on it must lay out on eight by
/// sixteen rather than on whatever the default happens to be.
#[test]
fn the_second_font_renders_at_its_own_size() {
    let font = &font::SPLEEN;
    assert_eq!((font.width(), font.height()), (8, 16));

    let mut con = Console::with_font_and_scale(MemSurface::new(320, 200), font, 1).unwrap();
    con.set_fg(Rgb::WHITE);
    con.clear();
    con.write_str_bytes("A");

    let (mx, my) = Console::<MemSurface>::auto_margin(320, 200);
    assert_eq!(con.font().width(), 8);
    let s = con.into_surface();
    assert_glyph_at(&s, font, b'A', mx, my, Rgb::WHITE, Rgb::BLACK);
}

/// Spleen is a bitmap font, so every pixel of it is fully on or fully off.
/// Widening it into the same coverage table the outline font uses must not have
/// invented an intermediate value anywhere.
#[test]
fn the_bitmap_font_stayed_one_bit() {
    for byte in 0x20..=0x7Eu8 {
        for (i, &coverage) in font::SPLEEN.cell(byte).iter().enumerate() {
            assert!(
                coverage == 0x00 || coverage == 0xFF,
                "Spleen {:?} pixel {i} is {coverage:#04x}, neither on nor off",
                char::from(byte)
            );
        }
    }
}

/// A byte the font has no glyph for must draw the replacement box, not whatever
/// bytes happen to follow the table. Both ends of the range and both fonts:
/// this is an index calculation, and an index calculation is wrong at the edges
/// or not at all.
#[test]
fn an_unmapped_byte_draws_the_replacement_box() {
    for font in [DEFAULT_FONT, &font::SPLEEN] {
        let box_glyph = font.cell(0x00);
        assert!(box_glyph.iter().any(|&c| c != 0), "the replacement box is blank");
        for byte in [0x00, 0x1F, 0x7F, 0x80, 0xFF] {
            assert_eq!(font.cell(byte), box_glyph, "byte {byte:#04x} in {}", font.name());
        }
        // ...and the code points that *are* mapped keep their own glyphs.
        assert_ne!(font.cell(b' '), box_glyph);
        assert_ne!(font.cell(b'~'), box_glyph);
    }
}

/// Coverage outside the cell reads as blank rather than panicking. The console
/// is what a kernel reports failures through; it must not be able to fail.
#[test]
fn coverage_outside_the_cell_is_blank() {
    let font = DEFAULT_FONT;
    assert_eq!(font.coverage(b'M', font.width(), 0), 0);
    assert_eq!(font.coverage(b'M', 0, font.height()), 0);
    assert_eq!(font.coverage(b'M', usize::MAX, usize::MAX), 0);
}

/// The blend is what makes an anti-aliased glyph look like the colour it was
/// asked for. Its two ends have to be exact: if full coverage lands a shade
/// short of the foreground, every solid glyph interior is subtly the wrong
/// colour and nothing about the output looks obviously broken.
#[test]
fn blending_is_exact_at_both_ends() {
    let (bg, fg) = (Rgb::BLACK, Rgb::TEXT);
    assert_eq!(bg.blend(fg, 0), bg);
    assert_eq!(bg.blend(fg, 255), fg);
    assert_eq!(bg.blend(fg, 128).r, 100, "0xC8 at 128/255, rounded");
    // Monotonic, and never outside the two colours it mixes.
    let mut last = bg;
    for c in 0..=255u8 {
        let got = bg.blend(fg, c);
        assert!(got.r >= last.r && got.r <= fg.r, "coverage {c} gave {got:?}");
        last = got;
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
    let font = DEFAULT_FONT;
    for gy in 0..font.height() {
        for gx in 0..font.width() {
            let want = Rgb::BLACK.blend(Rgb::WHITE, font.coverage(b'A', gx, gy));
            for dy in 0..scale {
                for dx in 0..scale {
                    let got = s.at(mx + gx * scale + dx, my + gy * scale + dy);
                    assert_eq!(
                        got, want,
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
    assert_glyph_at(&s, DEFAULT_FONT, expected_top, mx, my, Rgb::WHITE, Rgb::BLACK);
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
    let font = DEFAULT_FONT;
    let second_row_lit = (0..font.height())
        .flat_map(|y| (0..font.width()).map(move |x| (x, y)))
        .any(|(x, y)| s.at(mx + x, my + font.height() + y) != Rgb::BLACK);
    assert!(second_row_lit, "the wrapped text did not appear on the next row");
}

/// The scale is chosen from the resolution so one font serves a monitor and a
/// 4K television. These are the two machines this actually runs on.
#[test]
fn the_scale_suits_the_screen() {
    type C = Console<MemSurface>;
    // With the default 24-pixel font these are the scales that land near 48 rows.
    assert_eq!(C::auto_scale(DEFAULT_FONT, 768), 1, "1024x768 monitor");
    assert_eq!(C::auto_scale(DEFAULT_FONT, 1080), 1, "1080p");
    assert_eq!(C::auto_scale(DEFAULT_FONT, 2160), 2, "4K television");
    assert_eq!(C::auto_scale(DEFAULT_FONT, 480), 1, "640x480 fallback mode");
    assert_ne!(C::auto_scale(DEFAULT_FONT, 64), 0, "a tiny framebuffer still gets scale 1");

    // Half the cell height wants a bigger multiplier to reach the same rows.
    assert_eq!(C::auto_scale(&font::SPLEEN, 2160), 3, "4K television, small font");

    // Rounded to nearest, not truncated: 2160 is 1.875 cells' worth of the
    // default font, and truncating answers 1 -- ninety rows of text on a
    // television across a room, which is what the scale exists to prevent.
    assert_eq!(2160 / (DEFAULT_FONT.height() * 48), 1, "truncation would say 1");
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

    let full_redraw = cols * rows * DEFAULT_FONT.width() * DEFAULT_FONT.height();
    assert!(
        cost * 8 < full_redraw,
        "a scroll cost {cost} pixel writes, within 8x of a full redraw \
         ({full_redraw}) -- the changed-cell optimisation is not working"
    );
}

/// A usable terminal at every resolution — that is what the scale and the
/// font fallback are together for.
///
/// The floor is 72x24, not the 80x24 the chooser aims at, and the gap is
/// arithmetic rather than a compromise: 80 cells of the fallback font's
/// 8-pixel cell is 640 pixels exactly, so a 640x480 framebuffer cannot reach 80
/// columns *and* keep an overscan margin. No font choice fixes that, and the
/// chooser does the next best thing — it takes the larger of the two grids, 73
/// columns, where the default font would have given 49.
///
/// Every other mode clears 80 columns outright.
#[test]
fn every_real_resolution_gives_a_readable_grid() {
    for (w, h) in [(640, 480), (800, 600), (1024, 768), (1920, 1080), (3840, 2160)] {
        let con = Console::new(MemSurface::new(w, h)).unwrap();
        let least = if w == 640 { 72 } else { 80 };
        assert!(
            con.cols() >= least && con.rows() >= 24,
            "{w}x{h} gave {}x{} in {} at scale {}",
            con.cols(),
            con.rows(),
            con.font().name(),
            con.scale()
        );
        assert!(con.cols() <= MAX_COLS && con.rows() <= MAX_ROWS);
    }
}

/// When neither font can reach the target, the chooser still has to choose —
/// and it must take the bigger grid rather than defaulting to either name.
///
/// 640x480 is the case: 80 columns needs all 640 pixels at the fallback's cell
/// width, leaving nothing for the margin, so both fonts fall short and the
/// question becomes which falls short by less.
#[test]
fn neither_font_reaching_the_target_still_takes_the_better_grid() {
    type C = Console<MemSurface>;
    let (w, h) = (640, 480);
    let d = C::grid_for(DEFAULT_FONT, w, h, C::auto_scale(DEFAULT_FONT, h)).unwrap();
    let f = C::grid_for(FALLBACK_FONT, w, h, C::auto_scale(FALLBACK_FONT, h)).unwrap();
    assert!(d.0 < 80 && f.0 < 80, "one of them reached the target after all");
    assert!(f.0 * f.1 > d.0 * d.1);
    assert_eq!(C::choose_font(w, h).name(), FALLBACK_FONT.name());
}

/// The whole point of keeping a second font: where the default cannot make a
/// terminal, the fallback can.
///
/// Both halves matter. A fallback that never fires is dead weight, and one that
/// fires on a screen the default handles fine is a regression in how the output
/// looks — so this pins the boundary from both sides.
#[test]
fn the_font_falls_back_only_on_a_screen_that_needs_it() {
    type C = Console<MemSurface>;
    for (w, h) in [(640, 480), (800, 600), (1024, 768)] {
        assert_eq!(
            C::choose_font(w, h).name(),
            FALLBACK_FONT.name(),
            "{w}x{h} keeps the default at {:?}",
            C::grid_for(DEFAULT_FONT, w, h, C::auto_scale(DEFAULT_FONT, h))
        );
    }
    for (w, h) in [(1280, 720), (1280, 1024), (1920, 1080), (1920, 1200), (3840, 2160)] {
        assert_eq!(
            C::choose_font(w, h).name(),
            DEFAULT_FONT.name(),
            "{w}x{h} fell back at {:?}",
            C::grid_for(DEFAULT_FONT, w, h, C::auto_scale(DEFAULT_FONT, h))
        );
    }
}

/// The font that was chosen must be the font that gets drawn.
///
/// These are two separate calculations — one picks, one builds — and a console
/// that measured its grid with one font's cell and then blitted the other's
/// would put every glyph in the wrong place. `grid_for` is shared so that
/// cannot happen; this is the test that says so.
#[test]
fn the_console_is_built_in_the_font_that_was_chosen() {
    type C = Console<MemSurface>;
    for (w, h) in [(640, 480), (1024, 768), (1920, 1200), (3840, 2160)] {
        let con = Console::new(MemSurface::new(w, h)).unwrap();
        let want = C::choose_font(w, h);
        assert_eq!(con.font().name(), want.name(), "{w}x{h}");
        assert_eq!(
            (con.cols(), con.rows()),
            C::grid_for(want, w, h, C::auto_scale(want, h)).unwrap(),
            "{w}x{h} built a grid its own font does not describe"
        );
    }
}

/// The fallback is not simply "the smaller font".
///
/// Both fonts scale independently, and at 1920x1200 the 8x16 one rounds to
/// scale 2 while the 12x24 one stays at 1 — a 16x32 cell against a 12x24, so
/// the *small* font is the bigger of the two there. A chooser that compared
/// cell sizes rather than the grids they produce would get this backwards.
#[test]
fn the_fallback_font_is_not_always_the_smaller_cell() {
    type C = Console<MemSurface>;
    let (w, h) = (1920, 1200);
    let d = C::auto_scale(DEFAULT_FONT, h) * DEFAULT_FONT.height();
    let f = C::auto_scale(FALLBACK_FONT, h) * FALLBACK_FONT.height();
    assert!(f > d, "the fallback cell is {f} against the default's {d}");
    assert_eq!(C::choose_font(w, h).name(), DEFAULT_FONT.name());
}

/// An override stays an override: naming a font or a scale must not be second
/// guessed by the chooser, even on a screen the chooser would have rejected.
#[test]
fn naming_a_font_or_a_scale_disables_the_fallback() {
    let (w, h) = (640, 480);
    assert_eq!(
        Console::with_font(MemSurface::new(w, h), DEFAULT_FONT).unwrap().font().name(),
        DEFAULT_FONT.name()
    );
    assert_eq!(
        Console::with_scale(MemSurface::new(w, h), 1).unwrap().font().name(),
        DEFAULT_FONT.name()
    );
    // ...while the automatic path does fall back on that same screen.
    assert_eq!(
        Console::new(MemSurface::new(w, h)).unwrap().font().name(),
        FALLBACK_FONT.name()
    );
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
///
/// One cell is not enough surface for one cell, because the overscan margin is
/// reserved before any text is placed — and the margin is a fraction of the
/// surface, so the smallest size that works is searched for rather than
/// computed.
#[test]
fn a_surface_holding_exactly_one_glyph_is_accepted() {
    let font = DEFAULT_FONT;
    let (w, h) = (0..=font.height())
        .map(|margin| (font.width(), font.height() + 2 * margin))
        .find(|&(w, h)| Console::with_font(MemSurface::new(w, h), font).is_some())
        .expect("no surface at all holds a single glyph");

    let con = Console::with_font(MemSurface::new(w, h), font).unwrap();
    assert_eq!((con.cols(), con.rows()), (1, 1));
    assert!(
        Console::with_font(MemSurface::new(w, h - 1), font).is_none(),
        "one pixel less must be refused"
    );
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
