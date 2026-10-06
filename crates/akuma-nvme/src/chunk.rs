//! Byte-addressed I/O over a block device: which blocks the next command
//! covers, and which bytes of them belong to the caller.
//!
//! `akuma-ext2` asks for `(offset, len)` in bytes; the disk moves whole logical
//! blocks. [`next`] is one step of that loop — the arithmetic `xhci.rs`'s
//! `read_bytes`/`write_bytes` do inline, here with tests.

/// One command's worth of a byte transfer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chunk {
    /// First logical block, relative to whatever the offset was relative to.
    pub lba: u64,
    /// Blocks the command moves.
    pub blocks: u32,
    /// Where the caller's bytes start inside the first block.
    pub within: usize,
    /// How many of the caller's bytes this command carries.
    pub take: usize,
}

impl Chunk {
    /// Bytes the command moves.
    #[must_use]
    pub const fn span(&self, block: u32) -> usize {
        self.blocks as usize * block as usize
    }

    /// A write of this chunk must first read the blocks back: the caller's
    /// bytes do not cover them completely.
    #[must_use]
    pub const fn partial(&self, block: u32) -> bool {
        self.within != 0 || self.within + self.take < self.span(block)
    }
}

/// The chunk covering byte `offset + done` onward, for a transfer of `len`
/// bytes, in blocks of `block` bytes, at most `max_blocks` per command.
///
/// # Panics
/// If `block` or `max_blocks` is 0, or `done >= len` — caller bugs, not disk
/// states.
#[must_use]
pub fn next(offset: u64, done: usize, len: usize, block: u32, max_blocks: u32) -> Chunk {
    assert!(block > 0 && max_blocks > 0 && done < len);
    let bl = u64::from(block);
    let cur = offset + done as u64;
    let lba = cur / bl;
    let within = (cur % bl) as usize;
    let remaining = len - done;
    let blocks = (within + remaining).div_ceil(block as usize).clamp(1, max_blocks as usize) as u32;
    let span = blocks as usize * block as usize;
    Chunk { lba, blocks, within, take: (span - within).min(remaining) }
}
