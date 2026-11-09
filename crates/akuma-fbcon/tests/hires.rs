//! The smooth CJK and symbol tables (`src/hires.rs`) and the console paths that use them.

use akuma_fbcon::hires::{self, SYM_H, SYM_W};
use akuma_fbcon::{Console, Rgb, Surface};

struct Mem(usize, usize, Vec<Rgb>);
impl Surface for Mem {
    fn width(&self) -> usize {
        self.0
    }
    fn height(&self) -> usize {
        self.1
    }
    fn put(&mut self, x: usize, y: usize, c: Rgb) {
        if x < self.0 && y < self.1 {
            self.2[y * self.0 + x] = c;
        }
    }
}

#[test]
fn the_common_cjk_set_is_there_and_the_rest_is_left_to_unifont() {
    for ch in ['你', '好', '世', '界', '日', '本', '語', 'あ', 'カ', '한', '국', '，'] {
        assert!(hires::cjk(u32::from(ch), 40).is_some(), "{ch} U+{:04X} is common", u32::from(ch));
    }
    // U+4E02 is in no GB2312 / JIS X 0208 / KS X 1001 table: it stays on Unifont.
    assert!(hires::cjk(0x4E02, 40).is_none());
    assert!(hires::cjk(u32::from('a'), 40).is_none());
    assert!(hires::cjk(0x1F600, 40).is_none(), "past the BMP");
}

#[test]
fn the_source_size_is_the_baked_one_nearer_the_target() {
    let cp = u32::from('你');
    assert_eq!(hires::cjk(cp, 20).unwrap().side(), hires::CJK_SMALL);
    assert_eq!(hires::cjk(cp, 22).unwrap().side(), hires::CJK_SMALL);
    assert_eq!(hires::cjk(cp, 24).unwrap().side(), hires::CJK_SMALL);
    assert_eq!(hires::cjk(cp, 32).unwrap().side(), hires::CJK_LARGE);
    assert_eq!(hires::cjk(cp, 40).unwrap().side(), hires::CJK_LARGE);
    assert_eq!(hires::cjk(cp, 48).unwrap().side(), hires::CJK_LARGE);
}

#[test]
fn a_cjk_glyph_is_anti_aliased_and_inks_a_real_share_of_its_box() {
    for side in [22usize, 40] {
        let g = hires::cjk(u32::from('世'), side).unwrap();
        let (mut ink, mut partial) = (0usize, 0usize);
        for y in 0..side {
            for x in 0..side {
                match g.at(x, y) {
                    0 => {}
                    255 => ink += 1,
                    _ => {
                        ink += 1;
                        partial += 1;
                    }
                }
            }
        }
        assert!(ink > side * side / 10 && ink < side * side * 7 / 10, "{side}: {ink} inked pixels");
        assert!(partial > 0, "{side}: no partial coverage, so not anti-aliased");
    }
}

#[test]
fn symbols_greek_and_latin_extended_have_cells() {
    for cp in ['α', 'Ω', 'ɐ', 'ǎ', 'ệ', '→', '∑', '✓', '⚠'].map(u32::from) {
        let g = hires::sym(cp).unwrap_or_else(|| panic!("U+{cp:04X} has no symbol cell"));
        assert_eq!(g.len(), SYM_W * SYM_H);
        assert!(g.iter().any(|&v| v != 0), "U+{cp:04X} is blank");
    }
    assert!(hires::sym(u32::from('a')).is_none(), "ASCII is Plex's");
}

#[test]
fn resampling_to_the_same_size_is_a_copy() {
    let src: Vec<u8> = (0..24 * 48).map(|i| (i * 7 % 256) as u8).collect();
    let mut out = vec![0u8; 24 * 48];
    hires::resample((24, 48), (24, 48), |x, y| src[y * 24 + x], &mut |x, y, c| out[y * 24 + x] = c);
    for (i, (&a, &b)) in src.iter().zip(&out).enumerate() {
        // `put` is called only for ink, and never with 0.
        assert_eq!(a, b, "pixel {i}");
    }
}

#[test]
fn resampling_keeps_the_ink() {
    // Total coverage is conserved to within rounding, down and up, on both axes.
    let src: Vec<u8> = (0..24 * 48).map(|i| if (i / 24 + i % 24) % 5 < 2 { 255 } else { 0 }).collect();
    let mass = |v: &[u8]| v.iter().map(|&c| f64::from(c)).sum::<f64>() / 255.0;
    for (dw, dh) in [(11, 22), (12, 24), (16, 32), (20, 40), (30, 60)] {
        let mut out = vec![0u8; dw * dh];
        hires::resample((24, 48), (dw, dh), |x, y| src[y * 24 + x], &mut |x, y, c| out[y * dw + x] = c);
        let (a, b) = (mass(&src) * (dw * dh) as f64 / (24.0 * 48.0), mass(&out));
        assert!((a - b).abs() / a < 0.03, "{dw}x{dh}: expected ~{a:.1} px of ink, got {b:.1}");
    }
}

fn console_with(text: &str, w: usize, h: usize) -> Mem {
    let bg = Rgb::new(0x08, 0x0C, 0x14);
    let mut con = Console::new(Mem(w, h, vec![bg; w * h])).unwrap();
    con.set_margin(0, 0);
    con.clear();
    con.write_str_bytes(text);
    con.into_surface()
}

#[test]
fn the_console_draws_cjk_with_partial_coverage_not_a_bitmap() {
    let bg = Rgb::new(0x08, 0x0C, 0x14);
    for (w, h) in [(1920, 1080), (3840, 2160)] {
        let s = console_with("你好", w, h);
        let pixels: Vec<Rgb> = s.2.iter().copied().filter(|&c| c != bg).collect();
        assert!(!pixels.is_empty(), "{w}x{h}: nothing drawn");
        let distinct: std::collections::HashSet<_> = pixels.iter().map(|c| (c.r, c.g, c.b)).collect();
        assert!(distinct.len() > 4, "{w}x{h}: only {} colours: a 1-bit bitmap, not anti-aliased", distinct.len());
    }
}

#[test]
fn greek_is_drawn_from_the_smooth_table() {
    let bg = Rgb::new(0x08, 0x0C, 0x14);
    let s = console_with("αβγ", 1920, 1080);
    let distinct: std::collections::HashSet<_> = s.2.iter().filter(|&&c| c != bg).map(|c| (c.r, c.g, c.b)).collect();
    assert!(distinct.len() > 4, "Greek has {} colours: Unifont's 1-bit bitmap", distinct.len());
}
