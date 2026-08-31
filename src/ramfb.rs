//! QEMU ramfb (RAM-based framebuffer) driver
//!
//! ramfb provides a very simple graphics output for bare-metal guests.
//! The guest allocates a framebuffer in RAM, then writes a configuration
//! structure via the fw_cfg `etc/ramfb` entry.  After that, writes to
//! the framebuffer memory are immediately visible on the QEMU display window.
//!
//! Reference: <https://wiki.osdev.org/Ramfb>
//!
//! # This module forbids `unsafe`
//!
//! ramfb went from 4 `unsafe` blocks to 0 on 2026-08-31, and the ban below is
//! what keeps it there. The three things that used to need raw pointers all had
//! safe spellings that were simply not reached for:
//!
//! | was | is |
//! |---|---|
//! | `#[repr(C, packed)]` config + `slice::from_raw_parts` | [`cfg_bytes`], a `[u8; 28]` built with `to_be_bytes` |
//! | `alloc_zeroed` + `slice::from_raw_parts_mut` | a fallible `Vec`, leaked, page-aligned by hand |
//! | `copy_nonoverlapping` onto an `AtomicUsize`-held address | `copy_from_slice` onto the slice inside `FB_STATE` |
//!
//! `forbid`, not `deny`: `deny` can be switched back off by a module-local
//! `#[allow(unsafe_code)]`, which is exactly the move that would erode this.
//!
//! The one real cost is in `init`: page alignment is bought by over-allocating a
//! page and skipping to the boundary, which leaks ~2 KB more than an over-aligned
//! `alloc_zeroed` would (talc gives that padding back). One page, once, at boot.
//!
//! This module is not compiled at all on `extreme-size` (`--no-default-features`
//! drops `sc-framebuffer`) or on Firecracker, which has no fw_cfg to configure
//! ramfb through — so the ban costs those profiles nothing either.
#![forbid(unsafe_code)]

use crate::fw_cfg;
use core::sync::atomic::{AtomicBool, Ordering};
use spinning_top::Spinlock;

/// XRGB8888 fourcc code: 'X','R','2','4' in little-endian
const FOURCC_XRGB8888: u32 =
    ('X' as u32) | (('R' as u32) << 8) | (('2' as u32) << 16) | (('4' as u32) << 24);

/// Bytes per pixel for XRGB8888
const BPP: usize = 4;

/// Size of the `etc/ramfb` fw_cfg entry: `addr(8) fourcc(4) flags(4) width(4)
/// height(4) stride(4)`, all big-endian, no padding.
const RAMFB_CFG_LEN: usize = 28;

/// Marshal the `etc/ramfb` configuration into its wire form.
///
/// Built byte-wise rather than as a `#[repr(C, packed)]` struct reinterpreted
/// through `from_raw_parts`: the entry *is* a byte layout, and writing it as one
/// makes the offsets checkable against the spec instead of inferred from field
/// order, drops a packed struct (whose fields cannot be safely referenced), and
/// costs no `unsafe`.
pub fn cfg_bytes(fb_addr: u64, width: u32, height: u32, stride: u32) -> [u8; RAMFB_CFG_LEN] {
    let mut cfg = [0u8; RAMFB_CFG_LEN];
    cfg[0..8].copy_from_slice(&fb_addr.to_be_bytes());
    cfg[8..12].copy_from_slice(&FOURCC_XRGB8888.to_be_bytes());
    cfg[12..16].copy_from_slice(&0u32.to_be_bytes()); // flags
    cfg[16..20].copy_from_slice(&width.to_be_bytes());
    cfg[20..24].copy_from_slice(&height.to_be_bytes());
    cfg[24..28].copy_from_slice(&stride.to_be_bytes());
    cfg
}

/// Global framebuffer state
struct FramebufferState {
    width: u32,
    height: u32,
    stride: u32,
    /// The framebuffer itself, owned rather than reached through a stored
    /// address. Holding the pixels as a slice inside the same lock as the
    /// geometry is what makes [`draw`] a `copy_from_slice` instead of a raw
    /// `copy_nonoverlapping`, and it is what makes the `no-bkl-drivers`
    /// carve-out's claim true: that carve-out (see `DriverBkl` in
    /// `src/syscall/fs.rs`) drops the BKL for `sys_fb_*` on the grounds that
    /// `FB_STATE` is the driver's own fine-grained lock, but `draw` used to
    /// bypass `FB_STATE` entirely and reach the pixels through a pair of lock-free
    /// `AtomicUsize`s.
    pixels: &'static mut [u8],
}

static FB_STATE: Spinlock<Option<FramebufferState>> = Spinlock::new(None);
static FB_INITIALIZED: AtomicBool = AtomicBool::new(false);

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

    // `sys_fb_init` caps the dimensions at 1920x1080, but this is `pub` and the
    // in-kernel caller passes its own, so size the allocation with checked
    // arithmetic rather than trusting the syscall's bound to hold for everyone.
    let stride = width
        .checked_mul(BPP as u32)
        .ok_or("framebuffer stride overflows")?;
    let fb_size = (stride as usize)
        .checked_mul(height as usize)
        .ok_or("framebuffer size overflows")?;
    if fb_size == 0 {
        return Err("zero-sized framebuffer");
    }

    // Allocate the framebuffer: page-aligned, zeroed, owned, and never freed.
    //
    // No `unsafe` anywhere in this driver, which costs exactly one page, once, at
    // boot. `Vec<u8>` is 1-byte aligned, so page alignment is bought by
    // over-allocating a page and skipping forward to the first boundary — the
    // padding is leaked along with the rest, where an over-aligned
    // `alloc_zeroed` would have had talc `register_gap` it back onto the free
    // list. That is a ~2 KB difference against a 250 KB framebuffer, and buying
    // out the last raw-pointer-to-slice conversion in the driver is worth it.
    //
    // Fallible on purpose. `vec![0u8; n]` would reach `handle_alloc_error` and
    // abort the kernel; a framebuffer is a large allocation and `sys_fb_init`
    // can be called under whatever pressure userspace has already created, so
    // the caller gets `EIO` instead. `try_reserve_exact` + `resize` is also the
    // same work the old path did: this kernel's `alloc_zeroed`
    // (`src/allocator.rs:608`) is `talc_alloc` followed by `write_bytes(0)`.
    let pixels: &'static mut [u8] = {
        let want = fb_size
            .checked_add(4096)
            .ok_or("framebuffer size overflows")?;

        let mut buf: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
        buf.try_reserve_exact(want)
            .map_err(|_| "failed to allocate framebuffer memory")?;
        buf.resize(want, 0);

        let whole: &'static mut [u8] = buf.leak();
        let base = whole.as_ptr() as usize;
        let pad = base.next_multiple_of(4096) - base;

        // `pad` is in `0..4096` and `want` is `fb_size + 4096`, so `pad + fb_size`
        // is always in bounds — the two splits below cannot panic.
        whole.split_at_mut(pad).1.split_at_mut(fb_size).0
    };

    // Identity mapping: virtual address == physical address
    let fb_addr = akuma_exec::mmu::virt_to_phys(pixels.as_ptr() as usize);

    // Write configuration to QEMU via fw_cfg DMA
    fw_cfg::write_entry(
        selector,
        &cfg_bytes(fb_addr as u64, width, height, stride),
    );

    // Store state
    {
        let mut state = FB_STATE.lock();
        *state = Some(FramebufferState {
            width,
            height,
            stride,
            pixels,
        });
    }
    FB_INITIALIZED.store(true, Ordering::Release);

    crate::console::print("[ramfb] Framebuffer initialized: ");
    crate::safe_print!(64, "{}x{} XRGB8888 at 0x{:x}\n", width, height, fb_addr);

    Ok(())
}

/// Copy an XRGB8888 pixel buffer from userspace to the framebuffer.
///
/// `src` is copied to the *start* of the framebuffer, truncated to whatever
/// fits. Returns the number of bytes copied, or 0 on error.
pub fn draw(src: &[u8]) -> usize {
    let mut state = FB_STATE.lock();
    let Some(fb) = state.as_mut() else {
        return 0;
    };

    let copy_len = src.len().min(fb.pixels.len());
    if copy_len == 0 {
        return 0;
    }

    fb.pixels[..copy_len].copy_from_slice(&src[..copy_len]);
    copy_len
}

/// Framebuffer info returned to userspace
///
/// `Copy` is the marker `write_user_val` uses for "plain ABI data, safe to copy out
/// byte-wise" — four `u32`s qualify.
#[repr(C)]
#[derive(Clone, Copy)]
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

/// Base address of the visible framebuffer, or `None` if not initialized.
///
/// Exists for the boot self-test. Page alignment used to be *requested* from the
/// allocator (`Layout::from_size_align(_, 4096)`), where it was true by
/// construction; `init` now *earns* it by over-allocating and skipping to the
/// boundary, so it is an invariant that can be got wrong and therefore one worth
/// asserting.
pub fn pixels_base() -> Option<usize> {
    let state = FB_STATE.lock();
    state.as_ref().map(|s| s.pixels.as_ptr() as usize)
}

/// Check if the framebuffer has been initialized
pub fn is_initialized() -> bool {
    FB_INITIALIZED.load(Ordering::Relaxed)
}
