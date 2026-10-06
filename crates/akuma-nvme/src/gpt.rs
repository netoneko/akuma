//! The GUID Partition Table (UEFI spec §5.3), read-only, both CRCs checked.
//!
//! Partition numbers follow Linux: `nvme0n1p3` is entry slot 3 (1-based) of the
//! entry array, whether or not earlier slots are in use.

/// `"EFI PART"`.
pub const SIGNATURE: [u8; 8] = *b"EFI PART";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GptError {
    Short,
    Signature,
    HeaderSize,
    HeaderCrc,
    /// The header does not say it lives at LBA 1.
    NotPrimary,
    EntrySize,
    /// More entry bytes than this reader accepts (or than fit in memory here).
    TooManyEntries,
    EntriesCrc,
}

/// The primary header, decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub first_usable: u64,
    pub last_usable: u64,
    pub disk_guid: [u8; 16],
    pub entries_lba: u64,
    pub entries: u32,
    pub entry_size: u32,
    pub entries_crc: u32,
}

/// Largest entry array accepted: 128 entries of 128 bytes, the universal
/// layout, times four for headroom.
pub const MAX_ENTRY_BYTES: usize = 64 * 1024;

impl Header {
    /// Parse and CRC-check the header in `lba1` (one whole logical block).
    pub fn parse(lba1: &[u8]) -> Result<Self, GptError> {
        if lba1.len() < 92 {
            return Err(GptError::Short);
        }
        if lba1[0..8] != SIGNATURE {
            return Err(GptError::Signature);
        }
        let size = u32_at(lba1, 12) as usize;
        if size < 92 || size > lba1.len() {
            return Err(GptError::HeaderSize);
        }
        let want = u32_at(lba1, 16);
        let mut c = Crc32::new();
        c.update(&lba1[..16]);
        c.update(&[0; 4]);
        c.update(&lba1[20..size]);
        if c.finish() != want {
            return Err(GptError::HeaderCrc);
        }
        if u64_at(lba1, 24) != 1 {
            return Err(GptError::NotPrimary);
        }
        let entry_size = u32_at(lba1, 84);
        if entry_size < 128 || !entry_size.is_power_of_two() {
            return Err(GptError::EntrySize);
        }
        let h = Self {
            first_usable: u64_at(lba1, 40),
            last_usable: u64_at(lba1, 48),
            disk_guid: lba1[56..72].try_into().map_err(|_| GptError::Short)?,
            entries_lba: u64_at(lba1, 72),
            entries: u32_at(lba1, 80),
            entry_size,
            entries_crc: u32_at(lba1, 88),
        };
        if h.entry_bytes() > MAX_ENTRY_BYTES {
            return Err(GptError::TooManyEntries);
        }
        Ok(h)
    }

    /// Bytes of the entry array.
    #[must_use]
    pub const fn entry_bytes(&self) -> usize {
        self.entries as usize * self.entry_size as usize
    }

    /// Check the entry array's CRC; `entries` must hold at least
    /// [`Header::entry_bytes`] bytes read from [`Header::entries_lba`].
    pub fn check_entries(&self, entries: &[u8]) -> Result<(), GptError> {
        let n = self.entry_bytes();
        if entries.len() < n {
            return Err(GptError::Short);
        }
        if crc32(&entries[..n]) != self.entries_crc {
            return Err(GptError::EntriesCrc);
        }
        Ok(())
    }

    /// Partition `number` (1-based, Linux numbering) from a CRC-checked entry
    /// array. `None` for an unused slot, a number past the array, or an entry
    /// whose range is inverted or outside the usable area.
    #[must_use]
    pub fn partition(&self, entries: &[u8], number: u32) -> Option<Entry> {
        if number == 0 || number > self.entries {
            return None;
        }
        let off = (number as usize - 1) * self.entry_size as usize;
        let e = entries.get(off..off + 128)?;
        let type_guid: [u8; 16] = e[0..16].try_into().ok()?;
        if type_guid == [0; 16] {
            return None;
        }
        let first_lba = u64_at(e, 32);
        let last_lba = u64_at(e, 40);
        if first_lba > last_lba || first_lba < self.first_usable || last_lba > self.last_usable {
            return None;
        }
        let mut name = [0u16; 36];
        for (i, n) in name.iter_mut().enumerate() {
            *n = u16::from_le_bytes([e[56 + 2 * i], e[57 + 2 * i]]);
        }
        Some(Entry { type_guid, unique_guid: e[16..32].try_into().ok()?, first_lba, last_lba, name })
    }
}

/// One partition entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub type_guid: [u8; 16],
    pub unique_guid: [u8; 16],
    /// Inclusive.
    pub first_lba: u64,
    /// Inclusive.
    pub last_lba: u64,
    /// UTF-16LE, NUL-padded.
    pub name: [u16; 36],
}

impl Entry {
    /// The name's ASCII characters into `out`, for a log line; anything else
    /// becomes `?`. Returns the length written.
    pub fn name_ascii(&self, out: &mut [u8]) -> usize {
        let mut n = 0;
        for &c in self.name.iter().take_while(|&&c| c != 0) {
            if n == out.len() {
                break;
            }
            out[n] = if c < 0x80 { c as u8 } else { b'?' };
            n += 1;
        }
        n
    }
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from(u32_at(b, o)) | (u64::from(u32_at(b, o + 4)) << 32)
}

/// CRC-32 (IEEE 802.3, reflected, as GPT uses), bitwise: a table would be 1 KiB
/// of `.rodata` to speed up two checks per boot over at most 64 KiB.
#[must_use]
pub fn crc32(data: &[u8]) -> u32 {
    let mut c = Crc32::new();
    c.update(data);
    c.finish()
}

struct Crc32(u32);

impl Crc32 {
    const fn new() -> Self {
        Self(0xffff_ffff)
    }

    fn update(&mut self, data: &[u8]) {
        for &b in data {
            self.0 ^= u32::from(b);
            for _ in 0..8 {
                let mask = (self.0 & 1).wrapping_neg();
                self.0 = (self.0 >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
    }

    const fn finish(&self) -> u32 {
        !self.0
    }
}
