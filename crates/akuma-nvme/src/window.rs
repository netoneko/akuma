//! The partition window: the only range of the disk this driver will touch.

/// A partition as a range of logical blocks on the namespace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub first_lba: u64,
    /// Blocks in the partition.
    pub lbas: u64,
    pub lba_bytes: u32,
}

impl Window {
    /// The window for GPT's inclusive `[first, last]`, refused if it does not
    /// lie inside a namespace of `ns_lbas` blocks.
    #[must_use]
    pub const fn new(first: u64, last: u64, ns_lbas: u64, lba_bytes: u32) -> Option<Self> {
        if first > last || last >= ns_lbas || lba_bytes == 0 {
            return None;
        }
        Some(Self { first_lba: first, lbas: last - first + 1, lba_bytes })
    }

    /// Bytes in the partition.
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.lbas * self.lba_bytes as u64
    }

    /// `len` bytes at partition-relative `offset`: inside the window?
    #[must_use]
    pub const fn fits(&self, offset: u64, len: usize) -> bool {
        match offset.checked_add(len as u64) {
            Some(end) => end <= self.bytes(),
            None => false,
        }
    }

    /// A partition-relative LBA run as an absolute namespace LBA, or `None`
    /// if any block of it falls outside the window. This is the check every
    /// command passes before it is built.
    #[must_use]
    pub const fn absolute(&self, rel_lba: u64, blocks: u32) -> Option<u64> {
        match rel_lba.checked_add(blocks as u64) {
            Some(end) if blocks > 0 && end <= self.lbas => Some(self.first_lba + rel_lba),
            _ => None,
        }
    }
}
