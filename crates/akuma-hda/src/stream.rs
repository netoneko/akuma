//! Stream-side arithmetic: the format word, PCM conversion, the buffer
//! descriptor list and the playback ring's bookkeeping.
//!
//! Everything here is a function of its inputs. The metal half (`amd64/src/hda.rs`)
//! owns the DMA memory and the registers; it asks this module *what* to write.

/// OSS `AFMT_U8`.
pub const AFMT_U8: i32 = 0x0000_0008;
/// OSS `AFMT_S16_LE`.
pub const AFMT_S16_LE: i32 = 0x0000_0010;
/// The repo's `AFMT_S24_LE` (`akuma_virtio::audio`): 24-bit samples, **packed**
/// to 3 bytes.
pub const AFMT_S24_LE: i32 = 0x0001_0000;
/// The repo's `AFMT_S32_LE`.
pub const AFMT_S32_LE: i32 = 0x0100_0000;

/// A sample layout the `/dev/dsp` write path accepts. The hardware stream is
/// always 16-bit stereo; anything else is converted on the way into the ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleFormat {
    /// Unsigned 8-bit.
    U8,
    /// Signed 16-bit little-endian.
    S16,
    /// Signed 24-bit little-endian, 3 bytes per sample.
    S24,
    /// Signed 32-bit little-endian.
    S32,
}

impl SampleFormat {
    /// Decode an OSS `AFMT_*` code.
    #[must_use]
    pub fn from_oss(fmt: i32) -> Option<Self> {
        match fmt {
            AFMT_U8 => Some(Self::U8),
            AFMT_S16_LE => Some(Self::S16),
            AFMT_S24_LE => Some(Self::S24),
            AFMT_S32_LE => Some(Self::S32),
            _ => None,
        }
    }
    /// Bytes per sample.
    #[must_use]
    pub const fn bytes(self) -> usize {
        match self {
            Self::U8 => 1,
            Self::S16 => 2,
            Self::S24 => 3,
            Self::S32 => 4,
        }
    }
}

/// Rates the format word can express, as `(rate, base_is_44k, mult, div)`.
const RATES: [(u32, bool, u8, u8); 11] = [
    (8000, false, 1, 6),
    (11025, true, 1, 4),
    (16000, false, 1, 3),
    (22050, true, 1, 2),
    (32000, false, 2, 3),
    (44100, true, 1, 1),
    (48000, false, 1, 1),
    (88200, true, 2, 1),
    (96000, false, 2, 1),
    (176_400, true, 4, 1),
    (192_000, false, 4, 1),
];

/// The 16-bit stream format word shared by `SDnFMT` and `SET_CONVERTER_FORMAT`.
///
/// HDA 1.0a §3.7.1: `base[14] mult[13:11] div[10:8] bits[6:4] channels-1[3:0]`.
/// `None` for a rate/width/channel count the word cannot express.
#[must_use]
pub fn format_word(rate: u32, bits: u8, channels: u8) -> Option<u16> {
    let (_, base44, mult, div) = RATES.iter().copied().find(|r| r.0 == rate)?;
    let bits_code: u16 = match bits {
        8 => 0,
        16 => 1,
        20 => 2,
        24 => 3,
        32 => 4,
        _ => return None,
    };
    if channels == 0 || channels > 16 {
        return None;
    }
    Some(
        (u16::from(base44) << 14)
            | (u16::from(mult - 1) << 11)
            | (u16::from(div - 1) << 8)
            | (bits_code << 4)
            | u16::from(channels - 1),
    )
}

/// Whether a converter's `PCM sizes and rates` parameter (0xA) advertises
/// `rate` at `bits` per sample.
#[must_use]
pub fn pcm_supports(pcm_caps: u32, rate: u32, bits: u8) -> bool {
    let rate_bit = match rate {
        8000 => 0,
        11025 => 1,
        16000 => 2,
        22050 => 3,
        32000 => 4,
        44100 => 5,
        48000 => 6,
        88200 => 7,
        96000 => 8,
        176_400 => 9,
        192_000 => 10,
        _ => return false,
    };
    let bits_bit = match bits {
        8 => 16,
        16 => 17,
        20 => 18,
        24 => 19,
        32 => 20,
        _ => return false,
    };
    pcm_caps & (1 << rate_bit) != 0 && pcm_caps & (1 << bits_bit) != 0
}

/// One 16-byte buffer descriptor list entry: address (64), length, flags
/// (bit 0 = interrupt on completion).
#[must_use]
pub fn bdl_entry(addr: u64, len: u32, ioc: bool) -> [u32; 4] {
    [addr as u32, (addr >> 32) as u32, len, u32::from(ioc)]
}

/// Convert whole frames of `src` (`channels` of `fmt`) to 16-bit stereo LE.
///
/// Returns `(source bytes consumed, destination bytes produced)`; stops at the
/// end of either buffer, never mid-frame. `channels` other than 1 or 2 consumes
/// nothing.
#[must_use]
pub fn to_s16_stereo(fmt: SampleFormat, channels: u8, src: &[u8], dst: &mut [u8]) -> (usize, usize) {
    if channels != 1 && channels != 2 {
        return (0, 0);
    }
    let ch = usize::from(channels);
    let frame_in = fmt.bytes() * ch;
    let (mut i, mut o) = (0usize, 0usize);
    while i + frame_in <= src.len() && o + 4 <= dst.len() {
        let mut s = [0i16; 2];
        for (c, slot) in s.iter_mut().take(ch).enumerate() {
            let p = i + c * fmt.bytes();
            *slot = match fmt {
                SampleFormat::U8 => (i16::from(src[p]) - 128) << 8,
                SampleFormat::S16 => i16::from_le_bytes([src[p], src[p + 1]]),
                SampleFormat::S24 => i16::from_le_bytes([src[p + 1], src[p + 2]]),
                SampleFormat::S32 => i16::from_le_bytes([src[p + 2], src[p + 3]]),
            };
        }
        if ch == 1 {
            s[1] = s[0];
        }
        dst[o..o + 2].copy_from_slice(&s[0].to_le_bytes());
        dst[o + 2..o + 4].copy_from_slice(&s[1].to_le_bytes());
        i += frame_in;
        o += 4;
    }
    (i, o)
}

/// A playback ring's bookkeeping: bytes written, bytes consumed, next write.
///
/// The hardware reports
/// only its position *within* the buffer (`LPIB`, which wraps to 0 at `cbl`),
/// so consumption is reconstructed from successive observations — which is
/// only sound if [`PlayRing::observe`] runs at least once per lap; the caller
/// is expected to detect a lap it missed by other means (time) and treat it as
/// an underrun.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlayRing {
    cbl: u32,
    wr: u64,
    played: u64,
    last_lpib: u32,
}

impl PlayRing {
    /// A fresh ring of `cbl` bytes with the hardware at position 0.
    #[must_use]
    pub const fn new(cbl: u32) -> Self {
        Self { cbl, wr: 0, played: 0, last_lpib: 0 }
    }
    /// Ring size in bytes.
    #[must_use]
    pub const fn size(&self) -> u32 {
        self.cbl
    }
    /// Total bytes written since the ring was (re)started.
    #[must_use]
    pub const fn written(&self) -> u64 {
        self.wr
    }
    /// Total bytes the hardware is known to have consumed.
    #[must_use]
    pub const fn consumed(&self) -> u64 {
        self.played
    }
    /// Fold in a new `LPIB` reading. A reading at or past `cbl` is treated as
    /// a wrap to 0 (some controllers report `cbl` itself at the boundary).
    pub fn observe(&mut self, lpib: u32) {
        let pos = if lpib >= self.cbl { 0 } else { lpib };
        let delta = if pos >= self.last_lpib { pos - self.last_lpib } else { self.cbl - self.last_lpib + pos };
        self.played += u64::from(delta);
        self.last_lpib = pos;
    }
    /// Bytes written but not yet consumed.
    #[must_use]
    pub const fn buffered(&self) -> u64 {
        self.wr - self.played
    }
    /// The hardware has consumed everything written: it is now replaying
    /// whatever stale bytes the ring holds.
    #[must_use]
    pub const fn underrun(&self) -> bool {
        self.played >= self.wr
    }
    /// Bytes that can be written now while keeping `guard` bytes between the
    /// write point and the hardware's read point.
    #[must_use]
    pub fn space(&self, guard: u32) -> usize {
        let cap = u64::from(self.cbl.saturating_sub(guard));
        cap.saturating_sub(self.buffered()) as usize
    }
    /// Reserve `n` bytes at the write point. Returns `(offset, first, second)`:
    /// copy `first` bytes at `offset`, then `second` bytes at offset 0. The
    /// caller must have checked [`PlayRing::space`].
    pub fn reserve(&mut self, n: usize) -> (usize, usize, usize) {
        let off = (self.wr % u64::from(self.cbl)) as usize;
        let first = n.min(self.cbl as usize - off);
        self.wr += n as u64;
        (off, first, n - first)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_words() {
        assert_eq!(format_word(44100, 16, 2), Some(0x4011));
        assert_eq!(format_word(48000, 16, 2), Some(0x0011));
        assert_eq!(format_word(48000, 24, 2), Some(0x0031));
        assert_eq!(format_word(96000, 16, 2), Some(0x0811));
        assert_eq!(format_word(8000, 8, 1), Some(0x0500));
        assert_eq!(format_word(22050, 16, 1), Some(0x4100 | 0x10));
        assert_eq!(format_word(12345, 16, 2), None);
        assert_eq!(format_word(48000, 12, 2), None);
        assert_eq!(format_word(48000, 16, 0), None);
    }

    #[test]
    fn pcm_caps() {
        // ALC662-like: 44.1/48/96k and 16/20/24 bit.
        let caps = (1 << 5) | (1 << 6) | (1 << 8) | (1 << 17) | (1 << 18) | (1 << 19);
        assert!(pcm_supports(caps, 44100, 16));
        assert!(pcm_supports(caps, 48000, 24));
        assert!(!pcm_supports(caps, 22050, 16));
        assert!(!pcm_supports(caps, 48000, 32));
        assert!(!pcm_supports(caps, 12345, 16));
    }

    #[test]
    fn bdl_entry_layout() {
        assert_eq!(bdl_entry(0x1_2345_6000, 0x2000, true), [0x2345_6000, 1, 0x2000, 1]);
        assert_eq!(bdl_entry(0x1000, 16, false)[3], 0);
    }

    #[test]
    fn s16_passes_through() {
        let src = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let mut dst = [0u8; 8];
        assert_eq!(to_s16_stereo(SampleFormat::S16, 2, &src, &mut dst), (8, 8));
        assert_eq!(dst, src);
    }

    #[test]
    fn mono_is_duplicated_and_u8_is_recentred() {
        let mut dst = [0u8; 8];
        // u8 0x80 is silence, 0xFF is near +full-scale, 0x00 is -full-scale.
        let (i, o) = to_s16_stereo(SampleFormat::U8, 1, &[0x80, 0xFF], &mut dst);
        assert_eq!((i, o), (2, 8));
        assert_eq!(i16::from_le_bytes([dst[0], dst[1]]), 0);
        assert_eq!(&dst[0..2], &dst[2..4]);
        assert_eq!(i16::from_le_bytes([dst[4], dst[5]]), 127 << 8);
        let (_, _) = to_s16_stereo(SampleFormat::U8, 1, &[0x00], &mut dst);
        assert_eq!(i16::from_le_bytes([dst[0], dst[1]]), -32768);
    }

    #[test]
    fn s24_and_s32_keep_the_top_sixteen_bits() {
        let mut dst = [0u8; 4];
        // 24-bit stereo frame: L = 0x123456, R = 0xFEDCBA -> 0x1234, 0xFEDC.
        let src24 = [0x56, 0x34, 0x12, 0xBA, 0xDC, 0xFE];
        assert_eq!(to_s16_stereo(SampleFormat::S24, 2, &src24, &mut dst), (6, 4));
        assert_eq!(dst, [0x34, 0x12, 0xDC, 0xFE]);
        let src32 = [0x00, 0x78, 0x34, 0x12, 0x00, 0x88, 0xDC, 0xFE];
        assert_eq!(to_s16_stereo(SampleFormat::S32, 2, &src32, &mut dst), (8, 4));
        assert_eq!(dst, [0x34, 0x12, 0xDC, 0xFE]);
    }

    #[test]
    fn conversion_stops_on_frame_and_space_boundaries() {
        let mut dst = [0u8; 6];
        // Room for one output frame only; a trailing partial input frame is left.
        assert_eq!(to_s16_stereo(SampleFormat::S16, 2, &[0; 10], &mut dst), (4, 4));
        assert_eq!(to_s16_stereo(SampleFormat::S16, 2, &[0; 3], &mut dst), (0, 0));
        assert_eq!(to_s16_stereo(SampleFormat::S16, 6, &[0; 12], &mut dst), (0, 0));
    }

    #[test]
    fn oss_codes() {
        assert_eq!(SampleFormat::from_oss(0x10), Some(SampleFormat::S16));
        assert_eq!(SampleFormat::from_oss(0x8), Some(SampleFormat::U8));
        assert_eq!(SampleFormat::from_oss(0x10000), Some(SampleFormat::S24));
        assert_eq!(SampleFormat::from_oss(0x1), None);
    }

    #[test]
    fn ring_write_wrap_and_consumption() {
        let mut r = PlayRing::new(100);
        assert!(r.underrun());
        assert_eq!(r.space(10), 90);
        assert_eq!(r.reserve(60), (0, 60, 0));
        assert_eq!(r.buffered(), 60);
        assert!(!r.underrun());
        r.observe(40);
        assert_eq!(r.consumed(), 40);
        assert_eq!(r.space(10), 90 - 20);
        // 30 more from offset 60: 40 to the end is enough, no wrap.
        assert_eq!(r.reserve(30), (60, 30, 0));
        // 25 from offset 90: 10 then wrap 15.
        r.observe(90);
        assert_eq!(r.reserve(25), (90, 10, 15));
        assert_eq!(r.written(), 115);
    }

    #[test]
    fn observe_handles_wrap_and_boundary_reports() {
        let mut r = PlayRing::new(100);
        r.reserve(90);
        r.observe(90);
        r.observe(20); // wrapped past the end: 10 + 20 more
        assert_eq!(r.consumed(), 120);
        // Some controllers report `cbl` itself at the boundary.
        let mut r = PlayRing::new(100);
        r.reserve(90);
        r.observe(90);
        r.observe(100);
        assert_eq!(r.consumed(), 100);
        r.observe(0);
        assert_eq!(r.consumed(), 100);
    }

    #[test]
    fn space_never_underflows() {
        let mut r = PlayRing::new(100);
        r.reserve(100);
        assert_eq!(r.space(10), 0);
        assert_eq!(r.space(200), 0);
    }
}
