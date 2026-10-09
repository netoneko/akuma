//! A [`Page`] laid out in terminal cells.
//!
//! **Columns** are linear: `x / cw`, where `cw` is measured from the page's own
//! text (the median width per character of its line boxes). With `cells.js`
//! every character is one monospace advance, so this is exact; with the page's
//! own fonts it is an average and long proportional runs push later fragments
//! on the same row to the right rather than overwrite them.
//!
//! **Rows** are not linear. A line pitch of 18-24 px against a fixed cell
//! height would skip or merge lines, so rows come from the text itself: line
//! boxes whose vertical middles fall inside the current line join it, anything
//! lower starts a new row, and a vertical gap of most of a row or more (a
//! paragraph margin) becomes up to [`MAX_BLANK`] blank rows. Every row keeps
//! the document y it stands for ([`Grid::row_y`]), so the view can tell the
//! page where it scrolled to, and boxes (backgrounds) span the rows of the
//! lines they contain.
//!
//! Three layers, in paint order: the flow (scrolls with the view),
//! `position:fixed` content (placed on screen rows by its viewport offset at
//! [`ROW_PX`] each), and kami's link-hint labels on top (on the rows of what
//! they label).

use unicode_width::UnicodeWidthChar;

use super::page::{Fill, Item, Kind, Layer, Page, Rgb};

/// The CSS px a terminal row stands for where there is no text to measure:
/// blank space and the fixed layer. `kami tui` makes the page viewport
/// `rows * ROW_PX` tall to match.
pub const ROW_PX: f64 = 19.0;
/// At most this many blank rows for one vertical gap.
const MAX_BLANK: usize = 3;
/// Images and fields count as this tall when deciding which line they sit on.
/// They never make a line taller: an image beside a paragraph would
/// otherwise pull the paragraph's next line into its row.
const OBJECT_PX: f64 = 18.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub fg: Option<Rgb>,
    pub bg: Option<Rgb>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub dim: bool,
    /// The right half of the double-width character in the cell before.
    pub tail: bool,
}

impl Default for Cell {
    fn default() -> Cell {
        Cell { ch: ' ', fg: None, bg: None, bold: false, italic: false, underline: false, dim: false, tail: false }
    }
}

/// One cell of an overlay layer at a position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placed {
    /// Hint layer: a document row. Fixed layer: a screen row.
    pub row: usize,
    pub col: usize,
    pub cell: Cell,
}

#[derive(Debug, Default)]
pub struct Grid {
    /// The flow layer, the whole document.
    pub rows: Vec<Vec<Cell>>,
    /// `anchors[r]`: the document y (CSS px) row `r` stands for. Ascending.
    anchors: Vec<f64>,
    pub hints: Vec<Placed>,
    pub fixed: Vec<Placed>,
    /// CSS px per column.
    pub cw: f64,
    /// What shows where the page paints nothing (see [`Page::canvas`]).
    pub canvas: Rgb,
}

impl Grid {
    /// The row showing document y `y` (the last row starting at or above it).
    pub fn row_at(&self, y: f64) -> usize {
        self.anchors.partition_point(|&a| a <= y).saturating_sub(1)
    }

    /// The document y row `r` stands for.
    pub fn row_y(&self, r: usize) -> f64 {
        self.anchors.get(r).or(self.anchors.last()).copied().unwrap_or(0.0)
    }

    /// The rows a box from `top` to `bottom` covers: those of the lines and
    /// blank rows that start inside it (2 px of slack for rounding).
    fn rows_of(&self, top: f64, bottom: f64) -> std::ops::Range<usize> {
        let r0 = self.anchors.partition_point(|&a| a < top - 2.0);
        let r1 = self.anchors.partition_point(|&a| a < bottom - 2.0);
        r0..r1.max(r0)
    }

    #[cfg(test)]
    pub fn text(&self) -> Vec<String> {
        self.rows.iter().map(|r| r.iter().filter(|c| !c.tail).map(|c| c.ch).collect::<String>().trim_end().to_string()).collect()
    }
}

/// The median CSS px per character over the page's single-width text runs of
/// at least three characters; `fallback` when there are none.
pub fn cell_width(page: &Page, fallback: f64) -> f64 {
    let mut w: Vec<f64> = page
        .items
        .iter()
        .filter(|i| i.kind == Kind::Text && i.layer == Layer::Flow)
        .filter_map(|i| {
            let n = i.text.chars().count();
            (n >= 3 && i.text.chars().all(|c| c.width() == Some(1))).then(|| i.rect.w / n as f64)
        })
        .filter(|w| w.is_finite() && *w > 1.0)
        .collect();
    if w.is_empty() {
        return fallback;
    }
    w.sort_by(f64::total_cmp);
    w[w.len() / 2]
}

/// Lay `page` out `cols` wide, with a screen `view_rows` tall for the fixed
/// layer. `cw_fallback` is the CSS px per column to assume if the page has no
/// text to measure.
pub fn build(page: &Page, cols: usize, view_rows: usize, cw_fallback: f64) -> Grid {
    let cw = cell_width(page, cw_fallback);
    let canvas = page.canvas.unwrap_or(Rgb(255, 255, 255));
    let mut g = Grid { cw, canvas, ..Grid::default() };
    let col = |x: f64| (x / cw).round();
    let on_page = |i: &Item| i.rect.x + i.rect.w > 0.0 && col(i.rect.x) < cols as f64 && i.rect.y + i.rect.h > 0.0;

    // Rows, from the flow's line boxes.
    let mut flow: Vec<&Item> = page.items.iter().filter(|i| i.layer == Layer::Flow && on_page(i)).collect();
    flow.sort_by(|a, b| a.rect.y.total_cmp(&b.rect.y).then(a.rect.x.total_cmp(&b.rect.x)));
    let mut row_of = Vec::with_capacity(flow.len());
    let mut line: Option<(f64, f64)> = None; // the current line's top and bottom
    let mut bottom = 0.0f64; // where the previous line ended (the page top at first)
    for it in &flow {
        let text = it.kind == Kind::Text;
        let h = if text { it.rect.h } else { it.rect.h.min(OBJECT_PX) };
        let mid = it.rect.y + h / 2.0;
        if let Some((top, bot)) = line.as_mut() {
            if mid >= *top && mid < *bot {
                if text {
                    *bot = bot.max(it.rect.y + h);
                }
                row_of.push(g.anchors.len() - 1);
                continue;
            }
            bottom = *bot;
        }
        let gap = it.rect.y - bottom;
        let blanks = (((gap + ROW_PX * 0.25) / ROW_PX).floor().max(0.0) as usize).min(MAX_BLANK);
        for k in 0..blanks {
            g.anchors.push(bottom + gap * k as f64 / blanks as f64);
        }
        g.anchors.push(it.rect.y);
        row_of.push(g.anchors.len() - 1);
        // An object's line is one text line tall at most, and only as tall as
        // the object's own mid-point for the purpose of what joins it.
        line = Some((it.rect.y, it.rect.y + if text { h } else { h / 2.0 + 1.0 }));
    }
    if g.anchors.is_empty() {
        g.anchors.push(0.0);
    }
    // Rows are ascending by construction except where a blank run's anchors
    // meet a line that starts above the previous one's bottom; keep them
    // sorted so the binary searches stay valid.
    for r in 1..g.anchors.len() {
        if g.anchors[r] < g.anchors[r - 1] {
            g.anchors[r] = g.anchors[r - 1];
        }
    }
    g.rows = vec![vec![Cell { bg: Some(canvas), ..Cell::default() }; cols]; g.anchors.len()];

    // Backgrounds, in paint order.
    let mut fills: Vec<&Fill> = page.fills.iter().collect();
    fills.sort_by_key(|f| f.order);
    for f in fills.iter().filter(|f| f.layer == Layer::Flow) {
        let (c0, c1) = span(col(f.rect.x), col(f.rect.x + f.rect.w), cols);
        for r in g.rows_of(f.rect.y, f.rect.y + f.rect.h) {
            for c in &mut g.rows[r][c0..c1] {
                c.bg = Some(blend(f.bg, f.alpha, c.bg));
            }
        }
    }

    // Text, left to right along each row.
    let mut order: Vec<usize> = (0..flow.len()).collect();
    order.sort_by(|&a, &b| row_of[a].cmp(&row_of[b]).then(flow[a].rect.x.total_cmp(&flow[b].rect.x)));
    let mut end = (usize::MAX, 0usize, 0.0f64); // (row, first free column, right edge in px)
    for i in order {
        let (it, r) = (flow[i], row_of[i]);
        let (free, right) = if end.0 == r { (end.1, end.2) } else { (0, 0.0) };
        let c = place(col(it.rect.x), free, it.rect.x - right, it, &g.rows[r]);
        let stop = paint(&mut g.rows[r], c, it, cw);
        end = (r, stop, it.rect.x + it.rect.w);
    }

    // Link hints: on the rows of what they label. Labels are wider than many
    // of their targets (a vote arrow, a one-letter link), and in cells two
    // overlapping labels read as one longer label, so a label that would
    // touch the one before it moves right, one cell clear of it.
    let mut hints: Vec<(usize, &Item)> =
        page.items.iter().filter(|i| i.layer == Layer::Hint && on_page(i)).map(|i| (g.row_at(i.rect.y + 1.0), i)).collect();
    hints.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.rect.x.total_cmp(&b.1.rect.x)));
    let mut end = (usize::MAX, 0usize);
    let mut row = vec![Cell::default(); cols];
    for (r, it) in hints {
        let at = col(it.rect.x).max(0.0) as usize;
        let c0 = if end.0 == r && at <= end.1 { end.1 + 1 } else { at };
        let stop = paint(&mut row, c0.min(cols), it, cw);
        g.hints.extend((c0.min(stop)..stop).map(|c| Placed { row: r, col: c, cell: row[c] }));
        end = (r, stop);
    }

    // Fixed content: screen rows by its offset in the viewport. Opaque
    // backgrounds only: a translucent one is a modal's backdrop, and painting
    // it would bury the page under a solid colour.
    let mut fixed = vec![vec![None::<Cell>; cols]; view_rows];
    let srow = |y: f64| ((y - page.scroll_y) / ROW_PX).floor();
    for f in fills.iter().filter(|f| f.layer == Layer::Fixed && f.alpha > 0.9) {
        let (c0, c1) = span(col(f.rect.x), col(f.rect.x + f.rect.w), cols);
        let (r0, r1) = (srow(f.rect.y).max(0.0) as usize, (srow(f.rect.y + f.rect.h - 1.0) + 1.0).max(0.0) as usize);
        for row in fixed.iter_mut().take(r1).skip(r0) {
            for c in &mut row[c0..c1] {
                *c = Some(Cell { bg: Some(f.bg), ..Cell::default() });
            }
        }
    }
    let mut fixed_items: Vec<&Item> = page.items.iter().filter(|i| i.layer == Layer::Fixed && on_page(i)).collect();
    fixed_items.sort_by(|a, b| srow(a.rect.y + 1.0).total_cmp(&srow(b.rect.y + 1.0)).then(a.rect.x.total_cmp(&b.rect.x)));
    let mut end = (usize::MAX, 0usize, 0.0f64);
    for it in fixed_items {
        let r = srow(it.rect.y + it.rect.h.min(ROW_PX) / 2.0);
        if r < 0.0 || r as usize >= view_rows {
            continue;
        }
        let r = r as usize;
        let mut row: Vec<Cell> = fixed[r].iter().map(|c| c.unwrap_or_default()).collect();
        let (free, right) = if end.0 == r { (end.1, end.2) } else { (0, 0.0) };
        let c0 = place(col(it.rect.x), free, it.rect.x - right, it, &row);
        let stop = paint(&mut row, c0, it, cw);
        for c in c0..stop {
            fixed[r][c] = Some(row[c]);
        }
        end = (r, stop, it.rect.x + it.rect.w);
    }
    for (r, row) in fixed.into_iter().enumerate() {
        g.fixed.extend(row.into_iter().enumerate().filter_map(|(c, cell)| Some(Placed { row: r, col: c, cell: cell? })));
    }
    g
}

/// Columns `[a, b)` clamped to the grid.
fn span(a: f64, b: f64, cols: usize) -> (usize, usize) {
    let c0 = a.max(0.0).min(cols as f64) as usize;
    let c1 = b.max(0.0).min(cols as f64) as usize;
    (c0, c1.max(c0))
}

/// `fg` at `alpha` over `under` (white when nothing is under it yet).
fn blend(fg: Rgb, alpha: f64, under: Option<Rgb>) -> Rgb {
    if alpha >= 0.99 {
        return fg;
    }
    let u = under.unwrap_or(Rgb(255, 255, 255));
    let m = |a: u8, b: u8| (a as f64 * alpha + b as f64 * (1.0 - alpha)).round() as u8;
    Rgb(m(fg.0, u.0), m(fg.1, u.1), m(fg.2, u.2))
}

/// The column an item starts at: where its x says, unless the row's previous
/// item ran up to or past that (proportional text is wider than its cells), in
/// which case right after it. Two words that would then touch get a space
/// between them when they were really apart: the previous one overran by more
/// than rounding, or there were pixels between them (`gap_px`, from the
/// previous item's right edge) that came to less than a column, as between
/// two table cells.
fn place(at: f64, free: usize, gap_px: f64, it: &Item, row: &[Cell]) -> usize {
    let at = at.max(0.0) as usize;
    if at > free {
        return at;
    }
    let words = free > 0 && row.get(free - 1).is_some_and(|c| c.ch != ' ') && !it.text.starts_with(' ');
    free + (words && (free - at > 1 || gap_px >= 1.0)) as usize
}

/// Draw one item into `row` from column `c`; returns the first column after it.
fn paint(row: &mut [Cell], c: usize, it: &Item, cw: f64) -> usize {
    let cols = row.len();
    let mut style = Cell {
        fg: it.fg,
        bold: it.bold || it.size >= 24.0,
        italic: it.italic,
        underline: it.underline || it.link,
        dim: it.dim,
        ..Cell::default()
    };
    match it.kind {
        Kind::Image => {
            style.italic = true;
            style.fg = Some(Rgb(0x88, 0x88, 0x88));
            style.underline = it.link;
            // The label stays inside the image's box (at least a few cells).
            let room = ((it.rect.w / cw).round() as usize).max(6);
            if it.text.chars().count() > room {
                let cut: String = it.text.chars().take(room.saturating_sub(2)).collect();
                return paint(row, c, &Item { text: format!("{cut}…]"), kind: Kind::Text, ..it.clone() }, cw);
            }
        }
        Kind::Field => {
            // The whole box underlined, so an empty field is visible.
            style.underline = true;
            let width = ((it.rect.w / cw).round() as usize).max(4);
            for cell in row.iter_mut().skip(c).take(width) {
                *cell = Cell { bg: cell.bg, ..style };
            }
        }
        Kind::Text | Kind::Check => {}
    }
    let mut x = c;
    for ch in it.text.chars() {
        let ch = if ch.is_control() { ' ' } else { ch };
        // Private-use code points are icon-font glyphs: meaningless as text.
        if matches!(ch as u32, 0xe000..=0xf8ff | 0xf0000..) {
            continue;
        }
        let w = match ch.width() {
            Some(w @ 1..=2) => w,
            _ => continue,
        };
        if x + w > cols {
            break;
        }
        row[x] = Cell { ch, bg: it.bg.or(row[x].bg), ..style };
        if w == 2 {
            row[x + 1] = Cell { ch: ' ', tail: true, bg: it.bg.or(row[x + 1].bg), ..style };
        }
        x += w;
    }
    if it.kind == Kind::Field {
        x = x.max(c + ((it.rect.w / cw).round() as usize).max(4)).min(cols);
    }
    x
}

#[cfg(test)]
mod tests {
    use super::super::page::{Item, Kind, Layer, Page, Rect};
    use super::*;

    fn text(x: f64, y: f64, w: f64, h: f64, s: &str) -> Item {
        Item {
            rect: Rect { x, y, w, h },
            text: s.into(),
            kind: Kind::Text,
            layer: Layer::Flow,
            fg: None,
            bg: None,
            bold: false,
            italic: false,
            underline: false,
            link: false,
            dim: false,
            size: 16.0,
        }
    }

    fn page(items: Vec<Item>) -> Page {
        Page { items, ..Page::default() }
    }

    #[test]
    fn columns_come_from_the_measured_cell_width() {
        // 10 px per character, as cells.js makes it.
        let p = page(vec![text(0.0, 0.0, 50.0, 19.0, "hello"), text(60.0, 0.0, 50.0, 19.0, "world")]);
        let g = build(&p, 20, 10, 9.6);
        assert_eq!(g.cw, 10.0);
        assert_eq!(g.text()[0], "hello world");
    }

    #[test]
    fn line_pitch_does_not_alias_onto_rows() {
        // 24 px lines: dividing by a 19 px row would skip a row every few lines.
        let items = (0..6).map(|i| text(0.0, i as f64 * 24.0, 40.0, 19.0, &format!("l{i}"))).collect();
        let g = build(&page(items), 10, 10, 10.0);
        assert_eq!(g.text(), ["l0", "l1", "l2", "l3", "l4", "l5"]);
    }

    #[test]
    fn a_paragraph_gap_is_a_blank_row_and_a_huge_gap_is_capped() {
        let p = page(vec![
            text(0.0, 0.0, 40.0, 19.0, "a"),
            text(0.0, 19.0 + 16.0, 40.0, 19.0, "b"), // 16 px margin
            text(0.0, 2000.0, 40.0, 19.0, "c"),
        ]);
        let g = build(&p, 10, 10, 10.0);
        assert_eq!(g.text(), ["a", "", "b", "", "", "", "c"]);
        assert_eq!(g.row_at(2000.0), 6);
        assert_eq!(g.row_y(6), 2000.0);
    }

    #[test]
    fn fragments_of_one_visual_line_share_a_row_even_when_taller() {
        // A 32 px heading's box beside 16 px text, same line.
        let p = page(vec![text(0.0, 0.0, 100.0, 37.0, "Big"), text(200.0, 10.0, 40.0, 19.0, "small")]);
        let g = build(&p, 30, 10, 10.0);
        assert_eq!(g.text()[0].split_whitespace().collect::<Vec<_>>(), ["Big", "small"]);
        assert_eq!(g.rows.len(), 1);
    }

    #[test]
    fn proportional_overflow_pushes_right_instead_of_overwriting() {
        // Text wider than its cells: "abcdef" in 30 px at 10 px/col (measured
        // from the other two lines), then "gh" at 30 px.
        let p = page(vec![
            text(0.0, 0.0, 30.0, 19.0, "abcdef"),
            text(30.0, 0.0, 20.0, 19.0, "gh"),
            text(0.0, 19.0, 100.0, 19.0, "0123456789"),
            text(0.0, 38.0, 100.0, 19.0, "0123456789"),
        ]);
        let g = build(&p, 20, 10, 10.0);
        assert_eq!(g.text()[0], "abcdef gh");
    }

    #[test]
    fn cells_a_few_pixels_apart_do_not_run_together() {
        // Two table cells: "alpha" fills 50 px, the next starts 3 px later and
        // rounds onto the very next column.
        let p = page(vec![text(0.0, 0.0, 50.0, 19.0, "alpha"), text(53.0, 0.0, 10.0, 19.0, "1")]);
        assert_eq!(build(&p, 20, 10, 10.0).text()[0], "alpha 1");
        // Inline runs that abut ("<b>bo</b>ld") stay joined.
        let p = page(vec![text(0.0, 0.0, 20.0, 19.0, "bo"), text(20.0, 0.0, 20.0, 19.0, "ld")]);
        assert_eq!(build(&p, 20, 10, 10.0).text()[0], "bold");
    }

    #[test]
    fn an_image_beside_a_paragraph_does_not_merge_its_lines() {
        // A 200 px image whose top is between two text lines.
        let mut img = text(300.0, 25.0, 200.0, 200.0, "[photo]");
        img.kind = Kind::Image;
        let p = page(vec![text(0.0, 0.0, 40.0, 19.0, "one"), img, text(0.0, 38.0, 40.0, 19.0, "two"), text(0.0, 57.0, 40.0, 19.0, "three")]);
        let g = build(&p, 60, 10, 10.0);
        let rows: Vec<String> = g.text().iter().map(|r| r.split_whitespace().collect::<Vec<_>>().join(" ")).collect();
        assert_eq!(rows, ["one", "[photo]", "two", "three"]);
    }

    #[test]
    fn an_image_label_stays_in_its_box() {
        let mut img = text(0.0, 0.0, 80.0, 80.0, "[A photo of various products made from paper.]");
        img.kind = Kind::Image;
        let g = build(&page(vec![img]), 60, 10, 10.0);
        assert_eq!(g.text()[0], "[A pho…]", "80 px is 8 cells");
    }

    #[test]
    fn backgrounds_cover_the_rows_of_their_lines_only() {
        let mut p = page(vec![text(0.0, 0.0, 40.0, 19.0, "top"), text(0.0, 60.0, 40.0, 19.0, "card")]);
        p.fills.push(Fill { rect: Rect { x: 0.0, y: 52.0, w: 50.0, h: 35.0 }, bg: Rgb(1, 2, 3), alpha: 1.0, layer: Layer::Flow, order: 1 });
        let g = build(&p, 10, 10, 10.0);
        let card = g.row_at(60.0);
        assert_eq!(g.rows[card][0].bg, Some(Rgb(1, 2, 3)));
        assert_eq!(g.rows[card][5].bg, Some(Rgb(255, 255, 255)), "the box is 5 columns wide; white canvas beside it");
        assert!(g.rows[..card].iter().all(|r| r[0].bg == Some(g.canvas)), "the blank row above the card stays canvas");
    }

    #[test]
    fn fixed_content_sits_on_screen_rows_and_hints_on_their_targets() {
        let mut header = text(0.0, 500.0 + 4.0, 40.0, 19.0, "head");
        header.layer = Layer::Fixed;
        let mut hint = text(0.0, 120.0, 20.0, 15.0, "AS");
        hint.layer = Layer::Hint;
        let mut p = page(vec![text(0.0, 0.0, 40.0, 19.0, "a"), text(0.0, 120.0, 40.0, 19.0, "link"), header, hint]);
        p.scroll_y = 500.0;
        let g = build(&p, 10, 5, 10.0);
        assert_eq!(g.fixed.iter().map(|c| c.cell.ch).collect::<String>(), "head");
        assert!(g.fixed.iter().all(|c| c.row == 0), "top of the screen, wherever the page is scrolled");
        let r = g.row_at(120.0);
        assert!(g.hints.iter().all(|c| c.row == r));
        assert_eq!(g.hints.iter().map(|c| c.cell.ch).collect::<String>(), "AS");
    }

    #[test]
    fn overlapping_hint_labels_are_spread_out() {
        let label = |x: f64, s: &str| {
            let mut t = text(x, 0.0, 30.0, 15.0, s);
            t.layer = Layer::Hint;
            t
        };
        let p = page(vec![text(0.0, 0.0, 200.0, 19.0, "x"), label(0.0, "AAA"), label(10.0, "AAS"), label(80.0, "AAD")]);
        let g = build(&p, 30, 5, 10.0);
        let mut line = [' '; 12];
        for h in &g.hints {
            line[h.col] = h.cell.ch;
        }
        assert_eq!(line.iter().collect::<String>(), "AAA AAS AAD ");
    }

    #[test]
    fn wide_characters_take_two_cells_and_icon_glyphs_none() {
        let p = page(vec![text(0.0, 0.0, 60.0, 19.0, "日本\u{f007}x")]);
        let g = build(&p, 10, 5, 10.0);
        assert_eq!(g.text()[0], "日本x");
        assert!(g.rows[0][1].tail && g.rows[0][3].tail);
        assert_eq!(g.rows[0][4].ch, 'x');
    }
}
