//! `/dev/fb0`: the boot framebuffer as a Linux fbdev device (2026-10-03).
//!
//! GRUB hands this kernel one linear framebuffer in a fixed mode
//! ([`crate::multiboot2::kmain_mb2`]). Until now only the kernel console drew
//! into it. This module offers it to userspace the Linux-standard way, so a
//! program written for Linux fbdev — the `akuma-cli-wgpu` demo, later rio — runs
//! unchanged:
//!
//! - `open("/dev/fb0")` — [`open`], from `fd::sys_openat`. **One owner at a
//!   time**: while a process holds it, the console stops drawing (its text keeps
//!   arriving in the grid and is repainted when the owner lets go).
//! - `FBIOGET_VSCREENINFO`, `FBIOGET_FSCREENINFO`, `FBIOPUT_VSCREENINFO`,
//!   `FBIOPAN_DISPLAY`, `FBIOBLANK` — [`ioctl`], from `fd::sys_ioctl`.
//! - `mmap` of the pixels, **write-combining** — `mm::mmap_framebuffer`, which
//!   maps the physical framebuffer with `akuma_mmu::MemAttr::WriteCombine` (PAT
//!   entry 4, which `multiboot2::map_wc` already made WC on every core).
//!
//! Ownership ends on `close` of a `/dev/fb0` descriptor by the owner, on the
//! owner's exit (`usermode::run_process`), or — if neither path ran, a process
//! killed some other way — when the console pump notices the owner is gone
//! ([`poll_owner`]). A kernel crash takes the screen back unconditionally
//! (`serial::begin_fatal`): a failure is never hidden behind a program's pixels.
//!
//! The decisions (what the ioctls answer, which `FBIOPUT` requests succeed, who
//! may open, what a mapping may ask for) are `akuma-fbdev`'s, host-tested. This
//! module is the state and the user copies.

use core::sync::atomic::{AtomicU32, Ordering};

use akuma_fbdev::{Geometry, Open};
use akuma_syscalls_linux::fb::{
    FB_VAR_SCREENINFO_SIZE, FBIOBLANK, FBIOGET_FSCREENINFO, FBIOGET_VSCREENINFO, FBIOPAN_DISPLAY,
    FBIOPUT_VSCREENINFO, FbVarScreeninfo,
};

use crate::fd::errno;

/// The framebuffer, once [`register`] has run. Absent on a PVH boot (QEMU
/// `microvm`), which has no framebuffer — `/dev/fb0` then does not exist.
static GEOMETRY: akuma_primitives::OnceCopy<Geometry> = akuma_primitives::OnceCopy::new();

/// The owning thread group's id, or 0.
static OWNER: AtomicU32 = AtomicU32::new(0);

/// Publish the boot framebuffer. Called once from `kmain_mb2`.
pub fn register(g: Geometry) {
    GEOMETRY.set(g);
    akuma_vfs_glue::set_framebuffer_present(true);
}

/// The framebuffer, if there is one.
#[must_use]
pub fn geometry() -> Option<Geometry> {
    GEOMETRY.get()
}

/// The owner's thread-group id, or 0.
#[must_use]
pub fn owner() -> u32 {
    OWNER.load(Ordering::Acquire)
}

/// Take the screen for `tgid`. `Err(errno)` when there is no framebuffer
/// (`ENODEV`) or another process owns it (`EBUSY`).
pub fn open(tgid: u32) -> Result<(), u64> {
    if geometry().is_none() {
        return Err(errno::ENODEV);
    }
    loop {
        let cur = owner();
        match akuma_fbdev::open_decision(cur, tgid) {
            Open::AlreadyOwner => return Ok(()),
            Open::Busy => return Err(errno::EBUSY),
            Open::Acquire => {
                if OWNER.compare_exchange(0, tgid, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                    crate::multiboot2::fb_mute();
                    return Ok(());
                }
                // Lost the race to another opener; decide again against it.
            }
        }
    }
}

/// Give the screen back if `tgid` owns it: unmute the console and repaint it.
pub fn release(tgid: u32) {
    if tgid != 0 && OWNER.compare_exchange(tgid, 0, Ordering::AcqRel, Ordering::Acquire).is_ok() {
        crate::multiboot2::fb_unmute_and_repaint();
    }
}

/// Called from the console pump when output is idle: if the owner no longer
/// exists — it died on a path that did not reach [`release`] — take the screen
/// back. One atomic load when nobody owns it, which is almost always.
pub fn poll_owner() {
    let tgid = owner();
    if tgid != 0 && akuma_exec::process::lookup_process_shared(tgid).is_none() {
        release(tgid);
    }
}

/// The fbdev ioctls on a `/dev/fb0` descriptor. Anything else is `ENOTTY`, as
/// for every fbdev driver that does not implement a request.
pub fn ioctl(req: u32, arg: u64) -> u64 {
    let Some(g) = geometry() else { return errno::ENODEV };
    match req {
        FBIOGET_VSCREENINFO => copy_out(arg, &g.var().to_bytes()),
        FBIOGET_FSCREENINFO => copy_out(arg, &g.fix().to_bytes()),
        // The mode is GRUB's: accept a request for exactly it (and report it
        // back, as Linux does), refuse anything else.
        FBIOPUT_VSCREENINFO => {
            let Some(req) = read_var(arg) else { return errno::EFAULT };
            if !g.put_accepted(&req) {
                return errno::EINVAL;
            }
            copy_out(arg, &g.var().to_bytes())
        }
        // One buffer: panning to the origin is a no-op, anywhere else is
        // impossible.
        FBIOPAN_DISPLAY => {
            let Some(req) = read_var(arg) else { return errno::EFAULT };
            if req.xoffset == 0 && req.yoffset == 0 { 0 } else { errno::EINVAL }
        }
        // There is no way to blank a firmware scanout; succeeding is what
        // `simplefb` does, and programs call it unconditionally.
        FBIOBLANK => 0,
        _ => errno::ENOTTY,
    }
}

fn copy_out(arg: u64, bytes: &[u8]) -> u64 {
    if arg == 0 || !crate::uaccess::write_bytes(arg, bytes) { errno::EFAULT } else { 0 }
}

fn read_var(arg: u64) -> Option<FbVarScreeninfo> {
    let mut b = [0u8; FB_VAR_SCREENINFO_SIZE];
    if arg == 0 || !crate::uaccess::read_bytes(arg, &mut b) {
        return None;
    }
    Some(FbVarScreeninfo::from_bytes(&b))
}
