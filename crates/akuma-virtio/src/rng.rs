//! VirtIO RNG Device Driver
//!
//! Provides a hardware-backed random number generator using virtio-rng devices.
//! This replaces the weak timer-seeded PRNG for better entropy in cryptographic
//! operations like SSH key generation.
//!
//! This is a minimal, standalone VirtIO RNG driver that directly accesses MMIO
//! registers since virtio-drivers 0.7 doesn't expose an RNG device.
//! Drives the modern (version 2) VirtIO MMIO transport only; the legacy (v1)
//! path was removed when QEMU's force-legacy flag was dropped. A non-v2 device
//! panics at init. See docs/VIRTIO_MMIO_LEGACY_TO_MODERN.md.

use core::sync::atomic::{AtomicBool, Ordering, fence};

use akuma_dma::DmaArray;
use akuma_dma::align::A64;
use akuma_primitives::mmio::MmioWindow;

use spinning_top::Spinlock;

use crate::probe;

// ============================================================================
// Constants
// ============================================================================

/// Queue size (must be power of 2)
const QUEUE_SIZE: usize = 2;

// VirtIO MMIO register offsets (modern / version 2 transport).
const VIRTIO_MMIO_MAGIC_VALUE: usize = 0x000;
const VIRTIO_MMIO_VERSION: usize = 0x004;
const VIRTIO_MMIO_DEVICE_FEATURES: usize = 0x010;
const VIRTIO_MMIO_DEVICE_FEATURES_SEL: usize = 0x014; // Modern only
const VIRTIO_MMIO_DRIVER_FEATURES: usize = 0x020;
const VIRTIO_MMIO_DRIVER_FEATURES_SEL: usize = 0x024;
const VIRTIO_MMIO_QUEUE_SEL: usize = 0x030;
const VIRTIO_MMIO_QUEUE_NUM_MAX: usize = 0x034;
const VIRTIO_MMIO_QUEUE_NUM: usize = 0x038;
const VIRTIO_MMIO_QUEUE_READY: usize = 0x044;
const VIRTIO_MMIO_QUEUE_NOTIFY: usize = 0x050;
const VIRTIO_MMIO_STATUS: usize = 0x070;
// Split-virtqueue address registers (64-bit, low/high halves).
const VIRTIO_MMIO_QUEUE_DESC_LOW: usize = 0x080;
const VIRTIO_MMIO_QUEUE_DESC_HIGH: usize = 0x084;
const VIRTIO_MMIO_QUEUE_DRIVER_LOW: usize = 0x090; // avail ring
const VIRTIO_MMIO_QUEUE_DRIVER_HIGH: usize = 0x094;
const VIRTIO_MMIO_QUEUE_DEVICE_LOW: usize = 0x0a0; // used ring
const VIRTIO_MMIO_QUEUE_DEVICE_HIGH: usize = 0x0a4;


/// Bytes of a slot's register file this driver addresses (highest register is
/// `QUEUE_DEVICE_HIGH` at 0x0a4).
const REG_WINDOW_LEN: usize = 0x100;

// VirtIO status bits
const VIRTIO_STATUS_ACKNOWLEDGE: u32 = 1;
const VIRTIO_STATUS_DRIVER: u32 = 2;
const VIRTIO_STATUS_DRIVER_OK: u32 = 4;
const VIRTIO_STATUS_FEATURES_OK: u32 = 8; // Modern only

/// VIRTIO_F_VERSION_1 (feature bit 32 → word 1, bit 0). Must be negotiated to
/// drive a modern (version 2) device.
const VIRTIO_F_VERSION_1_WORD1: u32 = 1;

// VirtIO descriptor flags
const VIRTQ_DESC_F_WRITE: u16 = 2; // Device writes (vs read)

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

impl_display!(RngError {
    NotFound => "RNG device not found",
    NotInitialized => "RNG device not initialized",
    ReadError => "Failed to read random bytes",
    TransportError => "VirtIO transport error",
});


// ============================================================================
// VirtIO Data Structures (split-virtqueue layout; identical for v1 and v2)
// ============================================================================
//
// The queue lives in four statics rather than one `alloc_zeroed` page, so the
// rings are `DmaArray`s and this file holds no `unsafe` beyond the one
// `MmioWindow` claim. Each is aligned to at least a cache line, which exceeds
// the modern 16/2/4-byte requirements for desc/avail/used. Cells are viewed as
// the integers the layout is made of (all bit patterns valid); the device is
// little-endian and so is every target this runs on.
//
//   DESC  (u64 x 2*QUEUE_SIZE): descriptor i is cells 2i = addr,
//         2i+1 = len | flags << 32 | next << 48
//   AVAIL (u16 x QUEUE_SIZE+3): flags, idx, ring[QUEUE_SIZE], used_event
//   USED  (u32 x 2*QUEUE_SIZE+2): cell 0 = flags | idx << 16, then ring[i] =
//         (id, len) at cells 1+2i, 2+2i, then avail_event (unused, not mapped)
//
// `DmaArray::get`/`set` are volatile; the release/acquire the old `AtomicU16`
// fields carried is now an explicit `fence` beside the access that publishes or
// observes it.

static DESC: DmaArray<u64, { 2 * QUEUE_SIZE }, A64> = DmaArray::new();
static AVAIL: DmaArray<u16, { QUEUE_SIZE + 3 }, A64> = DmaArray::new();
static USED: DmaArray<u32, { 2 * QUEUE_SIZE + 2 }, A64> = DmaArray::new();
/// Staging buffer the device fills with random bytes.
static BUFFER: DmaArray<u8, STAGING_LEN, A64> = DmaArray::new();

/// Bytes one request asks the device for.
const STAGING_LEN: usize = 256;

/// AVAIL cell holding `idx`.
const AVAIL_IDX: usize = 1;
/// AVAIL cell of `ring[0]`.
const AVAIL_RING: usize = 2;

/// Set while a [`VirtioRngDevice`] owns the four statics above. They are the
/// device's only queue memory, so a second live device would share it.
static QUEUE_CLAIMED: AtomicBool = AtomicBool::new(false);

// ============================================================================
// VirtIO RNG Device
// ============================================================================

/// VirtIO RNG device driver (modern / version 2 transport)
pub struct VirtioRngDevice {
    regs: MmioWindow,
    // Queue state
    last_used_idx: u16,
    avail_idx: u16,
}

/// How many bytes to copy out of the staging buffer for one completed request.
///
/// `device_len` is the length the **device** wrote into the used ring, so it is
/// untrusted input. It must be clamped to what this request actually offered
/// (`offered`, i.e. the descriptor's own length) — **not** to the caller's
/// remaining space. The source is a fixed 256-byte staging allocation; clamping
/// against the caller's remaining space instead lets a device reporting
/// `len > offered` over-read that allocation and hand the spill straight to
/// userspace through `getrandom`. The same distrust already gates `used_elem.id`.
///
/// `0` means the device completed the descriptor without writing anything; the
/// caller must treat that as an error rather than looping, or `bytes_read` never
/// advances and the same request is reissued forever.
///
/// Split out of the completion path purely so it can be tested: the rest of that
/// path needs a real device. `UNSAFE_AUDIT.md` §4(e) describes both this clamp
/// and the acquire barrier above as open defects — they were fixed, and this
/// pins the clamp so it cannot silently regress.
fn completion_copy_len(device_len: u32, offered: usize) -> usize {
    core::cmp::min(device_len as usize, offered)
}


impl VirtioRngDevice {
    /// Create and initialize a new VirtIO RNG device (modern / version 2 transport)
    fn new(base_addr: usize) -> Result<Self, RngError> {
        // SAFETY: `base_addr` is a virtio-mmio slot base returned by
        // `probe::scan`, i.e. inside the device mapping the kernel establishes
        // before any driver init runs, page-aligned, and the slot's register file
        // is at least `REG_WINDOW_LEN` bytes; every offset used below is one of
        // the naturally-aligned u32 `VIRTIO_MMIO_*` registers within it.
        let w = unsafe { MmioWindow::new(base_addr, REG_WINDOW_LEN) };

        // Verify magic value
        let magic: u32 = w.read(VIRTIO_MMIO_MAGIC_VALUE);
        if magic != 0x74726976 {
            // "virt" in little-endian
            return Err(RngError::TransportError);
        }

        // Require the modern (version 2) MMIO transport. The legacy (v1) path was
        // removed once QEMU's force-legacy flag was dropped; a v1 device now means
        // a misconfigured runner (force-legacy re-added) and there is no fallback,
        // so fail loud and early rather than limping into a later networking panic
        // ("RNG required for networking"). See docs/VIRTIO_MMIO_LEGACY_TO_MODERN.md.
        let version: u32 = w.read(VIRTIO_MMIO_VERSION);
        assert!(version == 2,
            "[RNG] virtio-rng MMIO version {version} unsupported; modern v2 required \
             (is virtio-mmio.force-legacy set in the QEMU runner?)"
        );

        // Reset the device
        w.write(VIRTIO_MMIO_STATUS, 0u32);
        fence(Ordering::SeqCst);

        // Set ACKNOWLEDGE status bit
        w.write(VIRTIO_MMIO_STATUS, VIRTIO_STATUS_ACKNOWLEDGE);
        fence(Ordering::SeqCst);

        // Set DRIVER status bit
        w.write(VIRTIO_MMIO_STATUS, VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER);
        fence(Ordering::SeqCst);

        // Feature negotiation. The RNG device needs no feature bits of its own,
        // but a modern device requires VIRTIO_F_VERSION_1 (bit 32) plus a
        // FEATURES_OK handshake.
        //
        // Feature word 0: acknowledge nothing.
        w.write(VIRTIO_MMIO_DEVICE_FEATURES_SEL, 0u32);
        fence(Ordering::SeqCst);
        // The discarded read is a protocol step, not dead code: the device
        // expects the selected feature word to be read before it is answered.
        let _: u32 = w.read(VIRTIO_MMIO_DEVICE_FEATURES);
        w.write(VIRTIO_MMIO_DRIVER_FEATURES_SEL, 0u32);
        w.write(VIRTIO_MMIO_DRIVER_FEATURES, 0u32);
        fence(Ordering::SeqCst);
        // Feature word 1: acknowledge VIRTIO_F_VERSION_1.
        w.write(VIRTIO_MMIO_DEVICE_FEATURES_SEL, 1u32);
        fence(Ordering::SeqCst);
        let _: u32 = w.read(VIRTIO_MMIO_DEVICE_FEATURES);
        w.write(VIRTIO_MMIO_DRIVER_FEATURES_SEL, 1u32);
        w.write(VIRTIO_MMIO_DRIVER_FEATURES, VIRTIO_F_VERSION_1_WORD1);
        fence(Ordering::SeqCst);
        // Latch FEATURES_OK and confirm the device kept it.
        w.write(
            VIRTIO_MMIO_STATUS,
            VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER | VIRTIO_STATUS_FEATURES_OK,
        );
        fence(Ordering::SeqCst);
        let s: u32 = w.read(VIRTIO_MMIO_STATUS);
        if s & VIRTIO_STATUS_FEATURES_OK == 0 {
            return Err(RngError::TransportError);
        }

        // Set up virtqueue 0
        w.write(VIRTIO_MMIO_QUEUE_SEL, 0u32);
        fence(Ordering::SeqCst);

        // Check queue size
        let max_size: u32 = w.read(VIRTIO_MMIO_QUEUE_NUM_MAX);
        if max_size == 0 || (max_size as usize) < QUEUE_SIZE {
            return Err(RngError::TransportError);
        }

        // Set queue size
        w.write(VIRTIO_MMIO_QUEUE_NUM, QUEUE_SIZE as u32);
        fence(Ordering::SeqCst);

        // The queue memory is the four statics above; take them for this device.
        if QUEUE_CLAIMED.swap(true, Ordering::AcqRel) {
            return Err(RngError::TransportError);
        }
        // A reset device has dropped its idea of the rings, so ours start clean
        // too (a previous device may have left indices behind).
        DESC.view_mut().fill(0);
        AVAIL.view_mut().fill(0);
        USED.view_mut().fill(0);

        // Program the queue with three independent 64-bit physical addresses,
        // then mark it ready.
        let desc_phys = akuma_primitives::addr::virt_to_phys(DESC.addr()) as u64;
        let avail_phys = akuma_primitives::addr::virt_to_phys(AVAIL.addr()) as u64;
        let used_phys = akuma_primitives::addr::virt_to_phys(USED.addr()) as u64;
        w.write(VIRTIO_MMIO_QUEUE_DESC_LOW, desc_phys as u32);
        w.write(VIRTIO_MMIO_QUEUE_DESC_HIGH, (desc_phys >> 32) as u32);
        w.write(VIRTIO_MMIO_QUEUE_DRIVER_LOW, avail_phys as u32);
        w.write(VIRTIO_MMIO_QUEUE_DRIVER_HIGH, (avail_phys >> 32) as u32);
        w.write(VIRTIO_MMIO_QUEUE_DEVICE_LOW, used_phys as u32);
        w.write(VIRTIO_MMIO_QUEUE_DEVICE_HIGH, (used_phys >> 32) as u32);
        fence(Ordering::SeqCst);
        w.write(VIRTIO_MMIO_QUEUE_READY, 1u32);
        fence(Ordering::SeqCst);

        // Set DRIVER_OK to finish initialization
        let final_status = VIRTIO_STATUS_ACKNOWLEDGE
            | VIRTIO_STATUS_DRIVER
            | VIRTIO_STATUS_FEATURES_OK
            | VIRTIO_STATUS_DRIVER_OK;
        w.write(VIRTIO_MMIO_STATUS, final_status);
        fence(Ordering::SeqCst);

        // Verify device accepted our configuration
        let status: u32 = w.read(VIRTIO_MMIO_STATUS);
        if status != final_status {
            QUEUE_CLAIMED.store(false, Ordering::Release);
            return Err(RngError::TransportError);
        }

        Ok(Self { regs: w, last_used_idx: 0, avail_idx: 0 })
    }

    /// Read random bytes from the device
    pub fn read_bytes(&mut self, buf: &mut [u8]) -> Result<(), RngError> {
        if buf.is_empty() {
            return Ok(());
        }

        let mut bytes_read = 0;

        while bytes_read < buf.len() {
            let to_read = core::cmp::min(STAGING_LEN, buf.len() - bytes_read);

            // Set up descriptor for device-writable buffer
            // VirtIO descriptor needs physical address for DMA
            let desc_idx = 0u16;
            DESC.set(
                2 * usize::from(desc_idx),
                akuma_primitives::addr::virt_to_phys(BUFFER.addr()) as u64,
            );
            // len | flags << 32 | next(0) << 48
            DESC.set(
                2 * usize::from(desc_idx) + 1,
                to_read as u64 | (u64::from(VIRTQ_DESC_F_WRITE) << 32),
            );

            // Add to available ring. The descriptor fields written above and the
            // `ring[]` slot written here are both published by the release fence
            // before the `idx` store: a device that observes the new `idx` is
            // guaranteed to observe everything that preceded it.
            let ring_idx = (self.avail_idx as usize) % QUEUE_SIZE;
            AVAIL.set(AVAIL_RING + ring_idx, desc_idx);
            fence(Ordering::Release);
            AVAIL.set(AVAIL_IDX, self.avail_idx.wrapping_add(1));
            self.avail_idx = self.avail_idx.wrapping_add(1);

            // Barrier before the MMIO notify. The release fence above orders the
            // ring writes against each other, but this is Normal memory and the
            // notify below is a Device-memory store — nothing in the release
            // fence keeps the device from seeing the kick before the ring.
            fence(Ordering::SeqCst);

            // Notify device
            self.regs.write(VIRTIO_MMIO_QUEUE_NOTIFY, 0u32);

            // Wait for completion with timeout
            let mut attempts = 0u32;
            const MAX_ATTEMPTS: u32 = 10_000_000;

            loop {
                // Volatile read then an acquire fence: observing the new `idx`
                // is what makes `used.ring[…]` and the DMA'd staging buffer valid
                // to read, and the volatile read alone constrains only the
                // compiler, not the CPU's load/load reordering.
                let used_idx = (USED.get(0) >> 16) as u16;
                fence(Ordering::Acquire);
                if used_idx != self.last_used_idx {
                    break;
                }
                attempts += 1;
                if attempts > MAX_ATTEMPTS {
                    return Err(RngError::ReadError);
                }
                core::hint::spin_loop();
            }

            // Get the used element (plain read — covered by the acquire above)
            let used_ring_idx = (self.last_used_idx as usize) % QUEUE_SIZE;
            let used_id = USED.get(1 + 2 * used_ring_idx);
            let used_len = USED.get(2 + 2 * used_ring_idx);
            self.last_used_idx = self.last_used_idx.wrapping_add(1);

            // Check that the device returned our descriptor
            if used_id != u32::from(desc_idx) {
                return Err(RngError::ReadError);
            }

            let copy_len = completion_copy_len(used_len, to_read);
            if copy_len == 0 {
                // A device completing the descriptor without writing anything
                // would otherwise spin the outer loop forever: `bytes_read` never
                // advances, so the same request is reissued indefinitely.
                return Err(RngError::ReadError);
            }
            // `copy_len <= to_read <= STAGING_LEN`: the slice covers only the
            // bytes the device reported, clamped to what was offered.
            BUFFER.read_range(0, copy_len, |src| {
                buf[bytes_read..bytes_read + copy_len].copy_from_slice(src);
            });
            bytes_read += copy_len;
        }

        Ok(())
    }
}

impl Drop for VirtioRngDevice {
    fn drop(&mut self) {
        // Reset the device so it stops using the rings, then release them.
        self.regs.write(VIRTIO_MMIO_STATUS, 0u32);
        fence(Ordering::SeqCst);
        QUEUE_CLAIMED.store(false, Ordering::Release);
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

    // Find virtio-rng device. This driver hand-rolls its virtqueue against the
    // raw base address rather than using an `MmioTransport`, so it scans slots
    // itself instead of going through `probe::probe_with`.
    for (i, (addr, device_id)) in probe::scan().into_iter().enumerate() {
        if device_id != probe::device_id::RNG {
            continue;
        }

        log("[RNG] Found virtio-rng at slot ");
        crate::safe_print!(32, "{}\n", i);

        match VirtioRngDevice::new(addr) {
            Ok(mut device) => {
                // Test the device by reading some bytes
                let mut test_buf = [0u8; 8];
                if let Err(e) = device.read_bytes(&mut test_buf) {
                    log("[RNG] Test read failed: ");
                    crate::safe_print!(32, "{}\n", e);
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
                crate::safe_print!(32, "{}\n", e);
            }
        }
    }

    Err(RngError::NotFound)
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
    akuma_primitives::console::print_str(msg);
}

// ============================================================================
// Tests
// ============================================================================

/// `akuma-virtio` had **zero** tests before 2026-08-13, across ~1,470 lines —
/// the only crate in the workspace with none. Most of it genuinely needs
/// hardware, but not all: the two functions below are pure arithmetic that
/// decides how much memory a DMA region spans and how many bytes get copied out
/// of it on a device's say-so. Both are exactly the kind of thing that is
/// silently wrong until it corrupts memory.
#[cfg(test)]
mod tests {
    use super::{AVAIL, BUFFER, DESC, QUEUE_SIZE, STAGING_LEN, USED, completion_copy_len};

    /// A device that lies about how much it wrote must not cause a read past the
    /// staging buffer. Regression pin for `UNSAFE_AUDIT.md` §4(e) — which still
    /// describes this as open, though it was fixed before this test existed.
    #[test]
    fn completion_length_is_clamped_to_what_was_offered() {
        assert_eq!(completion_copy_len(64, 256), 64, "honest device: take its word");
        assert_eq!(completion_copy_len(256, 256), 256, "exactly full is fine");

        // The whole point: the device claims more than the descriptor offered.
        assert_eq!(completion_copy_len(4096, 256), 256);
        assert_eq!(completion_copy_len(u32::MAX, 256), 256);

        // A short final request offers less than the 256-byte buffer holds.
        assert_eq!(completion_copy_len(256, 7), 7);

        // Zero is the caller's signal to error out rather than spin forever.
        assert_eq!(completion_copy_len(0, 256), 0);
    }

    /// The rings are the device's DMA targets: each must meet the virtio 1.x
    /// split-queue alignment (desc 16, avail 2, used 4), be sized for the layout
    /// the driver indexes, and sit in its own memory.
    #[test]
    fn queue_statics_are_aligned_sized_and_disjoint() {
        assert_eq!(DESC.addr() % 16, 0);
        assert_eq!(AVAIL.addr() % 2, 0);
        assert_eq!(USED.addr() % 4, 0);
        // desc: 16 bytes each; avail: flags, idx, ring[], used_event;
        // used: flags|idx, ring[] of (id, len).
        assert_eq!(DESC.size_bytes(), 16 * QUEUE_SIZE);
        assert_eq!(AVAIL.size_bytes(), 2 + 2 + 2 * QUEUE_SIZE + 2);
        assert_eq!(USED.size_bytes(), 4 + 8 * QUEUE_SIZE + 4);
        assert_eq!(BUFFER.size_bytes(), STAGING_LEN);

        let mut spans = [
            (DESC.addr(), DESC.size_bytes()),
            (AVAIL.addr(), AVAIL.size_bytes()),
            (USED.addr(), USED.size_bytes()),
            (BUFFER.addr(), BUFFER.size_bytes()),
        ];
        spans.sort_unstable();
        for pair in spans.windows(2) {
            assert!(pair[0].0 + pair[0].1 <= pair[1].0, "queue statics overlap");
        }
    }
}
