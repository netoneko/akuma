//! Mouse reporting modes, and the pointer marker.

use akuma_fbcon::{Console, Rgb, Surface};

struct Buf {
    w: usize,
    h: usize,
    px: Vec<Rgb>,
}
impl Buf {
    fn new(w: usize, h: usize) -> Self {
        Self { w, h, px: vec![Rgb::BLACK; w * h] }
    }
}
impl Surface for Buf {
    fn width(&self) -> usize {
        self.w
    }
    fn height(&self) -> usize {
        self.h
    }
    fn put(&mut self, x: usize, y: usize, c: Rgb) {
        if x < self.w && y < self.h {
            self.px[y * self.w + x] = c;
        }
    }
}

fn console() -> Console<Buf> {
    Console::new(Buf::new(1024, 768)).expect("console")
}

fn feed(c: &mut Console<Buf>, s: &str) {
    for b in s.bytes() {
        c.write_byte(b);
    }
}

#[test]
fn mouse_modes_follow_the_program() {
    let mut c = console();
    assert_eq!(c.mouse_mode(), (0, false));
    feed(&mut c, "\x1b[?1000h\x1b[?1006h");
    assert_eq!(c.mouse_mode(), (1000, true));
    // Levels of one setting: the highest wins, and a lower one going off
    // leaves the higher one alone.
    feed(&mut c, "\x1b[?1003h");
    assert_eq!(c.mouse_mode().0, 1003);
    feed(&mut c, "\x1b[?1000l");
    assert_eq!(c.mouse_mode().0, 1003);
    feed(&mut c, "\x1b[?1003l");
    assert_eq!(c.mouse_mode().0, 0);
    // herdr-style: several modes in one sequence.
    feed(&mut c, "\x1b[?1002;1006h");
    assert_eq!(c.mouse_mode(), (1002, true));
    feed(&mut c, "\x1b[?1006l");
    assert!(!c.mouse_mode().1);
}

#[test]
fn the_grid_geometry_matches_the_cell() {
    let c = console();
    let (ox, oy, cw, ch, cols, rows) = c.text_grid();
    assert!(cw > 0 && ch > 0 && cols > 0 && rows > 0);
    assert_eq!((ox, oy), c.margin());
    assert!(ox + cols * cw <= 1024 && oy + rows * ch <= 768);
}

fn cell_pixels(c: &mut Console<Buf>, row: usize, col: usize) -> Vec<Rgb> {
    let (ox, oy, cw, ch, _, _) = c.text_grid();
    let s = c.surface_mut();
    let mut v = Vec::new();
    for y in 0..ch {
        for x in 0..cw {
            v.push(s.px[(oy + row * ch + y) * s.w + ox + col * cw + x]);
        }
    }
    v
}

#[test]
fn the_pointer_marker_inverts_its_cell_and_a_write_takes_it_down() {
    let mut c = console();
    feed(&mut c, "hello");
    c.show_cursor();
    let before_a = cell_pixels(&mut c, 5, 10);
    let before_b = cell_pixels(&mut c, 7, 3);
    c.set_pointer(Some((5, 10)));
    c.show_cursor();
    assert_ne!(before_a, cell_pixels(&mut c, 5, 10), "the marker must change the cell");
    // A write takes it down first, so output never lands on a stale marker.
    feed(&mut c, "x");
    assert_eq!(before_a, cell_pixels(&mut c, 5, 10), "the cell is restored once output resumes");
    // Moving it repaints the old cell back and marks the new one.
    c.show_cursor();
    c.set_pointer(Some((7, 3)));
    c.show_cursor();
    assert_eq!(before_a, cell_pixels(&mut c, 5, 10));
    assert_ne!(before_b, cell_pixels(&mut c, 7, 3));
    // Clearing it restores the last cell at once.
    c.set_pointer(None);
    assert_eq!(before_b, cell_pixels(&mut c, 7, 3));
}

#[test]
fn a_pointer_outside_the_grid_is_ignored() {
    let mut c = console();
    c.set_pointer(Some((9_999, 9_999)));
    c.show_cursor(); // must not panic
}
