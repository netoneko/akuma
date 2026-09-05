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
