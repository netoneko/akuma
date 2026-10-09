//! Image pixels for the terminal: half blocks.
//!
//! A cell shows two pixels, one above the other, as `▀` with the upper one as
//! the foreground colour and the lower one as the background. They come from a
//! small `Page.captureScreenshot` of the page's viewport (the page is scrolled
//! to match the view), taken after each snapshot. Every image slot the capture
//! covers is sampled into a per-image cache of cell colours, keyed by the
//! image's source and box, so pixels survive scrolling and relayout, and a slot
//! the capture only partly covered keeps what it has and fills in later.

use std::collections::HashMap;

use super::grid::Grid;
use super::page::Rgb;
use crate::machine::Clip;

/// Cell colours of one image: `(upper, lower)` per cell, row-major over the
/// slot's rows and columns; `None` where no capture has covered it yet.
#[derive(Debug)]
pub struct Block {
    rows: usize,
    cols: usize,
    cells: Vec<Option<(Rgb, Rgb)>>,
}

impl Block {
    pub fn get(&self, r: usize, c: usize) -> Option<(Rgb, Rgb)> {
        if r < self.rows && c < self.cols {
            self.cells[r * self.cols + c]
        } else {
            None
        }
    }
}

/// A decoded capture: `ch`-channel 8-bit pixels, `w` x `h`.
pub struct Pixels<'a> {
    pub px: &'a [u8],
    pub w: usize,
    pub h: usize,
    pub ch: usize,
}

impl Pixels<'_> {
    /// The mean colour of the capture pixels under document area
    /// `[x0, x1) x [y0, y1)`, if that area lies inside the capture.
    fn mean(&self, clip: &Clip, x0: f64, x1: f64, y0: f64, y1: f64) -> Option<Rgb> {
        if y0 < clip.y - 0.5 || y1 > clip.y + clip.h + 0.5 || x1 <= clip.x || x0 >= clip.x + clip.w {
            return None;
        }
        let s = clip.scale;
        let px = |v: f64, max: usize| (v.max(0.0) as usize).min(max);
        let (ax, bx) = (px(((x0 - clip.x) * s).floor(), self.w - 1), px(((x1 - clip.x) * s).ceil(), self.w));
        let (ay, by) = (px(((y0 - clip.y) * s).floor(), self.h - 1), px(((y1 - clip.y) * s).ceil(), self.h));
        let (bx, by) = (bx.max(ax + 1), by.max(ay + 1));
        let (mut sum, mut n) = ([0u64; 3], 0u64);
        for y in ay..by {
            for x in ax..bx {
                let i = (y * self.w + x) * self.ch;
                for (k, acc) in sum.iter_mut().enumerate() {
                    *acc += self.px[i + k] as u64;
                }
                n += 1;
            }
        }
        let m = |k: usize| (sum[k] / n.max(1)) as u8;
        Some(Rgb(m(0), m(1), m(2)))
    }
}

#[derive(Default)]
pub struct Cache {
    blocks: HashMap<u64, Block>,
}

impl Cache {
    pub fn get(&self, key: u64) -> Option<&Block> {
        self.blocks.get(&key)
    }

    /// Does any slot partly in document rows `rows` still lack pixels?
    pub fn missing(&self, grid: &Grid, rows: std::ops::Range<usize>) -> bool {
        grid.images.iter().filter(|s| s.rows.start < rows.end && rows.start < s.rows.end).any(|s| {
            self.blocks.get(&s.key).is_none_or(|b| {
                (s.rows.start.max(rows.start)..s.rows.end.min(rows.end))
                    .any(|r| (0..b.cols).any(|c| b.get(r - s.rows.start, c).is_none()))
            })
        })
    }

    /// Sample a capture of `clip` into every slot of `grid` it covers, and
    /// forget images that are no longer on the page.
    pub fn absorb(&mut self, grid: &Grid, shot: &Pixels, clip: &Clip) {
        self.blocks.retain(|k, _| grid.images.iter().any(|s| s.key == *k));
        if shot.w == 0 || shot.h == 0 || shot.ch < 3 {
            return;
        }
        for slot in &grid.images {
            let (rows, cols) = (slot.rows.len(), slot.cols.len());
            let block = self.blocks.entry(slot.key).or_insert_with(|| Block { rows, cols, cells: vec![None; rows * cols] });
            if block.rows != rows || block.cols != cols {
                *block = Block { rows, cols, cells: vec![None; rows * cols] };
            }
            let (top, bottom) = (slot.rect.y, slot.rect.y + slot.rect.h);
            for (i, r) in slot.rows.clone().enumerate() {
                let (y0, y1) = grid.band(r);
                let (y0, y1) = (y0.max(top), y1.min(bottom));
                if y1 <= y0 {
                    continue;
                }
                let mid = (y0 + y1) / 2.0;
                for (j, c) in slot.cols.clone().enumerate() {
                    let x0 = (c as f64 * grid.cw).max(slot.rect.x);
                    let x1 = ((c + 1) as f64 * grid.cw).min(slot.rect.x + slot.rect.w).max(x0 + 0.5);
                    if let (Some(up), Some(down)) = (shot.mean(clip, x0, x1, y0, mid), shot.mean(clip, x0, x1, mid, y1)) {
                        block.cells[i * cols + j] = Some((up, down));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::grid::build;
    use super::super::page::{Item, Kind, Layer, Page, Rect};
    use super::*;

    fn image(y: f64, h: f64) -> Item {
        Item {
            rect: Rect { x: 0.0, y, w: 40.0, h },
            text: String::new(),
            kind: Kind::Image,
            layer: Layer::Flow,
            fg: None,
            bg: None,
            bold: false,
            italic: false,
            underline: false,
            link: false,
            dim: false,
            size: 16.0,
            key: 7,
            backdrop: false,
            order: 0,
        }
    }

    /// A 40 x 76 px image (4 rows of 19 px, 4 columns of 10 px), captured at
    /// scale 1: red above y=38, blue below.
    #[test]
    fn half_blocks_sample_the_upper_and_lower_half_of_each_cell() {
        let page = Page { items: vec![image(0.0, 76.0)], ..Page::default() };
        let g = build(&page, 4, 10, 10.0);
        assert_eq!(g.images[0].rows, 0..4);
        let (w, h) = (40, 76);
        let mut px = vec![0u8; w * h * 3];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 3;
                if y < 38 { px[i] = 255 } else { px[i + 2] = 255 }
            }
        }
        let clip = Clip { x: 0.0, y: 0.0, w: 40.0, h: 76.0, scale: 1.0 };
        let mut cache = Cache::default();
        cache.absorb(&g, &Pixels { px: &px, w, h, ch: 3 }, &clip);
        let b = cache.get(7).unwrap();
        let (red, blue) = (Rgb(255, 0, 0), Rgb(0, 0, 255));
        assert_eq!(b.get(0, 0), Some((red, red)));
        assert_eq!(b.get(1, 3), Some((red, red)), "row 1 is y 19..38");
        assert_eq!(b.get(2, 0), Some((blue, blue)));
        assert!(!cache.missing(&g, 0..4));
    }

    #[test]
    fn a_partial_capture_fills_what_it_covers_and_the_rest_later() {
        let page = Page { items: vec![image(0.0, 76.0)], ..Page::default() };
        let g = build(&page, 4, 10, 10.0);
        let px = vec![200u8; 40 * 38 * 3];
        // Only the top half of the image was in the viewport.
        let clip = Clip { x: 0.0, y: 0.0, w: 40.0, h: 38.0, scale: 1.0 };
        let mut cache = Cache::default();
        cache.absorb(&g, &Pixels { px: &px, w: 40, h: 38, ch: 3 }, &clip);
        let b = cache.get(7).unwrap();
        assert!(b.get(1, 0).is_some() && b.get(2, 0).is_none());
        assert!(cache.missing(&g, 0..4) && !cache.missing(&g, 0..2));
        // The next capture, scrolled down, completes it.
        let clip = Clip { x: 0.0, y: 38.0, w: 40.0, h: 38.0, scale: 1.0 };
        cache.absorb(&g, &Pixels { px: &px, w: 40, h: 38, ch: 3 }, &clip);
        assert!(!cache.missing(&g, 0..4));
    }
}
