//! The procedurally drawn characters: geometry, joins and widths.
//!
//! Rendered into a plain bitmap so a failure can be read as a picture.

#![allow(clippy::cast_possible_wrap, clippy::many_single_char_names, clippy::manual_midpoint, clippy::needless_collect, clippy::naive_bytecount)]

use akuma_fbcon::glyph::{self, Kind};

/// A cell as booleans, painted by `f` through the module's closures.
fn bitmap(cw: usize, ch: usize, paint: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut bm = vec![0u8; cw * ch];
    paint(&mut bm);
    bm
}

fn art(bm: &[u8], cw: usize) -> String {
    bm.chunks(cw)
        .map(|r| r.iter().map(|&p| if p == 0 { '.' } else if p == 255 { '#' } else { '+' }).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn boxed(cp: u32, cw: usize, ch: usize) -> Vec<u8> {
    bitmap(cw, ch, |bm| {
        glyph::paint_box(cp, cw, ch, &mut |x, y, w, h| {
            for yy in y..y + h {
                for xx in x..x + w {
                    assert!(xx < cw && yy < ch, "U+{cp:04X} paints ({xx},{yy}) outside {cw}x{ch}");
                    bm[yy * cw + xx] = 255;
                }
            }
        });
    })
}

fn blocked(cp: u32, cw: usize, ch: usize) -> Vec<u8> {
    bitmap(cw, ch, |bm| {
        glyph::paint_block(cp, cw, ch, &mut |x, y, w, h, shade| {
            for yy in y..y + h {
                for xx in x..x + w {
                    assert!(xx < cw && yy < ch, "U+{cp:04X} paints outside the cell");
                    bm[yy * cw + xx] = shade;
                }
            }
        });
    })
}

fn braille(cp: u32, cw: usize, ch: usize) -> Vec<u8> {
    bitmap(cw, ch, |bm| {
        glyph::paint_braille(cp, cw, ch, &mut |x, y, w, h| {
            for yy in y..y + h {
                for xx in x..x + w {
                    assert!(xx < cw && yy < ch, "braille U+{cp:04X} paints outside the cell");
                    bm[yy * cw + xx] = 255;
                }
            }
        });
    })
}

const SIZES: &[(usize, usize)] = &[(5, 9), (8, 16), (12, 24), (16, 32), (24, 48), (33, 61)];

#[test]
fn every_box_character_stays_inside_the_cell_at_every_size() {
    for &(cw, ch) in SIZES {
        for cp in 0x2500..=0x257F {
            let bm = boxed(cp, cw, ch);
            // Every character in the block draws something.
            assert!(bm.iter().any(|&p| p != 0), "U+{cp:04X} drew nothing at {cw}x{ch}");
        }
    }
}

#[test]
fn horizontal_lines_reach_both_edges_so_neighbours_join() {
    for &(cw, ch) in SIZES {
        let bm = boxed(0x2500, cw, ch); // ─
        let row = (0..ch).find(|&y| bm[y * cw] != 0).expect("a line");
        assert_eq!(bm[row * cw], 255, "{cw}x{ch}: left edge");
        assert_eq!(bm[row * cw + cw - 1], 255, "{cw}x{ch}: right edge");
        assert!((0..cw).all(|x| bm[row * cw + x] == 255), "{cw}x{ch}: a gap\n{}", art(&bm, cw));
    }
}

#[test]
fn vertical_lines_reach_top_and_bottom() {
    for &(cw, ch) in SIZES {
        let bm = boxed(0x2502, cw, ch); // │
        let col = (0..cw).find(|&x| bm[x] != 0).expect("a line");
        assert!((0..ch).all(|y| bm[y * cw + col] == 255), "{cw}x{ch}: a gap\n{}", art(&bm, cw));
    }
}

#[test]
fn a_corner_connects_its_two_arms() {
    // ┌: arms go right and down from the centre.
    let (cw, ch) = (12, 24);
    let bm = boxed(0x250C, cw, ch);
    assert_eq!(bm[(ch / 2) * cw + cw - 1], 255, "right arm reaches the edge\n{}", art(&bm, cw));
    assert_eq!(bm[(ch - 1) * cw + cw / 2], 255, "down arm reaches the bottom\n{}", art(&bm, cw));
    assert_eq!(bm[0], 0, "nothing in the top-left\n{}", art(&bm, cw));
    assert_eq!(bm[(ch / 2) * cw + cw / 2], 255, "the corner itself is solid");
}

#[test]
fn a_cross_has_four_arms_and_a_solid_centre() {
    let (cw, ch) = (12, 24);
    let bm = boxed(0x253C, cw, ch);
    assert_eq!(bm[(ch / 2) * cw], 255, "left");
    assert_eq!(bm[(ch / 2) * cw + cw - 1], 255, "right");
    assert_eq!(bm[cw / 2], 255, "up");
    assert_eq!(bm[(ch - 1) * cw + cw / 2], 255, "down");
    assert_eq!(bm[0], 0, "corners stay empty");
}

#[test]
fn heavy_is_thicker_than_light_and_double_has_a_gap() {
    let (cw, ch) = (24, 48);
    let count = |cp| {
        let bm = boxed(cp, cw, ch);
        (0..ch).filter(|&y| bm[y * cw] == 255).count()
    };
    let (light, heavy, double) = (count(0x2500), count(0x2501), count(0x2550));
    assert!(heavy > light, "heavy {heavy} vs light {light}");
    assert_eq!(double, 2 * light, "two light strokes");
    // The two strokes of ═ are separated by blank rows.
    let bm = boxed(0x2550, cw, ch);
    let rows: Vec<usize> = (0..ch).filter(|&y| bm[y * cw] == 255).collect();
    assert!(rows.windows(2).any(|w| w[1] - w[0] > 1), "no gap between the strokes of ═");
}

#[test]
fn diagonals_cross_the_cell() {
    let (cw, ch) = (12, 24);
    let slash = boxed(0x2571, cw, ch); // ╱
    assert_ne!(slash[cw - 1], 0, "╱ starts top-right");
    assert_ne!(slash[(ch - 1) * cw], 0, "╱ ends bottom-left");
    let back = boxed(0x2572, cw, ch); // ╲
    assert_ne!(back[0], 0);
    assert_ne!(back[(ch - 1) * cw + cw - 1], 0);
}

#[test]
fn block_elements_cover_what_their_names_say() {
    let (cw, ch) = (12, 24);
    let area = |cp| blocked(cp, cw, ch).iter().filter(|&&p| p == 255).count();
    assert_eq!(area(0x2588), cw * ch, "█");
    assert_eq!(area(0x2580), cw * ch / 2, "▀");
    assert_eq!(area(0x2584), cw * ch / 2, "▄");
    assert_eq!(area(0x258C), cw * ch / 2, "▌");
    assert_eq!(area(0x2590), cw * ch / 2, "▐");
    assert_eq!(area(0x2596), cw * ch / 4, "▖");
    assert_eq!(area(0x2599), cw * ch * 3 / 4, "▙");
    let lower = blocked(0x2584, cw, ch);
    assert_eq!(lower[0], 0, "▄ leaves the top empty");
    assert_eq!(lower[(ch - 1) * cw], 255);
}

#[test]
fn shades_are_three_distinct_partial_blocks() {
    let (cw, ch) = (8, 16);
    let s = |cp| blocked(cp, cw, ch)[0];
    assert!(s(0x2591) < s(0x2592) && s(0x2592) < s(0x2593) && s(0x2593) < 255);
}

#[test]
fn every_block_element_stays_inside_the_cell() {
    for &(cw, ch) in SIZES {
        for cp in 0x2580..=0x259F {
            let _ = blocked(cp, cw, ch);
        }
    }
}

#[test]
fn braille_dots_land_in_unicodes_positions() {
    let (cw, ch) = (12, 24);
    assert!(braille(0x2800, cw, ch).iter().all(|&p| p == 0), "blank braille is blank");
    let ink = |bm: &[u8]| bm.iter().filter(|&&p| p != 0).count();
    assert!(ink(&braille(0x2801, cw, ch)) > 0, "dot 1");
    // Dot 1 is the top-left, dot 4 the top-right, dot 7 bottom-left, dot 8 bottom-right.
    let at = |cp, x, y| braille(cp, cw, ch)[y * cw + x] != 0;
    assert!(at(0x2801, cw / 4, ch / 8));
    assert!(at(0x2808, 3 * cw / 4, ch / 8));
    assert!(at(0x2840, cw / 4, 7 * ch / 8));
    assert!(at(0x2880, 3 * cw / 4, 7 * ch / 8));
    // All eight dots: eight separate blobs' worth of ink.
    assert_eq!(ink(&braille(0x28FF, cw, ch)), 8 * ink(&braille(0x2801, cw, ch)));
}

#[test]
fn every_braille_cell_stays_inside_the_cell() {
    for &(cw, ch) in SIZES {
        for cp in 0x2800..=0x28FF {
            let _ = braille(cp, cw, ch);
        }
    }
}

fn shape(cp: u32, cw: usize, ch: usize) -> Vec<u8> {
    (0..ch * cw).map(|i| glyph::shape_coverage(cp, i % cw, i / cw, cw, ch).expect("a shape")).collect()
}

#[test]
fn a_filled_circle_is_solid_in_the_middle_and_empty_in_the_corners() {
    let (cw, ch) = (24, 48);
    let bm = shape(0x25CF, cw, ch);
    // The shape sits a little above the cell's middle; find its centre by ink.
    let ys: Vec<usize> = (0..ch).filter(|&y| (0..cw).any(|x| bm[y * cw + x] > 0)).collect();
    let cy = (ys[0] + ys[ys.len() - 1]) / 2;
    assert_eq!(bm[cy * cw + cw / 2], 255, "centre\n{}", art(&bm, cw));
    assert_eq!(bm[0], 0);
    assert_eq!(bm[cw - 1], 0);
    // Roughly circular: about as tall as wide.
    let xs: Vec<usize> = (0..cw).filter(|&x| (0..ch).any(|y| bm[y * cw + x] > 0)).collect();
    let (w, h) = (xs.len() as i32, ys.len() as i32);
    assert!((w - h).abs() <= 2, "{w} wide, {h} tall\n{}", art(&bm, cw));
}

#[test]
fn a_ring_is_hollow_and_a_disc_is_not() {
    let (cw, ch) = (24, 48);
    let disc = shape(0x25CF, cw, ch);
    let ring = shape(0x25CB, cw, ch);
    let ink = |b: &[u8]| b.iter().filter(|&&p| p > 0).count();
    assert!(ink(&ring) < ink(&disc));
    let ys: Vec<usize> = (0..ch).filter(|&y| (0..cw).any(|x| disc[y * cw + x] > 0)).collect();
    let cy = (ys[0] + ys[ys.len() - 1]) / 2;
    assert_eq!(ring[cy * cw + cw / 2], 0, "the ring's centre is empty\n{}", art(&ring, cw));
}

#[test]
fn triangles_point_where_they_say() {
    let (cw, ch) = (24, 48);
    let row_width = |bm: &[u8], y: usize| (0..cw).filter(|&x| bm[y * cw + x] > 0).count();
    let up = shape(0x25B2, cw, ch);
    let ys: Vec<usize> = (0..ch).filter(|&y| row_width(&up, y) > 0).collect();
    assert!(row_width(&up, ys[0] + 1) < row_width(&up, ys[ys.len() - 1]), "▲ is narrow at the top\n{}", art(&up, cw));
    let down = shape(0x25BC, cw, ch);
    let ys: Vec<usize> = (0..ch).filter(|&y| row_width(&down, y) > 0).collect();
    assert!(row_width(&down, ys[0]) > row_width(&down, ys[ys.len() - 1]), "▼ is narrow at the bottom");
    // ▶ is narrow on the right: compare column heights.
    let right = shape(0x25B6, cw, ch);
    let col_h = |x: usize| (0..ch).filter(|&y| right[y * cw + x] > 0).count();
    let xs: Vec<usize> = (0..cw).filter(|&x| col_h(x) > 0).collect();
    assert!(col_h(xs[0]) > col_h(xs[xs.len() - 1]), "▶ is tall on the left\n{}", art(&right, cw));
}

#[test]
fn arrows_have_a_shaft_and_a_head_on_the_right_end() {
    let (cw, ch) = (24, 48);
    let right = shape(0x2192, cw, ch); // →
    let ys: Vec<usize> = (0..ch).filter(|&y| (0..cw).any(|x| right[y * cw + x] > 0)).collect();
    let cy = (ys[0] + ys[ys.len() - 1]) / 2;
    let xs: Vec<usize> = (0..cw).filter(|&x| (0..ch).any(|y| right[y * cw + x] > 0)).collect();
    let col_h = |x: usize| (0..ch).filter(|&y| right[y * cw + x] > 0).count();
    assert!(col_h(xs[xs.len() - 1] - 3) > col_h(xs[1]), "head is taller than shaft\n{}", art(&right, cw));
    assert!(right[cy * cw + xs[0]] > 0, "shaft starts at the left");
    // ← is → mirrored: same amount of ink, and its head is on the left.
    let left = shape(0x2190, cw, ch);
    let ink = |b: &[u8]| b.iter().map(|&p| u32::from(p)).sum::<u32>();
    let (a, b) = (ink(&left), ink(&right));
    assert!(a.abs_diff(b) * 20 <= a.max(b), "← has {a}, → has {b}");
    let lxs: Vec<usize> = (0..cw).filter(|&x| (0..ch).any(|y| left[y * cw + x] > 0)).collect();
    let lcol = |x: usize| (0..ch).filter(|&y| left[y * cw + x] > 0).count();
    assert!(lcol(lxs[2]) > lcol(lxs[lxs.len() - 2]), "← head is on the left\n{}", art(&left, cw));
}

#[test]
fn check_and_cross_marks_draw_something_inside_the_cell() {
    for cp in [0x2713, 0x2714, 0x2717, 0x2718] {
        let bm = shape(cp, 24, 48);
        assert!(bm.iter().filter(|&&p| p > 0).count() > 20, "U+{cp:04X}");
    }
}

#[test]
fn shapes_do_not_exist_for_ordinary_characters() {
    assert_eq!(glyph::shape_coverage(u32::from(b'a'), 0, 0, 12, 24), None);
    assert_eq!(glyph::kind(u32::from(b'a')), None);
    assert_eq!(glyph::kind(0x2500), Some(Kind::Box));
    assert_eq!(glyph::kind(0x2588), Some(Kind::Block));
    assert_eq!(glyph::kind(0x28FF), Some(Kind::Braille));
    assert_eq!(glyph::kind(0x25CF), Some(Kind::Shape));
    // The bullet stays with the font.
    assert_eq!(glyph::kind(0x2022), None);
}

#[test]
fn widths_follow_the_terminal_convention() {
    assert_eq!(glyph::width(u32::from(b'a')), 1);
    assert_eq!(glyph::width(0xE1), 1, "á");
    assert_eq!(glyph::width(0x2500), 1, "box drawing is narrow");
    assert_eq!(glyph::width(0x25CF), 1, "● is narrow");
    assert_eq!(glyph::width(0x4E2D), 2, "CJK");
    assert_eq!(glyph::width(0x1F602), 2, "😂");
    assert_eq!(glyph::width(0x1F3C6), 2, "🏆");
    assert_eq!(glyph::width(0x2705), 2, "✅");
    assert_eq!(glyph::width(0x200D), 0, "zero-width joiner");
    assert_eq!(glyph::width(0xFE0F), 0, "variation selector 16");
    assert_eq!(glyph::width(0x0301), 0, "combining acute");
}
