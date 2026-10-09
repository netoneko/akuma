//! Modal keyboard navigation (vim-style): the decision layer between decoded
//! tty input and what kami does with it. Pure state machine, no I/O, so it is
//! host-tested; `main.rs` performs the [`Action`]s.
//!
//! * **Normal** (the start mode): `j`/`k` scroll a line, `d`/`u` half a page,
//!   `gg`/`G` top/bottom, `H`/`L` back/forward, `r` reload, `f` link hints,
//!   `i` insert mode. Other printable keys are swallowed rather than typed
//!   into the page. Special keys (arrows, PgUp/PgDn, Enter, Tab, Esc, ...)
//!   still pass through, so Esc closes a page's own dialog.
//! * **Insert**: everything passes through to the page; Esc blurs the focused
//!   element and returns to Normal.
//! * **Hint**: after `f` every clickable element wears a letter label; typing
//!   the label clicks it. Esc cancels, Backspace un-types a letter. `v` (in
//!   `kami tui`) labels the images on screen the same way, and the label
//!   opens one in the viewer.
//! * **View**: an image fills the screen; Esc, `q` or `v` closes it.
//!
//! Ctrl-R (reload) and Ctrl-C/Ctrl-Q (detach) work in every mode.

use crate::Input;

/// Hint label letters: the home row, 9 of them, so a label is a fixed-width
/// base-9 number and no label is a prefix of another.
pub const ALPHABET: &[u8; 9] = b"asdfghjkl";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Hint,
    View,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scroll {
    Line(i32),
    Half(i32),
    Edge(i32),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Scroll(Scroll),
    Back,
    Forward,
    Reload,
    Quit,
    /// Collect clickable elements; the caller answers with [`Nav::begin_hints`].
    HintStart,
    /// Hide the labels that do not start with this prefix.
    HintFilter(String),
    /// Click hint number N and drop the overlay.
    HintClick(usize),
    HintCancel,
    /// Blur the focused element (leaving insert mode).
    Blur,
    /// Label the images on screen; the caller answers with [`Nav::begin_pick`].
    ImageStart,
    /// Close the image viewer.
    CloseView,
    /// Pass a special key to the page.
    Key(&'static str, u32),
    /// Type text into the page.
    Text(String),
}

pub struct Nav {
    pub mode: Mode,
    prefix: String,
    /// Hint count and label width, valid in Hint mode.
    n: usize,
    len: usize,
    pending_g: bool,
    /// What the labels in Hint mode are on: "LINKS" or "IMAGES".
    what: &'static str,
}

/// Label width for `n` hints: the smallest L with 9^L >= n (at least 1).
pub fn label_len(n: usize) -> usize {
    let (mut len, mut cap) = (1, ALPHABET.len());
    while cap < n {
        len += 1;
        cap *= ALPHABET.len();
    }
    len
}

/// Label of hint `i`, `len` letters wide.
pub fn label(mut i: usize, len: usize) -> String {
    let mut out = vec![b'a'; len];
    for slot in out.iter_mut().rev() {
        *slot = ALPHABET[i % ALPHABET.len()];
        i /= ALPHABET.len();
    }
    String::from_utf8(out).unwrap_or_default()
}

impl Nav {
    pub fn new() -> Nav {
        Nav { mode: Mode::Normal, prefix: String::new(), n: 0, len: 1, pending_g: false, what: "LINKS" }
    }

    /// The status-line text for the current state (kami draws it itself, on the
    /// framebuffer; see `bar.rs`).
    pub fn status(&self) -> String {
        match self.mode {
            Mode::Normal => "NORMAL  j/k scroll  f links  v images  i type  H/L back/fwd  Ctrl-Q quit".into(),
            Mode::Insert => "INSERT  Esc to leave".into(),
            Mode::Hint => format!("{}  {}", self.what, self.prefix),
            Mode::View => "IMAGE  Esc/q/v close".into(),
        }
    }

    /// The caller found `n` images on screen after [`Action::ImageStart`]:
    /// the same labels as links, and the same [`Action::HintClick`].
    pub fn begin_pick(&mut self, n: usize) {
        self.begin_hints(n);
        self.what = "IMAGES";
    }

    /// An image is open: every key but the ones that close it is swallowed.
    pub fn begin_view(&mut self) {
        self.mode = Mode::View;
        self.prefix.clear();
    }

    /// The caller found `n` clickable elements after [`Action::HintStart`].
    pub fn begin_hints(&mut self, n: usize) {
        self.what = "LINKS";
        self.prefix.clear();
        self.pending_g = false;
        if n == 0 {
            self.mode = Mode::Normal;
        } else {
            self.mode = Mode::Hint;
            self.n = n;
            self.len = label_len(n);
        }
    }

    /// Does any label start with `prefix` (already `<= len` letters)?
    fn any_label_with(&self, prefix: &str) -> bool {
        let k = prefix.len();
        let p = prefix.bytes().fold(0usize, |a, b| {
            a * ALPHABET.len() + ALPHABET.iter().position(|&c| c == b).unwrap_or(0)
        });
        let span = ALPHABET.len().pow((self.len - k) as u32);
        p * span < self.n
    }

    pub fn feed(&mut self, input: &Input) -> Vec<Action> {
        let mut out = Vec::new();
        match input {
            Input::Quit => out.push(Action::Quit),
            Input::Reload => out.push(Action::Reload),
            Input::Key(name, code) => match (self.mode, *name) {
                (Mode::Insert, "Escape") => {
                    self.mode = Mode::Normal;
                    out.push(Action::Blur);
                }
                (Mode::Hint, "Escape") => {
                    self.mode = Mode::Normal;
                    self.prefix.clear();
                    out.push(Action::HintCancel);
                }
                (Mode::Hint, "Backspace") => {
                    self.prefix.pop();
                    out.push(Action::HintFilter(self.prefix.clone()));
                }
                (Mode::Hint, _) => {}
                (Mode::View, "Escape") => {
                    self.mode = Mode::Normal;
                    out.push(Action::CloseView);
                }
                (Mode::View, _) => {}
                _ => out.push(Action::Key(name, *code)),
            },
            Input::Text(t) => match self.mode {
                Mode::Insert => out.push(Action::Text(t.clone())),
                Mode::Hint => self.hint_text(t, &mut out),
                Mode::Normal => self.normal_text(t, &mut out),
                Mode::View => {
                    if t.contains(['q', 'v']) {
                        self.mode = Mode::Normal;
                        out.push(Action::CloseView);
                    }
                }
            },
        }
        out
    }

    fn hint_text(&mut self, t: &str, out: &mut Vec<Action>) {
        for ch in t.chars() {
            let ch = ch.to_ascii_lowercase();
            if !ch.is_ascii() || !ALPHABET.contains(&(ch as u8)) {
                continue;
            }
            self.prefix.push(ch);
            if !self.any_label_with(&self.prefix) {
                self.prefix.pop();
                continue;
            }
            if self.prefix.len() == self.len {
                let idx = self.prefix.bytes().fold(0usize, |a, b| {
                    a * ALPHABET.len() + ALPHABET.iter().position(|&c| c == b).unwrap_or(0)
                });
                self.mode = Mode::Normal;
                self.prefix.clear();
                out.push(Action::HintClick(idx));
                return;
            }
            out.push(Action::HintFilter(self.prefix.clone()));
        }
    }

    fn normal_text(&mut self, t: &str, out: &mut Vec<Action>) {
        for ch in t.chars() {
            let was_g = std::mem::take(&mut self.pending_g);
            match ch {
                'j' => out.push(Action::Scroll(Scroll::Line(1))),
                'k' => out.push(Action::Scroll(Scroll::Line(-1))),
                'd' => out.push(Action::Scroll(Scroll::Half(1))),
                'u' => out.push(Action::Scroll(Scroll::Half(-1))),
                'g' if was_g => out.push(Action::Scroll(Scroll::Edge(-1))),
                'g' => self.pending_g = true,
                'G' => out.push(Action::Scroll(Scroll::Edge(1))),
                'H' => out.push(Action::Back),
                'L' => out.push(Action::Forward),
                'r' => out.push(Action::Reload),
                'i' => self.mode = Mode::Insert,
                'f' => {
                    // The rest of the chunk would be typed before the hints
                    // exist; drop it.
                    out.push(Action::HintStart);
                    return;
                }
                'v' => {
                    out.push(Action::ImageStart);
                    return;
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Input {
        Input::Text(s.into())
    }

    #[test]
    fn labels_are_fixed_width_and_unique() {
        assert_eq!(label_len(1), 1);
        assert_eq!(label_len(9), 1);
        assert_eq!(label_len(10), 2);
        assert_eq!(label_len(81), 2);
        assert_eq!(label_len(82), 3);
        assert_eq!(label(0, 1), "a");
        assert_eq!(label(8, 1), "l");
        assert_eq!(label(9, 2), "sa");
        let all: std::collections::HashSet<_> = (0..300).map(|i| label(i, 3)).collect();
        assert_eq!(all.len(), 300);
    }

    #[test]
    fn normal_mode_scrolls_and_swallows_text() {
        let mut n = Nav::new();
        assert_eq!(n.feed(&text("jk")), vec![Action::Scroll(Scroll::Line(1)), Action::Scroll(Scroll::Line(-1))]);
        assert_eq!(n.feed(&text("xyz")), vec![]);
        assert_eq!(n.feed(&text("gg")), vec![Action::Scroll(Scroll::Edge(-1))]);
        assert_eq!(n.feed(&text("gjg")), vec![Action::Scroll(Scroll::Line(1))]);
        assert_eq!(n.feed(&text("G")), vec![Action::Scroll(Scroll::Edge(1))]);
        assert_eq!(n.feed(&Input::Key("ArrowDown", 40)), vec![Action::Key("ArrowDown", 40)]);
        assert_eq!(n.feed(&Input::Key("Escape", 27)), vec![Action::Key("Escape", 27)]);
    }

    #[test]
    fn v_picks_an_image_and_the_viewer_swallows_keys_until_closed() {
        let mut n = Nav::new();
        assert_eq!(n.feed(&text("vj")), vec![Action::ImageStart]);
        n.begin_pick(3);
        assert!(n.status().starts_with("IMAGES"));
        assert_eq!(n.feed(&text("s")), vec![Action::HintClick(1)]);
        n.begin_view();
        assert_eq!(n.feed(&text("jk")), vec![]);
        assert_eq!(n.feed(&Input::Key("ArrowDown", 40)), vec![]);
        assert_eq!(n.feed(&text("q")), vec![Action::CloseView]);
        assert_eq!(n.mode, Mode::Normal);
        n.begin_view();
        assert_eq!(n.feed(&Input::Key("Escape", 27)), vec![Action::CloseView]);
        n.begin_hints(2);
        assert!(n.status().starts_with("LINKS"), "f after v is links again");
    }

    #[test]
    fn insert_mode_passes_everything_until_escape() {
        let mut n = Nav::new();
        assert_eq!(n.feed(&text("i")), vec![]);
        assert_eq!(n.mode, Mode::Insert);
        assert_eq!(n.feed(&text("jf")), vec![Action::Text("jf".into())]);
        assert_eq!(n.feed(&Input::Key("Enter", 13)), vec![Action::Key("Enter", 13)]);
        assert_eq!(n.feed(&Input::Key("Escape", 27)), vec![Action::Blur]);
        assert_eq!(n.mode, Mode::Normal);
    }

    #[test]
    fn hints_filter_then_click() {
        let mut n = Nav::new();
        assert_eq!(n.feed(&text("fj")), vec![Action::HintStart]);
        n.begin_hints(20); // two letters wide: labels aa..
        assert_eq!(n.mode, Mode::Hint);
        assert_eq!(n.feed(&text("s")), vec![Action::HintFilter("s".into())]);
        // 20 hints: "sa".."sd" is 9..=13, "sg" would be 15 - exists; "sl" 17, but
        // the third row "da".."dc" start at 18; "sl" = 9+8 = 17 < 20 exists.
        assert_eq!(n.feed(&text("a")), vec![Action::HintClick(9)]);
        assert_eq!(n.mode, Mode::Normal);
    }

    #[test]
    fn hint_letters_with_no_label_are_ignored() {
        let mut n = Nav::new();
        n.begin_hints(11); // 2 wide: aa..al (0..8), sa, ss (9, 10)
        assert_eq!(n.feed(&text("d")), vec![]); // "d.." starts at 18 > 11
        assert_eq!(n.prefix_for_test(), "");
        assert_eq!(n.feed(&text("s")), vec![Action::HintFilter("s".into())]);
        assert_eq!(n.feed(&text("d")), vec![]); // "sd" = 12 >= 11
        assert_eq!(n.feed(&text("S")), vec![Action::HintClick(10)]); // uppercase accepted
    }

    #[test]
    fn hint_escape_backspace_and_empty() {
        let mut n = Nav::new();
        n.begin_hints(0);
        assert_eq!(n.mode, Mode::Normal);
        n.begin_hints(30);
        n.feed(&text("a"));
        assert_eq!(n.feed(&Input::Key("Backspace", 8)), vec![Action::HintFilter(String::new())]);
        assert_eq!(n.feed(&Input::Key("Escape", 27)), vec![Action::HintCancel]);
        assert_eq!(n.mode, Mode::Normal);
    }

    #[test]
    fn quit_and_reload_work_in_every_mode() {
        let mut n = Nav::new();
        n.begin_hints(5);
        assert_eq!(n.feed(&Input::Quit), vec![Action::Quit]);
        assert_eq!(n.feed(&Input::Reload), vec![Action::Reload]);
    }

    #[test]
    fn f_drops_the_rest_of_its_chunk() {
        let mut n = Nav::new();
        assert_eq!(n.feed(&text("fjj")), vec![Action::HintStart]);
    }

    impl Nav {
        fn prefix_for_test(&self) -> &str {
            &self.prefix
        }
    }
}
