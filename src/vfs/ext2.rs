//! Kernel ext2 wrapper — bridges `akuma_ext2` to the kernel block device.

use alloc::boxed::Box;
use akuma_vfs::Filesystem;
pub use akuma_ext2::{BlockDevice, Ext2Filesystem};

/// Kernel block device adapter implementing `akuma_ext2::BlockDevice`.
pub struct KernelBlockDevice;

impl BlockDevice for KernelBlockDevice {
    fn read_bytes(&self, offset: u64, buf: &mut [u8]) -> Result<(), ()> {
        crate::block::read_bytes(offset, buf).map_err(|_| ())
    }

    fn write_bytes(&self, offset: u64, data: &[u8]) -> Result<(), ()> {
        crate::block::write_bytes(offset, data).map_err(|_| ())
    }
}

/// Mount ext2 from the kernel block device.
pub fn mount() -> Result<Box<dyn Filesystem>, akuma_vfs::FsError> {
    let fs = Ext2Filesystem::new(KernelBlockDevice, || {
        crate::timer::utc_time_us().unwrap_or(0)
    })?;
    Ok(Box::new(fs))
}
