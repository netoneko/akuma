//! Where kami puts the page and its status line.
//!
//! The session logic talks to a [`Display`], never to `/dev/fb0` directly, so
//! a second output can sit beside the framebuffer one without touching it. The
//! planned one is a terminal pane (rio): frames go out as an inline-image
//! protocol on stdout, sized from `TIOCGWINSZ`, and the status is an ordinary
//! text line, with no pixels of its own. See "Future: kami in a rio split pane"
//! in the README.

/// The smallest page kami will ask Chromium for when choosing a scale itself.
pub const MIN_PAGE: (usize, usize) = (1280, 720);

/// The largest integer `scale` that still leaves a page of at least
/// [`MIN_PAGE`] on a `screen` (status bar already taken out of the height).
/// A 1920x1176 area gives 1 (a 1920x1176 page); a 4K panel gives 2 (1920x1068),
/// where the old fixed default of 2 would have left a 960x588 page on the
/// 1920x1200 laptop. Capped at 2, the largest scale tried (a 4K television);
/// beyond that the pixels get too blocky to read.
pub fn auto_scale(screen: (usize, usize)) -> usize {
    let mut s = 1;
    while s < 2 && screen.0 / (s + 1) >= MIN_PAGE.0 && screen.1 / (s + 1) >= MIN_PAGE.1 {
        s += 1;
    }
    s
}

pub trait Display {
    /// Pixels of the whole screen minus the status line: what the page may
    /// use before any scaling.
    fn screen_size(&self) -> (usize, usize);

    /// Pixel size of the area the page occupies at `scale`x, status line
    /// excluded: the viewport kami asks Chromium for.
    fn page_size(&self, scale: usize) -> (usize, usize);

    /// Show an RGB/RGBA image (`ch` channels), enlarged `scale`x, at the top
    /// left of the page area.
    fn blit(&mut self, px: &[u8], w: usize, h: usize, ch: usize, scale: usize);

    /// Show the status line. Called whenever the text may have changed; an
    /// implementation should skip the work if it did not.
    fn status(&mut self, text: &str);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_follows_the_screen() {
        assert_eq!(auto_scale((1920, 1176)), 1, "ryzen: the full screen, not a quarter of it");
        assert_eq!(auto_scale((3840, 2136)), 2, "a 4K panel");
        assert_eq!(auto_scale((1366, 744)), 1);
        assert_eq!(auto_scale((1280, 720)), 1);
        assert_eq!(auto_scale((640, 480)), 1, "a screen below the minimum still gets 1");
        assert_eq!(auto_scale((7680, 4296)), 2, "capped: 8K would be blocky");
    }
}

/// A display that shows nothing: for runs on a machine whose screen someone is
/// using (`--fb none`). Frames still go through the decoder and `KAMI_DUMP`;
/// the status line goes to the input log through `on_status`.
pub struct Null {
    pub size: (usize, usize),
    pub last: String,
}

impl Display for Null {
    fn screen_size(&self) -> (usize, usize) {
        self.size
    }

    fn page_size(&self, scale: usize) -> (usize, usize) {
        (self.size.0 / scale, self.size.1 / scale)
    }

    fn blit(&mut self, _px: &[u8], _w: usize, _h: usize, _ch: usize, _scale: usize) {}

    fn status(&mut self, text: &str) {
        self.last = text.to_string();
    }
}
