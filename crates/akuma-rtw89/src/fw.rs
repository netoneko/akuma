//! The firmware file: `rtw8852c_fw-1.bin`, as `linux-firmware` ships it.
//!
//! # Layout
//!
//! The file is a **multi-firmware container** (Linux `struct rtw89_mfw_hdr`):
//! a 16-byte header (`sig = 0xff`, `fw_nr`, a version), then `fw_nr` 16-byte
//! entries `(cv, type, mp, _, shift, size, _)`, each naming one firmware image
//! by chip cut and type. ryzen's copy holds seven: a normal (`type 1`) and a
//! WoWLAN (`type 3`) image for each of cuts 0, 1 and 2, and a log-format table.
//! Linux picks the image of the wanted type with the **highest cut not above
//! the chip's** and `mp == 0` ([`Container::select`]); the cut is `SYS_CFG1`
//! bits 15:12, which on ryzen read 1.
//!
//! An image is a firmware header (`struct rtw89_fw_hdr`, version 0 on the
//! 8852C): eight words, then one 16-byte entry per **section**, then an
//! optional dynamic header, then the sections' bytes back to back. The header
//! goes to the chip first, as one H2C packet; then each section, cut into
//! [`PKT_LEN`]-byte packets ([`Image::packets`]).
//!
//! Nothing here holds the file. Everything is read through [`Source`] into
//! fixed buffers — the container's entry table (at most 4 KiB) and the image
//! header (at most [`MAX_HDR`] bytes) — and section bytes are copied by the
//! caller straight from the file into DMA memory, one packet at a time.
//!
//! # Checked against the real file
//!
//! For ryzen's `rtw8852c_fw-1.bin` (sha256 `95e4226f…`, Pop!_OS 22.04's
//! linux-firmware), this parser picks the cut-1 normal image at `0x57ef0`
//! (331 784 bytes, version 0.27.122.0), finds three sections ending exactly
//! at the image's end, and plans **166** data packets — the number of CH12
//! kicks Linux's driver made in the W0 trace after the header packet.

/// Bytes of section data per download packet (`FWDL_SECTION_PER_PKT_LEN`).
pub const PKT_LEN: u32 = 2020;
/// `FWDL_SECTION_CHKSUM_LEN`: added to a section that carries a checksum.
const SECTION_CHKSUM_LEN: u32 = 8;
/// `FWDL_SECURITY_SECTION_TYPE`.
const SECURITY_SECTION: u8 = 9;
/// `RTW89_MFW_SIG`.
const MFW_SIG: u8 = 0xff;
/// Linux's `FWDL_SECTION_MAX_NUM`.
pub const MAX_SECTIONS: usize = 10;
/// The largest image header this parser accepts: the base header with
/// [`MAX_SECTIONS`] entries. The dynamic header, when present, is not sent and
/// so never needs buffering.
pub const MAX_HDR: usize = 32 + 16 * MAX_SECTIONS;

/// `enum rtw89_fw_type`.
pub const TYPE_NORMAL: u8 = 1;

/// Where the firmware bytes come from: a file on disk in the kernel, a byte
/// slice in the tests.
pub trait Source {
    /// Total length in bytes.
    fn len(&self) -> u32;
    /// Whether there are no bytes at all.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Fill `buf` from byte `off`. `false` if the read failed or ran past the end.
    fn read_at(&mut self, off: u32, buf: &mut [u8]) -> bool;
}

impl Source for &[u8] {
    fn len(&self) -> u32 {
        <[u8]>::len(self) as u32
    }
    fn read_at(&mut self, off: u32, buf: &mut [u8]) -> bool {
        let start = off as usize;
        match self.get(start..start + buf.len()) {
            Some(src) => {
                buf.copy_from_slice(src);
                true
            }
            None => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// A read through [`Source`] failed.
    Read,
    /// The container header is not `0xff`-signed, or its entry table runs past the file.
    NotContainer,
    /// No image of the wanted type with a cut at or below the chip's.
    NoImage,
    /// An entry's `(shift, size)` runs past the file.
    ImageOutOfFile,
    /// The image header names more than [`MAX_SECTIONS`] sections, or none.
    Sections(u8),
    /// The image header version is not 0 (the 8852C's).
    HeaderVersion(u8),
    /// The dynamic header length disagrees with the header.
    DynamicHeader,
    /// The sections do not end exactly where the image does (`[ERR]fw bin size`).
    Size { sections_end: u32, image_size: u32 },
}

/// One container entry (`struct rtw89_mfw_info`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub cv: u8,
    pub ty: u8,
    pub mp: u8,
    pub shift: u32,
    pub size: u32,
}

/// The container's entry table, read once.
pub struct Container {
    entries: [Entry; 255],
    count: usize,
    /// The container's version bytes `(major, minor, sub, idx)`.
    pub version: [u8; 4],
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

impl Container {
    /// Read the header and entry table.
    ///
    /// # Errors
    ///
    /// [`Error::Read`] or [`Error::NotContainer`]. A file that is not a
    /// container at all ("legacy firmware" in Linux) is refused: no 8852C
    /// firmware has shipped in that form.
    pub fn read(src: &mut impl Source) -> Result<Self, Error> {
        let mut hdr = [0u8; 16];
        if !src.read_at(0, &mut hdr) {
            return Err(Error::Read);
        }
        if hdr[0] != MFW_SIG || hdr[1] == 0 {
            return Err(Error::NotContainer);
        }
        let count = usize::from(hdr[1]);
        if 16 + 16 * count as u32 > src.len() {
            return Err(Error::NotContainer);
        }
        let empty = Entry { cv: 0, ty: 0, mp: 0, shift: 0, size: 0 };
        let mut c = Self { entries: [empty; 255], count, version: [hdr[4], hdr[5], hdr[6], hdr[7]] };
        let mut raw = [0u8; 16];
        for (i, e) in c.entries[..count].iter_mut().enumerate() {
            if !src.read_at(16 + 16 * i as u32, &mut raw) {
                return Err(Error::Read);
            }
            *e = Entry { cv: raw[0], ty: raw[1], mp: raw[2], shift: le32(&raw, 4), size: le32(&raw, 8) };
        }
        Ok(c)
    }

    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries[..self.count]
    }

    /// Linux's `rtw89_mfw_recognize`: the image of type `ty` with the highest
    /// cut `<= cv`, skipping manufacturing (`mp`) images. The entries are not
    /// in version order in the file, so every one is looked at.
    ///
    /// # Errors
    ///
    /// [`Error::NoImage`], or [`Error::ImageOutOfFile`] when the chosen
    /// entry's bytes are not all inside a file of `file_len` bytes.
    pub fn select(&self, ty: u8, cv: u8, file_len: u32) -> Result<Entry, Error> {
        let best = self
            .entries()
            .iter()
            .filter(|e| e.ty == ty && e.cv <= cv && e.mp == 0)
            .fold(None::<Entry>, |best, e| match best {
                Some(b) if b.cv >= e.cv => Some(b),
                _ => Some(*e),
            })
            .ok_or(Error::NoImage)?;
        match best.shift.checked_add(best.size) {
            Some(end) if end <= file_len => Ok(best),
            _ => Err(Error::ImageOutOfFile),
        }
    }
}

/// One section of an image (`struct rtw89_fw_hdr_section_info`, the fields a
/// non-secure-boot 8852C download uses).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Section {
    pub ty: u8,
    /// Bytes sent, checksum included.
    pub len: u32,
    /// Where the chip puts it; informational.
    pub dl_addr: u32,
    /// File offset of the first byte.
    pub file_off: u32,
}

/// A parsed image header, plus the bytes the header packet carries.
#[derive(Clone, Debug)]
pub struct Image {
    pub entry: Entry,
    /// `(major, minor, sub, idx)` from header word 1.
    pub version: [u8; 4],
    sections: [Section; MAX_SECTIONS],
    section_count: usize,
    /// The base header — header words plus section table, no dynamic header —
    /// with `PART_SIZE` set to [`PKT_LEN`] (`__rtw89_fw_download_tweak_hdr_v0`):
    /// exactly the payload of the header packet.
    hdr: [u8; MAX_HDR],
    hdr_len: usize,
}

impl Image {
    /// Read and check the header of the image `entry` names.
    ///
    /// # Errors
    ///
    /// Any [`Error`] but the container ones: the checks are Linux's
    /// `rtw89_fw_hdr_parser_v0`, including that the sections tile the image
    /// exactly.
    pub fn read(src: &mut impl Source, entry: Entry) -> Result<Self, Error> {
        let base = entry.shift;
        let mut w = [0u8; 32];
        if !src.read_at(base, &mut w) {
            return Err(Error::Read);
        }
        let hdr_ver = w[15]; // w3[31:24]
        if hdr_ver != 0 {
            return Err(Error::HeaderVersion(hdr_ver));
        }
        let n = w[25]; // w6[15:8]
        if n == 0 || usize::from(n) > MAX_SECTIONS {
            return Err(Error::Sections(n));
        }
        let base_len = 32 + 16 * usize::from(n);
        let dyn_en = le32(&w, 28) & (1 << 16) != 0;
        let hdr_total = if dyn_en { u32::from(w[14]) } else { base_len as u32 }; // w3[23:16]
        let mut img = Self {
            entry,
            version: [w[4], w[5], w[6], w[7]],
            sections: [Section::default(); MAX_SECTIONS],
            section_count: usize::from(n),
            hdr: [0; MAX_HDR],
            hdr_len: base_len,
        };
        if !src.read_at(base, &mut img.hdr[..base_len]) {
            return Err(Error::Read);
        }
        if dyn_en {
            // `rtw89_fw_dynhdr_hdr.hdr_len` must equal what the base header implies.
            let dyn_len = hdr_total.checked_sub(base_len as u32).ok_or(Error::DynamicHeader)?;
            let mut d = [0u8; 4];
            if !src.read_at(base + base_len as u32, &mut d) {
                return Err(Error::Read);
            }
            if u32::from_le_bytes(d) != dyn_len {
                return Err(Error::DynamicHeader);
            }
        }
        let mut off = base + hdr_total;
        for i in 0..img.section_count {
            let s = &img.hdr[32 + 16 * i..48 + 16 * i];
            let w0 = le32(s, 0);
            let w1 = le32(s, 4);
            let mut len = w1 & 0x00ff_ffff;
            if w1 & (1 << 28) != 0 {
                len += SECTION_CHKSUM_LEN;
            }
            let ty = ((w1 >> 24) & 0xf) as u8;
            // A security section's signatures follow its data: `mssc` of them.
            // ryzen's has `mssc == 0`, and without secure boot (this chip's
            // efuse says off) Linux sends it as ordinary data.
            let mssc = le32(s, 8);
            if ty == SECURITY_SECTION && mssc != 0 {
                return Err(Error::Sections(n));
            }
            img.sections[i] = Section { ty, len, dl_addr: w0 & 0x1fff_ffff, file_off: off };
            off += len;
        }
        let end = off - base;
        if end != entry.size {
            return Err(Error::Size { sections_end: end, image_size: entry.size });
        }
        // `FW_HDR_W7_PART_SIZE` (w7[15:0]) := the packet size this download uses.
        img.hdr[28..30].copy_from_slice(&(PKT_LEN as u16).to_le_bytes());
        Ok(img)
    }

    #[must_use]
    pub fn sections(&self) -> &[Section] {
        &self.sections[..self.section_count]
    }

    /// The header packet's payload.
    #[must_use]
    pub fn header_payload(&self) -> &[u8] {
        &self.hdr[..self.hdr_len]
    }

    /// The data packets, in order: `(file offset, length)`.
    pub fn packets(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        self.sections().iter().flat_map(|s| {
            let n = s.len.div_ceil(PKT_LEN);
            (0..n).map(move |i| {
                let start = i * PKT_LEN;
                (s.file_off + start, (s.len - start).min(PKT_LEN))
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A container with the given entries, each image a header with the given
    /// section lengths followed by that many bytes.
    fn build(images: &[(u8, u8, &[u32])]) -> ([u8; 8192], u32) {
        let mut f = [0u8; 8192];
        f[0] = MFW_SIG;
        f[1] = images.len() as u8;
        let mut at = 16 + 16 * images.len();
        for (i, &(cv, ty, secs)) in images.iter().enumerate() {
            let hdr_len = 32 + 16 * secs.len();
            let size = hdr_len as u32 + secs.iter().sum::<u32>();
            let e = 16 + 16 * i;
            f[e] = cv;
            f[e + 1] = ty;
            f[e + 4..e + 8].copy_from_slice(&(at as u32).to_le_bytes());
            f[e + 8..e + 12].copy_from_slice(&size.to_le_bytes());
            f[at + 4] = 0;
            f[at + 5] = 27;
            f[at + 6] = 122;
            f[at + 25] = secs.len() as u8;
            for (j, &l) in secs.iter().enumerate() {
                let s = at + 32 + 16 * j;
                f[s + 4..s + 8].copy_from_slice(&(l | (2 << 24)).to_le_bytes());
            }
            at += size as usize;
        }
        (f, at as u32)
    }

    #[test]
    fn picks_the_highest_cut_not_above_the_chip() {
        let (f, len) = build(&[(0, 1, &[100]), (2, 1, &[100]), (1, 1, &[100]), (1, 3, &[100])]);
        let mut src: &[u8] = &f[..len as usize];
        let c = Container::read(&mut src).unwrap();
        assert_eq!(c.select(TYPE_NORMAL, 1, len).unwrap().cv, 1);
        assert_eq!(c.select(TYPE_NORMAL, 5, len).unwrap().cv, 2);
        assert_eq!(c.select(TYPE_NORMAL, 0, len).unwrap().cv, 0);
        assert_eq!(c.select(7, 1, len), Err(Error::NoImage));
    }

    #[test]
    fn packets_cut_each_section_separately() {
        let (f, len) = build(&[(1, 1, &[PKT_LEN * 2 + 5, PKT_LEN, 7])]);
        let mut src: &[u8] = &f[..len as usize];
        let c = Container::read(&mut src).unwrap();
        let img = Image::read(&mut src, c.select(TYPE_NORMAL, 1, len).unwrap()).unwrap();
        let lens: [u32; 5] = core::array::from_fn(|i| img.packets().nth(i).unwrap().1);
        assert_eq!(lens, [PKT_LEN, PKT_LEN, 5, PKT_LEN, 7]);
        assert_eq!(img.packets().count(), 5);
        // Packets are contiguous in the file, starting right after the header.
        let first = img.packets().next().unwrap().0;
        assert_eq!(first, img.entry.shift + 32 + 16 * 3);
        assert_eq!(&img.header_payload()[28..30], &(PKT_LEN as u16).to_le_bytes());
    }

    #[test]
    fn refuses_sections_that_do_not_tile_the_image() {
        let (mut f, len) = build(&[(1, 1, &[100])]);
        // Claim a longer image than the sections cover.
        f[16 + 8..16 + 12].copy_from_slice(&(32 + 16 + 101u32).to_le_bytes());
        let mut src: &[u8] = &f[..=(len as usize)];
        let c = Container::read(&mut src).unwrap();
        let e = c.select(TYPE_NORMAL, 1, len + 1).unwrap();
        assert_eq!(Image::read(&mut src, e).unwrap_err(), Error::Size { sections_end: 148, image_size: 149 });
    }

    #[test]
    fn refuses_a_file_that_is_not_a_container() {
        let f = [0u8; 64];
        let mut src: &[u8] = &f;
        assert!(matches!(Container::read(&mut src), Err(Error::NotContainer)));
    }

    #[test]
    fn refuses_an_image_past_the_end() {
        let (f, len) = build(&[(1, 1, &[100])]);
        let mut src: &[u8] = &f[..len as usize];
        let c = Container::read(&mut src).unwrap();
        assert_eq!(c.select(TYPE_NORMAL, 1, len - 1), Err(Error::ImageOutOfFile));
    }
}
