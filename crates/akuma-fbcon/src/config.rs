//! The per-machine console settings file, `/etc/console.conf`.
//!
//! ```text
//! # The trashcan's TV: the left half of the screen, no dead border.
//! margin = 0
//! cols = 50%
//! ```
//!
//! Read once, at boot, by `run_init`, so a machine's screen setup lives in its own
//! filesystem and not in the kernel or the boot loader's command line — a change is
//! an edit and a reboot, not a rebuild. Parsing is a pure function over a `&str`
//! (no allocation, no I/O), which is why it lives here and is tested on the host.
//!
//! | key | meaning | forms |
//! |---|---|---|
//! | `margin` | pixels between the text area and the screen edge | `0`, `24`, `24,12` (left/right, top/bottom) |
//! | `cols` | columns of the printing area, anchored top-left | `73`, `50%` |
//! | `rows` | rows of the printing area | `30`, `80%` |
//!
//! A percentage is of the grid the screen holds **after** the margin is applied, so
//! `cols = 50%` is half of whatever fits. Unknown keys, malformed values and
//! comments are ignored: a typo in this file must cost a setting, never a boot.

use crate::console::Console;
use crate::Surface;

/// A size: an absolute number of cells, or a percentage of what the screen holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dim {
    Cells(usize),
    Percent(usize),
}

impl Dim {
    /// The number of cells this is, out of `total`. Never 0, and never more than
    /// `total` (a percentage over 100 is the whole screen).
    #[must_use]
    pub fn resolve(self, total: usize) -> usize {
        let n = match self {
            Self::Cells(n) => n,
            Self::Percent(p) => total * p.min(100) / 100,
        };
        n.clamp(1, total.max(1))
    }

    fn parse(v: &str) -> Option<Self> {
        let v = v.trim();
        match v.strip_suffix('%') {
            Some(p) => p.trim().parse().ok().map(Self::Percent),
            None => v.parse().ok().map(Self::Cells),
        }
    }
}

/// What the file asked for. Every field is optional: absent means "leave it".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConsoleConfig {
    /// `(x, y)` margin in pixels.
    pub margin: Option<(usize, usize)>,
    pub cols: Option<Dim>,
    pub rows: Option<Dim>,
}

impl ConsoleConfig {
    /// Parse the file's text. Never fails: see the module docs.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut cfg = Self::default();
        for line in text.lines() {
            // `#` starts a comment anywhere on the line.
            let line = line.split('#').next().unwrap_or("").trim();
            let Some((key, value)) = line.split_once('=') else { continue };
            let (key, value) = (key.trim(), value.trim());
            match key {
                "margin" => {
                    let mut it = value.split(',');
                    if let Some(x) = it.next().and_then(|v| v.trim().parse().ok()) {
                        let y = it.next().and_then(|v| v.trim().parse().ok()).unwrap_or(x);
                        cfg.margin = Some((x, y));
                    }
                }
                "cols" => cfg.cols = Dim::parse(value).or(cfg.cols),
                "rows" => cfg.rows = Dim::parse(value).or(cfg.rows),
                _ => {}
            }
        }
        cfg
    }

    /// Is there anything to apply?
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

impl<S: Surface> Console<S> {
    /// Apply a parsed [`ConsoleConfig`]: the margin first (it changes how many
    /// cells there are, and clears the screen), then the printing area, which
    /// percentages are resolved against.
    ///
    /// Returns the printing area now in force as `(rows, columns)`.
    pub fn apply_config(&mut self, cfg: &ConsoleConfig) -> (usize, usize) {
        if let Some((mx, my)) = cfg.margin {
            self.set_margin(mx, my);
        }
        if cfg.cols.is_some() || cfg.rows.is_some() {
            let (max_cols, max_rows) = (self.max_cols(), self.max_rows());
            let cols = cfg.cols.map_or(max_cols, |d| d.resolve(max_cols));
            let rows = cfg.rows.map_or(max_rows, |d| d.resolve(max_rows));
            self.set_view(cols, rows);
        }
        // The caller is applying this at boot and reports the size itself.
        let _ = self.take_geometry();
        (self.rows(), self.cols())
    }
}
