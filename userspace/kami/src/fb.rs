//! /dev/fb0: the two screen-info ioctls and a shared mapping of the pixels.
//! This is the Linux fbdev ABI, which Akuma's `/dev/fb0` also implements
//! (`crates/akuma-fbdev`), so the same binary runs on both.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;

const FBIOGET_VSCREENINFO: libc::c_ulong = 0x4600;
const FBIOGET_FSCREENINFO: libc::c_ulong = 0x4602;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Bitfield {
    offset: u32,
    length: u32,
    msb_right: u32,
}

/// `struct fb_var_screeninfo` from `<linux/fb.h>`.
#[repr(C)]
#[derive(Default)]
struct VarInfo {
    xres: u32,
    yres: u32,
    xres_virtual: u32,
    yres_virtual: u32,
    xoffset: u32,
    yoffset: u32,
    bits_per_pixel: u32,
    grayscale: u32,
    red: Bitfield,
    green: Bitfield,
    blue: Bitfield,
    transp: Bitfield,
    nonstd: u32,
    activate: u32,
    height: u32,
    width: u32,
    accel_flags: u32,
    pixclock: u32,
    left_margin: u32,
    right_margin: u32,
    upper_margin: u32,
    lower_margin: u32,
    hsync_len: u32,
    vsync_len: u32,
    sync: u32,
    vmode: u32,
    rotate: u32,
    colorspace: u32,
    reserved: [u32; 4],
}

/// `struct fb_fix_screeninfo` from `<linux/fb.h>`.
#[repr(C)]
#[derive(Default)]
struct FixInfo {
    id: [u8; 16],
    smem_start: libc::c_ulong,
    smem_len: u32,
    kind: u32,
    type_aux: u32,
    visual: u32,
    xpanstep: u16,
    ypanstep: u16,
    ywrapstep: u16,
    line_length: u32,
    mmio_start: libc::c_ulong,
    mmio_len: u32,
    accel: u32,
    capabilities: u16,
    reserved: [u16; 2],
}

pub struct Fb {
    _file: File,
    base: *mut u8,
    map_len: usize,
    /// Byte offset of the visible origin (`yoffset`/`xoffset` panning).
    origin: usize,
    stride: usize,
    pub width: usize,
    pub height: usize,
    shifts: [u32; 3],
    alpha: u32,
    row: Vec<u32>,
}

impl Fb {
    pub fn open(path: &str) -> io::Result<Fb> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let fd = file.as_raw_fd();
        let mut var = VarInfo::default();
        let mut fix = FixInfo::default();
        // SAFETY: both requests write exactly one struct of the type passed,
        // whose layout is the kernel's (`repr(C)`, same field order).
        unsafe {
            if libc::ioctl(fd, FBIOGET_VSCREENINFO as _, &mut var as *mut VarInfo) < 0
                || libc::ioctl(fd, FBIOGET_FSCREENINFO as _, &mut fix as *mut FixInfo) < 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        if var.bits_per_pixel != 32 {
            return Err(io::Error::other(format!("{} bpp unsupported", var.bits_per_pixel)));
        }
        let stride = fix.line_length as usize;
        let map_len = if fix.smem_len != 0 {
            fix.smem_len as usize
        } else {
            stride * var.yres_virtual.max(var.yres) as usize
        };
        let origin = var.yoffset as usize * stride + var.xoffset as usize * 4;
        if origin + stride * var.yres as usize > map_len {
            return Err(io::Error::other("visible area exceeds smem_len"));
        }
        // SAFETY: a fresh shared mapping of the device; checked for failure.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                map_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let alpha = if var.transp.length > 0 { 0xff << var.transp.offset } else { 0 };
        let id_len = fix.id.iter().position(|&b| b == 0).unwrap_or(16);
        eprintln!(
            "[kami] fb {} {}x{} stride {} rgb@{}/{}/{} map {} bytes",
            String::from_utf8_lossy(&fix.id[..id_len]),
            var.xres, var.yres, stride,
            var.red.offset, var.green.offset, var.blue.offset, map_len
        );
        Ok(Fb {
            _file: file,
            base: base as *mut u8,
            map_len,
            origin,
            stride,
            width: var.xres as usize,
            height: var.yres as usize,
            shifts: [var.red.offset, var.green.offset, var.blue.offset],
            alpha,
            row: Vec::new(),
        })
    }

    /// Nearest-neighbour `scale`x blit of an RGB/RGBA image at the top left,
    /// clipped to the screen.
    pub fn blit(&mut self, px: &[u8], w: usize, h: usize, ch: usize, scale: usize) {
        let out_w = (w * scale).min(self.width);
        let [rs, gs, bs] = self.shifts;
        self.row.resize(out_w, 0);
        for y in 0..h {
            let first = y * scale;
            if first >= self.height {
                break;
            }
            let src = &px[y * w * ch..(y + 1) * w * ch];
            for (x, dst) in self.row.iter_mut().enumerate() {
                let p = &src[(x / scale) * ch..];
                *dst = self.alpha
                    | (p[0] as u32) << rs
                    | (p[1] as u32) << gs
                    | (p[2] as u32) << bs;
            }
            for dy in first..(first + scale).min(self.height) {
                let off = self.origin + dy * self.stride;
                debug_assert!(off + out_w * 4 <= self.map_len);
                // SAFETY: `off + out_w * 4` is inside the mapping: `origin +
                // height * stride <= map_len` was checked at open, `dy < height`,
                // and `out_w <= width <= stride / 4`.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        self.row.as_ptr() as *const u8,
                        self.base.add(off),
                        out_w * 4,
                    );
                }
            }
        }
    }
}

impl Drop for Fb {
    fn drop(&mut self) {
        // SAFETY: unmapping exactly the region mapped in `open`.
        unsafe { libc::munmap(self.base as *mut _, self.map_len) };
    }
}
