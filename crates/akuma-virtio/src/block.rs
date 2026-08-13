//! VirtIO Block Device Driver
//!
//! Provides a block device driver for virtio-blk devices with a generic
//! sector-based read/write API suitable for filesystem implementations.

use core::cell::UnsafeCell;

use spinning_top::Spinlock;
use virtio_drivers::device::blk::VirtIOBlk;
use virtio_drivers::transport::mmio::MmioTransport;

use crate::hal::VirtioHal;
use crate::probe;

// ============================================================================
// Constants
// ============================================================================

/// Sector size in bytes (standard for VirtIO block devices)
pub const SECTOR_SIZE: usize = 512;


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
            Self::NotFound => write!(f, "Block device not found"),
            Self::ReadError => write!(f, "Read error"),
            Self::WriteError => write!(f, "Write error"),
            Self::NotInitialized => write!(f, "Device not initialized"),
            Self::InvalidOffset => write!(f, "Invalid offset"),
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
    ///
    /// # Safety
    /// Caller must ensure exclusive access (e.g., via the BLOCK_DEVICE Spinlock).
    #[inline]
    #[allow(clippy::mut_from_ref)]
    fn inner_mut(&self) -> &mut VirtIOBlk<VirtioHal, MmioTransport> {
        unsafe { &mut *self.inner.get() }
    }

    /// Read sectors from the device
    ///
    /// # Arguments
    /// * `sector` - Starting sector number
    /// * `buf` - Buffer to read into (must be a multiple of SECTOR_SIZE)
    pub fn read_sectors(&self, sector: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        if !buf.len().is_multiple_of(SECTOR_SIZE) {
            crate::vprint!(96, "[Block] read_sectors: buf len {} not sector-aligned\n", buf.len());
            return Err(BlockError::InvalidOffset);
        }

        let num_sectors = buf.len() / SECTOR_SIZE;
        if sector + num_sectors as u64 > self.capacity_sectors {
            crate::vprint!(96, "[Block] read_sectors: sector {}+{} > capacity {}\n",
                sector, num_sectors, self.capacity_sectors);
            return Err(BlockError::InvalidOffset);
        }

        let inner = self.inner_mut();

        if let Err(e) = inner.read_blocks(sector as usize, buf) {
            crate::vprint!(96, "[Block] read_blocks FAILED: sector={}, len={}, err={:?}\n",
                sector, buf.len(), e);
            return Err(BlockError::ReadError);
        }

        Ok(())
    }

    /// Write sectors to the device
    ///
    /// # Arguments
    /// * `sector` - Starting sector number
    /// * `buf` - Buffer to write from (must be a multiple of SECTOR_SIZE)
    pub fn write_sectors(&self, sector: u64, buf: &[u8]) -> Result<(), BlockError> {
        if !buf.len().is_multiple_of(SECTOR_SIZE) {
            crate::vprint!(96, "[Block] write_sectors: buf len {} not sector-aligned\n", buf.len());
            return Err(BlockError::InvalidOffset);
        }

        let num_sectors = buf.len() / SECTOR_SIZE;
        if sector + num_sectors as u64 > self.capacity_sectors {
            crate::vprint!(96, "[Block] write_sectors: sector {}+{} > capacity {}\n",
                sector, num_sectors, self.capacity_sectors);
            return Err(BlockError::InvalidOffset);
        }

        let inner = self.inner_mut();

        if let Err(e) = inner.write_blocks(sector as usize, buf) {
            crate::vprint!(96, "[Block] write_blocks FAILED: sector={}, len={}, err={:?}\n",
                sector, buf.len(), e);
            return Err(BlockError::WriteError);
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
        let end_sector = end_offset.div_ceil(SECTOR_SIZE as u64);
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
        let end_sector = end_offset.div_ceil(SECTOR_SIZE as u64);
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

/// The virtio-blk device, behind a plain `Spinlock`.
///
/// **`no-bkl-vfs` invariant:** this lock is held across a full virtio round-trip
/// ([`VirtioBlockDevice::read_sectors`] busy-polls the virtqueue; it never yields), so it
/// must not be stranded by a context switch or nested exception on a core running an fs
/// syscall *without* the Big Kernel Lock. It isn't, and not by accident: every path that
/// reaches it does so through the kernel's `vfs::ext2` `BlockDevice` impl, i.e. from
/// inside `akuma-ext2`'s `read_block`/`write_block`/`write_superblock`, all of which
/// require an `Ext2State` guard — and that guard carries the `PreemptGuard` (preemption
/// off + IRQs masked) under `no-bkl-vfs`. So the hold is already covered transitively and
/// needs no guard of its own; a nested one would only re-save an already-masked DAIF.
///
/// The one exception is [`is_initialized`], a momentary probe from `fs::init` during
/// single-threaded boot.
///
/// If a *new* caller ever reaches [`with_device`] from outside an ext2 state guard, it
/// must take an `akuma_exec::sync::PreemptGuard` itself — otherwise it reopens the AB-BA
/// window (this core holding BLOCK_DEVICE while a nested IRQ hard-spins for the BKL).
static BLOCK_DEVICE: Spinlock<Option<VirtioBlockDevice>> = Spinlock::new(None);

// ============================================================================
// Public API
// ============================================================================

/// Initialize the block device driver
/// Scans for virtio-blk devices and initializes the first one found
pub fn init() -> Result<(), BlockError> {
    log("[Block] Initializing block device driver...\n");

    // Find virtio-blk device. `probe_with` keeps scanning if a matching slot
    // fails to yield a working device, which is what the hand-rolled loop did.
    let found_device = probe::probe_with(probe::device_id::BLOCK, |i, transport| {
        log("[Block] Found virtio-blk at slot ");
        crate::vprint!(32, "{}\n", i);

        let Ok(blk) = VirtIOBlk::<VirtioHal, MmioTransport>::new(transport) else {
            log("[Block] Failed to init virtio device\n");
            return None;
        };

        let device = VirtioBlockDevice::new(blk);
        log("[Block] Capacity: ");
        crate::vprint!(
            64,
            "{} MB ({} sectors)\n",
            device.capacity_bytes() / 1024 / 1024,
            device.capacity_sectors()
        );

        Some(device)
    });

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
    crate::print::print_str(msg);
}
