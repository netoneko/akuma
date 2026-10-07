//! Just enough PNG for Chromium's screencast frames: 8-bit RGB or RGBA,
//! non-interlaced. Inflate is miniz_oxide; the scanline filters are here.
//!
//! Buffers are owned by the decoder and reused frame to frame, so a steady
//! screencast allocates nothing after the first frame at a given size.

use miniz_oxide::inflate::core::{decompress, inflate_flags, DecompressorOxide};
use miniz_oxide::inflate::TINFLStatus;

const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

#[derive(Default)]
pub struct Decoder {
    idat: Vec<u8>,
    raw: Vec<u8>,
    zero_row: Vec<u8>,
    inflate: Option<Box<DecompressorOxide>>,
    /// Unfiltered pixels, `height` rows of `width * channels` bytes.
    pub pixels: Vec<u8>,
    pub width: usize,
    pub height: usize,
    pub channels: usize,
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

impl Decoder {
    pub fn decode(&mut self, data: &[u8]) -> Result<(), String> {
        if data.len() < 8 || data[..8] != SIGNATURE {
            return Err("not a PNG".into());
        }
        self.idat.clear();
        let mut header = None;
        let mut pos = 8;
        while pos + 12 <= data.len() {
            let len = be32(&data[pos..]) as usize;
            let kind = &data[pos + 4..pos + 8];
            let body = data
                .get(pos + 8..pos + 8 + len)
                .ok_or("truncated chunk")?;
            match kind {
                b"IHDR" => {
                    if body.len() < 13 {
                        return Err("short IHDR".into());
                    }
                    let (w, h) = (be32(body) as usize, be32(&body[4..]) as usize);
                    let (depth, color, interlace) = (body[8], body[9], body[12]);
                    let channels = match color {
                        2 => 3,
                        6 => 4,
                        _ => return Err(format!("colour type {color} unsupported")),
                    };
                    if depth != 8 || interlace != 0 {
                        return Err(format!("depth {depth} interlace {interlace} unsupported"));
                    }
                    header = Some((w, h, channels));
                }
                b"IDAT" => self.idat.extend_from_slice(body),
                b"IEND" => break,
                _ => {}
            }
            pos += 12 + len; // length + type + body + crc
        }
        let (w, h, ch) = header.ok_or("no IHDR")?;
        let stride = w * ch;
        let raw_len = h * (stride + 1);
        self.raw.resize(raw_len, 0);

        let r = self.inflate.get_or_insert_with(Box::default);
        r.init();
        let flags = inflate_flags::TINFL_FLAG_PARSE_ZLIB_HEADER
            | inflate_flags::TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF;
        let (status, _, written) = decompress(r, &self.idat, &mut self.raw, 0, flags);
        if !matches!(status, TINFLStatus::Done) || written != raw_len {
            return Err(format!("inflate {status:?}, {written}/{raw_len} bytes"));
        }

        self.pixels.resize(h * stride, 0);
        self.zero_row.resize(stride, 0);
        for y in 0..h {
            let line = &self.raw[y * (stride + 1)..(y + 1) * (stride + 1)];
            let (done, rest) = self.pixels.split_at_mut(y * stride);
            let prev = if y == 0 { &self.zero_row[..] } else { &done[(y - 1) * stride..] };
            unfilter(line[0], &line[1..], prev, &mut rest[..stride], ch)?;
        }
        self.width = w;
        self.height = h;
        self.channels = ch;
        Ok(())
    }
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = a as i16 + b as i16 - c as i16;
    let (pa, pb, pc) = ((p - a as i16).abs(), (p - b as i16).abs(), (p - c as i16).abs());
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

fn unfilter(kind: u8, src: &[u8], prev: &[u8], out: &mut [u8], bpp: usize) -> Result<(), String> {
    match kind {
        0 => out.copy_from_slice(src),
        1 => {
            for i in 0..src.len() {
                let left = if i >= bpp { out[i - bpp] } else { 0 };
                out[i] = src[i].wrapping_add(left);
            }
        }
        2 => {
            for i in 0..src.len() {
                out[i] = src[i].wrapping_add(prev[i]);
            }
        }
        3 => {
            for i in 0..src.len() {
                let left = if i >= bpp { out[i - bpp] } else { 0 };
                out[i] = src[i].wrapping_add(((left as u16 + prev[i] as u16) / 2) as u8);
            }
        }
        4 => {
            for i in 0..src.len() {
                let (left, upleft) = if i >= bpp { (out[i - bpp], prev[i - bpp]) } else { (0, 0) };
                out[i] = src[i].wrapping_add(paeth(left, prev[i], upleft));
            }
        }
        _ => return Err(format!("filter type {kind}")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real Chromium screencast frame (probe/cdp.py's animated page,
    /// background #123, a rotating #e64 square): exercises IDAT
    /// concatenation and whichever filters Chromium's encoder chose.
    #[test]
    fn decodes_chromium_frame() {
        let data = include_bytes!("../testdata/anim-1920x1080.png");
        let mut d = Decoder::default();
        d.decode(data).unwrap();
        assert_eq!((d.width, d.height, d.channels), (1920, 1080, 3));
        let px = |x: usize, y: usize| &d.pixels[(y * 1920 + x) * 3..][..3];
        assert_eq!(px(0, 0), &[0x11, 0x22, 0x33]);
        assert_eq!(px(1919, 1079), &[0x11, 0x22, 0x33]);
        assert_eq!(px(350, 350), &[0xee, 0x66, 0x44]);
        // Decoding again into the reused buffers gives the same pixels.
        let first = d.pixels.clone();
        d.decode(data).unwrap();
        assert_eq!(d.pixels, first);
    }

    /// Writes the decoded pixels to $CDPFB_DUMP for comparison with another
    /// decoder (`sips -s format bmp`): `cargo test dump -- --ignored`.
    #[test]
    #[ignore]
    fn dump() {
        let mut d = Decoder::default();
        d.decode(include_bytes!("../testdata/anim-1920x1080.png")).unwrap();
        std::fs::write(std::env::var("CDPFB_DUMP").unwrap(), &d.pixels).unwrap();
    }

    #[test]
    fn paeth_predictor() {
        assert_eq!(paeth(10, 20, 15), 15);
        assert_eq!(paeth(10, 20, 10), 20);
        assert_eq!(paeth(10, 20, 20), 10);
    }
}
