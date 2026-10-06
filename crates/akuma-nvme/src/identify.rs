//! Identify Controller (CNS 1) and Identify Namespace (CNS 0) — the few fields
//! a block driver needs from each 4 KiB page.

/// Bytes an Identify returns.
pub const PAGE_BYTES: usize = 4096;

/// Identify Controller, the fields this driver uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Controller {
    pub vid: u16,
    pub serial: [u8; 20],
    pub model: [u8; 40],
    pub firmware: [u8; 8],
    /// Maximum Data Transfer Size, as a power of two in units of the minimum
    /// page size; 0 = no limit.
    pub mdts: u8,
    /// Number of namespaces.
    pub nn: u32,
    /// A volatile write cache is present (so `Flush` matters).
    pub vwc: bool,
}

impl Controller {
    #[must_use]
    pub fn parse(page: &[u8]) -> Option<Self> {
        if page.len() < PAGE_BYTES {
            return None;
        }
        Some(Self {
            vid: u16::from_le_bytes([page[0], page[1]]),
            serial: page[4..24].try_into().ok()?,
            model: page[24..64].try_into().ok()?,
            firmware: page[64..72].try_into().ok()?,
            mdts: page[77],
            nn: u32::from_le_bytes(page[516..520].try_into().ok()?),
            vwc: page[525] & 1 != 0,
        })
    }

    /// The largest transfer one command may carry, given 4 KiB pages; `None`
    /// means the controller states no limit.
    #[must_use]
    pub const fn max_transfer_bytes(&self) -> Option<u64> {
        if self.mdts == 0 || self.mdts > 20 { None } else { Some(4096u64 << self.mdts) }
    }
}

/// Identify Namespace, the fields this driver uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Namespace {
    /// Namespace size, in logical blocks.
    pub size_lbas: u64,
    /// Bytes per logical block in the active format.
    pub lba_bytes: u32,
    /// Metadata bytes per block in the active format (this driver needs 0).
    pub metadata_bytes: u16,
}

impl Namespace {
    /// `None` for an inactive namespace (size 0), a format index past the
    /// table, or a block size outside 512 B ..= 64 KiB.
    #[must_use]
    pub fn parse(page: &[u8]) -> Option<Self> {
        if page.len() < PAGE_BYTES {
            return None;
        }
        let size_lbas = u64::from_le_bytes(page[0..8].try_into().ok()?);
        if size_lbas == 0 {
            return None;
        }
        let nlbaf = page[25] as usize; // 0-based count
        let flbas = page[26];
        // Format index: bits 3:0, extended by bits 6:5 when more than 16
        // formats exist (NVMe 2.0).
        let index = (flbas & 0x0f) as usize | (((flbas >> 5) & 0x3) as usize) << 4;
        if index > nlbaf || index >= 64 {
            return None;
        }
        let f = 128 + 4 * index;
        let lbaf = u32::from_le_bytes(page[f..f + 4].try_into().ok()?);
        let lbads = (lbaf >> 16) & 0xff;
        if !(9..=16).contains(&lbads) {
            return None;
        }
        Some(Self { size_lbas, lba_bytes: 1 << lbads, metadata_bytes: (lbaf & 0xffff) as u16 })
    }
}

/// An Identify text field (space-padded ASCII) without its padding, for logs.
#[must_use]
pub fn text(field: &[u8]) -> &str {
    let s = core::str::from_utf8(field).unwrap_or("?");
    s.trim_end_matches([' ', '\0']).trim_start()
}
