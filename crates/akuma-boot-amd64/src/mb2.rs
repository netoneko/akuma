//! The multiboot2 information block, from the address the loader passed to
//! the claims the kernel makes from it.
//!
//! GRUB leaves the block's physical address in `%ebx`; `boot.s` carries it to
//! `kmain_mb2` and, on the way, stores it in [`AKUMA_MB2_INFO_PHYS`]. That
//! store is the only writer, so the address this module reads is the one the
//! loader actually gave, not one a caller chose. Everything that turns the
//! block into memory the kernel may touch is here, next to that fact:
//!
//! - [`take`] copies the block out of the physmap once and hands back a
//!   `'static` view of the copy (the original sits in low memory the PMM will
//!   later reuse);
//! - [`framebuffer_window`] maps the firmware framebuffer the block describes,
//!   once;
//! - [`claim_first_module`] turns the root image the loader left in RAM into
//!   an `akuma_phys::RamSpan`, once, and only after `mem::init_reserving` has
//!   kept the PMM off its pages.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use akuma_multiboot2::{BootInfo, FramebufferKind};
use akuma_primitives::addr::PhysAddr;
use akuma_primitives::mmio::MmioWindow;

use crate::phys::{PHYSMAP_LIMIT, phys_to_virt};

/// The information block's physical address, as the loader passed it. Written
/// once by `boot.s` just before `call kmain_mb2`; 0 on a PVH boot.
#[unsafe(no_mangle)]
static AKUMA_MB2_INFO_PHYS: AtomicU64 = AtomicU64::new(0);

/// Largest block [`take`] copies. GRUB's blocks are a few KiB (the memory map,
/// the command line, a framebuffer and module tag each); 64 KiB is generous.
pub const INFO_MAX: usize = 64 * 1024;

static INFO: spinning_top::Spinlock<[u8; INFO_MAX]> = spinning_top::Spinlock::new([0; INFO_MAX]);
static TAKEN: AtomicBool = AtomicBool::new(false);

/// The framebuffer [`take`] found, for [`framebuffer_window`].
static FB_ADDR: AtomicU64 = AtomicU64::new(0);
static FB_LEN: AtomicU64 = AtomicU64::new(0);
static FB_TAKEN: AtomicBool = AtomicBool::new(false);

/// The root module [`take`] found, for [`claim_first_module`].
static MOD_START: AtomicU64 = AtomicU64::new(0);
static MOD_END: AtomicU64 = AtomicU64::new(0);
static MOD_TAKEN: AtomicBool = AtomicBool::new(false);

/// Why [`take`] produced no block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InfoError {
    /// Not a multiboot2 boot, or [`take`] already ran.
    Absent,
    /// The block's header lies outside the physmap.
    Outside,
    /// The block runs past the physmap.
    Truncated,
    /// Too short to hold its own header.
    Short,
}

/// The loader's information block, copied out.
#[derive(Debug, Clone, Copy)]
pub struct Boot {
    bytes: &'static [u8],
    phys: u64,
}

impl Boot {
    /// The parsed block.
    #[must_use]
    pub fn info(&self) -> BootInfo<'static> {
        // `take` built one from these bytes already.
        BootInfo::new(self.bytes).unwrap_or_else(|| unreachable!())
    }

    /// The original block's physical span, `[start, end)`: no memory map marks
    /// it, so the kernel reserves it from the PMM like the modules.
    #[must_use]
    pub fn phys_span(&self) -> (u64, u64) {
        (self.phys, self.phys + self.bytes.len() as u64)
    }
}

/// Copy the information block out of the physmap. Once per boot; [`InfoError::Absent`]
/// on a second call or a PVH boot.
///
/// The block's first `u32` is its own size, clamped to `8..=INFO_MAX` so a
/// corrupt size cannot make the copy run past the buffer.
///
/// # Errors
/// [`InfoError`], one per way the loader's address can be unusable.
pub fn take() -> Result<Boot, InfoError> {
    let phys = AKUMA_MB2_INFO_PHYS.load(Ordering::Acquire);
    if phys == 0 || TAKEN.swap(true, Ordering::AcqRel) {
        return Err(InfoError::Absent);
    }
    let at = PhysAddr::new(phys);
    let mut size = [0u8; 4];
    if !akuma_phys::peek_ram(at, &mut size) {
        return Err(InfoError::Outside);
    }
    let total = (u32::from_le_bytes(size) as usize).clamp(8, INFO_MAX);
    let copy = spinning_top::guard::SpinlockGuard::leak(INFO.lock());
    if !akuma_phys::peek_ram(at, &mut copy[..total]) {
        return Err(InfoError::Truncated);
    }
    let bytes: &'static [u8] = &copy[..total];
    let info = BootInfo::new(bytes).ok_or(InfoError::Short)?;
    record_framebuffer(&info);
    if let Some(m) = info.first_module() {
        MOD_START.store(u64::from(m.start), Ordering::Relaxed);
        MOD_END.store(u64::from(m.end), Ordering::Release);
    }
    Ok(Boot { bytes, phys })
}

fn record_framebuffer(info: &BootInfo<'_>) {
    let Some(fb) = info.framebuffer() else { return };
    if !matches!(fb.kind, FramebufferKind::Rgb) {
        return;
    }
    let len = fb.size_bytes();
    // A framebuffer above what boot.s maps cannot be written; refuse it rather than fault.
    if len == 0 || fb.addr.checked_add(len).is_none_or(|end| end > PHYSMAP_LIMIT) {
        return;
    }
    FB_ADDR.store(fb.addr, Ordering::Relaxed);
    FB_LEN.store(len, Ordering::Release);
}

/// The firmware framebuffer as an MMIO window, **once** per boot; `None` without one, or
/// on a second call.
///
/// It cannot name an address: the window is the one the loader's block described,
/// checked to lie below `PHYSMAP_LIMIT`.
#[must_use]
pub fn framebuffer_window() -> Option<MmioWindow> {
    let len = FB_LEN.load(Ordering::Acquire);
    if len == 0 || FB_TAKEN.swap(true, Ordering::AcqRel) {
        return None;
    }
    let addr = FB_ADDR.load(Ordering::Relaxed);
    // SAFETY: the firmware's framebuffer, as the loader reported it for this boot
    // (`AKUMA_MB2_INFO_PHYS` has one writer, `boot.s`), checked by `record_framebuffer`
    // to lie entirely below `PHYSMAP_LIMIT`, which boot.s maps through the physmap. It is
    // device memory no allocator owns, never freed or moved, and handed out once
    // (`FB_TAKEN`), so this is the only window over it.
    Some(unsafe { MmioWindow::new(phys_to_virt(addr) as usize, len as usize) })
}

/// The root image the loader left in RAM, as a `RamSpan`, **once** per boot.
///
/// `None` without a module, on a second call, or unless `mem::init_reserving`
/// has run and kept the PMM off every page of it.
#[must_use]
pub fn claim_first_module() -> Option<akuma_phys::RamSpan> {
    let end = MOD_END.load(Ordering::Acquire);
    let start = MOD_START.load(Ordering::Relaxed);
    if end <= start || !crate::mem::is_reserved(start, end) || MOD_TAKEN.swap(true, Ordering::AcqRel) {
        return None;
    }
    // SAFETY: the loader's module (`AKUMA_MB2_INFO_PHYS` has one writer), whose pages
    // `mem::is_reserved` confirms the PMM was never given, so no allocator hands them
    // out; `MOD_TAKEN` makes this the only span over them. `claim` checks the physmap
    // bounds.
    unsafe { akuma_phys::RamSpan::claim(PhysAddr::new(start), (end - start) as usize) }
}
