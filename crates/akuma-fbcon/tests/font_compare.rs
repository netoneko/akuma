//! Renders the same sample — code, prose, and the symbols Claude Code and herdr draw — in gray
//! and in subpixel text, and writes a 3x-zoomed PNG to look at. Not an assertion.
//!   AKUMA_FBCON_DUMP=/dir AKUMA_FBCON_TAG=name cargo test -p akuma-fbcon --test font_compare -- --ignored
use akuma_fbcon::{Console, Rgb, Smooth, Surface};

struct Mem(usize, usize, Vec<Rgb>);
impl Surface for Mem {
    fn width(&self) -> usize { self.0 }
    fn height(&self) -> usize { self.1 }
    fn put(&mut self, x: usize, y: usize, c: Rgb) { if x < self.0 && y < self.1 { self.2[y * self.0 + x] = c; } }
}

fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in data { c ^= u32::from(b); for _ in 0..8 { c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 }; } }
    !c
}
fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    let mut c = kind.to_vec(); c.extend_from_slice(body);
    out.extend_from_slice(&c); out.extend_from_slice(&crc32(&c).to_be_bytes());
}
fn png(path: &str, w: usize, h: usize, rgb: &[u8]) {
    let mut raw = Vec::with_capacity((w * 3 + 1) * h);
    for y in 0..h { raw.push(0); raw.extend_from_slice(&rgb[y * w * 3..(y + 1) * w * 3]); }
    let mut z = vec![0x78, 0x01];
    let mut chunks = raw.chunks(65535).peekable();
    while let Some(c) = chunks.next() {
        z.push(u8::from(chunks.peek().is_none()));
        z.extend_from_slice(&(c.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(c.len() as u16)).to_le_bytes());
        z.extend_from_slice(c);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in &raw { a = (a + u32::from(x)) % 65521; b = (b + a) % 65521; }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(w as u32).to_be_bytes()); ihdr.extend_from_slice(&(h as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr); chunk(&mut out, b"IDAT", &z); chunk(&mut out, b"IEND", &[]);
    std::fs::write(path, out).unwrap();
}

const SAMPLE: &str = "fn main() { let s = \"Hello, Akuma\"; println!(\"{s}\"); }\r\n\
The quick brown fox jumps over the lazy dog 0123456789 @#&%\r\n\
\u{23FA} \u{273B} \u{273D} \u{2736} \u{23BF} \u{276F} \u{279C} \u{2197} \u{21D2} \u{25D0} \u{26A0} \u{2605} \u{2217} \u{0531}\u{2002}x\r\n";

fn render(height: usize, smooth: Smooth) -> Mem {
    let (w, h) = (800usize, 90usize);
    let mut con = Console::new(Mem(w, h, vec![Rgb::BLACK; w * h])).unwrap();
    con.apply_config(&akuma_fbcon::config::ConsoleConfig::parse(&format!("font = {height}\nmargin = 0")));
    con.set_smooth(smooth);
    con.clear();
    con.write_str_bytes(SAMPLE);
    con.into_surface()
}

#[test]
#[ignore = "writes images; run on demand"]
fn font_compare() {
    let Ok(dir) = std::env::var("AKUMA_FBCON_DUMP") else { return };
    let tag = std::env::var("AKUMA_FBCON_TAG").unwrap_or_else(|_| "x".into());
    std::fs::create_dir_all(&dir).unwrap();
    // 1:1 crop (first 330 px) of each cut, then 4x nearest zoom of the left 330 px.
    let zoom = 3usize;
    let (cw, ch) = (300usize, 66usize);
    let mut rows: Vec<u8> = Vec::new();
    for (height, smooth) in [(20usize, Smooth::Gray), (20, Smooth::Rgb)] {
        let m = render(height, smooth);
        for y in 0..ch {
            for _ in 0..zoom {
                for x in 0..cw { let c = m.2[y * m.0 + x]; for _ in 0..zoom { rows.extend_from_slice(&[c.r, c.g, c.b]); } }
            }
        }
        // separator
        for _ in 0..4 { for _ in 0..cw * zoom { rows.extend_from_slice(&[80, 20, 20]); } }
    }
    let h = rows.len() / (cw * zoom * 3);
    png(&format!("{dir}/font-{tag}.png"), cw * zoom, h, &rows);
}
