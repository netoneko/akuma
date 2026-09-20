//! Kernel ext2 wrapper — bridges `akuma_ext2` to the kernel block devices.

use alloc::sync::Arc;
use akuma_vfs::Filesystem;
pub use akuma_ext2::{BlockDevice, Ext2Filesystem};

/// Kernel block device adapter implementing `akuma_ext2::BlockDevice`.
///
/// Wraps one registered virtio-blk device. `idx` 0 is the boot disk
/// (`vda`); runtime `mount(2)` passes the index its `source` name resolved to.
pub struct KernelBlockDevice {
    pub idx: usize,
}

impl BlockDevice for KernelBlockDevice {
    fn read_bytes(&self, offset: u64, buf: &mut [u8]) -> Result<(), ()> {
        akuma_virtio::block::read_bytes_at(self.idx, offset, buf).map_err(|_| ())
    }

    fn write_bytes(&self, offset: u64, data: &[u8]) -> Result<(), ()> {
        akuma_virtio::block::write_bytes_at(self.idx, offset, data).map_err(|_| ())
    }
}

/// Mount ext2 from the boot device (`vda`), cache sized from the global cap.
pub fn mount() -> Result<Arc<dyn Filesystem>, akuma_vfs::FsError> {
    mount_device(0, None)
}

/// Mount ext2 from device `idx` with an optional per-instance cache cap.
///
/// Non-root instances pass `Some(cap)` so a second disk does not re-commit the
/// whole global cache budget (the cache never shrinks — `src/fs.rs` sizing
/// comments). Callers gate on `akuma_virtio::block::device_name(idx)` first.
pub fn mount_device(
    idx: usize,
    cache_cap: Option<usize>,
) -> Result<Arc<dyn Filesystem>, akuma_vfs::FsError> {
    let fs = Arc::new(Ext2Filesystem::new_with_cache_cap(
        KernelBlockDevice { idx },
        || crate::utc_time_us().unwrap_or(0),
        cache_cap,
    )?);
    // No `register_for_reap` here since the registry moved to `lib.rs`:
    // `mount_with` enrols every filesystem that asks for it, so this path and
    // the amd64 kernel's — which builds its own `Ext2Filesystem` over a
    // different device type and never calls this function — cannot diverge on
    // whether orphaned-lock recovery is wired.
    Ok(fs)
}
