//! VirtIO Block Device Driver
//!
//! Provides a block device driver for virtio-blk devices with a generic
//! sector-based read/write API suitable for filesystem implementations.

use core::cell::UnsafeCell;

use spinning_top::Spinlock;
use virtio_drivers::device::blk::VirtIOBlk;
use crate::transport::SteppedMmioTransport;

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

impl_display!(BlockError {
    NotFound => "Block device not found",
    ReadError => "Read error",
    WriteError => "Write error",
    NotInitialized => "Device not initialized",
    InvalidOffset => "Invalid offset",
});

// ============================================================================
// VirtIO Block Device Wrapper
// ============================================================================

/// VirtIO block device wrapper with interior mutability
///
/// Uses UnsafeCell for interior mutability because VirtIOBlk needs &mut self
/// for read/write operations, but we want to share it through a Spinlock.
pub struct VirtioBlockDevice {
    inner: UnsafeCell<VirtIOBlk<VirtioHal, SteppedMmioTransport<'static>>>,
    capacity_sectors: u64,
}

// SAFETY: VirtioBlockDevice is only accessed through the global BLOCK_DEVICE Spinlock,
// which ensures exclusive access. The Spinlock provides the synchronization needed
// to safely access the UnsafeCell contents.
unsafe impl Sync for VirtioBlockDevice {}

impl VirtioBlockDevice {
    /// Create a new VirtIO block device wrapper
    fn new(inner: VirtIOBlk<VirtioHal, SteppedMmioTransport<'static>>) -> Self {
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
    fn inner_mut(&self) -> &mut VirtIOBlk<VirtioHal, SteppedMmioTransport<'static>> {
        unsafe { &mut *self.inner.get() }
    }

    /// Read sectors from the device
    ///
    /// # Arguments
    /// * `sector` - Starting sector number
    /// * `buf` - Buffer to read into (must be a multiple of SECTOR_SIZE)
    pub fn read_sectors(&self, sector: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        if !buf.len().is_multiple_of(SECTOR_SIZE) {
            crate::safe_print!(96, "[Block] read_sectors: buf len {} not sector-aligned\n", buf.len());
            return Err(BlockError::InvalidOffset);
        }

        let num_sectors = buf.len() / SECTOR_SIZE;
        if sector + num_sectors as u64 > self.capacity_sectors {
            crate::safe_print!(96, "[Block] read_sectors: sector {}+{} > capacity {}\n",
                sector, num_sectors, self.capacity_sectors);
            return Err(BlockError::InvalidOffset);
        }

        let inner = self.inner_mut();

        if let Err(e) = inner.read_blocks(sector as usize, buf) {
            crate::safe_print!(96, "[Block] read_blocks FAILED: sector={}, len={}, err={:?}\n",
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
            crate::safe_print!(96, "[Block] write_sectors: buf len {} not sector-aligned\n", buf.len());
            return Err(BlockError::InvalidOffset);
        }

        let num_sectors = buf.len() / SECTOR_SIZE;
        if sector + num_sectors as u64 > self.capacity_sectors {
            crate::safe_print!(96, "[Block] write_sectors: sector {}+{} > capacity {}\n",
                sector, num_sectors, self.capacity_sectors);
            return Err(BlockError::InvalidOffset);
        }

        let inner = self.inner_mut();

        if let Err(e) = inner.write_blocks(sector as usize, buf) {
            crate::safe_print!(96, "[Block] write_blocks FAILED: sector={}, len={}, err={:?}\n",
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

/// How many virtio-blk devices the kernel can have mounted-side by side.
///
/// The real bound is the machine's virtio-mmio slot count (8 on QEMU `virt`,
/// shared with NIC(s), rng, sound, the rump tap), so 4 is generous for every
/// configuration that exists. QEMU only wires a second `-drive` when the runner
/// asks for one (`docs/archive/MOUNT_MISSING_SYSCALLS.md` §5).
pub const MAX_BLOCK_DEVICES: usize = 4;

/// `/dev`-style names for the device table, indexed by discovery order.
/// `vda`, `vdb`, … — no allocation, just a static table lookup.
const DEVICE_NAMES: [&str; MAX_BLOCK_DEVICES] = ["vda", "vdb", "vdc", "vdd"];

/// The registered virtio-blk devices, by index. Slot 0 is the boot disk
/// (`vda`); further slots are data disks, mounted at runtime through
/// `mount(2)` with `source=/dev/vdX`.
///
/// **`no-bkl-vfs` invariant (unchanged from the single-device era):** each
/// per-device lock is held across a full virtio round-trip
/// ([`VirtioBlockDevice::read_sectors`] busy-polls the virtqueue; it never
/// yields), so it must not be stranded by a context switch or nested exception
/// on a core running an fs syscall *without* the Big Kernel Lock. It isn't, and
/// not by accident: every path that reaches it does so through the kernel's
/// `vfs::ext2` `BlockDevice` impl, i.e. from inside `akuma-ext2`'s
/// `read_block`/`write_block`/`write_superblock`, all of which require an
/// `Ext2State` guard — and that guard carries the `PreemptGuard` (preemption
/// off + IRQs masked) under `no-bkl-vfs`. So the hold is already covered
/// transitively and needs no guard of its own; a nested one would only re-save
/// an already-masked DAIF.
///
/// The one exception is [`is_initialized`], a momentary probe from `fs::init`
/// during single-threaded boot.
///
/// If a *new* caller ever reaches [`with_device_at`] from outside an ext2 state
/// guard, it must take an `akuma_primitives::PreemptGuard` itself — otherwise
/// it reopens the AB-BA window (this core holding a device lock while a nested
/// IRQ hard-spins for the BKL).
static BLOCK_DEVICES: [Spinlock<Option<VirtioBlockDevice>>; MAX_BLOCK_DEVICES] =
    [const { Spinlock::new(None) }; MAX_BLOCK_DEVICES];

// ============================================================================
// Public API
// ============================================================================

/// Initialize the block device driver.
///
/// Registers **every** virtio-blk the machine exposes, in slot order: the
/// first becomes `vda` (the boot disk), the rest `vdb`… Devices past
/// [`MAX_BLOCK_DEVICES`] are logged and skipped rather than aborting the
/// boot — the first disk is what boot needs, extras are runtime conveniences.
pub fn init() -> Result<(), BlockError> {
    log("[Block] Initializing block device driver...\n");

    let mut registered = 0usize;

    // Find virtio-blk devices. `probe_each` visits every matching slot; a slot
    // whose `VirtIOBlk::new` fails is logged and skipped, not fatal.
    probe::probe_each(probe::device_id::BLOCK, |i, transport| {
        if registered >= MAX_BLOCK_DEVICES {
            crate::safe_print!(96, "[Block] slot {i}: device table full ({MAX_BLOCK_DEVICES}), ignoring\n");
            return false;
        }

        log("[Block] Found virtio-blk at slot ");
        crate::safe_print!(32, "{}\n", i);

        let Ok(blk) = VirtIOBlk::<VirtioHal, SteppedMmioTransport<'static>>::new(transport) else {
            log("[Block] Failed to init virtio device\n");
            return true; // keep scanning
        };

        let device = VirtioBlockDevice::new(blk);
        log("[Block] Registered ");
        crate::safe_print!(
            96,
            "{}: {} MB ({} sectors)\n",
            DEVICE_NAMES[registered],
            device.capacity_bytes() / 1024 / 1024,
            device.capacity_sectors()
        );

        *BLOCK_DEVICES[registered].lock() = Some(device);
        registered += 1;
        true
    });

    if registered == 0 {
        return Err(BlockError::NotFound);
    }

    log("[Block] Block devices initialized: ");
    crate::safe_print!(32, "{}\n", registered);
    Ok(())
}

/// Check whether at least one block device is registered.
pub fn is_initialized() -> bool {
    BLOCK_DEVICES[0].lock().is_some()
}

/// How many block devices are registered (and thus nameable as `/dev/vdX`).
#[must_use]
pub fn device_count() -> usize {
    (0..MAX_BLOCK_DEVICES)
        .filter(|&i| BLOCK_DEVICES[i].lock().is_some())
        .count()
}

/// The `/dev` name (`vda`…`vdd`) of device `idx`, if it exists.
#[must_use]
pub fn device_name(idx: usize) -> Option<&'static str> {
    (idx < MAX_BLOCK_DEVICES && BLOCK_DEVICES[idx].lock().is_some()).then_some(DEVICE_NAMES[idx])
}

/// The index of the device a `/dev/`-style name refers to. Accepts both
/// `vdb` and `/dev/vdb` — `mount(2)`'s `source` arrives either way depending
/// on the caller.
#[must_use]
pub fn device_index_by_name(name: &str) -> Option<usize> {
    let name = name.strip_prefix("/dev/").unwrap_or(name);
    DEVICE_NAMES
        .iter()
        .position(|n| *n == name)
        .filter(|&i| BLOCK_DEVICES[i].lock().is_some())
}

/// Execute a closure with access to device `idx`.
/// Returns `None` if the device does not exist.
pub fn with_device_at<F, R>(idx: usize, f: F) -> Option<R>
where
    F: FnOnce(&VirtioBlockDevice) -> R,
{
    if idx >= MAX_BLOCK_DEVICES {
        return None;
    }
    let guard = BLOCK_DEVICES[idx].lock();
    guard.as_ref().map(f)
}

/// Read bytes at an arbitrary offset from device `idx`.
pub fn read_bytes_at(idx: usize, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
    with_device_at(idx, |dev| dev.read_bytes(offset, buf)).ok_or(BlockError::NotInitialized)?
}

/// Write bytes at an arbitrary offset to device `idx`.
pub fn write_bytes_at(idx: usize, offset: u64, buf: &[u8]) -> Result<(), BlockError> {
    with_device_at(idx, |dev| dev.write_bytes(offset, buf)).ok_or(BlockError::NotInitialized)?
}

/// Execute a closure with access to the boot device (`vda`).
/// Returns `None` if no block device is initialized.
pub fn with_device<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&VirtioBlockDevice) -> R,
{
    with_device_at(0, f)
}

/// Read bytes at an arbitrary offset from the boot device (`vda`).
pub fn read_bytes(offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
    read_bytes_at(0, offset, buf)
}

/// Write bytes at an arbitrary offset to the boot device (`vda`).
pub fn write_bytes(offset: u64, buf: &[u8]) -> Result<(), BlockError> {
    write_bytes_at(0, offset, buf)
}

// ============================================================================
// Logging
// ============================================================================

fn log(msg: &str) {
    akuma_primitives::console::print_str(msg);
}
