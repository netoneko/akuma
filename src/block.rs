//! VirtIO Block Device Driver
//!
//! Provides a block device driver for virtio-blk devices with a generic
//! sector-based read/write API suitable for filesystem implementations.

use core::cell::UnsafeCell;

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
    /// Invalid offset or size
    InvalidOffset,
}

impl core::fmt::Display for BlockError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BlockError::NotFound => write!(f, "Block device not found"),
            BlockError::ReadError => write!(f, "Read error"),
            BlockError::WriteError => write!(f, "Write error"),
            BlockError::NotInitialized => write!(f, "Device not initialized"),
            BlockError::InvalidOffset => write!(f, "Invalid offset"),
        }
    }
}

// ============================================================================
// VirtIO Block Device Wrapper
// ============================================================================

/// VirtIO block device wrapper with interior mutability
///
/// Uses UnsafeCell for interior mutability because VirtIOBlk needs &mut self
/// for read/write operations, but we want to share it through a Spinlock.
pub struct VirtioBlockDevice {
    inner: UnsafeCell<VirtIOBlk<VirtioHal, MmioTransport>>,
    capacity_sectors: u64,
}

// SAFETY: VirtioBlockDevice is only accessed through the global BLOCK_DEVICE Spinlock,
// which ensures exclusive access. The Spinlock provides the synchronization needed
// to safely access the UnsafeCell contents.
unsafe impl Sync for VirtioBlockDevice {}

impl VirtioBlockDevice {
    /// Create a new VirtIO block device wrapper
    fn new(inner: VirtIOBlk<VirtioHal, MmioTransport>) -> Self {
        let capacity_sectors = inner.capacity();
        Self {
            inner: UnsafeCell::new(inner),
            capacity_sectors,
        }
    }

    /// Get the capacity in sectors (512-byte blocks)
    pub fn capacity_sectors(&self) -> u64 {
        self.capacity_sectors
    }

    /// Get the capacity in bytes
    pub fn capacity_bytes(&self) -> u64 {
        self.capacity_sectors * SECTOR_SIZE as u64
    }

    /// Get mutable access to the inner VirtIOBlk
    /// SAFETY: Caller must ensure exclusive access (e.g., via the BLOCK_DEVICE Spinlock)
    #[inline]
    fn inner_mut(&self) -> &mut VirtIOBlk<VirtioHal, MmioTransport> {
        // SAFETY: We have exclusive access via the Spinlock guard
        unsafe { &mut *self.inner.get() }
    }

    /// Read sectors from the device
    ///
    /// # Arguments
    /// * `sector` - Starting sector number
    /// * `buf` - Buffer to read into (must be a multiple of SECTOR_SIZE)
    pub fn read_sectors(&self, sector: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        if buf.len() % SECTOR_SIZE != 0 {
            return Err(BlockError::InvalidOffset);
        }

        let num_sectors = buf.len() / SECTOR_SIZE;
        if sector + num_sectors as u64 > self.capacity_sectors {
            return Err(BlockError::InvalidOffset);
        }

        let inner = self.inner_mut();

        // VirtIOBlk::read_blocks reads one sector at a time
        for i in 0..num_sectors {
            let offset = i * SECTOR_SIZE;
            let sector_buf = &mut buf[offset..offset + SECTOR_SIZE];
            inner
                .read_blocks(sector as usize + i, sector_buf)
                .map_err(|_| BlockError::ReadError)?;
        }

        Ok(())
    }

    /// Write sectors to the device
    ///
    /// # Arguments
    /// * `sector` - Starting sector number
    /// * `buf` - Buffer to write from (must be a multiple of SECTOR_SIZE)
    pub fn write_sectors(&self, sector: u64, buf: &[u8]) -> Result<(), BlockError> {
        if buf.len() % SECTOR_SIZE != 0 {
            return Err(BlockError::InvalidOffset);
        }

        let num_sectors = buf.len() / SECTOR_SIZE;
        if sector + num_sectors as u64 > self.capacity_sectors {
            return Err(BlockError::InvalidOffset);
        }

        let inner = self.inner_mut();

        // VirtIOBlk::write_blocks writes one sector at a time
        for i in 0..num_sectors {
            let offset = i * SECTOR_SIZE;
            let sector_buf = &buf[offset..offset + SECTOR_SIZE];
            inner
                .write_blocks(sector as usize + i, sector_buf)
                .map_err(|_| BlockError::WriteError)?;
        }

        Ok(())
    }

    /// Read bytes at an arbitrary offset (handles sector alignment internally)
    pub fn read_bytes(&self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        if buf.is_empty() {
            return Ok(());
        }

        let start_sector = offset / SECTOR_SIZE as u64;
        let end_offset = offset + buf.len() as u64;
        let end_sector = (end_offset + SECTOR_SIZE as u64 - 1) / SECTOR_SIZE as u64;
        let num_sectors = (end_sector - start_sector) as usize;

        // Allocate temporary buffer for aligned read
        let mut temp = alloc::vec![0u8; num_sectors * SECTOR_SIZE];
        self.read_sectors(start_sector, &mut temp)?;

        // Copy the requested portion
        let start_offset = (offset % SECTOR_SIZE as u64) as usize;
        buf.copy_from_slice(&temp[start_offset..start_offset + buf.len()]);

        Ok(())
    }

    /// Write bytes at an arbitrary offset (handles sector alignment internally)
    pub fn write_bytes(&self, offset: u64, buf: &[u8]) -> Result<(), BlockError> {
        if buf.is_empty() {
            return Ok(());
        }

        let start_sector = offset / SECTOR_SIZE as u64;
        let end_offset = offset + buf.len() as u64;
        let end_sector = (end_offset + SECTOR_SIZE as u64 - 1) / SECTOR_SIZE as u64;
        let num_sectors = (end_sector - start_sector) as usize;

        // Read existing data for sectors we'll partially overwrite
        let mut temp = alloc::vec![0u8; num_sectors * SECTOR_SIZE];
        self.read_sectors(start_sector, &mut temp)?;

        // Overwrite with new data
        let start_offset = (offset % SECTOR_SIZE as u64) as usize;
        temp[start_offset..start_offset + buf.len()].copy_from_slice(buf);

        // Write back
        self.write_sectors(start_sector, &temp)?;

        Ok(())
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
            "{} MB ({} sectors)\n",
            device.capacity_bytes() / 1024 / 1024,
            device.capacity_sectors()
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

/// Execute a closure with access to the block device
/// Returns None if the block device is not initialized
pub fn with_device<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&VirtioBlockDevice) -> R,
{
    let guard = BLOCK_DEVICE.lock();
    guard.as_ref().map(f)
}

/// Get the block device capacity in bytes
pub fn capacity() -> Option<u64> {
    with_device(|dev| dev.capacity_bytes())
}

/// Get the block device capacity in sectors
pub fn capacity_sectors() -> Option<u64> {
    with_device(|dev| dev.capacity_sectors())
}

/// Read sectors from the block device
pub fn read_sectors(sector: u64, buf: &mut [u8]) -> Result<(), BlockError> {
    with_device(|dev| dev.read_sectors(sector, buf)).ok_or(BlockError::NotInitialized)?
}

/// Write sectors to the block device
pub fn write_sectors(sector: u64, buf: &[u8]) -> Result<(), BlockError> {
    with_device(|dev| dev.write_sectors(sector, buf)).ok_or(BlockError::NotInitialized)?
}

/// Read bytes at an arbitrary offset
pub fn read_bytes(offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
    with_device(|dev| dev.read_bytes(offset, buf)).ok_or(BlockError::NotInitialized)?
}

/// Write bytes at an arbitrary offset
pub fn write_bytes(offset: u64, buf: &[u8]) -> Result<(), BlockError> {
    with_device(|dev| dev.write_bytes(offset, buf)).ok_or(BlockError::NotInitialized)?
}

// ============================================================================
// Logging
// ============================================================================

fn log(msg: &str) {
    console::print(msg);
}
