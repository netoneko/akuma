//! wavplay - stream a WAV file to /dev/dsp (virtio-sound)
//!
//! Usage: wavplay <file.wav>
//!
//! File-backed, fixed-footprint: the WAV is streamed through a single small
//! buffer, so memory use is independent of file size — a 54 MB song plays in
//! ~tens of KB of RAM, which is what makes playback possible on the 4 MB
//! `extreme` kernel.
//!
//! Formats: 16-bit PCM is passed through; 24-bit packed PCM is downconverted to
//! 16-bit on the fly (keep the top two bytes of each 3-byte sample) because the
//! device universally supports S16. 8-bit unsigned is passed through as AFMT_U8.

#![no_std]
#![no_main]

use libakuma::{arg, argc, close, exit, open, print, read_fd, syscall, write_fd};

// ---- syscall / ioctl constants (mirror src/audio.rs) -----------------------
const IOCTL: u64 = 29;
const O_RDONLY: u32 = 0;
const SNDCTL_DSP_SPEED: u32 = 0xC004_5002;
const SNDCTL_DSP_SETFMT: u32 = 0xC004_5005;
const SNDCTL_DSP_CHANNELS: u32 = 0xC004_5006;
const AFMT_S16_LE: i32 = 0x0000_0010;
const AFMT_U8: i32 = 0x0000_0008;

// Input read buffer. 12288 = 6 * 2048: a whole number of stereo 24-bit frames
// (6 bytes) and of 16-bit frames (4 bytes). Its 24->16 output (8192 bytes)
// equals the kernel's PCM period, so each write is one period.
const IN_BUF: usize = 12288;

#[no_mangle]
pub extern "C" fn main() {
    if argc() < 2 {
        print("usage: wavplay <file.wav>\n");
        exit(2);
    }
    let path = match arg(1) {
        Some(p) => p,
        None => {
            print("wavplay: missing file argument\n");
            exit(2);
        }
    };

    let wav = open(path, O_RDONLY);
    if wav < 0 {
        print("wavplay: cannot open ");
        print(path);
        print("\n");
        exit(1);
    }

    let fmt = match parse_wav_header(wav) {
        Ok(f) => f,
        Err(msg) => {
            print("wavplay: ");
            print(msg);
            print("\n");
            close(wav);
            exit(1);
        }
    };

    // Decide output format / whether we downconvert.
    let (oss_fmt, downconvert_24) = match fmt.bits_per_sample {
        16 => (AFMT_S16_LE, false),
        24 => (AFMT_S16_LE, true), // pack down to 16-bit
        8 => (AFMT_U8, false),
        _ => {
            print("wavplay: unsupported bit depth\n");
            close(wav);
            exit(1);
        }
    };

    let dsp = open("/dev/dsp", 1 /* O_WRONLY */);
    if dsp < 0 {
        print("wavplay: cannot open /dev/dsp (is sound available?)\n");
        close(wav);
        exit(1);
    }

    // Configure the stream (order-independent; each is its own ioctl).
    if dsp_set(dsp, SNDCTL_DSP_CHANNELS, fmt.channels as i32) != 0
        || dsp_set(dsp, SNDCTL_DSP_SPEED, fmt.sample_rate as i32) != 0
        || dsp_set(dsp, SNDCTL_DSP_SETFMT, oss_fmt) != 0
    {
        print("wavplay: device rejected format/rate/channels\n");
        close(dsp);
        close(wav);
        exit(1);
    }

    print("wavplay: playing ");
    print(path);
    print(" (");
    print_u32(fmt.sample_rate);
    print(" Hz, ");
    print_u32(fmt.channels as u32);
    print(" ch, ");
    print_u32(fmt.bits_per_sample as u32);
    print("-bit)\n");

    // Stream the data chunk through one fixed buffer.
    let mut in_buf = [0u8; IN_BUF];
    let mut out_buf = [0u8; IN_BUF]; // worst case (passthrough) needs == IN_BUF
    let mut remaining = fmt.data_size;

    loop {
        if remaining == 0 {
            break;
        }
        let want = if remaining < IN_BUF as u64 {
            remaining as usize
        } else {
            IN_BUF
        };
        let n = read_fd(wav, &mut in_buf[..want]);
        if n <= 0 {
            break; // EOF or error
        }
        let n = n as usize;
        remaining -= n as u64;

        let out: &[u8] = if downconvert_24 {
            let mut o = 0;
            // Each 3-byte LE sample -> top 2 bytes (drop the low byte).
            let mut i = 0;
            while i + 3 <= n {
                out_buf[o] = in_buf[i + 1];
                out_buf[o + 1] = in_buf[i + 2];
                o += 2;
                i += 3;
            }
            &out_buf[..o]
        } else {
            &in_buf[..n]
        };

        if !write_all(dsp, out) {
            print("wavplay: write error\n");
            break;
        }
    }

    close(dsp);
    close(wav);
    print("wavplay: done\n");
    exit(0);
}

/// Parsed WAV format fields plus the byte length of the data chunk.
struct WavFmt {
    channels: u16,
    sample_rate: u32,
    bits_per_sample: u16,
    data_size: u64,
}

/// Parse the RIFF/WAVE header by reading sequentially; leaves the fd positioned
/// at the first PCM byte of the `data` chunk.
fn parse_wav_header(fd: i32) -> Result<WavFmt, &'static str> {
    let mut hdr = [0u8; 12];
    if !read_exact(fd, &mut hdr) {
        return Err("short read on RIFF header");
    }
    if &hdr[0..4] != b"RIFF" || &hdr[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE file");
    }

    let mut channels = 0u16;
    let mut sample_rate = 0u32;
    let mut bits_per_sample = 0u16;
    let mut audio_format = 0u16;
    let mut have_fmt = false;

    loop {
        let mut ch = [0u8; 8];
        if !read_exact(fd, &mut ch) {
            return Err("unexpected EOF before data chunk");
        }
        let id = [ch[0], ch[1], ch[2], ch[3]];
        let size = u32::from_le_bytes([ch[4], ch[5], ch[6], ch[7]]);

        if &id == b"fmt " {
            // Read the 16-byte canonical PCM fmt body; skip any extension.
            let mut body = [0u8; 16];
            if size < 16 || !read_exact(fd, &mut body) {
                return Err("bad fmt chunk");
            }
            audio_format = u16::from_le_bytes([body[0], body[1]]);
            channels = u16::from_le_bytes([body[2], body[3]]);
            sample_rate = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
            bits_per_sample = u16::from_le_bytes([body[14], body[15]]);
            have_fmt = true;
            // Skip extension bytes + pad.
            let extra = (size - 16) + (size & 1);
            if extra > 0 {
                skip(fd, extra as u64);
            }
        } else if &id == b"data" {
            if !have_fmt {
                return Err("data chunk before fmt");
            }
            if audio_format != 1 {
                return Err("not uncompressed PCM");
            }
            return Ok(WavFmt {
                channels,
                sample_rate,
                bits_per_sample,
                data_size: size as u64,
            });
        } else {
            // Unknown chunk: skip it (size + pad byte if odd).
            skip(fd, size as u64 + (size & 1) as u64);
        }
    }
}

/// ioctl(fd, cmd, &mut val); returns 0 on success.
fn dsp_set(fd: i32, cmd: u32, mut val: i32) -> i64 {
    syscall(
        IOCTL,
        fd as u64,
        cmd as u64,
        &mut val as *mut i32 as u64,
        0,
        0,
        0,
    ) as i64
}

/// Read exactly buf.len() bytes; false on EOF/error.
fn read_exact(fd: i32, buf: &mut [u8]) -> bool {
    let mut off = 0;
    while off < buf.len() {
        let n = read_fd(fd, &mut buf[off..]);
        if n <= 0 {
            return false;
        }
        off += n as usize;
    }
    true
}

/// Write all bytes; false on error.
fn write_all(fd: i32, buf: &[u8]) -> bool {
    let mut off = 0;
    while off < buf.len() {
        let n = write_fd(fd, &buf[off..]);
        if n <= 0 {
            return false;
        }
        off += n as usize;
    }
    true
}

/// Skip `n` bytes forward via lseek(SEEK_CUR).
fn skip(fd: i32, n: u64) {
    libakuma::lseek(fd, n as i64, 1 /* SEEK_CUR */);
}

/// Print a u32 in decimal (no allocation).
fn print_u32(mut v: u32) {
    let mut buf = [0u8; 10];
    if v == 0 {
        print("0");
        return;
    }
    let mut i = buf.len();
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    if let Ok(s) = core::str::from_utf8(&buf[i..]) {
        print(s);
    }
}
