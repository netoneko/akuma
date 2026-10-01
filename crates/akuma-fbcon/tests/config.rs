//! `/etc/console.conf`: parsing, and applying it to a console.

use akuma_fbcon::config::{ConsoleConfig, Dim};
use akuma_fbcon::{Console, Rgb, Surface};

struct Mem(usize, usize, usize);
impl Surface for Mem {
    fn width(&self) -> usize {
        self.0
    }
    fn height(&self) -> usize {
        self.1
    }
    fn put(&mut self, x: usize, y: usize, _c: Rgb) {
        if x >= self.0 || y >= self.1 {
            self.2 += 1;
        }
    }
}

#[test]
fn the_trashcan_example_parses() {
    let c = ConsoleConfig::parse("# left half\nmargin = 0\ncols = 50%\n");
    assert_eq!(c.margin, Some((0, 0)));
    assert_eq!(c.cols, Some(Dim::Percent(50)));
    assert_eq!(c.rows, None);
}

#[test]
fn margin_takes_one_or_two_numbers() {
    assert_eq!(ConsoleConfig::parse("margin = 24").margin, Some((24, 24)));
    assert_eq!(ConsoleConfig::parse("margin = 24, 12").margin, Some((24, 12)));
    assert_eq!(ConsoleConfig::parse("margin=8,0").margin, Some((8, 0)));
}

#[test]
fn cells_and_percent_for_both_dimensions() {
    let c = ConsoleConfig::parse("cols = 73\nrows = 80 %\n");
    assert_eq!(c.cols, Some(Dim::Cells(73)));
    assert_eq!(c.rows, Some(Dim::Percent(80)));
}

#[test]
fn comments_blank_lines_unknown_keys_and_junk_are_ignored() {
    let c = ConsoleConfig::parse(
        "\n   \n# only a comment\nfont = comic sans\nthis is not a setting\ncols = 40 # trailing comment\n= 5\nrows = banana\n",
    );
    assert_eq!(c, ConsoleConfig { margin: None, cols: Some(Dim::Cells(40)), rows: None });
}

#[test]
fn a_bad_value_keeps_the_earlier_good_one() {
    let c = ConsoleConfig::parse("cols = 50%\ncols = lots\n");
    assert_eq!(c.cols, Some(Dim::Percent(50)));
}

#[test]
fn an_empty_or_garbage_file_asks_for_nothing() {
    assert!(ConsoleConfig::parse("").is_empty());
    assert!(ConsoleConfig::parse("\u{0}\u{1}\u{2}\r\n\r\n???").is_empty());
}

#[test]
fn percent_resolves_against_the_grid_and_is_clamped() {
    assert_eq!(Dim::Percent(50).resolve(150), 75);
    assert_eq!(Dim::Percent(150).resolve(150), 150, "over 100 % is the whole screen");
    assert_eq!(Dim::Percent(0).resolve(150), 1, "never zero");
    assert_eq!(Dim::Cells(0).resolve(150), 1);
    assert_eq!(Dim::Cells(9999).resolve(150), 150);
}

#[test]
fn applying_half_the_screen_leaves_the_right_half_unused() {
    let mut con = Console::new(Mem(3840, 2160, 0)).unwrap();
    let full = con.cols();
    let cfg = ConsoleConfig::parse("margin = 0\ncols = 50%\n");
    let (rows, cols) = con.apply_config(&cfg);
    assert_eq!(cols, con.cols());
    assert_eq!(rows, con.rows());
    // With no margin the screen holds more, and the area is half of *that*.
    assert!(con.max_cols() >= full);
    assert_eq!(cols, con.max_cols() / 2);
    assert_eq!(rows, con.max_rows(), "rows were not asked for");
    assert_eq!(con.margin(), (0, 0));
    // Text wraps at the half-way column, not at the screen edge.
    for _ in 0..cols + 5 {
        con.write_byte(b'x');
    }
    assert_eq!(con.cursor().0, 1, "wrapped onto the second line");
}

#[test]
fn applying_reports_nothing_pending_and_stays_in_bounds() {
    let mut con = Console::new(Mem(1920, 1080, 0)).unwrap();
    con.apply_config(&ConsoleConfig::parse("margin = 9999\ncols = 1\nrows = 1"));
    assert_eq!(con.take_geometry(), None, "boot reports the size itself");
    con.write_str_bytes("wrap wrap wrap ─●→");
    assert_eq!(con.into_surface().2, 0, "nothing drawn outside the surface");
}

#[test]
fn an_empty_config_changes_nothing() {
    let mut con = Console::new(Mem(1920, 1080, 0)).unwrap();
    let before = (con.rows(), con.cols(), con.margin());
    con.apply_config(&ConsoleConfig::default());
    assert_eq!((con.rows(), con.cols(), con.margin()), before);
}
