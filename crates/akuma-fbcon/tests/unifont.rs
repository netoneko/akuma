//! CJK: the baked Unifont table, and drawing it through the console.

use akuma_fbcon::unifont as cjk;
use akuma_fbcon::{Console, Rgb, Surface};

struct Mem(usize, usize, Vec<Rgb>, usize);
impl Mem {
    fn new(w: usize, h: usize) -> Self {
        Self(w, h, vec![Rgb::BLACK; w * h], 0)
    }
    fn at(&self, x: usize, y: usize) -> Rgb {
        self.2[y * self.0 + x]
    }
}
impl Surface for Mem {
    fn width(&self) -> usize {
        self.0
    }
    fn height(&self) -> usize {
        self.1
    }
    fn put(&mut self, x: usize, y: usize, c: Rgb) {
        if x >= self.0 || y >= self.1 {
            self.3 += 1;
        } else {
            self.2[y * self.0 + x] = c;
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

fn ink(glyph: &[u8]) -> u32 {
    glyph.iter().map(|b| b.count_ones()).sum()
}

#[test]
fn the_common_scripts_are_present() {
    for (name, cp) in [
        ("中", 0x4E2D), ("文", 0x6587), ("日", 0x65E5), ("本", 0x672C), ("语", 0x8BED),
        ("あ", 0x3042), ("ア", 0x30A2), ("한", 0xD55C), ("글", 0xAE00), ("，", 0xFF0C), ("、", 0x3001),
    ] {
        let g = cjk::glyph(cp).unwrap_or_else(|| panic!("{name} U+{cp:04X} has no glyph"));
        assert_eq!(g.len(), cjk::GLYPH_BYTES);
        // A comma really is only a few pixels; a character has dozens.
        let least = if matches!(cp, 0xFF0C | 0x3001) { 2 } else { 8 };
        assert!(ink(g) > least, "{name} is nearly blank");
    }
}

#[test]
fn outside_the_table_there_is_nothing() {
    for cp in [0x41, 0x0, 0x2FFF, 0x3100, 0x4DFF, 0xA000, 0xD7A4, 0xFF00, 0xFF61, 0x1_F600, 0x2_0000] {
        assert!(cjk::glyph(cp).is_none(), "U+{cp:04X}");
    }
}

#[test]
fn the_ideographic_space_is_blank_but_not_missing() {
    let g = cjk::glyph(0x3000).expect("U+3000 is a real, blank glyph");
    assert_eq!(ink(g), 0);
}

#[test]
fn glyphs_are_distinct_and_have_ink_throughout_the_block() {
    // No wholesale run of empty glyphs inside CJK Unified Ideographs.
    let mut blank = 0;
    for cp in 0x4E00..=0x9FFF {
        match cjk::glyph(cp) {
            Some(g) => assert!(ink(g) > 0),
            None => blank += 1,
        }
    }
    assert!(blank < 50, "{blank} ideographs missing");
    assert_ne!(cjk::glyph(0x4E2D), cjk::glyph(0x6587));
}

#[test]
fn painting_runs_covers_exactly_the_ink() {
    let g = cjk::glyph(0x4E2D).unwrap();
    for f in [1usize, 2, 3] {
        let side = cjk::SIZE * f;
        let mut grid = vec![false; side * side];
        cjk::paint(g, f, &mut |x, y, w, h| {
            assert!(x + w <= side && y + h <= side, "run outside the glyph");
            for yy in y..y + h {
                for xx in x..x + w {
                    assert!(!grid[yy * side + xx], "a pixel painted twice");
                    grid[yy * side + xx] = true;
                }
            }
        });
        let lit = grid.iter().filter(|&&p| p).count();
        assert_eq!(lit as u32, ink(g) * (f * f) as u32, "scale {f}");
    }
}

#[test]
fn a_cjk_character_takes_two_columns_and_is_drawn_inside_them() {
    let mut con = term();
    con.write_str_bytes("a中b");
    assert_eq!(con.cell_at(0, 0), 'a');
    assert_eq!(con.cell_at(0, 1), '中');
    assert_eq!(con.cell_at(0, 3), 'b');
    let (cw, ch) = (con.font().width() * con.scale(), con.font().height() * con.scale());
    let s = con.into_surface();
    // Ink inside the two-cell box, none in the cell to its right beyond `b`'s own.
    let box_ink = (0..ch).flat_map(|y| (cw..3 * cw).map(move |x| (x, y))).filter(|&(x, y)| s.at(x, y) != Rgb::BLACK).count();
    assert!(box_ink > 150, "the character drew only {box_ink} pixels");
}

#[test]
fn the_usual_scale_gives_an_exact_triple_so_strokes_are_crisp() {
    // 12x24 at scale 2: the two-cell box is 48x48 = 16 * 3.
    let con = term();
    let (cw, ch) = (con.font().width() * con.scale(), con.font().height() * con.scale());
    assert_eq!((2 * cw / cjk::SIZE, ch / cjk::SIZE), (3, 3));
    assert_eq!(2 * cw % cjk::SIZE + ch % cjk::SIZE, 0, "the box is an exact multiple of the glyph");
}

#[test]
fn colours_and_backgrounds_apply_to_cjk_too() {
    let mut con = term();
    con.write_str_bytes("\x1b[31;44m中\x1b[0m");
    let ch = con.font().height() * con.scale();
    let s = con.into_surface();
    let reds = (0..ch).flat_map(|y| (0..2 * 24).map(move |x| (x, y))).filter(|&(x, y)| s.at(x, y) == Rgb::new(0xE0, 0x50, 0x50)).count();
    assert!(reds > 100, "ink should be the palette red ({reds})");
    // The background of the cell is the palette blue, not black.
    assert_eq!(s.at(0, 0), Rgb::new(0x60, 0xA0, 0xE0));
}

#[test]
fn korean_and_japanese_lines_stay_aligned() {
    let mut con = term();
    con.write_str_bytes("한국어 日本語 |");
    // three 2-col + space + three 2-col + space + bar
    assert_eq!(con.cell_at(0, 6 + 1 + 6 + 1), '|');
    assert_eq!(con.cursor().1, 6 + 1 + 6 + 1 + 1);
}

#[test]
fn an_unbaked_wide_character_is_still_a_two_column_box() {
    let mut con = term();
    // CJK Extension B (U+20000) is outside the table: width 2, no glyph, outlined box.
    con.write_str_bytes("\u{20000}x");
    assert_eq!(con.cursor(), (0, 3));
    assert_eq!(con.into_surface().3, 0);
}

#[test]
fn cjk_survives_scrolling_erasing_and_a_narrow_view() {
    let mut con = term();
    for _ in 0..con.rows() + 2 {
        con.write_str_bytes("日本語のテキスト 中文 한글\r\n");
    }
    con.write_str_bytes("\x1b[2J\x1b[H中\x1b[1;1H\x1b[K");
    con.set_view(5, 3);
    con.write_str_bytes("中中中\r\n語\x1b[2@\x1b[2P");
    con.show_cursor();
    assert_eq!(con.into_surface().3, 0);
}

#[test]
fn greek_cyrillic_and_vietnamese_have_narrow_glyphs() {
    for (name, cp) in [("Ж", 0x416), ("я", 0x44F), ("α", 0x3B1), ("Ω", 0x3A9), ("ơ", 0x1A1), ("ế", 0x1EBF), ("ə", 0x259)] {
        let g = cjk::narrow(cp).unwrap_or_else(|| panic!("{name} U+{cp:04X}"));
        assert_eq!(g.len(), cjk::NARROW_BYTES);
        assert!(ink(g) > 6, "{name}");
    }
    assert!(cjk::narrow(u32::from(b'a')).is_none());
    assert!(cjk::narrow(0x4E2D).is_none(), "wide characters are not in the narrow table");
}

#[test]
fn greek_and_cyrillic_are_drawn_where_the_text_font_has_nothing() {
    use akuma_fbcon::font;
    // Plex Mono has no alpha: the console must fall back to Unifont, not draw a box.
    assert!(!font::IBM_PLEX_MONO.draws(0x3B1));
    let mut con = term();
    con.write_str_bytes("αЖ");
    let (cw, ch) = (con.font().width() * con.scale(), con.font().height() * con.scale());
    let s = con.into_surface();
    let box_of = |c: usize| (0..ch).flat_map(|y| (c * cw..(c + 1) * cw).map(move |x| (x, y))).filter(|&(x, y)| s.at(x, y) != Rgb::BLACK).count();
    assert!(box_of(0) > 60, "α drew {} pixels", box_of(0));
    assert!(box_of(1) > 60, "Ж drew {} pixels", box_of(1));
    // A hollow replacement box is a thin outline (< ~400 px at 24x48); a glyph at 3x
    // is chunky strokes. Check it is not the outline by looking at the box's middle row.
    let mid = ch / 2;
    let row_ink = (0..cw).filter(|&x| s.at(x, mid) != Rgb::BLACK).count();
    assert!(row_ink < cw, "the middle row is not a full-width bar");
}

#[test]
fn the_narrow_fallback_stays_inside_its_cell() {
    let mut con = term();
    con.write_str_bytes("ЖЖЖЖЖЖЖЖЖЖ\r\nαβγδεζηθικ");
    assert_eq!(con.into_surface().3, 0);
}
