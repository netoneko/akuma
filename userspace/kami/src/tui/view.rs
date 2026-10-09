//! The terminal view: a [`Grid`] window at a scroll offset, plus the status line.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Widget;

use super::grid::{Cell, Grid, Slot};
use super::image::Cache;
use super::page::Rgb;

pub struct View<'a> {
    pub grid: Option<&'a Grid>,
    pub images: &'a Cache,
    /// The first document row on screen.
    pub top: usize,
    pub status: &'a str,
}

fn color(c: Option<Rgb>) -> Color {
    c.map_or(Color::Reset, |Rgb(r, g, b)| Color::Rgb(r, g, b))
}

fn style(c: &Cell) -> Style {
    let mut m = Modifier::empty();
    if c.bold {
        m |= Modifier::BOLD;
    }
    if c.italic {
        m |= Modifier::ITALIC;
    }
    if c.underline {
        m |= Modifier::UNDERLINED;
    }
    if c.dim {
        m |= Modifier::DIM;
    }
    Style::default().fg(color(c.fg)).bg(color(c.bg)).add_modifier(m)
}

fn put(buf: &mut Buffer, x: u16, y: u16, c: &Cell) {
    if c.tail {
        return; // the double-width character to its left covers it
    }
    if let Some(cell) = buf.cell_mut((x, y)) {
        // `set_style` patches: it adds modifiers and never clears them, so a
        // cell drawn over a bold, underlined link would stay bold and
        // underlined. Each layer replaces the cell outright.
        cell.reset();
        cell.set_char(c.ch).set_style(style(c));
    }
}

impl Widget for View<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }
        let page_h = area.height - 1;
        if let Some(g) = self.grid {
            // Below the end of a short page the browser shows canvas too.
            let canvas = Style::default().bg(color(Some(g.canvas)));
            buf.set_style(Rect { height: page_h, ..area }, canvas);
            for y in 0..page_h {
                let Some(row) = g.rows.get(self.top + y as usize) else { break };
                for (x, c) in row.iter().enumerate().take(area.width as usize) {
                    put(buf, area.x + x as u16, area.y + y, c);
                }
            }
            let rows = self.top..self.top + page_h as usize;
            for slot in g.images.iter().filter(|s| s.rows.start < rows.end && rows.start < s.rows.end) {
                draw_image(buf, area, g, slot, self.images, rows.clone());
            }
            for p in &g.fixed {
                if p.row < page_h as usize && p.col < area.width as usize {
                    put(buf, area.x + p.col as u16, area.y + p.row as u16, &p.cell);
                }
            }
            // Hint labels last: in the page they are above everything
            // (z-index max), including a fixed header's own links.
            for p in &g.hints {
                if p.row >= self.top && p.row < self.top + page_h as usize && p.col < area.width as usize {
                    put(buf, area.x + p.col as u16, area.y + (p.row - self.top) as u16, &p.cell);
                }
            }
        }
        // The status line: kami's own, then where the view is.
        let y = area.y + page_h;
        let bar = Style::default().add_modifier(Modifier::REVERSED);
        let pos = match self.grid {
            Some(g) if !g.rows.is_empty() => format!(" {}/{} ", (self.top + 1).min(g.rows.len()), g.rows.len()),
            _ => String::new(),
        };
        let room = (area.width as usize).saturating_sub(pos.chars().count());
        let text: String = self.status.chars().take(room).collect();
        buf.set_style(Rect { x: area.x, y, width: area.width, height: 1 }, bar);
        buf.set_string(area.x, y, &text, bar);
        buf.set_string(area.x + room as u16, y, &pos, bar);
    }
}

/// The placeholder an image shows until its pixels arrive.
const PENDING: Rgb = Rgb(0xd8, 0xd8, 0xd8);

fn mix(a: Rgb, b: Rgb) -> Rgb {
    let m = |x: u8, y: u8| ((x as u16 + y as u16) / 2) as u8;
    Rgb(m(a.0, b.0), m(a.1, b.1), m(a.2, b.2))
}

/// One image slot's visible rows: `▀` half blocks where there are pixels.
/// Text in the slot stays on top: over an `<img>` (a caption on a photo) it
/// keeps its glyph on the photo's colour; a background never covers text.
/// An `<img>` with no pixels yet is a grey box with its alt text.
fn draw_image(buf: &mut Buffer, area: Rect, g: &Grid, slot: &Slot, images: &Cache, rows: std::ops::Range<usize>) {
    let block = images.get(slot.key);
    let mut label = slot.label.chars();
    let (first, last) = (slot.rows.start.max(rows.start), slot.rows.end.min(rows.end));
    for (r, row) in g.rows.iter().enumerate().take(last).skip(first) {
        let y = area.y + (r - rows.start) as u16;
        for c in slot.cols.clone().take_while(|&c| c < area.width as usize) {
            let under = row[c];
            let x = area.x + c as u16;
            let has_text = under.ink;
            match block.and_then(|b| b.get(r - slot.rows.start, c - slot.cols.start)) {
                Some(_) if has_text && slot.backdrop => {}
                Some((up, down)) if has_text => put(buf, x, y, &Cell { bg: Some(mix(up, down)), ..under }),
                Some((up, down)) => put(buf, x, y, &Cell { ch: '▀', fg: Some(up), bg: Some(down), ..Cell::default() }),
                None if slot.backdrop || has_text => {}
                None => {
                    let ch = if r == slot.rows.start { label.next().unwrap_or(' ') } else { ' ' };
                    let fg = Some(Rgb(0x60, 0x60, 0x60));
                    put(buf, x, y, &Cell { ch, fg, bg: Some(PENDING), italic: true, ..Cell::default() });
                }
            }
        }
    }
}

/// The screen as plain text and as ANSI (truecolour SGR), for `KAMI_TUI_DUMP`.
pub fn dump(buf: &Buffer) -> (String, String) {
    let (mut plain, mut ansi) = (String::new(), String::new());
    let area = buf.area;
    for y in area.y..area.y + area.height {
        let mut line = String::new();
        let mut last: Option<Style> = None;
        for x in area.x..area.x + area.width {
            let Some(c) = buf.cell((x, y)) else { continue };
            line.push_str(c.symbol());
            let st = c.style();
            if last != Some(st) {
                ansi.push_str("\x1b[0");
                let m = st.add_modifier;
                for (bit, code) in [(Modifier::BOLD, "1"), (Modifier::DIM, "2"), (Modifier::ITALIC, "3"), (Modifier::UNDERLINED, "4"), (Modifier::REVERSED, "7")] {
                    if m.contains(bit) {
                        ansi.push(';');
                        ansi.push_str(code);
                    }
                }
                if let Some(Color::Rgb(r, g, b)) = st.fg {
                    ansi.push_str(&format!(";38;2;{r};{g};{b}"));
                }
                if let Some(Color::Rgb(r, g, b)) = st.bg {
                    ansi.push_str(&format!(";48;2;{r};{g};{b}"));
                }
                ansi.push('m');
                last = Some(st);
            }
            ansi.push_str(c.symbol());
        }
        ansi.push_str("\x1b[0m\n");
        plain.push_str(line.trim_end());
        plain.push('\n');
    }
    (plain, ansi)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_upper_layer_does_not_inherit_the_modifiers_below_it() {
        let mut buf = Buffer::empty(Rect { x: 0, y: 0, width: 2, height: 1 });
        put(&mut buf, 0, 0, &Cell { ch: 'a', bold: true, underline: true, ..Cell::default() });
        put(&mut buf, 0, 0, &Cell { ch: 'b', ..Cell::default() });
        assert_eq!(buf[(0, 0)].symbol(), "b");
        assert!(buf[(0, 0)].modifier.is_empty());
    }
}
