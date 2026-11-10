//! `CSI ? 2026 h` / `l` — synchronized output. The console only *records* the mode;
//! the owner of the screen (the amd64 kernel's `mirror_byte`) holds back presenting
//! a frame while it is set, which is what makes Claude Code's erase-and-rewrite
//! frames appear atomically (`docs/archive/FBCON_DOUBLE_BUFFER.md`).

use akuma_fbcon::{Console, Rgb, Surface};

struct Mem;
impl Surface for Mem {
    fn width(&self) -> usize {
        1280
    }
    fn height(&self) -> usize {
        800
    }
    fn put(&mut self, _x: usize, _y: usize, _c: Rgb) {}
}

fn console() -> Console<Mem> {
    Console::new(Mem).expect("1280x800 fits a console")
}

#[test]
fn off_by_default() {
    assert!(!console().sync_update());
}

#[test]
fn h_opens_a_frame_and_l_closes_it() {
    let mut c = console();
    c.write_str_bytes("\x1b[?2026h");
    assert!(c.sync_update());
    c.write_str_bytes("frame text\x1b[?2026l");
    assert!(!c.sync_update());
}

#[test]
fn the_mode_is_found_in_a_list_and_leaves_the_rest_alone() {
    let mut c = console();
    c.write_str_bytes("\x1b[?25;2026h");
    assert!(c.sync_update());
    c.write_str_bytes("\x1b[?2026;25l");
    assert!(!c.sync_update());
}

#[test]
fn text_inside_a_frame_still_reaches_the_grid() {
    let mut c = console();
    c.write_str_bytes("\x1b[?2026hhello\x1b[?2026l");
    let row: String = (0..5).map(|col| c.cell_at(0, col)).collect();
    assert_eq!(row, "hello");
}
