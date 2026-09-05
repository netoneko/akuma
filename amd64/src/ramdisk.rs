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

use crate::phys::phys_to_virt;

/// A contiguous span of physical memory, read and written as a block device.
pub struct RamDisk {
    base: u64,
    len: usize,
}

impl RamDisk {
    /// Wrap the physical range `[base, base + len)`.
    ///
    /// Returns `None` for an empty range, or one reaching past what the physmap
    /// covers — a span the kernel cannot address is not a device it can read,
    /// and finding that out through `phys_to_virt`'s assertion during a
    /// filesystem read is a much worse way to learn it.
    #[must_use]
    pub fn new(base: u64, len: usize) -> Option<Self> {
        if len == 0 {
            return None;
        }
        let end = base.checked_add(len as u64)?;
        if end > crate::phys::PHYSMAP_LIMIT {
            return None;
        }
        Some(Self { base, len })
    }

    /// The bytes, as a slice.
    fn bytes(&self) -> &[u8] {
        // SAFETY: `new` established that the whole span lies inside the
        // physmap, so `phys_to_virt` maps every byte of it, and the boot page
        // tables describe that window as ordinary writable memory. The frames
        // are reserved from the PMM by `mem::init_reserving`, so nothing else in
        // the kernel holds a reference to them.
        unsafe { core::slice::from_raw_parts(phys_to_virt(self.base) as *const u8, self.len) }
    }

    /// The bytes, mutably.
    #[allow(clippy::mut_from_ref)] // the SAFETY note below is the argument
    fn bytes_mut(&self) -> &mut [u8] {
        // SAFETY: as `bytes`. Taking `&self` rather than `&mut self` because
        // `BlockDevice::write_bytes` does, and the aliasing that permits is the
        // same aliasing a real disk's write path has: the filesystem above
        // serialises its own access.
        unsafe { core::slice::from_raw_parts_mut(phys_to_virt(self.base) as *mut u8, self.len) }
    }
}

impl BlockDevice for RamDisk {
    fn read_bytes(&self, offset: u64, buf: &mut [u8]) -> Result<(), ()> {
        let start = usize::try_from(offset).map_err(|_| ())?;
        let end = start.checked_add(buf.len()).ok_or(())?;
        if end > self.len {
            return Err(());
        }
        buf.copy_from_slice(&self.bytes()[start..end]);
        Ok(())
    }

    fn write_bytes(&self, offset: u64, data: &[u8]) -> Result<(), ()> {
        let start = usize::try_from(offset).map_err(|_| ())?;
        let end = start.checked_add(data.len()).ok_or(())?;
        if end > self.len {
            return Err(());
        }
        self.bytes_mut()[start..end].copy_from_slice(data);
        Ok(())
    }
}
