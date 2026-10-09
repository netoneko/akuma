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
mod page;
mod view;

use std::io::{self, Write};
use std::time::Instant;

use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::machine::{self, Effect, Event, Source};
use crate::nav::Scroll;
use crate::{common_effect, gather, input_log_open, pinned_target, spawn_input_pump, Args, RawTty, HOME, TUI_LEAVE, TUI_SCREEN};

use grid::{Grid, ROW_PX};
use page::Page;

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
}

impl State {
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
    let input_fd = match tty.0 {
        Some(saved) => spawn_input_pump(saved)?,
        None => 0,
    };
    let screen = Screen::enter()?;
    ilog!("startup: alternate screen");
    // Not `term.clear()`: it asks the terminal where the cursor is and reads
    // the answer from stdin, which the input pump owns (the answer never
    // comes, or comes as keys). The alternate screen is cleared on entry.
    let mut term = Terminal::new(CrosstermBackend::new(io::stdout()))?;
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
    };
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
            term.autoresize()?;
            st.relayout();
            view = viewport(now, args.page_fonts);
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
                                if follow_scroll && (p.scroll_y - st.sent_y).abs() > ROW_PX {
                                    st.anchor_y = p.scroll_y;
                                    st.sent_y = p.scroll_y;
                                }
                                st.page = Some(p);
                                st.relayout();
                                dirty = true;
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
                    Effect::Status(t) => {
                        st.status = t;
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
                f.render_widget(view::View { grid: st.grid.as_ref(), top: st.top, status: &st.status }, f.area())
            })?;
            if let Some(path) = &dump {
                let (plain, ansi) = view::dump(frame.buffer);
                let _ = std::fs::write(path, plain);
                let mut ans = path.clone();
                ans.push(".ans");
                let _ = std::fs::write(ans, ansi);
            }
        }
    }
}
