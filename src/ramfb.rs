//! QEMU ramfb (RAM-based framebuffer) driver
//!
//! ramfb provides a very simple graphics output for bare-metal guests.
//! The guest allocates a framebuffer in RAM, then writes a configuration
//! structure via the fw_cfg `etc/ramfb` entry.  After that, writes to
//! the framebuffer memory are immediately visible on the QEMU display window.
//!
//! Reference: <https://wiki.osdev.org/Ramfb>

use crate::fw_cfg;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use spinning_top::Spinlock;

/// XRGB8888 fourcc code: 'X','R','2','4' in little-endian
const FOURCC_XRGB8888: u32 =
    ('X' as u32) | (('R' as u32) << 8) | (('2' as u32) << 16) | (('4' as u32) << 24);

/// Bytes per pixel for XRGB8888
const BPP: usize = 4;

/// ramfb configuration structure written via fw_cfg DMA
#[repr(C, packed)]
struct RamFBCfg {
    addr: u64,
    fourcc: u32,
    flags: u32,
    width: u32,
    height: u32,
    stride: u32,
}

/// Global framebuffer state
struct FramebufferState {
    /// Physical (and virtual, due to identity mapping) address of pixel data
    addr: usize,
    width: u32,
    height: u32,
    stride: u32,
    /// Total size in bytes
    size: usize,
}

static FB_STATE: Spinlock<Option<FramebufferState>> = Spinlock::new(None);
static FB_INITIALIZED: AtomicBool = AtomicBool::new(false);
static FB_ADDR: AtomicUsize = AtomicUsize::new(0);
static FB_SIZE: AtomicUsize = AtomicUsize::new(0);

/// Initialize the ramfb device with the given resolution.
///
/// Allocates framebuffer memory, configures the device via fw_cfg, and
/// clears the screen to black.
///
/// Returns `Ok(())` on success, `Err(msg)` if fw_cfg entry is missing.
pub fn init(width: u32, height: u32) -> Result<(), &'static str> {
    if FB_INITIALIZED.load(Ordering::Relaxed) {
        return Ok(());
    }

    // Find the ramfb fw_cfg entry
    let (selector, _size) = fw_cfg::find_file("etc/ramfb")
        .ok_or("ramfb fw_cfg entry not found (add -device ramfb to QEMU)")?;

    let stride = width * BPP as u32;
    let fb_size = (stride as usize) * (height as usize);

    // Allocate framebuffer memory (page-aligned)
    let fb_pages = (fb_size + 4095) / 4096;
    let fb_addr = {
        use alloc::alloc::{alloc_zeroed, Layout};
        let layout = Layout::from_size_align(fb_pages * 4096, 4096).unwrap();
        let ptr = unsafe { alloc_zeroed(layout) };
        if ptr.is_null() {
            return Err("failed to allocate framebuffer memory");
        }
        // Identity mapping: virtual address == physical address
        crate::mmu::virt_to_phys(ptr as usize)
    };

    // Build the configuration structure (all fields big-endian)
    let cfg = RamFBCfg {
        addr: (fb_addr as u64).to_be(),
        fourcc: FOURCC_XRGB8888.to_be(),
        flags: 0u32.to_be(),
        width: width.to_be(),
        height: height.to_be(),
        stride: stride.to_be(),
    };

    // Write configuration to QEMU via fw_cfg DMA
    let cfg_bytes = unsafe {
        core::slice::from_raw_parts(
            &cfg as *const RamFBCfg as *const u8,
            core::mem::size_of::<RamFBCfg>(),
        )
    };
    unsafe {
        fw_cfg::write_entry(selector, cfg_bytes);
    }

    // Store state
    {
        let mut state = FB_STATE.lock();
        *state = Some(FramebufferState {
            addr: fb_addr,
            width,
            height,
            stride,
            size: fb_size,
        });
    }
    FB_ADDR.store(fb_addr, Ordering::Release);
    FB_SIZE.store(fb_size, Ordering::Release);
    FB_INITIALIZED.store(true, Ordering::Release);

    crate::console::print("[ramfb] Framebuffer initialized: ");
    crate::safe_print!(64, "{}x{} XRGB8888 at 0x{:x}\n", width, height, fb_addr);

    Ok(())
}

/// Copy an XRGB8888 pixel buffer from userspace to the framebuffer.
///
/// `src` must be exactly `width * height * 4` bytes of pixel data.
/// Returns the number of bytes copied, or 0 on error.
pub fn draw(src: &[u8]) -> usize {
    let fb_addr = FB_ADDR.load(Ordering::Acquire);
    let fb_size = FB_SIZE.load(Ordering::Acquire);

    if fb_addr == 0 || fb_size == 0 {
        return 0;
    }

    let copy_len = src.len().min(fb_size);
    if copy_len == 0 {
        return 0;
    }

    // Copy pixels to framebuffer (identity-mapped, so phys == virt)
    let fb_ptr = crate::mmu::phys_to_virt(fb_addr) as *mut u8;
    unsafe {
        core::ptr::copy_nonoverlapping(src.as_ptr(), fb_ptr, copy_len);
    }

    copy_len
}

/// Framebuffer info returned to userspace
#[repr(C)]
pub struct FBInfo {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: u32, // fourcc code
}

/// Return framebuffer information, or `None` if not initialized.
pub fn info() -> Option<FBInfo> {
    let state = FB_STATE.lock();
    state.as_ref().map(|s| FBInfo {
        width: s.width,
        height: s.height,
        stride: s.stride,
        format: FOURCC_XRGB8888,
    })
}

/// Check if the framebuffer has been initialized
pub fn is_initialized() -> bool {
    FB_INITIALIZED.load(Ordering::Relaxed)
}
