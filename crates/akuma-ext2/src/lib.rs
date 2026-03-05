#![no_std]
//! Ext2 Filesystem Implementation
//!
//! A full ext2 filesystem driver for `no_std` environments with read/write support.
//! The caller provides a `BlockDevice` implementation and a timestamp callback.

extern crate alloc;

mod ext2;

pub use ext2::Ext2Filesystem;

/// Trait abstracting raw block device I/O.
#[allow(clippy::result_unit_err)]
pub trait BlockDevice: Send + Sync {
    /// Read `buf.len()` bytes starting at byte offset `offset`.
    fn read_bytes(&self, offset: u64, buf: &mut [u8]) -> Result<(), ()>;

    /// Write `data` starting at byte offset `offset`.
    fn write_bytes(&self, offset: u64, data: &[u8]) -> Result<(), ()>;
}

#[cfg(test)]
mod tests;
