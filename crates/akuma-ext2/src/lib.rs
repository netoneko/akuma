#![cfg_attr(not(test), no_std)]
// Reached 2026-08-31, the end of `docs/archive/AKUMA_EXT2_CLEANUP.md`'s plan.
// The crate held 18 production `unsafe` sites in three families: 14 `repr(C)`
// struct blits over raw byte buffers (§2, replaced by an explicit
// offset-based parse/serialize codec), a `copy_nonoverlapping` symlink pair
// (§3, subsumed by the `Inode` codec), and three `force_unlock_write` calls
// (§4) whose contract — "no guard for this lock exists" — was a whole-program
// property no crate could check. The last three left when the state moved to
// `akuma-locks-rw-cell`, where release *is* the recovery operation.
#![forbid(unsafe_code)]
//! Ext2 Filesystem Implementation
//!
//! A full ext2 filesystem driver for `no_std` environments with read/write support.
//! The caller provides a `BlockDevice` implementation and a timestamp callback.

extern crate alloc;

mod ext2;

pub use ext2::{
    DEFERRED_DRAIN_CALLS, DEFERRED_DRAIN_FREED, DEFERRED_DRAIN_SKIPPED, DEFERRED_FREE_LEAKED,
    DEFERRED_FREE_PENDING, Ext2Filesystem, cache_occupancy, cache_stats, deferred_free_pending,
    init_inode_freed_hook, set_cache_cap_bytes,
};

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
