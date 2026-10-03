//! The Linux framebuffer device ABI (`<linux/fb.h>`): the two structs the
//! `FBIOGET_*` ioctls copy out, their ioctl numbers, and the constants a packed
//! true-colour framebuffer reports.
//!
//! Field order, widths and padding are `<linux/fb.h>`'s, on a 64-bit target
//! (`unsigned long` is 8 bytes). The structs are serialised by hand into byte
//! arrays rather than reinterpreted, because this crate forbids `unsafe` and
//! because the bytes are what crosses the boundary — the offsets are pinned by
//! the assertions below and by the host tests.

/// `FBIOGET_VSCREENINFO`: read the variable (mode) information.
pub const FBIOGET_VSCREENINFO: u32 = 0x4600;
/// `FBIOPUT_VSCREENINFO`: set it.
pub const FBIOPUT_VSCREENINFO: u32 = 0x4601;
/// `FBIOGET_FSCREENINFO`: read the fixed information.
pub const FBIOGET_FSCREENINFO: u32 = 0x4602;
/// `FBIOPAN_DISPLAY`: move the visible window inside the virtual one.
pub const FBIOPAN_DISPLAY: u32 = 0x4606;
/// `FBIOBLANK`: blank or unblank the display.
pub const FBIOBLANK: u32 = 0x4611;

/// `FB_TYPE_PACKED_PIXELS`.
pub const FB_TYPE_PACKED_PIXELS: u32 = 0;
/// `FB_VISUAL_TRUECOLOR`.
pub const FB_VISUAL_TRUECOLOR: u32 = 2;
/// `FB_VMODE_NONINTERLACED`.
pub const FB_VMODE_NONINTERLACED: u32 = 0;
/// `FB_ACTIVATE_NOW`.
pub const FB_ACTIVATE_NOW: u32 = 0;
/// `FB_ACCEL_NONE`.
pub const FB_ACCEL_NONE: u32 = 0;

/// `struct fb_bitfield`: where one colour channel sits in a pixel.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct FbBitfield {
    pub offset: u32,
    pub length: u32,
    pub msb_right: u32,
}

/// `struct fb_var_screeninfo` — 160 bytes.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct FbVarScreeninfo {
    pub xres: u32,
    pub yres: u32,
    pub xres_virtual: u32,
    pub yres_virtual: u32,
    pub xoffset: u32,
    pub yoffset: u32,
    pub bits_per_pixel: u32,
    pub grayscale: u32,
    pub red: FbBitfield,
    pub green: FbBitfield,
    pub blue: FbBitfield,
    pub transp: FbBitfield,
    pub nonstd: u32,
    pub activate: u32,
    pub height: u32,
    pub width: u32,
    pub accel_flags: u32,
    pub pixclock: u32,
    pub left_margin: u32,
    pub right_margin: u32,
    pub upper_margin: u32,
    pub lower_margin: u32,
    pub hsync_len: u32,
    pub vsync_len: u32,
    pub sync: u32,
    pub vmode: u32,
    pub rotate: u32,
    pub colorspace: u32,
    pub reserved: [u32; 4],
}

/// `struct fb_fix_screeninfo` — 80 bytes, with two implicit holes in C (after
/// `ywrapstep`, and after `line_length` to align `mmio_start`), named here as
/// `_pad0`/`_pad1` so nothing is uninitialised.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct FbFixScreeninfo {
    pub id: [u8; 16],
    pub smem_start: u64,
    pub smem_len: u32,
    pub type_: u32,
    pub type_aux: u32,
    pub visual: u32,
    pub xpanstep: u16,
    pub ypanstep: u16,
    pub ywrapstep: u16,
    pub _pad0: u16,
    pub line_length: u32,
    pub _pad1: u32,
    pub mmio_start: u64,
    pub mmio_len: u32,
    pub accel: u32,
    pub capabilities: u16,
    pub reserved: [u16; 2],
    pub _pad2: u16,
}

pub const FB_VAR_SCREENINFO_SIZE: usize = 160;
pub const FB_FIX_SCREENINFO_SIZE: usize = 80;

const _: () = assert!(core::mem::size_of::<FbBitfield>() == 12);
const _: () = assert!(core::mem::size_of::<FbVarScreeninfo>() == FB_VAR_SCREENINFO_SIZE);
const _: () = assert!(core::mem::size_of::<FbFixScreeninfo>() == FB_FIX_SCREENINFO_SIZE);
const _: () = assert!(core::mem::offset_of!(FbVarScreeninfo, red) == 32);
const _: () = assert!(core::mem::offset_of!(FbVarScreeninfo, nonstd) == 80);
const _: () = assert!(core::mem::offset_of!(FbVarScreeninfo, vmode) == 132);
const _: () = assert!(core::mem::offset_of!(FbVarScreeninfo, reserved) == 144);
const _: () = assert!(core::mem::offset_of!(FbFixScreeninfo, smem_start) == 16);
const _: () = assert!(core::mem::offset_of!(FbFixScreeninfo, smem_len) == 24);
const _: () = assert!(core::mem::offset_of!(FbFixScreeninfo, visual) == 36);
const _: () = assert!(core::mem::offset_of!(FbFixScreeninfo, xpanstep) == 40);
const _: () = assert!(core::mem::offset_of!(FbFixScreeninfo, line_length) == 48);
const _: () = assert!(core::mem::offset_of!(FbFixScreeninfo, mmio_start) == 56);
const _: () = assert!(core::mem::offset_of!(FbFixScreeninfo, mmio_len) == 64);
const _: () = assert!(core::mem::offset_of!(FbFixScreeninfo, capabilities) == 72);

/// A little-endian byte writer over a fixed array.
struct Put<'a> {
    out: &'a mut [u8],
    at: usize,
}

impl Put<'_> {
    fn u16(&mut self, v: u16) {
        self.out[self.at..self.at + 2].copy_from_slice(&v.to_le_bytes());
        self.at += 2;
    }
    fn u32(&mut self, v: u32) {
        self.out[self.at..self.at + 4].copy_from_slice(&v.to_le_bytes());
        self.at += 4;
    }
    fn u64(&mut self, v: u64) {
        self.out[self.at..self.at + 8].copy_from_slice(&v.to_le_bytes());
        self.at += 8;
    }
    fn bitfield(&mut self, b: FbBitfield) {
        self.u32(b.offset);
        self.u32(b.length);
        self.u32(b.msb_right);
    }
}

/// A little-endian byte reader, the inverse of [`Put`].
struct Get<'a> {
    src: &'a [u8],
    at: usize,
}

impl Get<'_> {
    fn u32(&mut self) -> u32 {
        let v = u32::from_le_bytes([self.src[self.at], self.src[self.at + 1], self.src[self.at + 2], self.src[self.at + 3]]);
        self.at += 4;
        v
    }
    fn bitfield(&mut self) -> FbBitfield {
        FbBitfield { offset: self.u32(), length: self.u32(), msb_right: self.u32() }
    }
}

impl FbVarScreeninfo {
    /// The bytes `FBIOGET_VSCREENINFO` copies to the caller (x86_64 and
    /// aarch64 are both little-endian).
    #[must_use]
    pub fn to_bytes(&self) -> [u8; FB_VAR_SCREENINFO_SIZE] {
        let mut out = [0u8; FB_VAR_SCREENINFO_SIZE];
        let mut p = Put { out: &mut out, at: 0 };
        for v in [self.xres, self.yres, self.xres_virtual, self.yres_virtual, self.xoffset,
                  self.yoffset, self.bits_per_pixel, self.grayscale] {
            p.u32(v);
        }
        for b in [self.red, self.green, self.blue, self.transp] {
            p.bitfield(b);
        }
        for v in [self.nonstd, self.activate, self.height, self.width, self.accel_flags,
                  self.pixclock, self.left_margin, self.right_margin, self.upper_margin,
                  self.lower_margin, self.hsync_len, self.vsync_len, self.sync, self.vmode,
                  self.rotate, self.colorspace] {
            p.u32(v);
        }
        for v in self.reserved {
            p.u32(v);
        }
        debug_assert_eq!(p.at, FB_VAR_SCREENINFO_SIZE);
        out
    }

    /// The struct a caller handed `FBIOPUT_VSCREENINFO`.
    #[must_use]
    pub fn from_bytes(b: &[u8; FB_VAR_SCREENINFO_SIZE]) -> Self {
        let mut g = Get { src: b, at: 0 };
        let mut v = Self {
            xres: g.u32(),
            yres: g.u32(),
            xres_virtual: g.u32(),
            yres_virtual: g.u32(),
            xoffset: g.u32(),
            yoffset: g.u32(),
            bits_per_pixel: g.u32(),
            grayscale: g.u32(),
            red: g.bitfield(),
            green: g.bitfield(),
            blue: g.bitfield(),
            transp: g.bitfield(),
            ..Self::default()
        };
        v.nonstd = g.u32();
        v.activate = g.u32();
        v.height = g.u32();
        v.width = g.u32();
        v.accel_flags = g.u32();
        v.pixclock = g.u32();
        v.left_margin = g.u32();
        v.right_margin = g.u32();
        v.upper_margin = g.u32();
        v.lower_margin = g.u32();
        v.hsync_len = g.u32();
        v.vsync_len = g.u32();
        v.sync = g.u32();
        v.vmode = g.u32();
        v.rotate = g.u32();
        v.colorspace = g.u32();
        for r in &mut v.reserved {
            *r = g.u32();
        }
        v
    }
}

impl FbFixScreeninfo {
    /// The bytes `FBIOGET_FSCREENINFO` copies to the caller.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; FB_FIX_SCREENINFO_SIZE] {
        let mut out = [0u8; FB_FIX_SCREENINFO_SIZE];
        out[..16].copy_from_slice(&self.id);
        let mut p = Put { out: &mut out, at: 16 };
        p.u64(self.smem_start);
        p.u32(self.smem_len);
        p.u32(self.type_);
        p.u32(self.type_aux);
        p.u32(self.visual);
        p.u16(self.xpanstep);
        p.u16(self.ypanstep);
        p.u16(self.ywrapstep);
        p.u16(self._pad0);
        p.u32(self.line_length);
        p.u32(self._pad1);
        p.u64(self.mmio_start);
        p.u32(self.mmio_len);
        p.u32(self.accel);
        p.u16(self.capabilities);
        p.u16(self.reserved[0]);
        p.u16(self.reserved[1]);
        p.u16(self._pad2);
        debug_assert_eq!(p.at, FB_FIX_SCREENINFO_SIZE);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn var_bytes_put_each_field_at_its_c_offset() {
        let v = FbVarScreeninfo {
            xres: 3840,
            yres: 2160,
            bits_per_pixel: 32,
            red: FbBitfield { offset: 16, length: 8, msb_right: 0 },
            vmode: 7,
            reserved: [0, 0, 0, 0xdead_beef],
            ..FbVarScreeninfo::default()
        };
        let b = v.to_bytes();
        let at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        assert_eq!(at(0), 3840);
        assert_eq!(at(4), 2160);
        assert_eq!(at(24), 32, "bits_per_pixel");
        assert_eq!(at(32), 16, "red.offset");
        assert_eq!(at(36), 8, "red.length");
        assert_eq!(at(132), 7, "vmode");
        assert_eq!(at(156), 0xdead_beef, "last reserved word");
        assert_eq!(FbVarScreeninfo::from_bytes(&b), v, "round trip");
    }

    #[test]
    fn fix_bytes_put_each_field_at_its_c_offset() {
        let mut id = [0u8; 16];
        id[..8].copy_from_slice(b"akuma-fb");
        let f = FbFixScreeninfo {
            id,
            smem_start: 0x8000_0000,
            smem_len: 3840 * 2160 * 4,
            visual: FB_VISUAL_TRUECOLOR,
            line_length: 16384,
            capabilities: 0x1234,
            ..FbFixScreeninfo::default()
        };
        let b = f.to_bytes();
        let u32_at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        assert_eq!(&b[..8], b"akuma-fb");
        assert_eq!(b[8..16], [0; 8], "id is NUL-padded");
        assert_eq!(u64::from_le_bytes(b[16..24].try_into().unwrap()), 0x8000_0000);
        assert_eq!(u32_at(24), 3840 * 2160 * 4, "smem_len");
        assert_eq!(u32_at(36), FB_VISUAL_TRUECOLOR, "visual");
        assert_eq!(u32_at(48), 16384, "line_length");
        assert_eq!(u16::from_le_bytes([b[72], b[73]]), 0x1234, "capabilities");
    }
}
