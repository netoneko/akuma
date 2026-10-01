//! Colour emoji: the baked set, the scaler, and what the console does with them.

#![allow(clippy::many_single_char_names)]

use akuma_fbcon::emoji;
use akuma_fbcon::{Console, Rgb, Surface};

struct Mem {
    w: usize,
    h: usize,
    px: Vec<Rgb>,
    oob: usize,
}

impl Mem {
    fn new(w: usize, h: usize) -> Self {
        Self { w, h, px: vec![Rgb::BLACK; w * h], oob: 0 }
    }
    fn at(&self, x: usize, y: usize) -> Rgb {
        self.px[y * self.w + x]
    }
}

impl Surface for Mem {
    fn width(&self) -> usize {
        self.w
    }
    fn height(&self) -> usize {
        self.h
    }
    fn put(&mut self, x: usize, y: usize, c: Rgb) {
        if x >= self.w || y >= self.h {
            self.oob += 1;
        } else {
            self.px[y * self.w + x] = c;
        }
    }
}

fn term() -> Console<Mem> {
    let mut con = Console::with_scale(Mem::new(640, 400), 2).unwrap();
    con.set_fg(Rgb::WHITE);
    con.clear();
    con.set_margin(0, 0);
    con
}

/// The box a two-column glyph at column `col` of row 0 occupies, in pixels.
fn span(con: &Console<Mem>, col: usize, cols: usize) -> (usize, usize, usize, usize) {
    let (cw, ch) = (con.font().width() * con.scale(), con.font().height() * con.scale());
    (col * cw, 0, cols * cw, ch)
}

fn pixels(s: &Mem, (x, y, w, h): (usize, usize, usize, usize)) -> Vec<Rgb> {
    (y..y + h).flat_map(|yy| (x..x + w).map(move |xx| s.at(xx, yy))).collect()
}

#[test]
fn the_set_is_sorted_unique_and_substantial() {
    let n = emoji::count();
    assert!(n > 400, "only {n} emoji baked");
    for i in 1..n {
        assert!(emoji::codepoint(i - 1) < emoji::codepoint(i), "index not strictly ascending at {i}");
    }
}

#[test]
fn lookup_finds_what_is_baked_and_only_that() {
    for cp in [0x1F602, 0x1F44D, 0x1F680, 0x1F3C6, 0x2705, 0x274C, 0x2764, 0x1F7E2, 0x1F525] {
        let i = emoji::index_of(cp).unwrap_or_else(|| panic!("U+{cp:X} is not baked"));
        assert_eq!(emoji::codepoint(i), cp);
    }
    for cp in [0x41, 0x0, 0x1F9D1, 0x10FFFF, 0x2022] {
        assert_eq!(emoji::index_of(cp), None, "U+{cp:X}");
    }
}

#[test]
fn every_image_has_ink_and_a_transparent_border_somewhere() {
    for i in 0..emoji::count() {
        let mut opaque = 0;
        for y in 0..emoji::SIZE {
            for x in 0..emoji::SIZE {
                if emoji::texel(i, x, y)[3] > 128 {
                    opaque += 1;
                }
            }
        }
        assert!(opaque > 20, "U+{:X} is blank ({opaque} opaque pixels)", emoji::codepoint(i));
    }
}

#[test]
fn the_scaler_visits_every_pixel_once_at_any_size() {
    let i = emoji::index_of(0x1F602).unwrap();
    for (w, h) in [(1, 1), (7, 5), (24, 24), (33, 61), (48, 48), (96, 96)] {
        let mut n = 0;
        emoji::paint(i, w, h, &mut |x, y, _, _| {
            assert!(x < w && y < h);
            n += 1;
        });
        assert_eq!(n, w * h, "{w}x{h}");
    }
    emoji::paint(i, 0, 10, &mut |_, _, _, _| panic!("a zero-width target draws nothing"));
}

#[test]
fn scaling_does_not_bleed_hidden_colour_into_transparent_edges() {
    // A face is round: its corners are transparent, and must stay so after
    // upscaling — a fringe would be the hidden RGB of transparent texels leaking in.
    let i = emoji::index_of(0x1F602).unwrap();
    let mut corner = None;
    emoji::paint(i, 48, 48, &mut |x, y, _, a| {
        if (x, y) == (0, 0) {
            corner = Some(a);
        }
    });
    assert_eq!(corner, Some(0));
}

fn dominant(s: &Mem, r: (usize, usize, usize, usize)) -> (u32, u32, u32) {
    // The mean colour of the pixels that are not the background.
    let (mut sr, mut sg, mut sb, mut n) = (0u32, 0u32, 0u32, 0u32);
    for c in pixels(s, r) {
        if c != Rgb::BLACK {
            sr += u32::from(c.r);
            sg += u32::from(c.g);
            sb += u32::from(c.b);
            n += 1;
        }
    }
    assert!(n > 100, "almost nothing drawn ({n} pixels)");
    (sr / n, sg / n, sb / n)
}

#[test]
fn an_emoji_is_drawn_in_colour_across_two_cells() {
    let mut con = term();
    con.write_str_bytes("😂");
    assert_eq!(con.cursor(), (0, 2), "two columns");
    assert_eq!(con.cell_at(0, 0), '😂');
    let r = span(&con, 0, 2);
    let s = con.into_surface();
    let (red, green, blue) = dominant(&s, r);
    // Noto's face is yellow (the mean includes the dark eyes and mouth, so not
    // saturated): clearly more red and green than blue.
    assert!(red > 130 && green > 110 && red > blue + 60 && green > blue + 50, "mean colour ({red},{green},{blue})");
}

#[test]
fn different_emoji_have_different_colours() {
    let mean = |text: &str| {
        let mut con = term();
        con.write_str_bytes(text);
        let r = span(&con, 0, 2);
        dominant(&con.into_surface(), r)
    };
    let (gr, gg, gb) = mean("🟢");
    assert!(gg > gr + 40 && gg > gb + 40, "🟢 is not green: ({gr},{gg},{gb})");
    let (rr, rg, rb) = mean("🔴");
    assert!(rr > rg + 60 && rr > rb + 60, "🔴 is not red: ({rr},{rg},{rb})");
    let (br, bg, bb) = mean("🔵");
    assert!(bb > br + 40 && bb > bg, "🔵 is not blue: ({br},{bg},{bb})");
}

#[test]
fn the_line_stays_aligned_around_an_emoji() {
    let mut con = term();
    con.write_str_bytes("a😂b");
    assert_eq!(con.cell_at(0, 0), 'a');
    assert_eq!(con.cell_at(0, 3), 'b');
    assert_eq!(con.cursor(), (0, 4));
}

#[test]
fn an_emoji_without_a_picture_is_still_two_columns() {
    // U+1F9D1 (a person) is not in the baked set: an outlined box, but the right width.
    let mut con = term();
    con.write_str_bytes("a\u{1F9D1}b");
    assert_eq!(con.cell_at(0, 3), 'b');
}

#[test]
fn variation_selector_16_widens_a_text_default_emoji() {
    // ❤ alone is one column; ❤️ (with VS16) is the two-column picture.
    let mut con = term();
    con.write_str_bytes("\u{2764}");
    assert_eq!(con.cursor(), (0, 1));
    let mut con = term();
    con.write_str_bytes("\u{2764}\u{FE0F}x");
    assert_eq!(con.cursor(), (0, 3), "❤️ took two columns, then x");
    assert_eq!(con.cell_at(0, 0), '\u{2764}');
    assert_eq!(con.cell_at(0, 2), 'x');
    let r = span(&con, 0, 2);
    let s = con.into_surface();
    let (red, green, blue) = dominant(&s, r);
    assert!(red > green + 60 && red > blue + 40, "not a red heart: ({red},{green},{blue})");
}

#[test]
fn a_joined_sequence_takes_one_picture_worth_of_columns() {
    let mut con = term();
    con.write_str_bytes("\u{1F600}\u{200D}\u{1F601}x");
    assert_eq!(con.cursor(), (0, 3), "one emoji (2) then x (1)");
    assert_eq!(con.cell_at(0, 0), '\u{1F600}');
    assert_eq!(con.cell_at(0, 2), 'x');
}

#[test]
fn a_skin_tone_modifier_joins_the_emoji_before_it() {
    let mut con = term();
    con.write_str_bytes("\u{1F44D}\u{1F3FD}x");
    assert_eq!(con.cursor(), (0, 3), "👍🏽 is two columns, then x");
    // A modifier on its own is just a character.
    let mut con = term();
    con.write_str_bytes("a\u{1F3FD}");
    assert_eq!(con.cursor(), (0, 3));
}

#[test]
fn an_emoji_is_drawn_over_the_current_background() {
    let mut con = term();
    con.write_str_bytes("\x1b[44m😂\x1b[0m");
    let r = span(&con, 0, 2);
    let s = con.into_surface();
    // A corner of the box is transparent in the picture: it shows the blue
    // background, not black.
    assert_ne!(s.at(r.0, r.1), Rgb::BLACK);
    assert_eq!(s.at(r.0, r.1), s.at(r.0 + r.2 - 1, r.1));
}

#[test]
fn overwriting_half_an_emoji_blanks_the_other_half() {
    let mut con = term();
    con.write_str_bytes("😂\r");
    con.write_byte(b'x');
    assert_eq!(con.cell_at(0, 0), 'x');
    assert_eq!(con.cell_at(0, 1), ' ');
    let mut con = term();
    con.write_str_bytes("😂\x1b[D");
    con.write_byte(b'y'); // lands on the right half
    assert_eq!(con.cell_at(0, 0), ' ');
    assert_eq!(con.cell_at(0, 1), 'y');
}

#[test]
fn emoji_survive_scrolling_erasing_and_the_view() {
    let mut con = term();
    for _ in 0..con.rows() + 3 {
        con.write_str_bytes("😂 hello 🚀 ✅ 🔥\r\n");
    }
    con.write_str_bytes("\x1b[2J\x1b[H😂\x1b[1;1H\x1b[K");
    con.set_view(5, 3);
    con.write_str_bytes("🏆🏆🏆🏆\r\n🚀\x1b[2@\x1b[2P");
    con.show_cursor();
    assert_eq!(con.into_surface().oob, 0);
}

#[test]
fn text_beside_an_emoji_is_unharmed() {
    // The picture must stay inside its own two cells.
    let mut con = term();
    con.write_str_bytes("|😂|");
    let (cw, ch) = (con.font().width() * con.scale(), con.font().height() * con.scale());
    let s = con.into_surface();
    // Nothing from the picture reaches the neighbouring `|` cells.
    let outside_left = pixels(&s, (0, 0, cw, ch));
    let outside_right = pixels(&s, (3 * cw, 0, cw, ch));
    assert!(outside_left.iter().any(|&c| c != Rgb::BLACK), "the left bar is drawn");
    assert!(outside_right.iter().any(|&c| c != Rgb::BLACK), "the right bar is drawn");
    // And the bar is the font's grey-white, not an emoji colour.
    assert!(outside_left.iter().all(|c| c.b > 100 || *c == Rgb::BLACK || c.r.abs_diff(c.b) < 60));
}
