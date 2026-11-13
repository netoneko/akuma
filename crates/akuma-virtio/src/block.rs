//! VirtIO Block Device Driver
//!
//! Provides a block device driver for virtio-blk devices with a generic
//! sector-based read/write API suitable for filesystem implementations.
//!
//! The device is driven in-tree since 2026-11-01 (it was `virtio-drivers`'
//! `VirtIOBlk`): one request at a time, three descriptors (header, data,
//! status), busy-polled to completion. The ring, the header and the status
//! byte are `DmaArray` statics indexed by the device's registration slot; the
//! data is the caller's buffer, which outlives the request because the request
//! is waited for before the call returns.


use spinning_top::Spinlock;

use akuma_dma::DmaArray;
use akuma_dma::align::A64;
use akuma_virtqueue::{Buf, Layout, blk};

use crate::mmio::Transport;
use crate::ring::{Queue, Ring, phys_of};

use crate::probe;

// ============================================================================
// Constants
// ============================================================================

/// Sector size in bytes (standard for VirtIO block devices)
pub const SECTOR_SIZE: usize = blk::SECTOR_SIZE;


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

/// Entries in a block device's queue. One request is in flight at a time and
/// takes three descriptors; eight leaves room and is accepted by every VMM.
const QUEUE_SIZE: usize = 8;
/// 16-bit cells of ring memory a queue of [`QUEUE_SIZE`] needs.
const RING_WORDS: usize = Layout::new(QUEUE_SIZE).words();
/// Where the status byte sits in a device's request statics, after the header.
const STATUS_AT: usize = blk::HEADER_LEN;

/// Each registered device's queue memory, by registration slot.
static RINGS: [Ring<RING_WORDS>; MAX_BLOCK_DEVICES] = [const { Ring::new() }; MAX_BLOCK_DEVICES];
/// Each registered device's request header (16 bytes) and status byte.
static REQUESTS: [DmaArray<u8, 32, A64>; MAX_BLOCK_DEVICES] = [const { DmaArray::new() }; MAX_BLOCK_DEVICES];

/// VirtIO block device wrapper.
///
/// A request needs `&mut self`; the per-device Spinlock in `BLOCK_DEVICES`
/// hands that out, so no interior mutability is needed.
pub struct VirtioBlockDevice {
    transport: Transport,
    queue: Queue<QUEUE_SIZE, RING_WORDS>,
    /// Which of [`RINGS`] and [`REQUESTS`] is this device's.
    index: usize,
    capacity_sectors: u64,
    read_only: bool,
}

impl VirtioBlockDevice {
    /// Bring the device in `transport` up on the first unclaimed ring. A ring a
    /// failed device was given stays claimed: that device may still hold its
    /// address.
    fn new(mut transport: Transport) -> Result<Self, BlockError> {
        let (index, ring) = RINGS
            .iter()
            .enumerate()
            .find_map(|(i, r)| r.claim().map(|r| (i, r)))
            .ok_or(BlockError::NotFound)?;
        let features = transport.begin(blk::F_RO).map_err(|_| BlockError::NotFound)?;
        let queue = Queue::setup(&mut transport, 0, ring).map_err(|_| BlockError::NotFound)?;
        transport.driver_ok();
        let capacity_sectors = transport.config_u64(blk::CAPACITY);
        Ok(Self { transport, queue, index, capacity_sectors, read_only: features & blk::F_RO != 0 })
    }

    /// One request, waited for: `kind` at `sector`, over `len` bytes at `data`.
    ///
    /// Does not return until the device has completed the request, which is what
    /// lets the caller's buffer be used as the data descriptor directly.
    fn request(&mut self, kind: u32, sector: u64, data: *const u8, len: usize) -> Result<(), ()> {
        let Ok(len) = u32::try_from(len) else { return Err(()) };
        let req = &REQUESTS[self.index];
        for (i, b) in blk::header(kind, sector).into_iter().enumerate() {
            req.set(i, b);
        }
        // Anything but S_OK reads as failure if the device never writes it.
        req.set(STATUS_AT, 0xff);
        #[allow(clippy::cast_possible_truncation)]
        let bufs = [
            Buf { addr: phys_of(req.addr() as *const u8), len: blk::HEADER_LEN as u32, device_writes: false },
            Buf { addr: phys_of(data), len, device_writes: kind == blk::T_IN },
            Buf { addr: phys_of(req.addr_of(STATUS_AT) as *const u8), len: 1, device_writes: true },
        ];
        let head = self.queue.submit(&self.transport, &bufs).map_err(|_| ())?;
        // Spin until *this* request is back. With one request in flight nothing
        // else can complete; an element the queue refuses (a device naming a
        // head it was never given) frees nothing and is skipped.
        loop {
            match self.queue.sq.pop_used(self.queue.mem) {
                Some(Ok((h, _))) if h == head => break,
                Some(_) => {}
                None => core::hint::spin_loop(),
            }
        }
        if req.get(STATUS_AT) == blk::S_OK { Ok(()) } else { Err(()) }
    }

    /// Get the capacity in sectors (512-byte blocks)
    #[must_use]
    pub fn capacity_sectors(&self) -> u64 {
        self.capacity_sectors
    }

    /// Get the capacity in bytes
    #[must_use]
    pub fn capacity_bytes(&self) -> u64 {
        self.capacity_sectors * SECTOR_SIZE as u64
    }

    /// Read sectors from the device
    ///
    /// # Arguments
    /// * `sector` - Starting sector number
    /// * `buf` - Buffer to read into (must be a multiple of SECTOR_SIZE)
    pub fn read_sectors(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        if !buf.len().is_multiple_of(SECTOR_SIZE) {
            crate::safe_print!(96, "[Block] read_sectors: buf len {} not sector-aligned\n", buf.len());
            return Err(BlockError::InvalidOffset);
        }

        let num_sectors = buf.len() / SECTOR_SIZE;
        if !blk::in_range(sector, num_sectors as u64, self.capacity_sectors) {
            crate::safe_print!(96, "[Block] read_sectors: sector {}+{} > capacity {}\n",
                sector, num_sectors, self.capacity_sectors);
            return Err(BlockError::InvalidOffset);
        }

        if buf.is_empty() {
            return Ok(());
        }
        if self.request(blk::T_IN, sector, buf.as_mut_ptr(), buf.len()).is_err() {
            crate::safe_print!(96, "[Block] read FAILED: sector={}, len={}\n", sector, buf.len());
            return Err(BlockError::ReadError);
        }

        Ok(())
    }

    /// Write sectors to the device
    ///
    /// # Arguments
    /// * `sector` - Starting sector number
    /// * `buf` - Buffer to write from (must be a multiple of SECTOR_SIZE)
    pub fn write_sectors(&mut self, sector: u64, buf: &[u8]) -> Result<(), BlockError> {
        if !buf.len().is_multiple_of(SECTOR_SIZE) {
            crate::safe_print!(96, "[Block] write_sectors: buf len {} not sector-aligned\n", buf.len());
            return Err(BlockError::InvalidOffset);
        }

        let num_sectors = buf.len() / SECTOR_SIZE;
        if !blk::in_range(sector, num_sectors as u64, self.capacity_sectors) {
            crate::safe_print!(96, "[Block] write_sectors: sector {}+{} > capacity {}\n",
                sector, num_sectors, self.capacity_sectors);
            return Err(BlockError::InvalidOffset);
        }

        if buf.is_empty() {
            return Ok(());
        }
        if self.read_only {
            return Err(BlockError::WriteError);
        }
        if self.request(blk::T_OUT, sector, buf.as_ptr(), buf.len()).is_err() {
            crate::safe_print!(96, "[Block] write FAILED: sector={}, len={}\n", sector, buf.len());
            return Err(BlockError::WriteError);
        }

        Ok(())
    }

    /// Read bytes at an arbitrary offset (handles sector alignment internally).
    ///
    /// **Fast path**: when `offset` and `buf.len()` are both sector-multiples —
    /// true of every ext2 call site, since `block_size` (4096) is a multiple of
    /// `SECTOR_SIZE` (512) — `buf` reads straight from the device with no
    /// intermediate allocation. The slow path below is what a misaligned
    /// caller still needs: a temp buffer sized to whole sectors, because
    /// `read_sectors` cannot write a partial one.
    /// (`docs/archive/AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md` audit item 4 —
    /// measured: every one of a self-host build's ~62k page reads paid this
    /// allocate-then-copy for bytes that were already going to the right
    /// place.)
    pub fn read_bytes(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        if buf.is_empty() {
            return Ok(());
        }

        if offset.is_multiple_of(SECTOR_SIZE as u64) && buf.len().is_multiple_of(SECTOR_SIZE) {
            return self.read_sectors(offset / SECTOR_SIZE as u64, buf);
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

    /// Write bytes at an arbitrary offset (handles sector alignment internally).
    ///
    /// **Fast path**, same condition and reason as [`Self::read_bytes`]: an
    /// aligned write needs no read-modify-write at all, not just no temp
    /// buffer — the existing sector contents are being replaced wholesale, so
    /// reading them first was pure waste, not caution.
    pub fn write_bytes(&mut self, offset: u64, buf: &[u8]) -> Result<(), BlockError> {
        if buf.is_empty() {
            return Ok(());
        }

        if offset.is_multiple_of(SECTOR_SIZE as u64) && buf.len().is_multiple_of(SECTOR_SIZE) {
            return self.write_sectors(offset / SECTOR_SIZE as u64, buf);
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
    // whose device fails to come up is logged and skipped, not fatal.
    probe::probe_each(probe::device_id::BLOCK, |i, transport| {
        if registered >= MAX_BLOCK_DEVICES {
            crate::safe_print!(96, "[Block] slot {i}: device table full ({MAX_BLOCK_DEVICES}), ignoring\n");
            return false;
        }

        log("[Block] Found virtio-blk at slot ");
        crate::safe_print!(32, "{}\n", i);

        let Ok(device) = VirtioBlockDevice::new(transport) else {
            log("[Block] Failed to init virtio device\n");
            return true; // keep scanning
        };

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
    F: FnOnce(&mut VirtioBlockDevice) -> R,
{
    if idx >= MAX_BLOCK_DEVICES {
        return None;
    }
    let mut guard = BLOCK_DEVICES[idx].lock();
    guard.as_mut().map(f)
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
    F: FnOnce(&mut VirtioBlockDevice) -> R,
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
