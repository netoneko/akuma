//! The pure half: on-disk vocabulary with no device behind it.
//!
//! Everything here is a function of bytes, so it is host-tested exhaustively.

#![allow(clippy::many_single_char_names, clippy::cast_possible_wrap)]

use akuma_vfs::FsError;
use alloc::string::String;
use alloc::vec::Vec;

pub const ATTR_READ_ONLY: u8 = 0x01;
pub const ATTR_HIDDEN: u8 = 0x02;
pub const ATTR_SYSTEM: u8 = 0x04;
pub const ATTR_VOLUME_ID: u8 = 0x08;
pub const ATTR_DIRECTORY: u8 = 0x10;
pub const ATTR_ARCHIVE: u8 = 0x20;
/// READ_ONLY | HIDDEN | SYSTEM | VOLUME_ID: marks a long-name slot.
pub const ATTR_LONG_NAME: u8 = 0x0F;

/// Size of one directory slot.
pub const DIR_ENTRY_SIZE: usize = 32;
/// A directory never grows past this many bytes (the spec's 65 536 entries).
pub const MAX_DIR_BYTES: usize = 65_536 * DIR_ENTRY_SIZE;
/// UTF-16 units one long-name slot carries.
pub const LFN_CHARS_PER_SLOT: usize = 13;
/// Longest long name, in UTF-16 units.
pub const MAX_NAME_UNITS: usize = 255;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FatKind {
    Fat12,
    Fat16,
    Fat32,
}

impl FatKind {
    /// End-of-chain value written to the FAT.
    #[must_use]
    pub const fn eoc(self) -> u32 {
        match self {
            Self::Fat12 => 0xFFF,
            Self::Fat16 => 0xFFFF,
            Self::Fat32 => 0x0FFF_FFFF,
        }
    }

    /// Values at or above this are end-of-chain.
    #[must_use]
    pub const fn eoc_min(self) -> u32 {
        match self {
            Self::Fat12 => 0xFF8,
            Self::Fat16 => 0xFFF8,
            Self::Fat32 => 0x0FFF_FFF8,
        }
    }

    #[must_use]
    pub const fn bad(self) -> u32 {
        match self {
            Self::Fat12 => 0xFF7,
            Self::Fat16 => 0xFFF7,
            Self::Fat32 => 0x0FFF_FFF7,
        }
    }

    /// Bits per FAT entry.
    #[must_use]
    pub const fn bits(self) -> u32 {
        match self {
            Self::Fat12 => 12,
            Self::Fat16 => 16,
            Self::Fat32 => 32,
        }
    }

    /// The type is decided by the number of data clusters and by nothing else
    /// (not by the `FilSysType` string, which is informational).
    #[must_use]
    pub const fn from_cluster_count(count: u32) -> Self {
        if count < 4085 {
            Self::Fat12
        } else if count < 65525 {
            Self::Fat16
        } else {
            Self::Fat32
        }
    }
}

/// A parsed, validated boot sector plus every derived byte offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub kind: FatKind,
    pub bytes_per_sector: u32,
    pub sectors_per_cluster: u32,
    pub reserved_sectors: u32,
    pub num_fats: u32,
    /// Fixed-root entries (0 on FAT32).
    pub root_entries: u32,
    pub fat_sectors: u32,
    pub total_sectors: u32,
    /// First cluster of the root directory (FAT32 only; 0 otherwise).
    pub root_cluster: u32,
    /// FSInfo sector number (FAT32 only; 0 = none).
    pub fsinfo_sector: u32,
    /// Number of data clusters. Valid cluster numbers are `2..=cluster_count + 1`.
    pub cluster_count: u32,
    pub cluster_bytes: u32,
    pub fat_start: u64,
    pub fat_bytes: u64,
    pub root_dir_start: u64,
    pub root_dir_bytes: u64,
    pub data_start: u64,
}

fn le16(b: &[u8], o: usize) -> u32 {
    u32::from(u16::from_le_bytes([b[o], b[o + 1]]))
}

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

impl Geometry {
    /// Parse and validate sector 0. Anything the spec forbids is `Corrupt`;
    /// "not a FAT volume at all" is `NoFilesystem`.
    pub fn parse(sector: &[u8]) -> Result<Self, FsError> {
        if sector.len() < 512 {
            return Err(FsError::NoFilesystem);
        }
        if sector[510] != 0x55 || sector[511] != 0xAA {
            return Err(FsError::NoFilesystem);
        }
        let bytes_per_sector = le16(sector, 11);
        let sectors_per_cluster = u32::from(sector[13]);
        let reserved_sectors = le16(sector, 14);
        let num_fats = u32::from(sector[16]);
        let root_entries = le16(sector, 17);
        let total16 = le16(sector, 19);
        let fat16 = le16(sector, 22);
        let total32 = le32(sector, 32);
        let fat32 = le32(sector, 36);

        if !matches!(bytes_per_sector, 512 | 1024 | 2048 | 4096) {
            return Err(FsError::NoFilesystem);
        }
        if sectors_per_cluster == 0 || !sectors_per_cluster.is_power_of_two() || sectors_per_cluster > 128 {
            return Err(FsError::Corrupt);
        }
        if bytes_per_sector * sectors_per_cluster > 65_536 {
            return Err(FsError::Corrupt);
        }
        if reserved_sectors == 0 || num_fats == 0 || num_fats > 2 {
            return Err(FsError::Corrupt);
        }
        let total_sectors = if total16 != 0 { total16 } else { total32 };
        let fat_sectors = if fat16 != 0 { fat16 } else { fat32 };
        if total_sectors == 0 || fat_sectors == 0 {
            return Err(FsError::Corrupt);
        }
        let root_dir_sectors = (root_entries * 32).div_ceil(bytes_per_sector);
        let overhead = reserved_sectors
            .checked_add(num_fats.checked_mul(fat_sectors).ok_or(FsError::Corrupt)?)
            .and_then(|v| v.checked_add(root_dir_sectors))
            .ok_or(FsError::Corrupt)?;
        if overhead >= total_sectors {
            return Err(FsError::Corrupt);
        }
        let cluster_count = (total_sectors - overhead) / sectors_per_cluster;
        if cluster_count == 0 {
            return Err(FsError::Corrupt);
        }
        let kind = FatKind::from_cluster_count(cluster_count);
        let (root_cluster, fsinfo_sector) = if kind == FatKind::Fat32 {
            // FAT32: no fixed root, a 32-bit FAT size, a root cluster.
            if root_entries != 0 || fat16 != 0 {
                return Err(FsError::Corrupt);
            }
            let rc = le32(sector, 44);
            if rc < 2 || rc > cluster_count + 1 {
                return Err(FsError::Corrupt);
            }
            (rc, le16(sector, 48))
        } else {
            if root_entries == 0 {
                return Err(FsError::Corrupt);
            }
            (0, 0)
        };
        // The FAT must be big enough to describe every cluster.
        let need = (u64::from(cluster_count) + 2) * u64::from(kind.bits());
        let need_bytes = need.div_ceil(8);
        let fat_bytes = u64::from(fat_sectors) * u64::from(bytes_per_sector);
        if fat_bytes < need_bytes {
            return Err(FsError::Corrupt);
        }
        let bps = u64::from(bytes_per_sector);
        let fat_start = u64::from(reserved_sectors) * bps;
        let root_dir_start = fat_start + u64::from(num_fats) * fat_bytes;
        let root_dir_bytes = u64::from(root_dir_sectors) * bps;
        Ok(Self {
            kind,
            bytes_per_sector,
            sectors_per_cluster,
            reserved_sectors,
            num_fats,
            root_entries,
            fat_sectors,
            total_sectors,
            root_cluster,
            fsinfo_sector,
            cluster_count,
            cluster_bytes: bytes_per_sector * sectors_per_cluster,
            fat_start,
            fat_bytes,
            root_dir_start,
            root_dir_bytes,
            data_start: root_dir_start + root_dir_bytes,
        })
    }

    /// Byte offset of data cluster `c` (`2..=cluster_count + 1`).
    #[must_use]
    pub fn cluster_offset(&self, c: u32) -> u64 {
        self.data_start + u64::from(c - 2) * u64::from(self.cluster_bytes)
    }

    #[must_use]
    pub fn valid_cluster(&self, c: u32) -> bool {
        (2..=self.cluster_count + 1).contains(&c)
    }
}

// ---------------------------------------------------------------- DOS time

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Unix seconds -> `(date, time, tenths_of_a_second_remainder)`. The range is
/// 1980-01-01 .. 2107-12-31; outside it the value is clamped.
#[must_use]
pub fn unix_to_dos(secs: u64) -> (u16, u16, u8) {
    const FIRST: u64 = 315_532_800; // 1980-01-01T00:00:00Z
    const LAST: u64 = 4_354_819_199; // 2107-12-31T23:59:59Z
    let secs = secs.clamp(FIRST, LAST);
    let days = (secs / 86_400) as i64;
    let rem = (secs % 86_400) as u32;
    let (y, m, d) = civil_from_days(days);
    let date = (((y - 1980) as u16) << 9) | ((m as u16) << 5) | d as u16;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let time = ((h as u16) << 11) | ((mi as u16) << 5) | (s / 2) as u16;
    // Time has 2-second resolution; creation carries the odd second here.
    (date, time, if s % 2 == 1 { 100 } else { 0 })
}

/// `(date, time)` -> Unix seconds; `None` for the unset value (date 0) or an
/// impossible date.
#[must_use]
pub fn dos_to_unix(date: u16, time: u16) -> Option<u64> {
    if date == 0 {
        return None;
    }
    let y = i64::from(date >> 9) + 1980;
    let m = i64::from((date >> 5) & 0xF);
    let d = i64::from(date & 0x1F);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let h = i64::from(time >> 11);
    let mi = i64::from((time >> 5) & 0x3F);
    let s = i64::from(time & 0x1F) * 2;
    if h > 23 || mi > 59 || s > 59 {
        return None;
    }
    let secs = days_from_civil(y, m, d) * 86_400 + h * 3600 + mi * 60 + s;
    u64::try_from(secs).ok()
}

// --------------------------------------------------------------- short names

/// The long-name checksum: rotate-right-and-add over the 11 name bytes.
#[must_use]
pub fn lfn_checksum(short: &[u8; 11]) -> u8 {
    let mut sum: u8 = 0;
    for &b in short {
        sum = sum.rotate_right(1).wrapping_add(b);
    }
    sum
}

fn is_short_char(c: u8) -> bool {
    c.is_ascii_uppercase()
        || c.is_ascii_digit()
        || b"!#$%&'()-@^_`{}~".contains(&c)
}

/// Is `name` something a file may be called at all? (No reserved characters,
/// no control characters, no trailing dot or space, 1..=255 UTF-16 units, and
/// not `.`/`..`.)
#[must_use]
pub fn valid_name(name: &str) -> bool {
    if name.is_empty() || name == "." || name == ".." {
        return false;
    }
    if name.encode_utf16().count() > MAX_NAME_UNITS {
        return false;
    }
    if name.ends_with('.') || name.ends_with(' ') || name.starts_with(' ') {
        return false;
    }
    !name
        .chars()
        .any(|c| (c as u32) < 0x20 || c as u32 == 0x7F || "\"*/:<>?\\|".contains(c))
}

/// Render an on-disk 8.3 name (with the NT lower-case flags, byte 12) as text.
#[must_use]
pub fn short_display(short: &[u8; 11], nt: u8) -> String {
    let mut s = String::new();
    for (i, &b) in short[..8].iter().enumerate() {
        if b == b' ' {
            break;
        }
        let b = if i == 0 && b == 0x05 { 0xE5 } else { b };
        s.push(if nt & 0x08 != 0 { char::from(b.to_ascii_lowercase()) } else { char::from(b) });
    }
    if short[8] != b' ' {
        s.push('.');
        for &b in &short[8..] {
            if b == b' ' {
                break;
            }
            s.push(if nt & 0x10 != 0 { char::from(b.to_ascii_lowercase()) } else { char::from(b) });
        }
    }
    s
}

fn case_class(s: &str) -> Option<bool> {
    // Some(true) = has lowercase, Some(false) = no lowercase; None = mixed.
    let lower = s.bytes().any(|b| b.is_ascii_lowercase());
    let upper = s.bytes().any(|b| b.is_ascii_uppercase());
    if lower && upper { None } else { Some(lower) }
}

/// If `name` is representable as a bare 8.3 entry — each of base and extension
/// being all one case — return the stored bytes and the NT case byte.
#[must_use]
pub fn encode_short_exact(name: &str) -> Option<([u8; 11], u8)> {
    if !name.is_ascii() {
        return None;
    }
    let (base, ext) = match name.rfind('.') {
        Some(i) => (&name[..i], &name[i + 1..]),
        None => (name, ""),
    };
    if base.is_empty() || base.len() > 8 || ext.len() > 3 || base.contains('.') {
        return None;
    }
    let ok = |s: &str| s.bytes().all(|b| is_short_char(b.to_ascii_uppercase()));
    if !ok(base) || !ok(ext) {
        return None;
    }
    let base_lower = case_class(base)?;
    let ext_lower = case_class(ext)?;
    let mut out = [b' '; 11];
    for (i, b) in base.bytes().enumerate() {
        out[i] = b.to_ascii_uppercase();
    }
    for (i, b) in ext.bytes().enumerate() {
        out[8 + i] = b.to_ascii_uppercase();
    }
    let nt = (if base_lower { 0x08 } else { 0 }) | (if ext_lower && !ext.is_empty() { 0x10 } else { 0 });
    Some((out, nt))
}

/// The numeric-tail alias (`LONGNA~1.TXT`) for a name that needs long-name
/// slots. `n` is the tail number, 1-based.
#[must_use]
pub fn make_alias(name: &str, n: u32) -> [u8; 11] {
    let trimmed = name.trim_start_matches('.');
    let (base, ext) = match trimmed.rfind('.') {
        Some(i) => (&trimmed[..i], &trimmed[i + 1..]),
        None => (trimmed, ""),
    };
    let clean = |s: &str, max: usize| -> Vec<u8> {
        let mut v = Vec::new();
        for c in s.chars() {
            if c == ' ' || c == '.' {
                continue;
            }
            let b = if c.is_ascii() { (c as u8).to_ascii_uppercase() } else { b'_' };
            v.push(if is_short_char(b) { b } else { b'_' });
            if v.len() == max {
                break;
            }
        }
        v
    };
    let mut tail = Vec::new();
    {
        let mut digits = Vec::new();
        let mut k = n.max(1);
        while k > 0 {
            digits.push(b'0' + (k % 10) as u8);
            k /= 10;
        }
        tail.push(b'~');
        tail.extend(digits.iter().rev());
    }
    let b = clean(base, 8usize.saturating_sub(tail.len()));
    let e = clean(ext, 3);
    let mut out = [b' '; 11];
    for (i, &c) in b.iter().chain(tail.iter()).enumerate() {
        out[i] = c;
    }
    for (j, &c) in e.iter().enumerate() {
        out[8 + j] = c;
    }
    out
}

// ---------------------------------------------------------- long-name slots

/// How many long-name slots `name` needs.
#[must_use]
pub fn lfn_slots_needed(name: &str) -> usize {
    name.encode_utf16().count().div_ceil(LFN_CHARS_PER_SLOT)
}

/// Build the long-name slots for `name`, in on-disk order (highest ordinal
/// first, flagged `0x40`), for a short entry whose bytes are `short`.
#[must_use]
pub fn build_lfn(name: &str, short: &[u8; 11]) -> Vec<[u8; DIR_ENTRY_SIZE]> {
    let units: Vec<u16> = name.encode_utf16().collect();
    let count = units.len().div_ceil(LFN_CHARS_PER_SLOT);
    let sum = lfn_checksum(short);
    let mut out = Vec::with_capacity(count);
    for slot in (0..count).rev() {
        let mut e = [0u8; DIR_ENTRY_SIZE];
        e[0] = (slot as u8 + 1) | if slot + 1 == count { 0x40 } else { 0 };
        e[11] = ATTR_LONG_NAME;
        e[13] = sum;
        for k in 0..LFN_CHARS_PER_SLOT {
            let idx = slot * LFN_CHARS_PER_SLOT + k;
            let u = match idx.cmp(&units.len()) {
                core::cmp::Ordering::Less => units[idx],
                core::cmp::Ordering::Equal => 0x0000,
                core::cmp::Ordering::Greater => 0xFFFF,
            };
            let off = match k {
                0..=4 => 1 + k * 2,
                5..=10 => 14 + (k - 5) * 2,
                _ => 28 + (k - 11) * 2,
            };
            e[off..off + 2].copy_from_slice(&u.to_le_bytes());
        }
        out.push(e);
    }
    out
}

/// The 13 UTF-16 units of one long-name slot.
#[must_use]
pub fn lfn_units(e: &[u8; DIR_ENTRY_SIZE]) -> [u16; LFN_CHARS_PER_SLOT] {
    let mut out = [0u16; LFN_CHARS_PER_SLOT];
    for (k, u) in out.iter_mut().enumerate() {
        let off = match k {
            0..=4 => 1 + k * 2,
            5..=10 => 14 + (k - 5) * 2,
            _ => 28 + (k - 11) * 2,
        };
        *u = u16::from_le_bytes([e[off], e[off + 1]]);
    }
    out
}

/// Case-insensitive name equality (Unicode simple lower-casing, which is what
/// is needed to make `README.TXT` and `readme.txt` the same file).
#[must_use]
pub fn names_equal(a: &str, b: &str) -> bool {
    let mut x = a.chars().flat_map(char::to_lowercase);
    let mut y = b.chars().flat_map(char::to_lowercase);
    loop {
        match (x.next(), y.next()) {
            (None, None) => return true,
            (Some(p), Some(q)) if p == q => {}
            _ => return false,
        }
    }
}
