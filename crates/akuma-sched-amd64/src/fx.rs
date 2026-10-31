//! The FPU/SSE/AVX state of one thread while it is not running, and the instructions
//! that move it. The byte algebra (sizes, offsets, the power-on image, the sanitising
//! rule) is `akuma-fpu-amd64`; this file owns the area, the asm and the two statics a
//! kernel sets at boot.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// Whether `xsave`/`xrstor` (AVX state) are on, set once at boot by the kernel that
/// called `enable_xsave`. Off: the legacy `fxsave`/`fxrstor` pair, 512 bytes.
pub static XSAVE_ON: AtomicBool = AtomicBool::new(false);

/// The `XCR0` feature set `xsave`/`xrstor` are asked for.
pub const XCR0_ENABLED: u64 = akuma_fpu_amd64::XCR0_ENABLED;

/// One thread's saved FP state: 64-byte aligned (`xsave` requires it), sized for the
/// widest format; `fxsave` uses the first 512 bytes.
#[repr(C, align(64))]
#[derive(Clone, Copy)]
pub struct FxArea([u8; akuma_fpu_amd64::XSAVE_SIZE]);

impl FxArea {
    pub const SIZE: usize = akuma_fpu_amd64::XSAVE_SIZE;
    const MXCSR_MASK: usize = akuma_fpu_amd64::MXCSR_MASK;
    const MXCSR_MASK_DEFAULT: u32 = akuma_fpu_amd64::MXCSR_MASK_DEFAULT;

    /// All zero: not a valid `xrstor` input until initialised.
    #[must_use]
    pub const fn zeroed() -> Self {
        Self([0; akuma_fpu_amd64::XSAVE_SIZE])
    }

    /// The power-on state (`FCW = 0x037F`, `MXCSR = 0x1F80`, every SSE exception masked).
    #[must_use]
    pub const fn initial() -> Self {
        Self(akuma_fpu_amd64::initial_xsave())
    }

    /// The state a guest or a fresh program starts with (an alias of [`FxArea::initial`]).
    #[must_use]
    pub const fn reset_state() -> Self {
        Self::initial()
    }

    /// The legacy 512-byte area, for a signal frame or `ptrace`.
    pub fn legacy_mut(&mut self) -> &mut [u8] {
        &mut self.0[..512]
    }

    /// Raw bytes, for the one place that copies a whole area (a signal frame).
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.0
    }

    /// Make a user-supplied image safe to `xrstor`: reserved bits cleared, `MXCSR`
    /// masked to what this CPU implements.
    pub fn sanitize(&mut self) {
        akuma_fpu_amd64::sanitize(
            &mut self.0,
            mxcsr_mask(),
            akuma_fpu_amd64::SaveFormat::select(XSAVE_ON.load(Ordering::Relaxed)),
        );
    }
}

static MXCSR_MASK: AtomicU32 = AtomicU32::new(0);

/// This CPU's `MXCSR_MASK` (the bits it implements), probed once with an `fxsave`; a
/// reported zero means "the default" per the SDM.
#[must_use]
pub fn mxcsr_mask() -> u32 {
    let cached = MXCSR_MASK.load(Ordering::Relaxed);
    if cached != 0 {
        return cached;
    }
    let mut probe = FxArea::zeroed();
    // SAFETY: `fxsave64` writes 512 bytes through a pointer to a 64-byte-aligned area at
    // least that large, and touches no flags.
    unsafe {
        core::arch::asm!("fxsave64 [{out}]", out = in(reg) probe.0.as_mut_ptr(), options(nostack, preserves_flags));
    }
    let raw = u32::from_le_bytes(
        probe.0[FxArea::MXCSR_MASK..FxArea::MXCSR_MASK + 4].try_into().unwrap_or([0; 4]),
    );
    let mask = if raw == 0 { FxArea::MXCSR_MASK_DEFAULT } else { raw };
    MXCSR_MASK.store(mask, Ordering::Relaxed);
    mask
}

/// Save the live FP state into `out`.
pub fn fpu_save_live(out: &mut FxArea) {
    // SAFETY: `out` is a 64-byte-aligned area sized for the widest format.
    unsafe {
        if XSAVE_ON.load(Ordering::Relaxed) {
            core::arch::asm!(
                "xsave64 [{out}]",
                out = in(reg) out.0.as_mut_ptr(),
                in("eax") XCR0_ENABLED as u32,
                in("edx") 0u32,
                options(nostack, preserves_flags)
            );
        } else {
            core::arch::asm!("fxsave64 [{out}]", out = in(reg) out.0.as_mut_ptr(), options(nostack, preserves_flags));
        }
    }
}

/// Load `inp` into the live FP state. `inp` must be a valid image (`initial`, a prior
/// save, or [`FxArea::sanitize`]d).
pub fn fpu_restore_live(inp: &FxArea) {
    // SAFETY: as above; the caller promises a valid image.
    unsafe {
        if XSAVE_ON.load(Ordering::Relaxed) {
            core::arch::asm!(
                "xrstor64 [{inp}]",
                inp = in(reg) inp.0.as_ptr(),
                in("eax") XCR0_ENABLED as u32,
                in("edx") 0u32,
                options(nostack, preserves_flags)
            );
        } else {
            core::arch::asm!("fxrstor64 [{inp}]", inp = in(reg) inp.0.as_ptr(), options(nostack, preserves_flags));
        }
    }
}

/// Put the live FP state back to the power-on image.
pub fn fpu_reset_live() {
    static INITIAL: FxArea = FxArea::initial();
    fpu_restore_live(&INITIAL);
}
