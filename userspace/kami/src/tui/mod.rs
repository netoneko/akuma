//! `kami tui`: the page's layout painted into the terminal, with ratatui.
//!
//! A second front end beside the framebuffer one (`fb.rs` + `display.rs`): the
//! same daemon, tab, session machine, keys and link hints, but no pixels.
//! Instead of a screencast the machine runs in layout mode
//! ([`machine::Output::Layout`]): it asks the page whether it changed
//! (`layout.js`, a mutation counter) and, when it did, for a
//! `DOMSnapshot.captureSnapshot` — Chromium's own layout, every box and line
//! of text with its position, colours and paint order. This shell turns that
//! into cells ([`page`] -> [`grid`]) and ratatui draws the visible window of
//! it ([`view`]).
//!
//! The page is laid out at `cols x cell-width` CSS px, so it wraps for the
//! terminal; by default `cells.js` gives all text one monospace face and size
//! so every character is exactly one cell (`--page-fonts` keeps the page's
//! own). The whole document is in the grid, so `j`/`k`/`d`/`u`/`gg`/`G` and
//! the arrows scroll the view locally with no round-trip, and the page is then
//! scrolled to match (lazy content loads, link hints label what is shown).
//!
//! Debugging: `KAMI_TUI_DUMP=<path>` writes the screen after every draw as
//! plain text to `<path>` and with colour to `<path>.ans`;
//! `KAMI_SNAPSHOT_DUMP=<path>` keeps the latest raw snapshot reply.

mod grid;
mod image;
mod kitty;
mod page;
mod view;

use std::io::{self, Write};
use std::time::Instant;

use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::{Terminal, TerminalOptions, Viewport};

use crate::machine::{self, Clip, Effect, Event, Source};
use crate::nav::Scroll;
use crate::{common_effect, gather, input_log_open, pinned_target, spawn_input_pump, Args, RawTty, HOME, TUI_LEAVE, TUI_SCREEN};

use std::collections::{HashMap, HashSet};

use grid::{Grid, Slot, ROW_PX};
use page::Page;

/// Capture tags (`Clip::tag`): the viewport sample for half blocks, the image
/// viewer, and otherwise an image's own key (kitty graphics).
const TAG_VIEWPORT: u64 = 0;
const TAG_VIEWER: u64 = u64::MAX;
/// The viewer's kitty image id; page images count up from `FIRST_ID`.
const VIEWER_ID: u32 = 1;
const FIRST_ID: u32 = 100;

/// A page image the terminal holds (kitty graphics): its id and pixel size.
#[derive(Clone, Copy, Debug)]
pub struct Held {
    pub id: u32,
    pub w: u32,
    pub h: u32,
}

/// Kitty graphics state: which page images the terminal holds, and which
/// captures are out for it.
#[derive(Default)]
struct Kitty {
    next_id: u32,
    held: HashMap<u64, Held>,
    asked: HashSet<u64>,
}

/// The image viewer (`v`): one image, full screen.
pub struct Viewer {
    pub slot: Slot,
    /// Decoded pixels (`w`, `h`, channels, data), for half blocks.
    pub px: Option<(usize, usize, usize, Vec<u8>)>,
    /// The PNG's size, once the terminal holds it (kitty).
    pub held: Option<(u32, u32)>,
}

/// CSS px per column the viewport is sized with: a 16 px monospace advance
/// (0.6 em in Menlo and DejaVu Sans Mono) with `cells.js`, an average
/// proportional character without it. The grid measures the real width from
/// the page; this only decides where Chromium wraps.
fn cell_px(page_fonts: bool) -> f64 {
    if page_fonts { 7.5 } else { 9.63 }
}

/// The terminal size in cells, `(cols, rows)`.
fn term_size() -> (u16, u16) {
    ratatui::crossterm::terminal::size().ok().filter(|&(c, r)| c > 0 && r > 1).unwrap_or((80, 24))
}

/// The page viewport for a terminal: everything but the status line.
fn viewport(size: (u16, u16), page_fonts: bool) -> (usize, usize) {
    let (cols, rows) = size;
    ((cols as f64 * cell_px(page_fonts)).round() as usize, (rows.saturating_sub(1) as f64 * ROW_PX).round() as usize)
}

/// The alternate screen, cursor hidden, for as long as this lives.
struct Screen;

impl Screen {
    fn enter() -> io::Result<Screen> {
        let mut out = io::stdout();
        out.write_all(b"\x1b[?1049h\x1b[?25l\x1b[2J")?;
        out.flush()?;
        TUI_SCREEN.store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(Screen)
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        TUI_SCREEN.store(false, std::sync::atomic::Ordering::Relaxed);
        let mut out = io::stdout();
        let _ = out.write_all(TUI_LEAVE.as_bytes());
        let _ = out.flush();
    }
}

/// Where the view is, and what it shows.
struct State {
    page: Option<Page>,
    grid: Option<Grid>,
    /// The first document row on screen.
    top: usize,
    /// The document y the view starts at (CSS px): what survives a new grid,
    /// whose rows may differ (an image loaded above, a reflow).
    anchor_y: f64,
    /// The last y the page was told to scroll to.
    sent_y: f64,
    status: String,
    size: (u16, u16),
    page_fonts: bool,
    images: image::Cache,
    /// The page viewport (CSS px).
    view: (usize, usize),
    /// Kitty graphics, when the terminal has them.
    kitty: Option<Kitty>,
    /// One terminal cell in device pixels (from `TIOCGWINSZ`), for sizing
    /// captures and keeping an image's aspect in the viewer.
    cell_px: (f64, f64),
    /// `v`: the images labelled, and the label prefix typed so far.
    pick: Vec<Slot>,
    pick_prefix: String,
    viewer: Option<Viewer>,
}

impl State {
    /// A capture of the page's viewport for the images on screen, if any of
    /// them still lacks pixels. The page is where the last snapshot found it.
    fn capture(&self) -> Option<Clip> {
        let (g, p) = (self.grid.as_ref()?, self.page.as_ref()?);
        if !self.images.missing(g, self.top..self.top + self.page_rows()) {
            return None;
        }
        // Two samples per cell across, about four per row down: enough for
        // half blocks, and a 100-column view is a ~200 px wide PNG.
        let scale = (2.0 / g.cw).min(1.0);
        Some(Clip { x: 0.0, y: p.scroll_y, w: self.view.0 as f64, h: self.view.1 as f64, scale, full: false, tag: TAG_VIEWPORT })
    }

    /// Kitty graphics: captures of each image on screen the terminal does not
    /// hold yet and that lies wholly inside the page's viewport (a capture
    /// outside it comes back blank). Each at about the terminal's own pixel
    /// density for the cells it covers. Images that never fit the viewport
    /// stay half blocks; the viewer shows them whole.
    fn kitty_captures(&mut self) -> Vec<Clip> {
        let (Some(k), Some(g), Some(p)) = (self.kitty.as_mut(), self.grid.as_ref(), self.page.as_ref()) else { return vec![] };
        let rows = self.top..self.top + self.size.1.saturating_sub(1) as usize;
        let (vy0, vy1, vw) = (p.scroll_y, p.scroll_y + self.view.1 as f64, self.view.0 as f64);
        // A capture is of what is painted, fixed bars included: an image a
        // fixed box covers now waits until the view has moved off it.
        let covers: Vec<page::Rect> =
            p.fills.iter().filter(|f| f.layer == page::Layer::Fixed && f.alpha > 0.9).map(|f| f.rect).collect();
        let mut out = vec![];
        for s in g.images.iter().filter(|s| !s.backdrop && s.rows.start < rows.end && rows.start < s.rows.end) {
            let r = s.rect;
            let inside = r.y >= vy0 - 1.0 && r.y + r.h <= vy1 + 1.0 && r.x >= -1.0 && r.x + r.w <= vw + 1.0;
            let covered = covers.iter().any(|c| c.x < r.x + r.w && r.x < c.x + c.w && c.y < r.y + r.h && r.y < c.y + c.h);
            if !inside || covered || k.held.contains_key(&s.key) || !k.asked.insert(s.key) {
                continue;
            }
            let scale = (self.cell_px.0 * s.cols.len() as f64 / r.w.max(1.0)).clamp(0.5, 2.0);
            out.push(Clip { x: r.x, y: r.y, w: r.w, h: r.h, scale, full: false, tag: s.key });
        }
        out
    }

    /// Forget (and free in the terminal) images no longer on the page.
    fn kitty_prune(&mut self, out: &mut String) {
        let (Some(k), Some(g)) = (self.kitty.as_mut(), self.grid.as_ref()) else { return };
        let live: HashSet<u64> = g.images.iter().map(|s| s.key).collect();
        k.held.retain(|key, h| {
            let keep = live.contains(key);
            if !keep {
                kitty::free(out, h.id);
            }
            keep
        });
        k.asked.retain(|key| live.contains(key));
    }

    /// Where the kitty images go this frame: the viewer's, or every held page
    /// image on screen, cropped to the rows the view shows and kept off rows
    /// the fixed layer covers (a header over a photo).
    fn placements(&self) -> Vec<kitty::Placement> {
        let (cols, page_rows) = (self.size.0 as usize, self.page_rows());
        if let Some(v) = &self.viewer {
            let Some((w, h)) = v.held else { return vec![] };
            // The bottom row is the caption's.
            let (c, r) = fit((w as f64, h as f64), (cols, page_rows.saturating_sub(1)), self.cell_px);
            let (col, row) = ((cols - c) / 2, (page_rows.saturating_sub(1) - r) / 2);
            return vec![kitty::Placement { id: VIEWER_ID, col: col as u16, row: row as u16, cols: c as u16, rows: r as u16, crop: (0, 0, w, h) }];
        }
        // While `v` labels are up, half blocks: a label's background would be
        // under the pixels.
        if !self.pick.is_empty() {
            return vec![];
        }
        let (Some(k), Some(g)) = (self.kitty.as_ref(), self.grid.as_ref()) else { return vec![] };
        let mut out = vec![];
        for s in g.images.iter().filter(|s| !s.backdrop) {
            let Some(held) = k.held.get(&s.key) else { continue };
            let (c0, c1) = (s.cols.start, s.cols.end.min(cols));
            // Screen rows where fixed content with a background crosses this
            // image's columns (a header; not a sidebar beside it).
            let covered: HashSet<usize> =
                g.fixed.iter().filter(|p| p.cell.bg.is_some() && p.col >= c0 && p.col < c1).map(|p| p.row).collect();
            let (mut r0, mut r1) = (s.rows.start.max(self.top), s.rows.end.min(self.top + page_rows));
            while r0 < r1 && covered.contains(&(r0 - self.top)) {
                r0 += 1;
            }
            while r1 > r0 && covered.contains(&(r1 - 1 - self.top)) {
                r1 -= 1;
            }
            if r0 >= r1 || c0 >= c1 {
                continue;
            }
            let (rows, ncols) = (s.rows.len() as f64, s.cols.len() as f64);
            let (w, h) = (held.w as f64, held.h as f64);
            let crop_y = ((r0 - s.rows.start) as f64 / rows * h) as u32;
            let crop_h = (((r1 - r0) as f64 / rows * h) as u32).max(1);
            let crop_w = (((c1 - c0) as f64 / ncols * w) as u32).max(1);
            out.push(kitty::Placement {
                id: held.id,
                col: c0 as u16,
                row: (r0 - self.top) as u16,
                cols: (c1 - c0) as u16,
                rows: (r1 - r0) as u16,
                crop: (0, crop_y, crop_w, crop_h),
            });
        }
        out
    }

    /// `v`: the images on screen worth a closer look, in reading order.
    fn pickable(&self) -> Vec<Slot> {
        let Some(g) = self.grid.as_ref() else { return vec![] };
        let rows = self.top..self.top + self.page_rows();
        let screen = (self.size.0 as usize * self.page_rows()) as f64;
        let mut v: Vec<Slot> = g
            .images
            .iter()
            .filter(|s| s.rows.start < rows.end && rows.start < s.rows.end && s.cols.start < self.size.0 as usize)
            .filter(|s| s.cols.len() >= 3 && s.rows.len() >= 2)
            // A background as big as half the screen is the page's backdrop.
            .filter(|s| !s.backdrop || ((s.cols.len() * s.rows.len()) as f64) < screen / 2.0)
            .cloned()
            .collect();
        v.sort_by_key(|s| (s.rows.start.max(self.top), s.cols.start));
        v
    }

    /// The labels to draw for `v`: screen row, column, label.
    fn pick_labels(&self) -> Vec<(u16, u16, String)> {
        let len = crate::nav::label_len(self.pick.len());
        self.pick
            .iter()
            .enumerate()
            .map(|(i, s)| (crate::nav::label(i, len), s))
            .filter(|(l, _)| l.starts_with(&self.pick_prefix))
            .map(|(l, s)| ((s.rows.start.max(self.top) - self.top) as u16, s.cols.start as u16, l.to_uppercase()))
            .collect()
    }

    /// The viewer's capture: the whole image, beyond the viewport if need be,
    /// at about the screen's pixel size.
    fn viewer_clip(&self, s: &Slot) -> Clip {
        let r = s.rect;
        let (sw, sh) = (self.size.0 as f64 * self.cell_px.0, self.page_rows() as f64 * self.cell_px.1);
        let scale = (sw / r.w.max(1.0)).min(sh / r.h.max(1.0));
        let scale = if self.kitty.is_some() { scale.clamp(0.25, 4.0) } else { (scale / self.cell_px.0 * 2.0).clamp(0.1, 2.0) };
        Clip { x: r.x, y: r.y, w: r.w, h: r.h, scale, full: true, tag: TAG_VIEWER }
    }

    fn page_rows(&self) -> usize {
        self.size.1.saturating_sub(1) as usize
    }

    fn max_top(&self) -> usize {
        self.grid.as_ref().map_or(0, |g| g.rows.len().saturating_sub(self.page_rows()))
    }

    fn relayout(&mut self) {
        if let Some(p) = &self.page {
            let t = Instant::now();
            let g = grid::build(p, self.size.0 as usize, self.page_rows(), cell_px(self.page_fonts));
            ilog!(
                "layout: {} items, {} fills -> {} rows at {:.2} px/col in {} ms",
                p.items.len(),
                p.fills.len(),
                g.rows.len(),
                g.cw,
                t.elapsed().as_millis()
            );
            self.top = g.row_at(self.anchor_y);
            self.grid = Some(g);
            self.top = self.top.min(self.max_top());
        }
    }

    /// Move the view; the document y it now starts at, if it moved.
    fn scroll(&mut self, s: Scroll) -> Option<f64> {
        let g = self.grid.as_ref()?;
        let h = self.page_rows() as i64;
        let top = self.top as i64;
        let to = match s {
            Scroll::Line(n) => top + 3 * n as i64,
            Scroll::Half(n) => top + n as i64 * (h / 2).max(1),
            Scroll::Edge(n) if n < 0 => 0,
            Scroll::Edge(_) => self.max_top() as i64,
        };
        let to = (to.max(0) as usize).min(self.max_top());
        if to == self.top {
            return None;
        }
        self.top = to;
        self.anchor_y = g.row_y(to);
        self.sent_y = self.anchor_y;
        Some(self.anchor_y)
    }
}

/// The largest cells box with an image's aspect inside `(cols, rows)`, given
/// a cell's pixel size.
pub fn fit(img: (f64, f64), area: (usize, usize), cell: (f64, f64)) -> (usize, usize) {
    let (iw, ih) = (img.0.max(1.0), img.1.max(1.0));
    let (aw, ah) = (area.0.max(1) as f64 * cell.0, area.1.max(1) as f64 * cell.1);
    let s = (aw / iw).min(ah / ih);
    let c = ((iw * s / cell.0).round() as usize).clamp(1, area.0.max(1));
    let r = ((ih * s / cell.1).round() as usize).clamp(1, area.1.max(1));
    (c, r)
}

/// One terminal cell in device pixels, from the terminal if it says.
fn cell_pixels() -> (f64, f64) {
    match ratatui::crossterm::terminal::window_size() {
        Ok(w) if w.width > 0 && w.height > 0 && w.columns > 0 && w.rows > 0 => {
            (w.width as f64 / w.columns as f64, w.height as f64 / w.rows as f64)
        }
        _ => (10.0, 20.0),
    }
}

fn write_out(s: &str) {
    if !s.is_empty() {
        let mut out = io::stdout();
        let _ = out.write_all(s.as_bytes());
        let _ = out.flush();
    }
}

pub fn run(args: &Args) -> io::Result<()> {
    input_log_open();
    ilog!("---- tui session start, pid {} ----", std::process::id());
    let size = term_size();
    let view = viewport(size, args.page_fonts);
    ilog!("startup: terminal {}x{} -> page {}x{}{}", size.0, size.1, view.0, view.1, if args.page_fonts { " (page fonts)" } else { "" });
    let mut m = machine::Machine::new(machine::Config {
        url: args.url.clone(),
        home: HOME.into(),
        view,
        poll_ms: None,
        max_frames: args.frames,
        seconds: args.seconds,
        pinned: pinned_target(args),
        output: machine::Output::Layout,
        cell_fonts: !args.page_fonts,
    });

    let tty = RawTty::enter();
    let mut stdin_open = tty.0.is_some();
    // How images are drawn. Decided before the input pump starts: asking the
    // terminal means reading its answer from the tty.
    let use_kitty = match args.images.as_deref().or(std::env::var("KAMI_IMAGES").ok().as_deref()) {
        Some("kitty") => true,
        Some("blocks") => false,
        _ => kitty::named() || (stdin_open && kitty::probe(500)),
    };
    let input_fd = match tty.0 {
        Some(saved) => spawn_input_pump(saved)?,
        None => 0,
    };
    let screen = Screen::enter()?;
    ilog!("startup: alternate screen");
    // Not `term.clear()`: it asks the terminal where the cursor is and reads
    // the answer from stdin, which the input pump owns (the answer never
    // comes, or comes as keys). The alternate screen is cleared on entry.
    // A fixed viewport at kami's own idea of the size, not ratatui's: a pty
    // that reports 0x0 (ssh -tt with no terminal behind it, seen on Akuma
    // 2026-10-10) made ratatui draw nothing at all, where `term_size` falls
    // back to 80x24.
    let full = |(c, r): (u16, u16)| Rect { x: 0, y: 0, width: c, height: r };
    let mut term = Terminal::with_options(CrosstermBackend::new(io::stdout()), TerminalOptions { viewport: Viewport::Fixed(full(size)) })?;
    ilog!("startup: terminal ready");

    let mut st = State {
        page: None,
        grid: None,
        top: 0,
        anchor_y: 0.0,
        sent_y: 0.0,
        status: String::new(),
        size,
        page_fonts: args.page_fonts,
        images: image::Cache::default(),
        view,
        kitty: None,
        cell_px: cell_pixels(),
        pick: vec![],
        pick_prefix: String::new(),
        viewer: None,
    };
    if use_kitty {
        st.kitty = Some(Kitty { next_id: FIRST_ID, ..Kitty::default() });
    }
    ilog!("startup: images as {}, cell {:.1}x{:.1} px", if use_kitty { "kitty graphics" } else { "half blocks" }, st.cell_px.0, st.cell_px.1);
    let mut shot = crate::png::Decoder::default();
    let mut shot_bytes = Vec::new();
    let dump = std::env::var_os("KAMI_TUI_DUMP");
    let snap_dump = std::env::var_os("KAMI_SNAPSHOT_DUMP");
    let t0 = Instant::now();
    let mut c = None;
    let mut view = view;

    loop {
        let mut queue = gather(&mut c, input_fd, &mut stdin_open, t0)?;
        let mut dirty = false;
        let now = term_size();
        if now != st.size {
            ilog!("terminal resized to {}x{}", now.0, now.1);
            st.size = now;
            term.resize(full(now))?;
            st.relayout();
            view = viewport(now, args.page_fonts);
            st.view = view;
            queue.push_back(Event::Resized(view));
            dirty = true;
        }
        while let Some(ev) = queue.pop_front() {
            for eff in m.handle(ev) {
                let Some(eff) = common_effect(eff, &mut c, &mut queue, args, view) else { continue };
                match eff {
                    Effect::Layout { msg, follow_scroll } => {
                        if let Some(path) = &snap_dump {
                            let _ = std::fs::write(path, &msg);
                        }
                        let t = Instant::now();
                        match page::parse(&msg) {
                            Ok(p) => {
                                ilog!("layout: parsed {} KB in {} ms, scroll {}", msg.len() / 1024, t.elapsed().as_millis(), p.scroll_y);
                                // The page moved itself (a navigation, an anchor,
                                // its own script): follow it. Not when the snapshot
                                // predates our own last scroll.
                                // Nor while the viewer is open: its capture resizes
                                // the page, which can reset the page's scroll.
                                if follow_scroll && st.viewer.is_none() && (p.scroll_y - st.sent_y).abs() > ROW_PX {
                                    st.anchor_y = p.scroll_y;
                                    st.sent_y = p.scroll_y;
                                }
                                st.page = Some(p);
                                st.relayout();
                                dirty = true;
                                let mut out = String::new();
                                st.kitty_prune(&mut out);
                                write_out(&out);
                                if let Some(clip) = st.capture() {
                                    queue.push_back(Event::Capture(clip));
                                }
                                for clip in st.kitty_captures() {
                                    queue.push_back(Event::Capture(clip));
                                }
                                queue.push_back(Event::Presented { source: Source::Layout, ok: true, empty: false });
                            }
                            Err(e) => {
                                ilog!("layout: {e}");
                                queue.push_back(Event::Presented { source: Source::Layout, ok: false, empty: false });
                            }
                        }
                    }
                    Effect::Scroll(s) => {
                        if let Some(y) = st.scroll(s) {
                            dirty = true;
                            queue.push_back(Event::ViewScrolled(y));
                        }
                    }
                    Effect::Captured { b64, clip } if clip.tag == TAG_VIEWER => {
                        let Some(v) = st.viewer.as_mut() else { continue };
                        let decoded = crate::b64_decode(b64.as_bytes(), &mut shot_bytes);
                        if st.kitty.is_some() {
                            let mut out = String::new();
                            kitty::transmit(&mut out, VIEWER_ID, &b64);
                            write_out(&out);
                            v.held = kitty::png_size(&shot_bytes);
                        } else if decoded.and_then(|_| shot.decode(&shot_bytes)).is_ok() {
                            v.px = Some((shot.width, shot.height, shot.channels, std::mem::take(&mut shot.pixels)));
                        }
                        ilog!("viewer: image {} KB", b64.len() * 3 / 4 / 1024);
                        dirty = true;
                    }
                    Effect::Captured { b64, clip } if clip.tag != TAG_VIEWPORT => {
                        let Some(k) = st.kitty.as_mut() else { continue };
                        k.asked.remove(&clip.tag);
                        if crate::b64_decode(b64.as_bytes(), &mut shot_bytes).is_err() {
                            continue;
                        }
                        let Some((w, h)) = kitty::png_size(&shot_bytes) else { continue };
                        let id = k.next_id;
                        k.next_id = k.next_id.wrapping_add(1).max(FIRST_ID);
                        let mut out = String::new();
                        kitty::transmit(&mut out, id, &b64);
                        write_out(&out);
                        k.held.insert(clip.tag, Held { id, w, h });
                        ilog!("kitty: image {id} {w}x{h}, {} KB", b64.len() * 3 / 4 / 1024);
                        dirty = true;
                    }
                    Effect::Captured { b64, clip } => {
                        let t = Instant::now();
                        let decoded = crate::b64_decode(b64.as_bytes(), &mut shot_bytes).and_then(|_| shot.decode(&shot_bytes));
                        match (decoded, st.grid.as_ref()) {
                            (Ok(()), Some(g)) => {
                                let px = image::Pixels { px: &shot.pixels, w: shot.width, h: shot.height, ch: shot.channels };
                                st.images.absorb(g, &px, &clip);
                                ilog!("layout: capture {}x{} sampled in {} ms", shot.width, shot.height, t.elapsed().as_millis());
                                dirty = true;
                            }
                            (Err(e), _) => ilog!("layout: capture not decoded: {e}"),
                            _ => {}
                        }
                    }
                    Effect::Status(t) => {
                        st.status = t;
                        dirty = true;
                    }
                    Effect::PickImages => {
                        st.pick = st.pickable();
                        st.pick_prefix.clear();
                        ilog!("images: {} on screen", st.pick.len());
                        queue.push_back(Event::Picked(st.pick.len()));
                        dirty = true;
                    }
                    Effect::PickFilter(p) => {
                        st.pick_prefix = p;
                        dirty = true;
                    }
                    Effect::PickCancel => {
                        st.pick.clear();
                        dirty = true;
                    }
                    Effect::ViewImage(i) => {
                        if let Some(slot) = st.pick.get(i).cloned() {
                            let clip = st.viewer_clip(&slot);
                            ilog!("viewer: image {i}, {:.0}x{:.0} px at scale {:.2}", slot.rect.w, slot.rect.h, clip.scale);
                            st.viewer = Some(Viewer { slot, px: None, held: None });
                            queue.push_back(Event::Capture(clip));
                        }
                        st.pick.clear();
                        dirty = true;
                    }
                    Effect::CloseView => {
                        if st.viewer.take().is_some() && st.kitty.is_some() {
                            let mut out = String::new();
                            kitty::free(&mut out, VIEWER_ID);
                            write_out(&out);
                        }
                        // Put the page back where the view is.
                        queue.push_back(Event::ViewScrolled(st.anchor_y));
                        dirty = true;
                    }
                    Effect::Done(err) => {
                        ilog!("session done: {err:?}");
                        drop(term);
                        drop(screen);
                        drop(tty);
                        eprintln!("[kami] detached; chromium keeps running (kami --kill stops it)");
                        return match err {
                            None => Ok(()),
                            Some(e) => Err(io::Error::other(e)),
                        };
                    }
                    // Pixels are the framebuffer's business.
                    Effect::Present { .. } => {}
                    Effect::TryConnect { .. } | Effect::Send(_) | Effect::Pin(_) | Effect::Log(_) => {}
                }
            }
        }
        if dirty {
            let frame = term.draw(|f| {
                let labels = if st.pick.is_empty() { vec![] } else { st.pick_labels() };
                let held: Option<HashSet<u64>> =
                    st.kitty.as_ref().filter(|_| st.pick.is_empty()).map(|k| k.held.keys().copied().collect());
                f.render_widget(
                    view::View {
                        grid: st.grid.as_ref(),
                        images: &st.images,
                        held: held.as_ref(),
                        top: st.top,
                        status: &st.status,
                        labels: &labels,
                        viewer: st.viewer.as_ref(),
                        cell_px: st.cell_px,
                    },
                    f.area(),
                )
            })?;
            let dumped = dump.as_ref().map(|_| view::dump(frame.buffer));
            if st.kitty.is_some() {
                let mut out = String::new();
                kitty::clear(&mut out);
                let placed = st.placements();
                for p in &placed {
                    kitty::place(&mut out, p);
                }
                write_out(&out);
                let held = st.kitty.as_ref().map_or(0, |k| k.held.len());
                ilog!("kitty: {} placed of {held} held, top {}", placed.len(), st.top);
            }
            if let (Some(path), Some((plain, ansi))) = (&dump, dumped) {
                let _ = std::fs::write(path, plain);
                let mut ans = path.clone();
                ans.push(".ans");
                let _ = std::fs::write(ans, ansi);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fit;

    #[test]
    fn the_viewer_keeps_an_image_aspect_in_cells_twice_as_tall_as_wide() {
        // A square image in a 100x40 area of 10x20 px cells: 40 rows tall
        // is 800 px, so 80 columns wide.
        assert_eq!(fit((500.0, 500.0), (100, 40), (10.0, 20.0)), (80, 40));
        // A wide one is bounded by the width instead.
        assert_eq!(fit((2000.0, 500.0), (100, 40), (10.0, 20.0)), (100, 13));
    }
}
