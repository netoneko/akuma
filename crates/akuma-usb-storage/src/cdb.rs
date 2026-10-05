//! SCSI Command Descriptor Block builders.
//!
//! The six commands the disk driver needs to size, read and write the drive.
//! Each returns a [`Command`] with the CDB zero-padded to 16 bytes, its real
//! length, and the data-phase shape.

use crate::{Command, Direction};

/// Opcodes (SPC-4 / SBC-3).
mod op {
    pub const TEST_UNIT_READY: u8 = 0x00;
    pub const REQUEST_SENSE: u8 = 0x03;
    pub const INQUIRY: u8 = 0x12;
    pub const READ_CAPACITY_10: u8 = 0x25;
    pub const READ_10: u8 = 0x28;
    pub const WRITE_10: u8 = 0x2a;
    /// SBC-3: force the medium's volatile cache to media.
    pub const SYNCHRONIZE_CACHE_10: u8 = 0x35;
}

fn cmd(cdb_bytes: &[u8], data_len: u32, direction: Direction) -> Command {
    let mut cdb = [0u8; 16];
    cdb[..cdb_bytes.len()].copy_from_slice(cdb_bytes);
    Command {
        cdb,
        cdb_len: cdb_bytes.len() as u8,
        data_len,
        direction,
    }
}

/// `TEST UNIT READY` — no data. A "passed" CSW means the medium is ready.
#[must_use]
pub fn test_unit_ready() -> Command {
    cmd(&[op::TEST_UNIT_READY, 0, 0, 0, 0, 0], 0, Direction::None)
}

/// `REQUEST SENSE` — 18 bytes of fixed-format sense data, IN.
#[must_use]
pub fn request_sense() -> Command {
    cmd(&[op::REQUEST_SENSE, 0, 0, 0, 18, 0], 18, Direction::In)
}

/// `INQUIRY` — `alloc_len` bytes of standard inquiry data, IN. 36 is enough for
/// the device type + version fields the driver checks.
#[must_use]
pub fn inquiry(alloc_len: u8) -> Command {
    cmd(
        &[op::INQUIRY, 0, 0, 0, alloc_len, 0],
        u32::from(alloc_len),
        Direction::In,
    )
}

/// `READ CAPACITY (10)` — 8 bytes: last LBA + block length, both big-endian, IN.
#[must_use]
pub fn read_capacity_10() -> Command {
    cmd(
        &[op::READ_CAPACITY_10, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        8,
        Direction::In,
    )
}

/// `READ (10)` — `blocks` logical blocks starting at `lba`, IN. `block_len` is
/// the drive's logical block size (from `READ CAPACITY`), used only to size the
/// data phase.
#[must_use]
pub fn read_10(lba: u32, blocks: u16, block_len: u32) -> Command {
    let l = lba.to_be_bytes();
    let n = blocks.to_be_bytes();
    cmd(
        &[op::READ_10, 0, l[0], l[1], l[2], l[3], 0, n[0], n[1], 0],
        u32::from(blocks) * block_len,
        Direction::In,
    )
}

/// `WRITE (10)` — `blocks` logical blocks starting at `lba`, OUT.
#[must_use]
pub fn write_10(lba: u32, blocks: u16, block_len: u32) -> Command {
    let l = lba.to_be_bytes();
    let n = blocks.to_be_bytes();
    cmd(
        &[op::WRITE_10, 0, l[0], l[1], l[2], l[3], 0, n[0], n[1], 0],
        u32::from(blocks) * block_len,
        Direction::Out,
    )
}

/// `SYNCHRONIZE CACHE (10)` — the durability barrier.
///
/// The drive (and any
/// bridge behind it) must have its volatile write cache flushed for the given
/// range before the command reports `Passed`; `blocks == 0` means "from `lba`
/// to the end of the medium", and `lba == 0` with `blocks == 0` is therefore
/// the whole medium. No data phase.
///
/// Without this, a `WRITE (10)`'s CSW acknowledgement means only "the command
/// landed" — the bytes may still sit in the drive's volatile cache, and a
/// reset (which can cut port power) loses them in whatever order the drive
/// pleases. That is exactly what the 2026-10-05 reboot observed: renames
/// lost, one freed inode half-persisted
/// (`docs/archive/AKUMA_AMD64_EXT2_CROSS_FILE_CORRUPTION.md` §11).
///
/// Bridges that do not implement the command answer `ILLEGAL REQUEST`; the
/// caller decides whether that is fatal (it is not — but it must be *seen*,
/// because the guarantee it wanted does not exist on that hardware).
#[must_use]
pub fn synchronize_cache_10(lba: u32, blocks: u16) -> Command {
    let l = lba.to_be_bytes();
    let n = blocks.to_be_bytes();
    cmd(
        &[op::SYNCHRONIZE_CACHE_10, 0, l[0], l[1], l[2], l[3], 0, n[0], n[1], 0],
        0,
        Direction::None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synchronize_cache_10_whole_medium_is_lba0_blocks0_no_data() {
        let c = synchronize_cache_10(0, 0);
        assert_eq!(c.cdb[0], 0x35);
        assert_eq!(c.cdb_len, 10);
        assert_eq!(c.data_len, 0);
        assert_eq!(c.direction, Direction::None);
        // LBA (bytes 2..=5) and block count (bytes 7..=8) all zero.
        assert!(c.cdb[2..=5].iter().all(|&b| b == 0));
        assert!(c.cdb[7..=8].iter().all(|&b| b == 0));
    }

    #[test]
    fn synchronize_cache_10_encodes_lba_and_count_big_endian() {
        let c = synchronize_cache_10(0x1122_3344, 0x5566);
        assert_eq!(&c.cdb[2..=5], &[0x11, 0x22, 0x33, 0x44]);
        assert_eq!(&c.cdb[7..=8], &[0x55, 0x66]);
        assert_eq!(c.data_len, 0);
        assert_eq!(c.direction, Direction::None);
    }
}
