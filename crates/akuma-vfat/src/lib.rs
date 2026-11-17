#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
//! FAT12 / FAT16 / FAT32 with VFAT long file names.
//!
//! Written clean-room from the Microsoft "FAT32 File System Specification"
//! (the BPB, the FAT entry encodings, the 32-byte directory entry, the long
//! name entry and its checksum) and checked against `mkfs.fat`/`fsck.fat` as
//! black boxes. No other implementation's source was read.
//!
//! The reason this exists is the EFI System Partition: GRUB's config lives on a
//! FAT volume, and the kernel had no way to edit it. The crate is a
//! [`akuma_vfs::Filesystem`] over a caller-supplied [`BlockDevice`]; it has no
//! `unsafe`.
//!
//! Layout: [`layout`] is pure (BPB parse, DOS time, short names, checksum),
//! [`mkfs`] formats a device, [`fs`] is the filesystem. What is *not*
//! supported is listed in `docs/reference/subsystems/vfat.md`.

extern crate alloc;

pub mod fs;
pub mod layout;
pub mod mkfs;

pub use fs::Vfat;
pub use layout::{FatKind, Geometry};
pub use mkfs::{FormatOptions, format};

/// Raw, byte-addressed block I/O. Same shape as `akuma_ext2::BlockDevice`, so a
/// kernel device adapter implements both with the same body.
#[allow(clippy::result_unit_err)]
pub trait BlockDevice: Send + Sync {
    /// Read `buf.len()` bytes starting at byte offset `offset`.
    fn read_bytes(&self, offset: u64, buf: &mut [u8]) -> Result<(), ()>;
    /// Write `data` starting at byte offset `offset`.
    fn write_bytes(&self, offset: u64, data: &[u8]) -> Result<(), ()>;
    /// Durability barrier; see `akuma_ext2::BlockDevice::flush`.
    fn flush(&self) -> Result<(), ()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
