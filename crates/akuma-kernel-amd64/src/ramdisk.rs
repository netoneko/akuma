//! A block device that is simply memory.
//!
//! On a machine with no storage driver, this is how the kernel gets a root
//! filesystem: GRUB reads the ext2 image off whatever it booted from — a disk
//! it already knows how to read — and leaves it in RAM, then tells us where via
//! a multiboot2 module tag. `akuma-ext2` never learns the difference.
//!
//! It is a real filesystem in every way that matters: real superblock, real
//! inodes, real directory traversal, real file reads. What it is not is
//! **persistent** — writes land in RAM and are gone at the next power cycle,
//! which is worth remembering before concluding that a file "saved".
//!
//! # The frames must be reserved
//!
//! Nothing in the multiboot2 memory map marks a module's pages as taken; the
//! loader reports them as ordinary available memory. `mem::init_reserving` is
//! what keeps the physical allocator away from them, and without it the kernel
//! hands out the pages holding its own root filesystem — which corrupts later,
//! elsewhere, and looks like a filesystem bug.

use akuma_ext2::BlockDevice;


/// A contiguous span of physical memory, read and written as a block device.
///
/// The span is an [`akuma_phys::RamSpan`]: whoever made it stated, once, that
/// the memory is this disk's alone (`multiboot2`'s module reservation). The lock
/// gives writes the `&mut` the span requires; `BlockDevice` takes `&self`.
pub struct RamDisk {
    span: spinning_top::Spinlock<akuma_phys::RamSpan>,
    len: usize,
}

impl RamDisk {
    /// Wrap `span`.
    #[must_use]
    pub fn new(span: akuma_phys::RamSpan) -> Self {
        let len = span.len();
        Self { span: spinning_top::Spinlock::new(span), len }
    }
}

impl BlockDevice for RamDisk {
    fn read_bytes(&self, offset: u64, buf: &mut [u8]) -> Result<(), ()> {
        let start = usize::try_from(offset).map_err(|_| ())?;
        let end = start.checked_add(buf.len()).ok_or(())?;
        if end > self.len {
            return Err(());
        }
        if self.span.lock().read(start, buf) { Ok(()) } else { Err(()) }
    }

    fn write_bytes(&self, offset: u64, data: &[u8]) -> Result<(), ()> {
        let start = usize::try_from(offset).map_err(|_| ())?;
        let end = start.checked_add(data.len()).ok_or(())?;
        if end > self.len {
            return Err(());
        }
        if self.span.lock().write(start, data) { Ok(()) } else { Err(()) }
    }
}
