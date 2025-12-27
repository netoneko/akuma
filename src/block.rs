//! VirtIO Block Device Driver
//!
//! Provides a block device driver for virtio-blk devices that implements
//! the embedded-sdmmc BlockDevice trait.

use embedded_sdmmc::BlockDevice;
use spinning_top::Spinlock;
use virtio_drivers::device::blk::VirtIOBlk;
use virtio_drivers::transport::mmio::{MmioTransport, VirtIOHeader};

use crate::console;
use crate::virtio_hal::VirtioHal;

// ============================================================================
// Constants
// ============================================================================

/// Sector size in bytes (standard for VirtIO block devices)
pub const SECTOR_SIZE: usize = 512;

/// QEMU virt machine virtio MMIO addresses
const VIRTIO_MMIO_ADDRS: [usize; 8] = [
    0x0a000000, 0x0a000200, 0x0a000400, 0x0a000600, 0x0a000800, 0x0a000a00, 0x0a000c00, 0x0a000e00,
];

/// VirtIO device ID for block devices
const VIRTIO_DEVICE_ID_BLK: u32 = 2;

// ============================================================================
// Block Device Error
// ============================================================================

/// Block device error type
#[derive(Debug, Clone, Copy)]
pub enum BlockError {
    /// Device not found
    NotFound,
    /// I/O error during read
    ReadError,
    /// I/O error during write
    WriteError,
    /// Device not initialized
    NotInitialized,
}

impl core::fmt::Display for BlockError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BlockError::NotFound => write!(f, "Block device not found"),
            BlockError::ReadError => write!(f, "Read error"),
            BlockError::WriteError => write!(f, "Write error"),
            BlockError::NotInitialized => write!(f, "Device not initialized"),
        }
    }
}

// ============================================================================
// VirtIO Block Device Wrapper
// ============================================================================

/// VirtIO block device wrapper implementing embedded-sdmmc BlockDevice trait
pub struct VirtioBlockDevice {
    inner: VirtIOBlk<VirtioHal, MmioTransport>,
    capacity_blocks: u64,
}

impl VirtioBlockDevice {
    /// Create a new VirtIO block device wrapper
    fn new(inner: VirtIOBlk<VirtioHal, MmioTransport>) -> Self {
        let capacity_blocks = inner.capacity();
        Self {
            inner,
            capacity_blocks,
        }
    }

    /// Get the capacity in blocks
    pub fn capacity_blocks(&self) -> u64 {
        self.capacity_blocks
    }

    /// Get the capacity in bytes
    pub fn capacity_bytes(&self) -> u64 {
        self.capacity_blocks * SECTOR_SIZE as u64
    }
}

// ============================================================================
// embedded-sdmmc BlockDevice Implementation
// ============================================================================

impl BlockDevice for VirtioBlockDevice {
    type Error = BlockError;

    fn read(
        &self,
        blocks: &mut [embedded_sdmmc::Block],
        start_block_idx: embedded_sdmmc::BlockIdx,
        _reason: &str,
    ) -> Result<(), Self::Error> {
        let start = start_block_idx.0 as usize;

        // VirtIOBlk requires mutable reference, but BlockDevice::read takes &self
        // We need to use interior mutability here
        let inner_ptr = &self.inner as *const VirtIOBlk<VirtioHal, MmioTransport>
            as *mut VirtIOBlk<VirtioHal, MmioTransport>;

        for (i, block) in blocks.iter_mut().enumerate() {
            let block_idx = start + i;
            // SAFETY: We're the only accessor due to the global spinlock
            unsafe {
                (*inner_ptr)
                    .read_blocks(block_idx, &mut block.contents)
                    .map_err(|_| BlockError::ReadError)?;
            }
        }

        Ok(())
    }

    fn write(
        &self,
        blocks: &[embedded_sdmmc::Block],
        start_block_idx: embedded_sdmmc::BlockIdx,
    ) -> Result<(), Self::Error> {
        let start = start_block_idx.0 as usize;

        // VirtIOBlk requires mutable reference, but BlockDevice::write takes &self
        let inner_ptr = &self.inner as *const VirtIOBlk<VirtioHal, MmioTransport>
            as *mut VirtIOBlk<VirtioHal, MmioTransport>;

        for (i, block) in blocks.iter().enumerate() {
            let block_idx = start + i;
            // SAFETY: We're the only accessor due to the global spinlock
            unsafe {
                (*inner_ptr)
                    .write_blocks(block_idx, &block.contents)
                    .map_err(|_| BlockError::WriteError)?;
            }
        }

        Ok(())
    }

    fn num_blocks(&self) -> Result<embedded_sdmmc::BlockCount, Self::Error> {
        Ok(embedded_sdmmc::BlockCount(self.capacity_blocks as u32))
    }
}

// Also implement for mutable references to allow use with with_device
impl BlockDevice for &mut VirtioBlockDevice {
    type Error = BlockError;

    fn read(
        &self,
        blocks: &mut [embedded_sdmmc::Block],
        start_block_idx: embedded_sdmmc::BlockIdx,
        reason: &str,
    ) -> Result<(), Self::Error> {
        (**self).read(blocks, start_block_idx, reason)
    }

    fn write(
        &self,
        blocks: &[embedded_sdmmc::Block],
        start_block_idx: embedded_sdmmc::BlockIdx,
    ) -> Result<(), Self::Error> {
        (**self).write(blocks, start_block_idx)
    }

    fn num_blocks(&self) -> Result<embedded_sdmmc::BlockCount, Self::Error> {
        (**self).num_blocks()
    }
}

// ============================================================================
// Global Block Device State
// ============================================================================

static BLOCK_DEVICE: Spinlock<Option<VirtioBlockDevice>> = Spinlock::new(None);

// ============================================================================
// Public API
// ============================================================================

/// Initialize the block device driver
/// Scans for virtio-blk devices and initializes the first one found
pub fn init() -> Result<(), BlockError> {
    log("[Block] Initializing block device driver...\n");

    // Find virtio-blk device
    let mut found_device: Option<VirtioBlockDevice> = None;

    for (i, &addr) in VIRTIO_MMIO_ADDRS.iter().enumerate() {
        // SAFETY: Reading from MMIO registers at known QEMU virt machine addresses
        let device_id = unsafe { core::ptr::read_volatile((addr + 0x008) as *const u32) };
        if device_id != VIRTIO_DEVICE_ID_BLK {
            continue;
        }

        log("[Block] Found virtio-blk at slot ");
        console::print(&alloc::format!("{}\n", i));

        let header_ptr = match core::ptr::NonNull::new(addr as *mut VirtIOHeader) {
            Some(p) => p,
            None => continue,
        };

        // SAFETY: Creating MmioTransport for verified virtio device
        let transport = match unsafe { MmioTransport::new(header_ptr) } {
            Ok(t) => t,
            Err(_) => {
                log("[Block] Failed to create transport\n");
                continue;
            }
        };

        let blk = match VirtIOBlk::<VirtioHal, MmioTransport>::new(transport) {
            Ok(b) => b,
            Err(_) => {
                log("[Block] Failed to init virtio device\n");
                continue;
            }
        };

        let device = VirtioBlockDevice::new(blk);
        log("[Block] Capacity: ");
        console::print(&alloc::format!(
            "{} MB ({} blocks)\n",
            device.capacity_bytes() / 1024 / 1024,
            device.capacity_blocks()
        ));

        found_device = Some(device);
        break;
    }

    let device = found_device.ok_or(BlockError::NotFound)?;

    // Store in global state
    *BLOCK_DEVICE.lock() = Some(device);

    log("[Block] Block device initialized\n");
    Ok(())
}

/// Check if block device is initialized
pub fn is_initialized() -> bool {
    BLOCK_DEVICE.lock().is_some()
}

/// Execute a closure with mutable access to the block device
/// Returns None if the block device is not initialized
pub fn with_device<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&mut VirtioBlockDevice) -> R,
{
    let mut guard = BLOCK_DEVICE.lock();
    guard.as_mut().map(f)
}

/// Get the block device capacity in bytes
pub fn capacity() -> Option<u64> {
    with_device(|dev| dev.capacity_bytes())
}

// ============================================================================
// Logging
// ============================================================================

fn log(msg: &str) {
    console::print(msg);
}
