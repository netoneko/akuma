//! VirtIO RNG Device Driver
//!
//! Provides a hardware-backed random number generator using virtio-rng devices.
//! This replaces the weak timer-seeded PRNG for better entropy in cryptographic
//! operations like SSH key generation.
//!
//! This is a minimal, standalone VirtIO RNG driver that directly accesses MMIO
//! registers since virtio-drivers 0.7 doesn't expose an RNG device.
//! Supports legacy VirtIO mode (used with QEMU's force-legacy=true).

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{Ordering, fence};

use alloc::alloc::{Layout, alloc_zeroed, dealloc};
use spinning_top::Spinlock;

use crate::console;

// ============================================================================
// Constants
// ============================================================================

/// QEMU virt machine virtio MMIO addresses
const VIRTIO_MMIO_ADDRS: [usize; 8] = [
    0x0a000000, 0x0a000200, 0x0a000400, 0x0a000600, 0x0a000800, 0x0a000a00, 0x0a000c00, 0x0a000e00,
];

/// VirtIO device ID for RNG devices (entropy device)
const VIRTIO_DEVICE_ID_RNG: u32 = 4;

/// Queue size (must be power of 2)
const QUEUE_SIZE: usize = 2;

// Legacy VirtIO MMIO register offsets (version 1, force-legacy mode)
const VIRTIO_MMIO_MAGIC_VALUE: usize = 0x000;
const VIRTIO_MMIO_VERSION: usize = 0x004;
const VIRTIO_MMIO_DEVICE_ID: usize = 0x008;
const VIRTIO_MMIO_DEVICE_FEATURES: usize = 0x010;
const VIRTIO_MMIO_DRIVER_FEATURES: usize = 0x020;
const VIRTIO_MMIO_GUEST_PAGE_SIZE: usize = 0x028; // Legacy only
const VIRTIO_MMIO_QUEUE_SEL: usize = 0x030;
const VIRTIO_MMIO_QUEUE_NUM_MAX: usize = 0x034;
const VIRTIO_MMIO_QUEUE_NUM: usize = 0x038;
const VIRTIO_MMIO_QUEUE_ALIGN: usize = 0x03c; // Legacy only
const VIRTIO_MMIO_QUEUE_PFN: usize = 0x040; // Legacy only
const VIRTIO_MMIO_QUEUE_NOTIFY: usize = 0x050;
const VIRTIO_MMIO_STATUS: usize = 0x070;

// VirtIO status bits
const VIRTIO_STATUS_ACKNOWLEDGE: u32 = 1;
const VIRTIO_STATUS_DRIVER: u32 = 2;
const VIRTIO_STATUS_DRIVER_OK: u32 = 4;

// VirtIO descriptor flags
const VIRTQ_DESC_F_WRITE: u16 = 2; // Device writes (vs read)

/// Page size for legacy VirtIO
const PAGE_SIZE: usize = 4096;

// ============================================================================
// RNG Error
// ============================================================================

/// RNG device error type
#[derive(Debug, Clone, Copy)]
pub enum RngError {
    /// Device not found
    NotFound,
    /// Device not initialized
    NotInitialized,
    /// Failed to read random bytes
    ReadError,
    /// Transport error
    TransportError,
}

impl core::fmt::Display for RngError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            RngError::NotFound => write!(f, "RNG device not found"),
            RngError::NotInitialized => write!(f, "RNG device not initialized"),
            RngError::ReadError => write!(f, "Failed to read random bytes"),
            RngError::TransportError => write!(f, "VirtIO transport error"),
        }
    }
}

// ============================================================================
// VirtIO Data Structures (matching legacy layout exactly)
// ============================================================================

/// VirtIO descriptor (16 bytes each)
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VirtqDesc {
    addr: u64,  // Guest physical address
    len: u32,   // Length
    flags: u16, // Flags
    next: u16,  // Next descriptor index
}

/// VirtIO available ring
/// Layout: flags (u16), idx (u16), ring[QUEUE_SIZE] (u16 each), used_event (u16)
#[repr(C)]
struct VirtqAvail {
    flags: u16,
    idx: u16,
    ring: [u16; QUEUE_SIZE],
    used_event: u16,
}

/// VirtIO used ring element
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VirtqUsedElem {
    id: u32,  // Index of descriptor chain head
    len: u32, // Total length of the descriptor chain
}

/// VirtIO used ring
/// Layout: flags (u16), idx (u16), ring[QUEUE_SIZE], avail_event (u16)
#[repr(C)]
struct VirtqUsed {
    flags: u16,
    idx: u16,
    ring: [VirtqUsedElem; QUEUE_SIZE],
    avail_event: u16,
}

// ============================================================================
// VirtIO RNG Device
// ============================================================================

/// VirtIO RNG device driver (legacy mode)
pub struct VirtioRngDevice {
    base_addr: usize,
    // Queue memory pointers (all in one page-aligned allocation)
    queue_mem: *mut u8,
    queue_layout: Layout,
    // Individual component pointers
    desc: *mut VirtqDesc,
    avail: *mut VirtqAvail,
    used: *mut VirtqUsed,
    // Data buffer
    buffer: *mut u8,
    buffer_layout: Layout,
    // Queue state
    last_used_idx: u16,
    avail_idx: u16,
}

// SAFETY: VirtioRngDevice is only accessed through the global RNG_DEVICE Spinlock,
// which ensures exclusive access.
unsafe impl Send for VirtioRngDevice {}
unsafe impl Sync for VirtioRngDevice {}

/// Calculate legacy virtqueue memory layout
/// Returns (desc_offset, avail_offset, used_offset, total_size)
fn calc_queue_layout(queue_size: usize) -> (usize, usize, usize, usize) {
    // Descriptor table: 16 bytes * queue_size, 16-byte aligned
    let desc_size = 16 * queue_size;

    // Available ring: flags(2) + idx(2) + ring(2*queue_size) + used_event(2)
    let avail_size = 2 + 2 + 2 * queue_size + 2;

    // Used ring: flags(2) + idx(2) + ring(8*queue_size) + avail_event(2)
    let used_size = 2 + 2 + 8 * queue_size + 2;

    // Legacy layout:
    // - Descriptor table at offset 0
    // - Available ring immediately follows
    // - Used ring at next page boundary
    let desc_offset = 0;
    let avail_offset = desc_size; // Immediately after descriptors
    let used_offset = (avail_offset + avail_size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1); // Page aligned
    let total_size = used_offset + used_size;

    (desc_offset, avail_offset, used_offset, total_size)
}

impl VirtioRngDevice {
    /// Create and initialize a new VirtIO RNG device (legacy mode)
    fn new(base_addr: usize) -> Result<Self, RngError> {
        // Verify magic value
        let magic = unsafe { read_volatile((base_addr + VIRTIO_MMIO_MAGIC_VALUE) as *const u32) };
        if magic != 0x74726976 {
            // "virt" in little-endian
            return Err(RngError::TransportError);
        }

        // Check version (1 = legacy, 2 = modern)
        let version = unsafe { read_volatile((base_addr + VIRTIO_MMIO_VERSION) as *const u32) };
        if version != 1 {
            // Only support legacy mode for now
            return Err(RngError::TransportError);
        }

        // Reset the device
        unsafe {
            write_volatile((base_addr + VIRTIO_MMIO_STATUS) as *mut u32, 0);
        }
        fence(Ordering::SeqCst);

        // Set ACKNOWLEDGE status bit
        unsafe {
            write_volatile(
                (base_addr + VIRTIO_MMIO_STATUS) as *mut u32,
                VIRTIO_STATUS_ACKNOWLEDGE,
            );
        }
        fence(Ordering::SeqCst);

        // Set DRIVER status bit
        unsafe {
            write_volatile(
                (base_addr + VIRTIO_MMIO_STATUS) as *mut u32,
                VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER,
            );
        }
        fence(Ordering::SeqCst);

        // Read and acknowledge features (RNG has no required features, don't negotiate any)
        let _features =
            unsafe { read_volatile((base_addr + VIRTIO_MMIO_DEVICE_FEATURES) as *const u32) };
        unsafe {
            write_volatile((base_addr + VIRTIO_MMIO_DRIVER_FEATURES) as *mut u32, 0);
        }
        fence(Ordering::SeqCst);

        // Set guest page size (required for legacy mode)
        unsafe {
            write_volatile(
                (base_addr + VIRTIO_MMIO_GUEST_PAGE_SIZE) as *mut u32,
                PAGE_SIZE as u32,
            );
        }
        fence(Ordering::SeqCst);

        // Set up virtqueue 0
        unsafe {
            write_volatile((base_addr + VIRTIO_MMIO_QUEUE_SEL) as *mut u32, 0);
        }
        fence(Ordering::SeqCst);

        // Check queue size
        let max_size =
            unsafe { read_volatile((base_addr + VIRTIO_MMIO_QUEUE_NUM_MAX) as *const u32) };
        if max_size == 0 || (max_size as usize) < QUEUE_SIZE {
            return Err(RngError::TransportError);
        }

        // Set queue size
        unsafe {
            write_volatile(
                (base_addr + VIRTIO_MMIO_QUEUE_NUM) as *mut u32,
                QUEUE_SIZE as u32,
            );
        }
        fence(Ordering::SeqCst);

        // Calculate queue layout
        let (desc_offset, avail_offset, used_offset, total_size) = calc_queue_layout(QUEUE_SIZE);

        // Allocate queue memory (must be page-aligned)
        let queue_layout = Layout::from_size_align(total_size, PAGE_SIZE).unwrap();
        let queue_mem = unsafe { alloc_zeroed(queue_layout) };
        if queue_mem.is_null() {
            return Err(RngError::TransportError);
        }

        let desc = unsafe { queue_mem.add(desc_offset) } as *mut VirtqDesc;
        let avail = unsafe { queue_mem.add(avail_offset) } as *mut VirtqAvail;
        let used = unsafe { queue_mem.add(used_offset) } as *mut VirtqUsed;

        // Set queue alignment (legacy mode)
        unsafe {
            write_volatile(
                (base_addr + VIRTIO_MMIO_QUEUE_ALIGN) as *mut u32,
                PAGE_SIZE as u32,
            );
        }
        fence(Ordering::SeqCst);

        // Set queue PFN (page frame number) - legacy mode
        // VirtIO needs the physical address for DMA
        let queue_phys = crate::mmu::virt_to_phys(queue_mem as usize);
        let queue_pfn = queue_phys / PAGE_SIZE;
        unsafe {
            write_volatile(
                (base_addr + VIRTIO_MMIO_QUEUE_PFN) as *mut u32,
                queue_pfn as u32,
            );
        }
        fence(Ordering::SeqCst);

        // Set DRIVER_OK to finish initialization
        let final_status =
            VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER | VIRTIO_STATUS_DRIVER_OK;
        unsafe {
            write_volatile((base_addr + VIRTIO_MMIO_STATUS) as *mut u32, final_status);
        }
        fence(Ordering::SeqCst);

        // Verify device accepted our configuration
        let status = unsafe { read_volatile((base_addr + VIRTIO_MMIO_STATUS) as *const u32) };
        if status != final_status {
            unsafe { dealloc(queue_mem, queue_layout) };
            return Err(RngError::TransportError);
        }

        // Allocate a buffer for random data (separate from queue memory)
        let buffer_layout = Layout::from_size_align(256, 64).unwrap();
        let buffer = unsafe { alloc_zeroed(buffer_layout) };
        if buffer.is_null() {
            unsafe { dealloc(queue_mem, queue_layout) };
            return Err(RngError::TransportError);
        }

        Ok(Self {
            base_addr,
            queue_mem,
            queue_layout,
            desc,
            avail,
            used,
            buffer,
            buffer_layout,
            last_used_idx: 0,
            avail_idx: 0,
        })
    }

    /// Read random bytes from the device
    pub fn read_bytes(&mut self, buf: &mut [u8]) -> Result<(), RngError> {
        if buf.is_empty() {
            return Ok(());
        }

        let mut bytes_read = 0;

        while bytes_read < buf.len() {
            let to_read = core::cmp::min(256, buf.len() - bytes_read);

            // Set up descriptor for device-writable buffer
            // VirtIO descriptor needs physical address for DMA
            let desc_idx = 0u16;
            unsafe {
                let d = &mut *self.desc.add(desc_idx as usize);
                d.addr = crate::mmu::virt_to_phys(self.buffer as usize) as u64;
                d.len = to_read as u32;
                d.flags = VIRTQ_DESC_F_WRITE;
                d.next = 0;
            }

            // Memory barrier before updating available ring
            fence(Ordering::SeqCst);

            // Add to available ring
            let ring_idx = (self.avail_idx as usize) % QUEUE_SIZE;
            unsafe {
                let avail = &mut *self.avail;
                avail.ring[ring_idx] = desc_idx;
                fence(Ordering::SeqCst);
                avail.idx = self.avail_idx.wrapping_add(1);
            }
            self.avail_idx = self.avail_idx.wrapping_add(1);

            // Memory barrier before notifying device
            fence(Ordering::SeqCst);

            // Notify device
            unsafe {
                write_volatile((self.base_addr + VIRTIO_MMIO_QUEUE_NOTIFY) as *mut u32, 0);
            }

            // Wait for completion with timeout
            let mut attempts = 0u32;
            const MAX_ATTEMPTS: u32 = 10_000_000;

            loop {
                fence(Ordering::SeqCst);
                let used_idx = unsafe { read_volatile(&(*self.used).idx) };
                if used_idx != self.last_used_idx {
                    break;
                }
                attempts += 1;
                if attempts > MAX_ATTEMPTS {
                    return Err(RngError::ReadError);
                }
                core::hint::spin_loop();
            }

            // Get the used element
            let used_ring_idx = (self.last_used_idx as usize) % QUEUE_SIZE;
            let used_elem = unsafe { (*self.used).ring[used_ring_idx] };
            self.last_used_idx = self.last_used_idx.wrapping_add(1);

            // Check that the device returned our descriptor
            if used_elem.id != desc_idx as u32 {
                return Err(RngError::ReadError);
            }

            // Copy data from buffer
            let copy_len = core::cmp::min(used_elem.len as usize, buf.len() - bytes_read);
            unsafe {
                core::ptr::copy_nonoverlapping(
                    self.buffer,
                    buf.as_mut_ptr().add(bytes_read),
                    copy_len,
                );
            }
            bytes_read += copy_len;
        }

        Ok(())
    }
}

impl Drop for VirtioRngDevice {
    fn drop(&mut self) {
        // Free allocations
        unsafe {
            dealloc(self.buffer, self.buffer_layout);
            dealloc(self.queue_mem, self.queue_layout);
        }
    }
}

// ============================================================================
// Global RNG Device State
// ============================================================================

static RNG_DEVICE: Spinlock<Option<VirtioRngDevice>> = Spinlock::new(None);

// ============================================================================
// Public API
// ============================================================================

/// Initialize the RNG device driver
/// Scans for virtio-rng devices and initializes the first one found
pub fn init() -> Result<(), RngError> {
    log("[RNG] Initializing RNG device driver...\n");

    // Find virtio-rng device
    for (i, &addr) in VIRTIO_MMIO_ADDRS.iter().enumerate() {
        // SAFETY: Reading from MMIO registers at known QEMU virt machine addresses
        let device_id = unsafe { read_volatile((addr + VIRTIO_MMIO_DEVICE_ID) as *const u32) };
        if device_id != VIRTIO_DEVICE_ID_RNG {
            continue;
        }

        log("[RNG] Found virtio-rng at slot ");
        console::print(&alloc::format!("{}\n", i));

        match VirtioRngDevice::new(addr) {
            Ok(mut device) => {
                // Test the device by reading some bytes
                let mut test_buf = [0u8; 8];
                if let Err(e) = device.read_bytes(&mut test_buf) {
                    log("[RNG] Test read failed: ");
                    console::print(&alloc::format!("{}\n", e));
                    continue;
                }
                log("[RNG] Test read successful\n");

                // Store in global state
                *RNG_DEVICE.lock() = Some(device);
                log("[RNG] RNG device initialized\n");
                return Ok(());
            }
            Err(e) => {
                log("[RNG] Failed to init virtio device: ");
                console::print(&alloc::format!("{}\n", e));
                continue;
            }
        }
    }

    Err(RngError::NotFound)
}

/// Check if RNG device is initialized
pub fn is_initialized() -> bool {
    RNG_DEVICE.lock().is_some()
}

/// Fill a buffer with random bytes from the hardware RNG
///
/// # Arguments
/// * `buf` - Buffer to fill with random bytes
///
/// # Returns
/// * `Ok(())` if successful
/// * `Err(RngError::NotInitialized)` if the RNG device is not initialized
/// * `Err(RngError::ReadError)` if reading from the device failed
pub fn fill_bytes(buf: &mut [u8]) -> Result<(), RngError> {
    let mut guard = RNG_DEVICE.lock();
    let device = guard.as_mut().ok_or(RngError::NotInitialized)?;
    device.read_bytes(buf)
}

// ============================================================================
// Logging
// ============================================================================

fn log(msg: &str) {
    console::print(msg);
}
