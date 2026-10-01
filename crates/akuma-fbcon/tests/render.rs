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
        // 0x80..=0x9F are C1 controls and 0xA0.. is Latin-1 now, so `0xFF` is `ÿ`.
        for byte in [0x00, 0x1F, 0x7F, 0x80, 0x9F] {
            assert_eq!(font.cell(byte), box_glyph, "byte {byte:#04x} in {}", font.name());
        }
        // ...and the code points that *are* mapped keep their own glyphs.
        assert_ne!(font.cell(b' '), box_glyph);
        assert_ne!(font.cell(b'~'), box_glyph);
        assert_ne!(font.cell(0xFF), box_glyph, "ÿ is a real glyph now");
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
    let shift = Console::<MemSurface>::SCROLL_ROWS.min(rows - 1);

    // One distinct character per line, driven to exactly one scroll: `rows`
    // lines fill the screen, and the next line is the one that scrolls. The
    // newline goes *between* lines, never after the last one — a trailing
    // newline opens a further empty line and shifts what "the top line" means by
    // one, which is exactly the off-by-one this test exists to pin down.
    let total = rows + 1;
    for i in 0..total {
        if i > 0 {
            con.write_byte(b'\n');
        }
        con.write_byte(b'a' + (i % 26) as u8);
    }

    let (mx, my) = Console::<MemSurface>::auto_margin(320, 200);
    let s = con.into_surface();

    // That one scroll advanced by `shift` rows, so lines `0..shift` fell off and
    // line `shift` is now at the top.
    let expected_top = b'a' + (shift % 26) as u8;
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
    // 1024x768 used to be here: the 4 % margin cost it the 80 columns. With the
    // margin under 1 % it reaches 84x31 in the default face, which is the point.
    for (w, h) in [(640, 480), (800, 600)] {
        assert_eq!(
            C::choose_font(w, h).name(),
            FALLBACK_FONT.name(),
            "{w}x{h} keeps the default at {:?}",
            C::grid_for(DEFAULT_FONT, w, h, C::auto_scale(DEFAULT_FONT, h))
        );
    }
    for (w, h) in [(1024, 768), (1280, 720), (1280, 1024), (1920, 1080), (1920, 1200)] {
        assert_eq!(
            C::choose_font(w, h).name(),
            DEFAULT_FONT.name(),
            "{w}x{h} fell back at {:?}",
            C::grid_for(DEFAULT_FONT, w, h, C::auto_scale(DEFAULT_FONT, h))
        );
    }
}

/// Where the default font would be drawn doubled (scale 2) the console uses the HD
/// cut instead: the same cell on the glass, so the grid must not change, and the
/// scale is 1 -- glyphs rasterized at 24x48, not 12x24 with every pixel repeated.
#[test]
fn a_4k_screen_gets_the_hd_cut_with_the_same_grid() {
    type C = Console<MemSurface>;
    let (w, h) = (3840, 2160);
    assert_eq!(C::auto_scale(DEFAULT_FONT, h), 2, "premise: the default would be doubled");
    let font = C::choose_font(w, h);
    assert_eq!(font.name(), akuma_fbcon::console::HD_FONT.name());
    assert_eq!((font.width(), font.height()), (2 * DEFAULT_FONT.width(), 2 * DEFAULT_FONT.height()));
    assert_eq!(C::auto_scale(font, h), 1);
    let old = C::grid_for(DEFAULT_FONT, w, h, 2).unwrap();
    assert_eq!(C::grid_for(font, w, h, 1).unwrap(), old, "the grid moved");
    assert_eq!(old, (157, 44));
}

/// The HD cut is the same typeface, not a different one: every ASCII glyph's ink
/// footprint, scaled down 2x, lands where the 12x24 glyph's does (within a pixel).
#[test]
fn the_hd_cut_has_the_same_shapes_as_the_default() {
    let (sd, hd) = (DEFAULT_FONT, akuma_fbcon::console::HD_FONT);
    for c in (0x21u32..0x7F).filter(|&c| char::from_u32(c).is_some_and(char::is_alphanumeric)) {
        let mass = |f: &akuma_fbcon::font::Font| -> f64 {
            f.cell_cp(c).iter().map(|&v| f64::from(v)).sum::<f64>() / 255.0 / (f.width() * f.height()) as f64
        };
        let (a, b) = (mass(sd), mass(hd));
        // Ink fraction of the cell agrees to within ~35%: the HD cut carries the
        // coverage boost, so it is a little heavier on edges, never lighter.
        assert!(b >= a * 0.95 && b <= a * 1.35, "{c:#x}: ink {a:.3} vs {b:.3}");
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

// ---------------------------------------------------------------------------
// The shell's half of the contract: backspace and the ANSI subset busybox's line
// editor writes. Each test types what a shell types and then asks the console
// what it believes is on the screen (`cell_at`) *and*, where it matters, what is
// actually on the surface — a grid that says "blank" over pixels that still
// show a glyph is the failure that only a TV would reveal.
// ---------------------------------------------------------------------------

fn term() -> Console<MemSurface> {
    let mut con = Console::with_scale(MemSurface::new(640, 400), 1).unwrap();
    con.set_fg(Rgb::WHITE);
    con.clear();
    con
}

fn row_text(con: &Console<MemSurface>, row: usize) -> String {
    let cols = con.cols();
    (0..cols).map(|c| con.cell_at(row, c)).collect::<String>().trim_end().to_string()
}

/// Prove the pixels agree with the grid for every cell of `row`.
fn assert_row_on_glass(con: Console<MemSurface>, row: usize) {
    let (cols, font) = (con.cols(), con.font());
    let want: Vec<u8> = (0..cols).map(|c| con.cell_at(row, c) as u8).collect();
    let s = con.into_surface();
    let (mx, my) = Console::<MemSurface>::auto_margin(s.w, s.h);
    for (c, &b) in want.iter().enumerate() {
        assert_glyph_at(&s, font, b, mx + c * font.width(), my + row * font.height(), Rgb::WHITE, Rgb::BLACK);
    }
}

#[test]
fn backspace_moves_left_without_erasing() {
    let mut con = term();
    con.write_str_bytes("abc\x08");
    assert_eq!(con.cursor(), (0, 2));
    assert_eq!(row_text(&con, 0), "abc", "BS alone must not erase");
}

#[test]
fn the_line_disciplines_erase_sequence_erases() {
    // ECHOE: backspace, space, backspace.
    let mut con = term();
    con.write_str_bytes("abc\x08 \x08");
    assert_eq!(con.cursor(), (0, 2));
    assert_eq!(row_text(&con, 0), "ab");
    assert_row_on_glass(con, 0);
}

#[test]
fn backspace_at_column_zero_stays_put() {
    let mut con = term();
    con.write_str_bytes("\x08\x08");
    assert_eq!(con.cursor(), (0, 0));
}

#[test]
fn escape_sequences_are_never_drawn() {
    let mut con = term();
    con.write_str_bytes("a\x1b[31mb\x1b[0m\x1b[?25h\x1b[?2004hc\x1b]0;title\x07d\x1b=e");
    assert_eq!(row_text(&con, 0), "abcde");
}

#[test]
fn a_sequence_split_across_writes_still_parses() {
    // The console sees one byte per call, and the pump hands it whatever a lap
    // moved — a sequence cut anywhere must not leak its tail as text.
    let mut con = term();
    con.write_str_bytes("xy\x1b");
    con.write_str_bytes("[");
    con.write_str_bytes("1");
    con.write_str_bytes("D");
    con.write_str_bytes("Z");
    assert_eq!(row_text(&con, 0), "xZ");
}

#[test]
fn cursor_left_and_right_move_by_the_count_and_default_to_one() {
    let mut con = term();
    con.write_str_bytes("0123456789\x1b[3D");
    assert_eq!(con.cursor(), (0, 7));
    con.write_str_bytes("\x1b[D");
    assert_eq!(con.cursor(), (0, 6));
    con.write_str_bytes("\x1b[0D"); // 0 means 1
    assert_eq!(con.cursor(), (0, 5));
    con.write_str_bytes("\x1b[2C");
    assert_eq!(con.cursor(), (0, 7));
    con.write_str_bytes("\x1b[999C");
    assert_eq!(con.cursor(), (0, con.cols() - 1), "clamped, not wrapped");
    con.write_str_bytes("\x1b[999D");
    assert_eq!(con.cursor(), (0, 0));
}

#[test]
fn erase_to_end_of_line_keeps_what_is_left_of_the_cursor() {
    let mut con = term();
    con.write_str_bytes("hello world\x1b[6D\x1b[K");
    assert_eq!(row_text(&con, 0), "hello");
    assert_eq!(con.cursor(), (0, 5), "erasing does not move the cursor");
    assert_row_on_glass(con, 0);
}

#[test]
fn erase_variants_of_k() {
    let mut con = term();
    con.write_str_bytes("abcdef\x1b[3D\x1b[1K");
    assert_eq!(row_text(&con, 0), "    ef", "1K erases up to and including the cursor");
    con.write_str_bytes("\x1b[2K");
    assert_eq!(row_text(&con, 0), "");
}

#[test]
fn erase_to_end_of_screen_is_what_a_prompt_redraw_uses() {
    let mut con = term();
    con.write_str_bytes("one\r\ntwo\r\nthree\x1b[A\x1b[A\r\x1b[2C\x1b[J");
    assert_eq!(con.cursor(), (0, 2));
    assert_eq!(row_text(&con, 0), "on");
    assert_eq!(row_text(&con, 1), "");
    assert_eq!(row_text(&con, 2), "");
}

#[test]
fn clear_screen_and_home() {
    let mut con = term();
    con.write_str_bytes("junk\r\nmore\x1b[2J\x1b[Hok");
    assert_eq!(row_text(&con, 0), "ok");
    assert_eq!(row_text(&con, 1), "");
    assert_eq!(con.cursor(), (0, 2));
}

#[test]
fn absolute_positioning_is_one_based_and_clamped() {
    let mut con = term();
    con.write_str_bytes("\x1b[3;5Hx");
    assert_eq!(con.cell_at(2, 4), 'x');
    con.write_str_bytes("\x1b[;;H");
    assert_eq!(con.cursor(), (0, 0));
    con.write_str_bytes("\x1b[500;500H");
    assert_eq!(con.cursor(), (con.rows() - 1, con.cols() - 1));
}

#[test]
fn a_cursor_move_from_a_pending_wrap_starts_at_the_last_column() {
    let mut con = term();
    let cols = con.cols();
    for _ in 0..cols {
        con.write_byte(b'x');
    }
    assert_eq!(con.cursor(), (0, cols), "the deferred-wrap state");
    con.write_str_bytes("\x1b[D");
    assert_eq!(con.cursor(), (0, cols - 2));
}

#[test]
fn cancel_aborts_a_sequence() {
    let mut con = term();
    con.write_str_bytes("\x1b[12\x18ok");
    assert_eq!(row_text(&con, 0), "ok");
}

#[test]
fn ordinary_output_is_untouched_by_the_parser() {
    // The kernel's own output is the whole of what this console showed until a
    // shell was attached; it must come out exactly as it did.
    let mut con = term();
    con.write_str_bytes("[ ok ] boot: 100% {x} <y> 1;2 [3]\n\ttab");
    assert_eq!(row_text(&con, 0), "[ ok ] boot: 100% {x} <y> 1;2 [3]");
    assert_eq!(row_text(&con, 1), "        tab");
}

#[test]
fn escape_handling_never_draws_outside_the_surface() {
    let mut con = term();
    con.write_str_bytes("\x1b[9999;9999H\x1b[J\x1b[1J\x1b[2K\x1b[999Cx\x1b[999Bx");
    assert_eq!(con.into_surface().out_of_bounds, 0);
}

// ---------------------------------------------------------------------------
// The cursor: drawn on request, taken down by the next byte, never a stale one.
// ---------------------------------------------------------------------------

#[test]
fn the_cursor_is_the_cell_with_its_colours_swapped() {
    let mut con = term();
    con.write_str_bytes("ab");
    con.show_cursor();
    let font = con.font();
    let (cols_px, rows_px) = (font.width(), font.height());
    let s = con.into_surface();
    let (mx, my) = Console::<MemSurface>::auto_margin(s.w, s.h);
    // Column 2 holds nothing: a swapped blank is a solid block of foreground.
    for y in 0..rows_px {
        for x in 0..cols_px {
            assert_eq!(s.at(mx + 2 * cols_px + x, my + y), Rgb::WHITE, "block pixel ({x},{y})");
        }
    }
    // And column 1's `b` is untouched.
    assert_glyph_at(&s, font, b'b', mx + cols_px, my, Rgb::WHITE, Rgb::BLACK);
}

#[test]
fn the_next_byte_takes_the_cursor_down_before_drawing() {
    let mut con = term();
    con.show_cursor();
    con.write_byte(b'x');
    assert_eq!(row_text(&con, 0), "x");
    // Column 0 was under the cursor and now holds `x`; column 1 must be plain
    // background, not a leftover block.
    assert_row_on_glass(con, 0);
}

#[test]
fn showing_the_cursor_twice_draws_once() {
    let mut con = Console::with_scale(MemSurface::new(640, 400), 1).unwrap();
    con.clear();
    con.show_cursor();
    let writes = con.surface_mut().writes;
    con.show_cursor();
    assert_eq!(con.surface_mut().writes, writes, "idempotent: the idle lap must be free");
}

#[test]
fn the_cursor_follows_a_move_without_leaving_a_ghost() {
    let mut con = term();
    con.write_str_bytes("abc");
    con.show_cursor();
    con.write_str_bytes("\x08\x08"); // hides, then moves left twice
    con.show_cursor();
    assert_eq!(con.cursor(), (0, 1));
    let font = con.font();
    let s = con.into_surface();
    let (mx, my) = Console::<MemSurface>::auto_margin(s.w, s.h);
    // `c` at column 2 was under the first cursor: it must be a plain `c` again.
    assert_glyph_at(&s, font, b'c', mx + 2 * font.width(), my, Rgb::WHITE, Rgb::BLACK);
    // `b` at column 1 is under the cursor now: solid-ish inverse, not plain.
    assert_ne!(s.at(mx + font.width(), my), Rgb::BLACK);
}

#[test]
fn a_cursor_after_a_full_line_sits_on_the_last_column() {
    let mut con = term();
    let cols = con.cols();
    for _ in 0..cols {
        con.write_byte(b'x');
    }
    con.show_cursor(); // pending-wrap state: col == cols
    assert_eq!(con.into_surface().out_of_bounds, 0);
}

#[test]
fn a_cursor_survives_a_scroll() {
    let mut con = term();
    con.show_cursor();
    for _ in 0..con.rows() + 3 {
        con.write_str_bytes("line\n");
    }
    con.show_cursor();
    assert_eq!(con.into_surface().out_of_bounds, 0);
}

// ---------------------------------------------------------------------------
// Unicode, attributes, wide characters, scrolling regions, replies, the view.
// ---------------------------------------------------------------------------

#[test]
fn utf8_text_is_decoded_not_drawn_byte_by_byte() {
    // Before 2026-10-01 every byte of a multi-byte character was its own box.
    let mut con = term();
    con.write_str_bytes("café ─ ● naïve · Łódź");
    assert_eq!(row_text(&con, 0), "café ─ ● naïve · Łódź");
}

#[test]
fn utf8_split_across_writes_still_decodes() {
    let mut con = term();
    for b in "é─".bytes() {
        con.write_byte(b);
    }
    assert_eq!(row_text(&con, 0), "é─");
    assert_eq!(con.cursor(), (0, 2), "two characters, two columns");
}

#[test]
fn invalid_utf8_shows_one_box_per_fault_and_recovers() {
    let mut con = term();
    for b in [b'a', 0xC3, b'b', 0xFF, b'c', 0xE2, 0x94] {
        con.write_byte(b);
    }
    con.write_byte(b'd'); // the truncated 3-byte sequence is broken by 'd'
    let t = row_text(&con, 0);
    assert_eq!(t.chars().filter(|&c| c == '\u{FFFD}').count(), 3, "{t:?}");
    assert!(t.starts_with('a') && t.contains('b') && t.contains('c') && t.ends_with('d'));
}

#[test]
fn overlong_and_surrogate_encodings_are_refused() {
    let mut con = term();
    for b in [0xC0, 0x80, 0xED, 0xA0, 0x80] {
        con.write_byte(b);
    }
    let t = row_text(&con, 0);
    assert!(t.chars().all(|c| c == '\u{FFFD}'), "{t:?}");
}

#[test]
fn a_wide_character_takes_two_columns_and_wraps_early() {
    let mut con = term();
    con.write_str_bytes("中");
    assert_eq!(con.cursor(), (0, 2));
    assert_eq!(con.cell_at(0, 0), '中');
    assert_eq!(con.cell_at(0, 1), '\u{FFFF}', "the right half is a continuation");
    // One column left on the line: a wide character wraps instead of splitting.
    let mut con = term();
    let cols = con.cols();
    for _ in 0..cols - 1 {
        con.write_byte(b'x');
    }
    con.write_str_bytes("中");
    assert_eq!(con.cursor(), (1, 2), "wrapped to the next line");
}

#[test]
fn an_emoji_keeps_the_line_aligned() {
    let mut con = term();
    con.write_str_bytes("a😂b");
    // a, the emoji (2 columns), b: b lands in column 3.
    assert_eq!(con.cursor(), (0, 4));
    assert_eq!(con.cell_at(0, 3), 'b');
}

#[test]
fn zero_width_characters_take_no_column() {
    let mut con = term();
    con.write_str_bytes("a\u{200D}\u{FE0F}b");
    assert_eq!(row_text(&con, 0), "ab");
}

#[test]
fn overwriting_half_a_wide_character_blanks_the_other_half() {
    let mut con = term();
    con.write_str_bytes("中\r");
    con.write_byte(b'x'); // lands on the left half
    assert_eq!(con.cell_at(0, 0), 'x');
    assert_eq!(con.cell_at(0, 1), ' ', "the orphaned right half is blank");
}

#[test]
fn box_drawing_is_drawn_without_gaps_between_cells() {
    // A horizontal rule across five cells must be one unbroken run of pixels.
    let mut con = Console::with_scale(MemSurface::new(640, 400), 1).unwrap();
    con.set_fg(Rgb::WHITE);
    con.clear();
    con.write_str_bytes("─────");
    let font = con.font();
    let (cw, ch) = (font.width(), font.height());
    let s = con.into_surface();
    let (mx, my) = Console::<MemSurface>::auto_margin(s.w, s.h);
    let y = (0..ch).find(|&y| s.at(mx, my + y) == Rgb::WHITE).expect("a line");
    for x in 0..5 * cw {
        assert_eq!(s.at(mx + x, my + y), Rgb::WHITE, "gap at x={x}");
    }
}

#[test]
fn a_filled_circle_and_an_arrow_are_drawn_not_boxed() {
    let mut con = Console::with_scale(MemSurface::new(640, 400), 1).unwrap();
    con.set_fg(Rgb::WHITE);
    con.clear();
    con.write_str_bytes("●→");
    let font = con.font();
    let (cw, ch) = (font.width(), font.height());
    let s = con.into_surface();
    let (mx, my) = Console::<MemSurface>::auto_margin(s.w, s.h);
    let ink = |c: usize| {
        (0..ch).flat_map(|y| (0..cw).map(move |x| (x, y)))
            .filter(|&(x, y)| s.at(mx + c * cw + x, my + y) != Rgb::BLACK)
            .count()
    };
    assert!(ink(0) > 40, "● drew {} pixels", ink(0));
    assert!(ink(1) > 12, "→ drew {} pixels", ink(1));
    // The replacement box is a hollow rectangle: its centre is empty. A disc's is not.
    let (cx, cy) = (mx + cw / 2, my + ch * 55 / 100);
    assert_eq!(s.at(cx, cy), Rgb::WHITE, "● is solid in the middle");
}

#[test]
fn colours_and_attributes_are_stored_per_cell() {
    let mut con = term();
    con.write_str_bytes("\x1b[31mR\x1b[0mn\x1b[1mB\x1b[0m");
    let font = con.font();
    let (cw, ch) = (font.width(), font.height());
    let s = con.into_surface();
    let (mx, my) = Console::<MemSurface>::auto_margin(s.w, s.h);
    let s = &s;
    let brightest = |col: usize| {
        (0..ch).flat_map(|y| (0..cw).map(move |x| s.at(mx + col * cw + x, my + y)))
            .max_by_key(|c| u32::from(c.r) + u32::from(c.g) + u32::from(c.b)).unwrap()
    };
    assert_eq!(brightest(0), Rgb::new(0xE0, 0x50, 0x50), "ESC[31m is the palette red");
    assert_ne!(brightest(1), Rgb::new(0xE0, 0x50, 0x50), "ESC[0m ends it");
    assert_eq!(brightest(2), Rgb::WHITE, "bold default text is bright");
}

#[test]
fn reverse_video_swaps_foreground_and_background() {
    let mut con = term();
    con.write_str_bytes("\x1b[7m \x1b[0m");
    let (cw, ch) = (con.font().width(), con.font().height());
    let s = con.into_surface();
    let (mx, my) = Console::<MemSurface>::auto_margin(s.w, s.h);
    // A reversed space is a solid block of the foreground colour.
    for y in 0..ch {
        for x in 0..cw {
            assert_eq!(s.at(mx + x, my + y), Rgb::WHITE, "({x},{y})");
        }
    }
}

#[test]
fn truecolor_is_mapped_to_the_nearest_palette_entry() {
    let mut con = term();
    con.write_str_bytes("\x1b[38;2;255;0;0mX\x1b[38;5;21mY");
    let (cw, ch) = (con.font().width(), con.font().height());
    let s = con.into_surface();
    let (mx, my) = Console::<MemSurface>::auto_margin(s.w, s.h);
    let s = &s;
    let top = |col: usize| {
        (0..ch).flat_map(|y| (0..cw).map(move |x| s.at(mx + col * cw + x, my + y)))
            .max_by_key(|c| u32::from(c.r) + u32::from(c.g) + u32::from(c.b)).unwrap()
    };
    assert_eq!(top(0), Rgb::new(255, 0, 0), "pure red is cube entry 196");
    assert_eq!(top(1), Rgb::new(0, 0, 255), "palette 21 is pure blue");
}

#[test]
fn erase_characters_insert_and_delete_work_in_place() {
    let mut con = term();
    con.write_str_bytes("abcdef\x1b[4D\x1b[2X"); // erase 2 at column 2
    assert_eq!(row_text(&con, 0), "ab  ef");
    let mut con = term();
    con.write_str_bytes("abcdef\x1b[4D\x1b[2@"); // insert 2 blanks at column 2
    assert_eq!(row_text(&con, 0), "ab  cdef");
    let mut con = term();
    con.write_str_bytes("abcdef\x1b[4D\x1b[2P"); // delete 2 at column 2
    assert_eq!(row_text(&con, 0), "abef");
}

#[test]
fn repeat_repeats_the_last_character() {
    let mut con = term();
    con.write_str_bytes("─\x1b[4b");
    assert_eq!(row_text(&con, 0), "─────");
}

#[test]
fn insert_and_delete_line_shift_the_rows_below() {
    let mut con = term();
    con.write_str_bytes("one\r\ntwo\r\nthree\x1b[2;1H\x1b[L");
    assert_eq!(row_text(&con, 1), "");
    assert_eq!(row_text(&con, 2), "two");
    assert_eq!(row_text(&con, 3), "three");
    con.write_str_bytes("\x1b[M");
    assert_eq!(row_text(&con, 1), "two");
    assert_eq!(row_text(&con, 2), "three");
}

#[test]
fn a_scroll_region_scrolls_only_its_rows_one_at_a_time() {
    let mut con = term();
    con.write_str_bytes("top\x1b[2;4r"); // rows 2..4 scroll; row 1 does not
    con.write_str_bytes("\x1b[2;1Hr2\r\nr3\r\nr4\r\nr5");
    assert_eq!(row_text(&con, 0), "top", "outside the region: untouched");
    assert_eq!(row_text(&con, 1), "r3");
    assert_eq!(row_text(&con, 2), "r4");
    assert_eq!(row_text(&con, 3), "r5", "scrolled by exactly one row");
}

#[test]
fn save_and_restore_cursor() {
    let mut con = term();
    con.write_str_bytes("ab\x1b7\r\n\r\nxx\x1b8Z");
    assert_eq!(con.cell_at(0, 2), 'Z');
    let mut con = term();
    con.write_str_bytes("ab\x1b[s\x1b[3;3H\x1b[uZ");
    assert_eq!(con.cell_at(0, 2), 'Z');
}

#[test]
fn the_alternate_screen_clears_on_entry_and_exit() {
    let mut con = term();
    con.write_str_bytes("shell$ ");
    con.write_str_bytes("\x1b[?1049h");
    assert_eq!(row_text(&con, 0), "", "entry clears");
    con.write_str_bytes("\x1b[1;1Hfull screen app");
    con.write_str_bytes("\x1b[?1049l");
    assert_eq!(row_text(&con, 0), "", "exit clears");
    assert_eq!(con.cursor(), (0, 7), "the cursor comes back");
}

#[test]
fn the_terminal_answers_cursor_position_and_device_queries() {
    let mut con = term();
    con.write_str_bytes("abc\x1b[6n");
    let mut out = [0u8; 32];
    let n = con.take_reply(&mut out);
    assert_eq!(&out[..n], b"\x1b[1;4R", "row 1, column 4");
    assert_eq!(con.take_reply(&mut out), 0, "taken once");
    con.write_str_bytes("\x1b[c");
    let n = con.take_reply(&mut out);
    assert_eq!(&out[..n], b"\x1b[?1;2c");
    con.write_str_bytes("\x1b[5n");
    let n = con.take_reply(&mut out);
    assert_eq!(&out[..n], b"\x1b[0n");
}

#[test]
fn the_terminal_answers_a_background_colour_query() {
    let mut con = term();
    con.write_str_bytes("\x1b]11;?\x07");
    let mut out = [0u8; 48];
    let n = con.take_reply(&mut out);
    let s = core::str::from_utf8(&out[..n]).unwrap();
    assert!(s.starts_with("\x1b]11;rgb:") && s.ends_with("\x1b\\"), "{s:?}");
    // Other OSC strings (titles) get no answer.
    con.write_str_bytes("\x1b]0;a title\x07");
    assert_eq!(con.take_reply(&mut out), 0);
}

#[test]
fn the_printing_area_wraps_and_scrolls_inside_the_view() {
    let mut con = term();
    con.set_view(10, 5);
    assert_eq!((con.cols(), con.rows()), (10, 5));
    con.write_str_bytes("0123456789ABC");
    assert_eq!(row_text(&con, 0), "0123456789");
    assert_eq!(row_text(&con, 1), "ABC", "wrapped at column 10, not the screen edge");
    for _ in 0..9 {
        con.write_str_bytes("\r\nline");
    }
    assert!(con.cursor().0 < 5, "scrolled inside five rows");
}

#[test]
fn narrowing_the_view_blanks_what_is_outside_it_on_the_glass() {
    let mut con = term();
    let cols = con.cols();
    for r in 0..3 {
        con.write_str_bytes(&format!("{}\r\n", "x".repeat(cols)));
        let _ = r;
    }
    con.set_view(20, 2);
    let (cw, ch) = (con.font().width(), con.font().height());
    let s = con.into_surface();
    let (mx, my) = Console::<MemSurface>::auto_margin(s.w, s.h);
    // A pixel in column 30 (outside the view) and in row 2 (outside) is background.
    assert_eq!(s.at(mx + 30 * cw + cw / 2, my + ch / 2), Rgb::BLACK, "right of the view");
    assert_eq!(s.at(mx + 2, my + 2 * ch + ch / 2), Rgb::BLACK, "below the view");
}

#[test]
fn a_view_larger_than_the_screen_is_clamped() {
    let mut con = term();
    let (c, r) = (con.max_cols(), con.max_rows());
    con.set_view(10_000, 10_000);
    assert_eq!((con.cols(), con.rows()), (c, r));
    con.set_view(0, 0);
    assert_eq!((con.cols(), con.rows()), (1, 1));
}

#[test]
fn drawing_never_escapes_the_surface_with_every_new_feature() {
    let mut con = term();
    con.write_str_bytes("\x1b[7m中😂─●→✓\x1b[0m\x1b[9999;9999H中\x1b[999@\x1b[999P\x1b[999L\x1b[999M\x1b[999S\x1b[999T");
    con.write_str_bytes("\x1b[?1049h\x1b[2;3r\r\n\r\n\r\n\x1b[?1049l");
    con.set_view(3, 3);
    con.write_str_bytes("wrap wrap wrap 中中中");
    con.show_cursor();
    assert_eq!(con.into_surface().out_of_bounds, 0);
}

// ---------------------------------------------------------------------------
// The margin: small by default, adjustable at run time.
// ---------------------------------------------------------------------------

#[test]
fn the_default_margin_is_under_one_percent() {
    let (mx, my) = Console::<MemSurface>::auto_margin(3840, 2160);
    assert!(mx < 3840 / 100 && my < 2160 / 100, "{mx},{my}");
    assert!(mx > 0, "but not zero: text must not touch the bezel");
}

#[test]
fn a_smaller_margin_is_more_columns() {
    let mut con = Console::new(MemSurface::new(1920, 1080)).unwrap();
    let before = (con.cols(), con.rows());
    con.set_margin(0, 0);
    assert!(con.cols() >= before.0 && con.rows() >= before.1, "{before:?} -> {}x{}", con.cols(), con.rows());
    con.set_margin(200, 100);
    assert!(con.cols() < before.0 && con.rows() < before.1);
}

#[test]
fn text_is_drawn_at_the_new_origin() {
    let mut con = Console::with_scale(MemSurface::new(640, 400), 1).unwrap();
    con.set_fg(Rgb::WHITE);
    con.clear();
    con.set_margin(0, 0);
    assert_eq!(con.margin(), (0, 0));
    con.write_str_bytes("M");
    let s = con.into_surface();
    // With no margin the glyph's ink starts in the cell at the very corner.
    let ink_in_corner_cell = (0..24).any(|y| (0..12).any(|x| s.at(x, y) != Rgb::BLACK));
    assert!(ink_in_corner_cell);
}

#[test]
fn a_margin_change_clears_the_screen_and_is_reported_once() {
    let mut con = term();
    con.write_str_bytes("old text");
    assert_eq!(con.take_geometry(), None, "nothing changed yet");
    con.set_margin(0, 0);
    assert_eq!(row_text(&con, 0), "", "every cell moved, so the screen is cleared");
    assert_eq!(con.cursor(), (0, 0));
    let g = con.take_geometry().expect("reported");
    assert_eq!(g, (con.rows(), con.cols()));
    assert_eq!(con.take_geometry(), None, "once");
}

#[test]
fn the_margin_escape_sets_and_restores_it() {
    let mut con = term();
    let default = (con.cols(), con.rows());
    con.write_str_bytes("\x1b[?9001;0;0h");
    assert_eq!(con.margin(), (0, 0));
    assert!(con.cols() >= default.0);
    con.write_str_bytes("\x1b[?9001;30h"); // y defaults to x
    assert_eq!(con.margin(), (30, 30));
    con.write_str_bytes("\x1b[?9001l");
    let (w, h) = (640, 400);
    assert_eq!(con.margin(), Console::<MemSurface>::auto_margin(w, h));
    // The parser carried on after a handler that cleared the screen.
    con.write_str_bytes("ok");
    assert_eq!(row_text(&con, 0), "ok");
}

#[test]
fn an_absurd_margin_is_clamped_and_never_draws_outside() {
    let mut con = term();
    con.write_str_bytes("\x1b[?9001;9999;9999h");
    assert!(con.cols() >= 1 && con.rows() >= 1);
    con.write_str_bytes("text that wraps and wraps and wraps ─●→");
    con.show_cursor();
    assert_eq!(con.into_surface().out_of_bounds, 0);
}
