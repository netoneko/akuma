//! The amd64 FPU save area, as bytes and rules.
//!
//! Two kernels save x87/SSE(/AVX) state around a context switch: the full amd64 kernel
//! (`amd64/src/sched.rs`, `xsave64` when the CPU has AVX, else `fxsave64`) and taoxichuan
//! (`taoxichuan/src/exec_twin.rs`, `fxsave64` only). They each had their own copy of the
//! same facts - the area size, the offsets, the power-on image - and one of those facts
//! (`MXCSR = 0x1F80`, not zero) is the difference between a program that runs and a
//! `#XM` halt on its first inexact result. This crate is the one copy.
//!
//! It is **pure**: no `asm!`, no statics, no `unsafe` (`forbid`). The instructions
//! (`fxsave64`, `fxrstor64`, `xsave64`, `xrstor64`, `xsetbv`) and the per-slot storage
//! stay in the kernels; what moves here is what the instructions are *given* and what is
//! *checked* before one reads an image a program supplied.
//!
//! # The two formats
//! * [`SaveFormat::Fxsave`]: 512 bytes, legacy layout (x87, MMX, XMM0-15, `MXCSR`).
//! * [`SaveFormat::Xsave`]: 832 bytes = the 512 legacy bytes + a 64-byte header at 512 +
//!   256 bytes of `ymm` upper halves at 576, i.e. `XCR0 = 0b111` ([`XCR0_ENABLED`]).
//!   The first 512 bytes are byte-for-byte the `fxsave` image, which is why one
//!   power-on image serves both and [`initial_xsave`] differs from [`initial_legacy`] only
//!   in `XSTATE_BV`.
//!
//! Both need 16 bytes of alignment (`fxsave`) / 64 (`xsave`); the kernels' wrapper types
//! carry `repr(align(64))` ([`ALIGN`]) and the crate cannot enforce it for them.

#![no_std]
#![forbid(unsafe_code)]

use core::ops::Range;

/// Bytes of the legacy (`fxsave`) area.
pub const LEGACY_SIZE: usize = 512;
/// Bytes of the `xsave` standard-format area for `XCR0 =` [`XCR0_ENABLED`]:
/// 512 legacy + 64 header + 256 `ymm` upper halves.
pub const XSAVE_SIZE: usize = 832;
/// The alignment `xsave64` requires of its operand.
pub const ALIGN: usize = 64;

/// `XCR0`: x87 (bit 0), SSE (1), AVX (2). AVX-512 state is deliberately left off, which
/// is what makes [`XSAVE_SIZE`] 832 and not larger.
pub const XCR0_ENABLED: u64 = 0b111;

/// x87 control word at power-on: all exceptions masked, 64-bit precision, round to nearest.
pub const FCW_INIT: u16 = 0x037F;
/// `MXCSR` at power-on: every SSE exception masked. **Not zero**: zero unmasks them all
/// and the first inexact result in ring 3 raises `#XM`.
pub const MXCSR_INIT: u32 = 0x1F80;

/// Offset of the x87 control word.
pub const FCW: usize = 0;
/// Offset of `MXCSR` in the legacy area.
pub const MXCSR: usize = 24;
/// Offset of `MXCSR_MASK` in the legacy area (written by `fxsave`; `0` means the pre-SSE2
/// default applies).
pub const MXCSR_MASK: usize = 28;
/// Offset of `XSTATE_BV` (which components the image holds).
pub const XSTATE_BV: usize = 512;
/// Offset of `XCOMP_BV` (must be 0 for the standard, non-compacted form).
pub const XCOMP_BV: usize = 520;
/// The header's reserved tail, which `xrstor` requires to be zero.
pub const HEADER_RESERVED: Range<usize> = 528..576;
/// Default `MXCSR_MASK` when the CPU reports 0 (Intel SDM, `FXRSTOR`).
pub const MXCSR_MASK_DEFAULT: u32 = 0xFFBF;
/// `XSTATE_BV` of the power-on image: x87 and SSE are live (so `xrstor` loads the
/// values above), AVX is in its init state (all zero).
pub const XSTATE_BV_INIT: u64 = 0b11;

/// Which instruction pair a kernel saves with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SaveFormat {
    /// `fxsave64` / `fxrstor64`: [`LEGACY_SIZE`] bytes.
    Fxsave,
    /// `xsave64` / `xrstor64` with `XCR0 =` [`XCR0_ENABLED`]: [`XSAVE_SIZE`] bytes.
    Xsave,
}

impl SaveFormat {
    /// The format a kernel uses: `xsave` when it turned `CR4.OSXSAVE` on, else `fxsave`.
    #[must_use]
    pub const fn select(xsave_on: bool) -> Self {
        if xsave_on { Self::Xsave } else { Self::Fxsave }
    }

    /// Bytes the instruction writes.
    #[must_use]
    pub const fn size(self) -> usize {
        match self {
            Self::Fxsave => LEGACY_SIZE,
            Self::Xsave => XSAVE_SIZE,
        }
    }
}

/// The power-on legacy image: `FCW = 0x037F`, `MXCSR = 0x1F80`, everything else zero.
#[must_use]
pub const fn initial_legacy() -> [u8; LEGACY_SIZE] {
    let mut a = [0u8; LEGACY_SIZE];
    let f = FCW_INIT.to_le_bytes();
    a[FCW] = f[0];
    a[FCW + 1] = f[1];
    let m = MXCSR_INIT.to_le_bytes();
    a[MXCSR] = m[0];
    a[MXCSR + 1] = m[1];
    a[MXCSR + 2] = m[2];
    a[MXCSR + 3] = m[3];
    a
}

/// The power-on `xsave` image: [`initial_legacy`] in the first 512 bytes and
/// `XSTATE_BV =` [`XSTATE_BV_INIT`].
#[must_use]
pub const fn initial_xsave() -> [u8; XSAVE_SIZE] {
    let legacy = initial_legacy();
    let mut a = [0u8; XSAVE_SIZE];
    let mut i = 0;
    while i < LEGACY_SIZE {
        a[i] = legacy[i];
        i += 1;
    }
    a[XSTATE_BV] = XSTATE_BV_INIT as u8;
    a
}

fn get32(image: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([image[at], image[at + 1], image[at + 2], image[at + 3]])
}

fn get64(image: &[u8], at: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&image[at..at + 8]);
    u64::from_le_bytes(b)
}

/// The effective `MXCSR_MASK`: the CPU's own value from an `fxsave` image, or
/// [`MXCSR_MASK_DEFAULT`] when it reports 0 (that is how the SDM says to discover it).
#[must_use]
pub const fn effective_mxcsr_mask(reported: u32) -> u32 {
    if reported == 0 { MXCSR_MASK_DEFAULT } else { reported }
}

/// Make an image a ring-3 program handed back (`rt_sigreturn`) safe to `xrstor`/`fxrstor`.
///
/// Both instructions take a `#GP` - in ring 0, on the path restoring the frame - for exactly
/// four things: reserved `MXCSR` bits set, `XSTATE_BV` naming a component `XCR0` does not
/// enable, a non-zero `XCOMP_BV` (the compacted form), and a non-zero reserved header.
/// Everything else is the program's own business. `mask` is [`effective_mxcsr_mask`] of the
/// CPU's value; `format` is what the kernel restores with - an `Fxsave` kernel has no header
/// to check, so only the `MXCSR` fix applies.
///
/// # Panics
/// If `image` is shorter than `format.size()`.
pub fn sanitize(image: &mut [u8], mask: u32, format: SaveFormat) {
    assert!(image.len() >= format.size(), "FPU image shorter than its format");
    let mxcsr = get32(image, MXCSR) & mask;
    image[MXCSR..MXCSR + 4].copy_from_slice(&mxcsr.to_le_bytes());
    image[MXCSR_MASK..MXCSR_MASK + 4].copy_from_slice(&mask.to_le_bytes());
    if format == SaveFormat::Xsave {
        let bv = get64(image, XSTATE_BV) & XCR0_ENABLED;
        image[XSTATE_BV..XSTATE_BV + 8].copy_from_slice(&bv.to_le_bytes());
        image[XCOMP_BV..XCOMP_BV + 8].fill(0);
        image[HEADER_RESERVED].fill(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_and_layout_are_the_architectural_ones() {
        assert_eq!(SaveFormat::Fxsave.size(), 512);
        assert_eq!(SaveFormat::Xsave.size(), 832);
        // 512 legacy + 64 header + 256 ymm_hi (16 x 16 bytes).
        assert_eq!(LEGACY_SIZE + 64 + 16 * 16, XSAVE_SIZE);
        assert_eq!(HEADER_RESERVED, 528..576);
        assert_eq!(XSTATE_BV + 8, XCOMP_BV);
        assert_eq!(XCOMP_BV + 8, HEADER_RESERVED.start);
        assert_eq!(ALIGN, 64);
        assert_eq!(XCR0_ENABLED, 0b111);
    }

    #[test]
    fn select_follows_the_osxsave_flag() {
        assert_eq!(SaveFormat::select(false), SaveFormat::Fxsave);
        assert_eq!(SaveFormat::select(true), SaveFormat::Xsave);
    }

    /// The bytes both kernels hard-coded before this crate: `a[0]=0x7F a[1]=0x03 a[24]=0x80 a[25]=0x1F`.
    #[test]
    fn power_on_image_is_the_bytes_the_kernels_wrote_by_hand() {
        let mut want = [0u8; 512];
        want[0] = 0x7F;
        want[1] = 0x03;
        want[24] = 0x80;
        want[25] = 0x1F;
        assert_eq!(initial_legacy(), want);
        // And the thing it protects against: MXCSR zero would unmask every SSE exception.
        assert_ne!(get32(&initial_legacy(), MXCSR), 0);
        assert_eq!(get32(&initial_legacy(), MXCSR), 0x1F80);
    }

    /// The pin for the fxsave-vs-xsave divergence: the two kernels' images agree on the
    /// legacy half and differ only in `XSTATE_BV`.
    #[test]
    fn xsave_image_is_the_legacy_image_plus_xstate_bv() {
        let x = initial_xsave();
        assert_eq!(x[..512], initial_legacy());
        assert_eq!(get64(&x, XSTATE_BV), 0b11);
        assert!(x[513..].iter().all(|&b| b == 0), "nothing else is set");
    }

    #[test]
    fn mxcsr_mask_zero_means_the_default() {
        assert_eq!(effective_mxcsr_mask(0), 0xFFBF);
        assert_eq!(effective_mxcsr_mask(0xFFFF), 0xFFFF);
    }

    /// Dirty every field `xrstor` faults on; `sanitize` must clear exactly those.
    fn hostile() -> [u8; XSAVE_SIZE] {
        let mut a = [0xAAu8; XSAVE_SIZE];
        a[MXCSR..MXCSR + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        a[XSTATE_BV..XSTATE_BV + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        a[XCOMP_BV..XCOMP_BV + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        a
    }

    #[test]
    fn xsave_sanitize_clears_the_four_faulting_things() {
        let mut a = hostile();
        sanitize(&mut a, 0xFFBF, SaveFormat::Xsave);
        assert_eq!(get32(&a, MXCSR), 0xFFBF, "reserved MXCSR bits cleared");
        assert_eq!(get32(&a, MXCSR_MASK), 0xFFBF, "the mask is recorded");
        assert_eq!(get64(&a, XSTATE_BV), 0b111, "components XCR0 does not enable cleared");
        assert_eq!(get64(&a, XCOMP_BV), 0, "compacted form refused");
        assert!(a[HEADER_RESERVED].iter().all(|&b| b == 0), "reserved header zero");
    }

    #[test]
    fn sanitize_leaves_the_programs_own_state_alone() {
        let mut a = hostile();
        let before = a;
        sanitize(&mut a, 0xFFBF, SaveFormat::Xsave);
        // x87 control word, XMM0, the ymm halves: the program's business.
        assert_eq!(a[0..24], before[0..24]);
        assert_eq!(a[32..512], before[32..512]);
        assert_eq!(a[576..], before[576..]);
    }

    #[test]
    fn fxsave_sanitize_touches_only_mxcsr() {
        let mut a = hostile();
        let before = a;
        sanitize(&mut a, 0xFFBF, SaveFormat::Fxsave);
        assert_eq!(get32(&a, MXCSR), 0xFFBF);
        assert_eq!(a[512..], before[512..], "no header on an fxsave kernel: left as is");
    }

    #[test]
    fn a_valid_power_on_image_is_a_fixed_point() {
        let mut a = initial_xsave();
        let before = a;
        sanitize(&mut a, 0xFFBF, SaveFormat::Xsave);
        // Only MXCSR_MASK is (re)written; every other byte is untouched.
        let mut want = before;
        want[MXCSR_MASK..MXCSR_MASK + 4].copy_from_slice(&0xFFBFu32.to_le_bytes());
        assert_eq!(a, want);
    }

    #[test]
    #[should_panic(expected = "shorter")]
    fn a_short_buffer_is_refused() {
        sanitize(&mut [0u8; 512], 0xFFBF, SaveFormat::Xsave);
    }
}
