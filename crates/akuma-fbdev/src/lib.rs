//! `/dev/fb0` — the decisions, host-tested.
//!
//! The amd64 kernel boots with one linear framebuffer that GRUB set up
//! (multiboot2): a fixed mode, no modesetting, no page flips. That is exactly
//! the model Linux's fbdev interface describes, so userspace gets the
//! Linux-standard device for it — `open("/dev/fb0")`, `FBIOGET_VSCREENINFO`,
//! `FBIOGET_FSCREENINFO`, and `mmap` of the pixels — and a program written for
//! Linux fbdev (the `akuma-cli-wgpu` demo, later rio through a wgpu backend)
//! runs without knowing it is on Akuma.
//!
//! This crate answers the questions that have a right answer independent of
//! the hardware:
//!
//! - [`Geometry::var`] / [`Geometry::fix`]: what the two `GET` ioctls report.
//! - [`Geometry::put_accepted`]: `FBIOPUT_VSCREENINFO` cannot change the mode
//!   (GRUB chose it), but programs round-trip it with the values they just read,
//!   so an identical request must succeed and a different one must be `EINVAL`.
//! - [`open_decision`]: one owner at a time, because the console and a program
//!   drawing into the same pixels would fight.
//! - [`mmap_check`]: what a mapping of the device may ask for.
//!
//! What stays in the kernel: the geometry itself (from the multiboot2 tag), the
//! page-table mapping (write-combining, `akuma_mmu::MemAttr::WriteCombine`),
//! muting the console while a program owns the screen, and the user copies.
//! Background and the slice plan: `docs/runbooks/amd64-fbdev-wgpu-demo.md`.

#![no_std]
#![forbid(unsafe_code)]

use akuma_syscalls_linux::fb::{
    FB_ACCEL_NONE, FB_ACTIVATE_NOW, FB_TYPE_PACKED_PIXELS, FB_VISUAL_TRUECOLOR, FB_VMODE_NONINTERLACED,
    FbBitfield, FbFixScreeninfo, FbVarScreeninfo,
};

/// The page size mappings are made in.
pub const PAGE_SIZE: u64 = 4096;

/// What `FBIOGET_FSCREENINFO` reports as `id` (NUL-padded to 16 bytes).
pub const DRIVER_ID: &[u8] = b"akuma-fb";

/// One colour channel: bit position and width in a pixel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Channel {
    pub pos: u8,
    pub len: u8,
}

/// The framebuffer the boot loader handed over.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Geometry {
    /// Physical address of the first pixel.
    pub phys: u64,
    pub width: u32,
    pub height: u32,
    /// Bytes per scanline (may exceed `width * bytes_per_pixel`).
    pub pitch: u32,
    pub bpp: u32,
    pub red: Channel,
    pub green: Channel,
    pub blue: Channel,
}

impl Geometry {
    /// Bytes of framebuffer memory: `pitch * height`.
    #[must_use]
    pub const fn size(&self) -> u64 {
        self.pitch as u64 * self.height as u64
    }

    /// [`size`](Self::size), rounded up to whole pages — the most a mapping
    /// may cover.
    #[must_use]
    pub const fn mappable(&self) -> u64 {
        self.size().div_ceil(PAGE_SIZE) * PAGE_SIZE
    }

    const fn bitfield(c: Channel) -> FbBitfield {
        FbBitfield { offset: c.pos as u32, length: c.len as u32, msb_right: 0 }
    }

    /// The answer to `FBIOGET_VSCREENINFO`.
    ///
    /// One buffer, so the virtual resolution is the visible one and both
    /// offsets are 0 — nothing to pan. Timings (`pixclock` and the margins) are
    /// 0: the firmware drives the scanout and they are not known, which is what
    /// Linux's `efifb`/`simplefb` report too. Physical size in mm is unknown and
    /// therefore 0 (`-1` would be "unknown" by convention in some drivers, but
    /// 0 is what `simplefb` gives).
    #[must_use]
    pub const fn var(&self) -> FbVarScreeninfo {
        FbVarScreeninfo {
            xres: self.width,
            yres: self.height,
            xres_virtual: self.width,
            yres_virtual: self.height,
            xoffset: 0,
            yoffset: 0,
            bits_per_pixel: self.bpp,
            grayscale: 0,
            red: Self::bitfield(self.red),
            green: Self::bitfield(self.green),
            blue: Self::bitfield(self.blue),
            transp: FbBitfield { offset: 0, length: 0, msb_right: 0 },
            nonstd: 0,
            activate: FB_ACTIVATE_NOW,
            height: 0,
            width: 0,
            accel_flags: 0,
            pixclock: 0,
            left_margin: 0,
            right_margin: 0,
            upper_margin: 0,
            lower_margin: 0,
            hsync_len: 0,
            vsync_len: 0,
            sync: 0,
            vmode: FB_VMODE_NONINTERLACED,
            rotate: 0,
            colorspace: 0,
            reserved: [0; 4],
        }
    }

    /// The answer to `FBIOGET_FSCREENINFO`.
    #[must_use]
    pub fn fix(&self) -> FbFixScreeninfo {
        let mut id = [0u8; 16];
        id[..DRIVER_ID.len()].copy_from_slice(DRIVER_ID);
        FbFixScreeninfo {
            id,
            smem_start: self.phys,
            smem_len: u32::try_from(self.size()).unwrap_or(u32::MAX),
            type_: FB_TYPE_PACKED_PIXELS,
            type_aux: 0,
            visual: FB_VISUAL_TRUECOLOR,
            xpanstep: 0,
            ypanstep: 0,
            ywrapstep: 0,
            line_length: self.pitch,
            accel: FB_ACCEL_NONE,
            ..FbFixScreeninfo::default()
        }
    }

    /// Does `FBIOPUT_VSCREENINFO` with `req` succeed?
    ///
    /// The mode is the boot loader's and cannot change, so only a request for
    /// **this** mode is accepted: same visible and virtual resolution (virtual
    /// may not exceed visible — there is one buffer), same depth, no offset,
    /// and colour layout either unspecified (all zero, which many programs
    /// send) or the real one. Timing fields are ignored: they are
    /// informational here and programs echo back whatever they read.
    #[must_use]
    pub fn put_accepted(&self, req: &FbVarScreeninfo) -> bool {
        let rgb_ok = |r: FbBitfield, real: Channel| {
            (r.offset == 0 && r.length == 0) || (r.offset == u32::from(real.pos) && r.length == u32::from(real.len))
        };
        req.xres == self.width
            && req.yres == self.height
            && (req.xres_virtual == 0 || req.xres_virtual == self.width)
            && (req.yres_virtual == 0 || req.yres_virtual == self.height)
            && req.xoffset == 0
            && req.yoffset == 0
            && (req.bits_per_pixel == 0 || req.bits_per_pixel == self.bpp)
            && rgb_ok(req.red, self.red)
            && rgb_ok(req.green, self.green)
            && rgb_ok(req.blue, self.blue)
    }
}

/// What `open("/dev/fb0")` does, given the current owner (`0` = nobody) and
/// the opener's thread-group id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Open {
    /// The screen is free: the opener now owns it, and the console must go quiet.
    Acquire,
    /// The opener already owns it (a second `open` from the same process).
    AlreadyOwner,
    /// Another live process owns it: `EBUSY`.
    Busy,
}

/// One owner at a time — see [`Open`]. `tgid` is never 0.
#[must_use]
pub const fn open_decision(owner: u32, tgid: u32) -> Open {
    if owner == 0 {
        Open::Acquire
    } else if owner == tgid {
        Open::AlreadyOwner
    } else {
        Open::Busy
    }
}

/// Why an `mmap` of the device was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapRefusal {
    /// `MAP_PRIVATE`, an unaligned offset, an empty or oversized range: `EINVAL`.
    Invalid,
    /// `PROT_EXEC`: the framebuffer is never code. `EACCES`.
    Exec,
}

/// Check an `mmap` of `len` bytes at `offset` into the framebuffer.
///
/// Returns the number of pages to map. Linux fbdev maps `[offset, offset+len)`
/// of the framebuffer and refuses a range past its end; it requires
/// `MAP_SHARED` (a private copy of a device's pixels means nothing) and a
/// page-aligned offset. `len` is rounded up to whole pages, as `mmap` does.
pub const fn mmap_check(
    len: u64,
    offset: u64,
    shared: bool,
    exec: bool,
    geometry: &Geometry,
) -> Result<u64, MapRefusal> {
    if exec {
        return Err(MapRefusal::Exec);
    }
    if !shared || len == 0 || !offset.is_multiple_of(PAGE_SIZE) {
        return Err(MapRefusal::Invalid);
    }
    let pages = len.div_ceil(PAGE_SIZE);
    // Saturating: `len` and `offset` are ring-3 registers.
    let end = offset.saturating_add(pages.saturating_mul(PAGE_SIZE));
    if end > geometry.mappable() {
        return Err(MapRefusal::Invalid);
    }
    Ok(pages)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The trashcan: 3840x2160x32, pitch 16384, `x8r8g8b8`.
    const BOX: Geometry = Geometry {
        phys: 0x4000_0000,
        width: 3840,
        height: 2160,
        pitch: 16384,
        bpp: 32,
        red: Channel { pos: 16, len: 8 },
        green: Channel { pos: 8, len: 8 },
        blue: Channel { pos: 0, len: 8 },
    };

    #[test]
    fn var_reports_the_boot_mode_as_one_unpannable_buffer() {
        let v = BOX.var();
        assert_eq!((v.xres, v.yres, v.xres_virtual, v.yres_virtual), (3840, 2160, 3840, 2160));
        assert_eq!((v.xoffset, v.yoffset), (0, 0));
        assert_eq!(v.bits_per_pixel, 32);
        assert_eq!((v.red.offset, v.red.length), (16, 8));
        assert_eq!((v.green.offset, v.green.length), (8, 8));
        assert_eq!((v.blue.offset, v.blue.length), (0, 8));
        assert_eq!(v.transp.length, 0, "no alpha channel");
        assert_eq!(v.vmode, FB_VMODE_NONINTERLACED);
    }

    #[test]
    fn fix_reports_the_pitch_and_the_real_size() {
        let f = BOX.fix();
        assert_eq!(&f.id[..8], b"akuma-fb");
        assert_eq!(f.line_length, 16384, "pitch, not width*4: the two differ on this box");
        assert_eq!(f.smem_len, 16384 * 2160);
        assert_eq!(f.smem_start, 0x4000_0000);
        assert_eq!(f.visual, FB_VISUAL_TRUECOLOR);
        assert_eq!(f.type_, FB_TYPE_PACKED_PIXELS);
    }

    #[test]
    fn put_accepts_the_mode_it_reported_and_nothing_else() {
        let v = BOX.var();
        assert!(BOX.put_accepted(&v), "a round trip of GET must succeed");
        let mut bare = FbVarScreeninfo { xres: 3840, yres: 2160, ..FbVarScreeninfo::default() };
        assert!(BOX.put_accepted(&bare), "unspecified depth and layout are fine");
        bare.bits_per_pixel = 16;
        assert!(!BOX.put_accepted(&bare), "a depth change is refused");
        let mut smaller = v;
        smaller.xres = 1920;
        assert!(!BOX.put_accepted(&smaller), "a resolution change is refused");
        let mut panned = v;
        panned.yoffset = 10;
        assert!(!BOX.put_accepted(&panned), "there is nothing to pan to");
        let mut double = v;
        double.yres_virtual = 4320;
        assert!(!BOX.put_accepted(&double), "no second buffer to flip to");
        let mut bgr = v;
        bgr.red.offset = 0;
        bgr.red.length = 8;
        assert!(!BOX.put_accepted(&bgr), "a channel layout change is refused");
        let mut timing = v;
        timing.pixclock = 12345;
        assert!(BOX.put_accepted(&timing), "timings are informational and ignored");
    }

    #[test]
    fn one_owner_at_a_time() {
        assert_eq!(open_decision(0, 7), Open::Acquire);
        assert_eq!(open_decision(7, 7), Open::AlreadyOwner);
        assert_eq!(open_decision(7, 8), Open::Busy);
    }

    #[test]
    fn mmap_covers_the_framebuffer_and_no_further() {
        let all = BOX.mappable();
        assert_eq!(all, 16384 * 2160, "already page-multiple here");
        assert_eq!(mmap_check(all, 0, true, false, &BOX), Ok(all / PAGE_SIZE));
        assert_eq!(mmap_check(1, 0, true, false, &BOX), Ok(1), "rounded up to a page");
        assert_eq!(mmap_check(PAGE_SIZE, all - PAGE_SIZE, true, false, &BOX), Ok(1), "the last page");
        assert_eq!(mmap_check(PAGE_SIZE, all, true, false, &BOX), Err(MapRefusal::Invalid), "past the end");
        assert_eq!(mmap_check(all + 1, 0, true, false, &BOX), Err(MapRefusal::Invalid));
        assert_eq!(mmap_check(all, 0, false, false, &BOX), Err(MapRefusal::Invalid), "MAP_PRIVATE");
        assert_eq!(mmap_check(all, 0, true, true, &BOX), Err(MapRefusal::Exec));
        assert_eq!(mmap_check(0, 0, true, false, &BOX), Err(MapRefusal::Invalid));
        assert_eq!(mmap_check(PAGE_SIZE, 100, true, false, &BOX), Err(MapRefusal::Invalid), "unaligned offset");
        assert_eq!(mmap_check(u64::MAX, u64::MAX - 10, true, false, &BOX), Err(MapRefusal::Invalid), "no wrap");
    }

    #[test]
    fn a_ragged_last_page_is_still_mappable() {
        // 1366x768x32 with a pitch that is not page-aligned in total.
        let g = Geometry { pitch: 5464, height: 768, width: 1366, bpp: 32, ..BOX };
        assert_eq!(g.size(), 5464 * 768);
        assert_eq!(g.mappable() % PAGE_SIZE, 0);
        assert!(g.mappable() >= g.size());
        assert!(mmap_check(g.size(), 0, true, false, &g).is_ok());
    }
}
