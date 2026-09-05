// Unsafe-free by design: `forbid` (not `deny`) so no module can opt back in.
#![forbid(unsafe_code)]
#![no_std]
//! USB Mass Storage, Bulk-Only Transport (USB MSC BOT 1.0) — pure wire format.
//!
//! The amd64 bare-metal target's only spare disk is a SATA drive in a USB-to-SATA
//! enclosure (`docs/archive/AKUMA_SELF_HEALING_PORT.md`). The enclosure presents
//! the standard BOT interface: class 0x08, subclass 0x06 (SCSI transparent),
//! protocol 0x50 (Bulk-Only). Every operation is
//!
//! 1. a **31-byte Command Block Wrapper** ([`Cbw`]) on the bulk OUT endpoint,
//!    carrying a SCSI CDB,
//! 2. an optional **data phase** on the bulk IN or OUT endpoint, then
//! 3. a **13-byte Command Status Wrapper** ([`Csw`]) on the bulk IN endpoint.
//!
//! This crate is only the encode/decode: the [`Cbw`]/[`Csw`] structs, the small
//! SCSI CDB set ([`cdb`]) the driver needs to size, read and write the disk, and
//! the response parsers ([`InquiryData`], [`ReadCapacity10`], [`RequestSense`]).
//! The three bulk transfers, the tag bookkeeping, and STALL recovery are the
//! caller's — `amd64/src/usb_storage.rs` over `akuma-xhci`.

pub mod cdb;

/// `dCBWSignature` — "USBC" little-endian.
pub const CBW_SIGNATURE: u32 = 0x4342_5355;
/// `dCSWSignature` — "USBS" little-endian.
pub const CSW_SIGNATURE: u32 = 0x5342_5355;

/// A CBW is always 31 bytes on the wire.
pub const CBW_LEN: usize = 31;
/// A CSW is always 13 bytes on the wire.
pub const CSW_LEN: usize = 13;

/// Direction of a command's data phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// No data phase.
    None,
    /// Data moves device → host (bulk IN) — `READ`, `INQUIRY`, `READ CAPACITY`.
    In,
    /// Data moves host → device (bulk OUT) — `WRITE`.
    Out,
}

/// A ready-to-issue command: the SCSI CDB, how long it is, and the shape of the
/// data phase. [`cdb`]'s builders return one of these so the glue has a single
/// thing to drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Command {
    /// The CDB bytes, zero-padded to 16 (the CBW's `CBWCB` field width).
    pub cdb: [u8; 16],
    /// Meaningful CDB length (6, 10, …) — goes in `bCBWCBLength`.
    pub cdb_len: u8,
    /// Expected data-phase byte count — goes in `dCBWDataTransferLength`.
    pub data_len: u32,
    pub direction: Direction,
}

/// The Command Block Wrapper (USB MSC BOT §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cbw {
    /// `dCBWTag` — echoed in the CSW; the caller increments it per command so a
    /// stale CSW from a previous command is detectable.
    pub tag: u32,
    pub command: Command,
    /// Logical Unit Number — 0 for these single-drive enclosures.
    pub lun: u8,
}

impl Cbw {
    /// The 31 wire bytes.
    #[must_use]
    pub fn encode(&self) -> [u8; CBW_LEN] {
        let mut b = [0u8; CBW_LEN];
        b[0..4].copy_from_slice(&CBW_SIGNATURE.to_le_bytes());
        b[4..8].copy_from_slice(&self.tag.to_le_bytes());
        b[8..12].copy_from_slice(&self.command.data_len.to_le_bytes());
        b[12] = match self.command.direction {
            Direction::In => 0x80,
            Direction::Out | Direction::None => 0x00,
        };
        b[13] = self.lun & 0x0f;
        b[14] = self.command.cdb_len & 0x1f;
        b[15..31].copy_from_slice(&self.command.cdb);
        b
    }
}

/// `bCSWStatus` (USB MSC BOT §5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CswStatus {
    /// Command Passed ("good status").
    Passed,
    /// Command Failed — the caller issues `REQUEST SENSE` to learn why.
    Failed,
    /// Phase Error — the caller must reset the interface (Bulk-Only Mass
    /// Storage Reset + clear both endpoint STALLs) before the next command.
    PhaseError,
    /// A reserved / unknown status byte.
    Unknown(u8),
}

/// The Command Status Wrapper (USB MSC BOT §5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Csw {
    pub tag: u32,
    /// `dCSWDataResidue` — expected data length minus what was actually moved.
    pub residue: u32,
    pub status: CswStatus,
}

impl Csw {
    /// Parse the 13 wire bytes. `None` when the buffer is short or the
    /// signature is wrong (a wrong signature means the bulk pipe is out of sync
    /// and the interface must be reset).
    #[must_use]
    pub fn parse(b: &[u8]) -> Option<Self> {
        if b.len() < CSW_LEN {
            return None;
        }
        let sig = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        if sig != CSW_SIGNATURE {
            return None;
        }
        Some(Self {
            tag: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
            residue: u32::from_le_bytes([b[8], b[9], b[10], b[11]]),
            status: match b[12] {
                0 => CswStatus::Passed,
                1 => CswStatus::Failed,
                2 => CswStatus::PhaseError,
                other => CswStatus::Unknown(other),
            },
        })
    }

    /// The command succeeded and the CSW belongs to the command with `tag`.
    #[must_use]
    pub fn is_good(&self, tag: u32) -> bool {
        self.tag == tag && self.status == CswStatus::Passed
    }
}

// ===========================================================================
// SCSI response parsers
// ===========================================================================

/// The parts of a `INQUIRY` standard-data response the driver cares about
/// (SPC-4 §6.4). Response is at least 36 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InquiryData {
    /// Peripheral Device Type — 0x00 is "direct-access block device" (a disk),
    /// which is the only thing this driver supports.
    pub device_type: u8,
    /// Removable Media bit.
    pub removable: bool,
    /// SCSI version byte.
    pub version: u8,
}

impl InquiryData {
    #[must_use]
    pub fn parse(b: &[u8]) -> Option<Self> {
        if b.len() < 8 {
            return None;
        }
        Some(Self {
            device_type: b[0] & 0x1f,
            removable: b[1] & 0x80 != 0,
            version: b[2],
        })
    }

    /// A direct-access block device.
    #[must_use]
    pub fn is_disk(&self) -> bool {
        self.device_type == 0x00
    }
}

/// `READ CAPACITY (10)` response (SBC-3 §5.16): last LBA and block length, both
/// big-endian.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadCapacity10 {
    /// The LBA of the **last** addressable block.
    pub last_lba: u32,
    /// Bytes per logical block (512 for these enclosures, even on 4K-native
    /// drives — the bridge presents 512-byte logical blocks).
    pub block_len: u32,
}

impl ReadCapacity10 {
    #[must_use]
    pub fn parse(b: &[u8]) -> Option<Self> {
        if b.len() < 8 {
            return None;
        }
        Some(Self {
            last_lba: u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
            block_len: u32::from_be_bytes([b[4], b[5], b[6], b[7]]),
        })
    }

    /// Total addressable blocks (`last_lba + 1`).
    #[must_use]
    pub fn block_count(&self) -> u64 {
        u64::from(self.last_lba) + 1
    }

    /// Total capacity in bytes.
    #[must_use]
    pub fn capacity_bytes(&self) -> u64 {
        self.block_count() * u64::from(self.block_len)
    }
}

/// The parts of a `REQUEST SENSE` fixed-format response the driver logs
/// (SPC-4 §4.5.3). Response is 18 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestSense {
    /// Sense Key (byte 2, low nibble): 0 = No Sense, 2 = Not Ready, 6 = Unit
    /// Attention (media changed), …
    pub sense_key: u8,
    /// Additional Sense Code (byte 12).
    pub asc: u8,
    /// Additional Sense Code Qualifier (byte 13).
    pub ascq: u8,
}

impl RequestSense {
    #[must_use]
    pub fn parse(b: &[u8]) -> Option<Self> {
        if b.len() < 14 {
            return None;
        }
        Some(Self { sense_key: b[2] & 0x0f, asc: b[12], ascq: b[13] })
    }

    /// The classic "spinning up, not ready yet" — sense key 2, ASC 0x04 (Logical
    /// Unit Not Ready), ASCQ 0x01 (Becoming Ready). The driver retries on this.
    #[must_use]
    pub fn is_becoming_ready(&self) -> bool {
        self.sense_key == 0x02 && self.asc == 0x04
    }
}
