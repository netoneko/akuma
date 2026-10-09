//! The kitty graphics protocol (kitty, Ghostty, WezTerm): real pixels in the
//! terminal.
//!
//! Supported by kitty, Ghostty, WezTerm and rio (whose CPU renderer, the
//! one on the Akuma framebuffer, draws them too).
//!
//! An image is transmitted once, as the PNG Chromium captured (`f=100`: the
//! terminal decodes it, kami does not), under a numeric id; every frame then
//! places the visible ones at their cells (`a=p`), cropped to what is on
//! screen, under the text (`z=-1`) so captions and hint labels stay readable.
//! Placements are all dropped and redone whenever the view changes, which is
//! simpler than tracking them and cheap: only the image data is large, and it
//! is sent once. Nothing here goes through ratatui; the shell writes it after
//! each draw.
//!
//! <https://sw.kovidgoyal.net/kitty/graphics-protocol/>

use std::fmt::Write as _;

/// Base64 payload bytes per escape sequence (the protocol's limit is 4096).
const CHUNK: usize = 4096;

/// Does the environment name a terminal that speaks the protocol? Only
/// `TERM` crosses ssh, and rio falls back to `xterm-256color` where there is
/// no `xterm-rio` terminfo, so a `false` here is not the last word: see
/// [`probe`].
pub fn named() -> bool {
    let var = |k: &str| std::env::var(k).unwrap_or_default();
    let term = var("TERM");
    let program = var("TERM_PROGRAM").to_ascii_lowercase();
    ["kitty", "ghostty", "rio"].iter().any(|t| term.contains(t))
        || ["ghostty", "wezterm", "rio", "kitty"].contains(&program.as_str())
        || std::env::var_os("KITTY_WINDOW_ID").is_some()
}

/// The question: a 1x1 image query (kitty's own detection recipe), then
/// Primary Device Attributes, which every terminal answers. A terminal with
/// the protocol answers the query first.
pub const PROBE: &str = "\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[c";

/// What the answers so far say: `Some(true)` the query was answered OK,
/// `Some(false)` the device attributes came without it, `None` not yet.
pub fn probe_answer(buf: &[u8]) -> Option<bool> {
    let has = |needle: &[u8]| buf.windows(needle.len()).any(|w| w == needle);
    if has(b"\x1b_Gi=31;OK") {
        return Some(true);
    }
    // ESC [ ? ... c
    let da = buf.windows(3).position(|w| w == b"\x1b[?").is_some_and(|i| buf[i..].contains(&b'c'));
    if da { Some(false) } else { None }
}

/// Ask the terminal on the tty (raw mode already on, nothing else reading
/// it yet). Waits at most `budget_ms` for an answer, so a terminal that says
/// nothing at all costs that and no more.
pub fn probe(budget_ms: u64) -> bool {
    use std::io::Write;
    let mut out = std::io::stdout();
    if out.write_all(PROBE.as_bytes()).and_then(|_| out.flush()).is_err() {
        return false;
    }
    let start = std::time::Instant::now();
    let mut buf = Vec::new();
    loop {
        let left = budget_ms.saturating_sub(start.elapsed().as_millis() as u64);
        if left == 0 {
            return false;
        }
        let mut fd = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd.
        if unsafe { libc::poll(&mut fd, 1, left as i32) } <= 0 {
            return false;
        }
        let mut chunk = [0u8; 256];
        // SAFETY: reading into a stack buffer of the stated size.
        let n = unsafe { libc::read(0, chunk.as_mut_ptr() as *mut _, chunk.len()) };
        if n <= 0 {
            return false;
        }
        buf.extend_from_slice(&chunk[..n as usize]);
        if let Some(yes) = probe_answer(&buf) {
            return yes;
        }
    }
}

/// Transmit a base64 PNG as image `id` (replacing any image with that id).
pub fn transmit(out: &mut String, id: u32, png_b64: &str) {
    let b = png_b64.as_bytes();
    let n = b.len().div_ceil(CHUNK).max(1);
    for (i, chunk) in b.chunks(CHUNK).enumerate() {
        let more = (i + 1 < n) as u8;
        let chunk = std::str::from_utf8(chunk).unwrap_or("");
        if i == 0 {
            let _ = write!(out, "\x1b_Ga=t,f=100,t=d,i={id},q=2,m={more};{chunk}\x1b\\");
        } else {
            let _ = write!(out, "\x1b_Gm={more};{chunk}\x1b\\");
        }
    }
}

/// Where and how much of an image to show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    pub id: u32,
    /// Screen cell of the top-left corner (0-based).
    pub col: u16,
    pub row: u16,
    /// Size on screen, in cells.
    pub cols: u16,
    pub rows: u16,
    /// The part of the image shown, in image pixels: `(x, y, w, h)`.
    pub crop: (u32, u32, u32, u32),
}

pub fn place(out: &mut String, p: &Placement) {
    let (x, y, w, h) = p.crop;
    let _ = write!(
        out,
        "\x1b[{};{}H\x1b_Ga=p,i={},p=1,c={},r={},x={x},y={y},w={w},h={h},C=1,z=-1,q=2\x1b\\",
        p.row + 1,
        p.col + 1,
        p.id,
        p.cols,
        p.rows
    );
}

/// Drop every placement on screen; the image data stays for the next frame.
pub fn clear(out: &mut String) {
    out.push_str("\x1b_Ga=d,d=a,q=2\x1b\\");
}

/// Free image `id`'s data and placements.
pub fn free(out: &mut String, id: u32) {
    let _ = write!(out, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\");
}

/// The pixel size of a PNG, from its IHDR.
pub fn png_size(png: &[u8]) -> Option<(u32, u32)> {
    if png.len() < 24 || &png[..8] != b"\x89PNG\r\n\x1a\n" || &png[12..16] != b"IHDR" {
        return None;
    }
    let be = |i: usize| u32::from_be_bytes([png[i], png[i + 1], png[i + 2], png[i + 3]]);
    Some((be(16), be(20)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transmission_is_chunked_with_the_header_on_the_first_chunk_only() {
        let data = "A".repeat(CHUNK * 2 + 10);
        let mut out = String::new();
        transmit(&mut out, 7, &data);
        let parts: Vec<&str> = out.split("\x1b\\").filter(|s| !s.is_empty()).collect();
        assert_eq!(parts.len(), 3);
        assert!(parts[0].starts_with("\x1b_Ga=t,f=100,t=d,i=7,q=2,m=1;"));
        assert!(parts[1].starts_with("\x1b_Gm=1;"));
        assert!(parts[2].starts_with("\x1b_Gm=0;"));
        let payload: usize = parts.iter().map(|p| p.split(';').nth(1).unwrap().len()).sum();
        assert_eq!(payload, data.len());
    }

    #[test]
    fn a_small_image_is_one_chunk_with_m0() {
        let mut out = String::new();
        transmit(&mut out, 1, "QUJD");
        assert_eq!(out, "\x1b_Ga=t,f=100,t=d,i=1,q=2,m=0;QUJD\x1b\\");
    }

    #[test]
    fn placement_moves_the_cursor_and_crops() {
        let mut out = String::new();
        place(&mut out, &Placement { id: 3, col: 4, row: 2, cols: 10, rows: 5, crop: (0, 20, 100, 50) });
        assert_eq!(out, "\x1b[3;5H\x1b_Ga=p,i=3,p=1,c=10,r=5,x=0,y=20,w=100,h=50,C=1,z=-1,q=2\x1b\\");
    }

    #[test]
    fn the_probe_answer_is_ok_before_the_device_attributes_or_nothing() {
        // kitty / Ghostty / rio: the graphics reply, then DA1.
        assert_eq!(probe_answer(b"\x1b_Gi=31;OK\x1b\\\x1b[?62;22c"), Some(true));
        // A terminal without the protocol: DA1 alone.
        assert_eq!(probe_answer(b"\x1b[?1;2c"), Some(false));
        // Half an answer: keep reading.
        assert_eq!(probe_answer(b"\x1b_Gi=3"), None);
        assert_eq!(probe_answer(b"\x1b[?62;2"), None);
    }

    #[test]
    fn png_size_reads_the_header() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&640u32.to_be_bytes());
        png.extend_from_slice(&480u32.to_be_bytes());
        assert_eq!(png_size(&png), Some((640, 480)));
        assert_eq!(png_size(b"GIF89a"), None);
    }
}
