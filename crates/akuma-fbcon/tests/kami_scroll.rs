//! What a `kami tui` scroll costs the console, in surface writes.
//!
//! `KAMI_STREAM=<file> cargo test -p akuma-fbcon --test kami_scroll -- --ignored --nocapture`
//! replays a captured terminal stream (`userspace/kami/probe/tui_scroll_ssh.py --out`)
//! split at its `=====SCROLL=====` marker and prints the pixel writes the scroll
//! phase made. Pixel writes to write-combined video memory are the cost that
//! matters; the host time printed is only for comparison between changes.

use akuma_fbcon::{Console, Rgb, Surface};
use std::fmt::Write;

struct Counting {
    w: usize,
    h: usize,
    writes: usize,
}

impl Surface for Counting {
    fn width(&self) -> usize {
        self.w
    }
    fn height(&self) -> usize {
        self.h
    }
    fn put(&mut self, _: usize, _: usize, _: Rgb) {
        self.writes += 1;
    }
    fn fill(&mut self, _: usize, _: usize, w: usize, h: usize, _: Rgb) {
        self.writes += w * h;
    }
}

#[test]
#[ignore]
fn scroll_phase_cost() {
    let path = std::env::var("KAMI_STREAM").expect("KAMI_STREAM=<file>");
    let data = std::fs::read(path).unwrap();
    let text = String::from_utf8_lossy(&data).into_owned();
    let (pre, post) = text.split_once("=====SCROLL=====").expect("marker");
    let (w, h): (usize, usize) = (
        std::env::var("W").ok().and_then(|v| v.parse().ok()).unwrap_or(1920),
        std::env::var("H").ok().and_then(|v| v.parse().ok()).unwrap_or(1200),
    );
    let mut c = Console::new(Counting { w, h, writes: 0 }).unwrap();
    c.write_str(pre).unwrap();
    let before = c.surface_mut().writes;
    let t = std::time::Instant::now();
    c.write_str(post).unwrap();
    let writes = c.surface_mut().writes - before;
    println!("scroll phase: {} KB in, {} pixel writes ({:.1} MB at 4 B), host {:?}", post.len() / 1024, writes, writes as f64 * 4.0 / 1e6, t.elapsed());
}
