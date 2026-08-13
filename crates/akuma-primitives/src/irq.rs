//! Local-IRQ masking: one DAIF implementation, and the RAII guard over it.
//!
//! Before this module the tree had **three** implementations of "save DAIF, mask
//! IRQs, restore DAIF":
//!
//! | copy | `isb` after the mask? |
//! |---|---|
//! | `src/irq.rs:12` `IrqGuard` | yes |
//! | `akuma-exec/src/runtime.rs:280` `IrqGuard` (same name, second crate) | yes |
//! | `akuma-exec/src/sync.rs:17` `irq_save_mask`/`irq_restore` | **no** |
//!
//! Two guards written twice, plus a barrier-less twin of the same operation.
//! `TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md` §5.5 counted the two guards and
//! missed the third.
//!
//! # The `isb` divergence is preserved, not resolved
//!
//! The layering here is deliberate: [`irq_save_mask`]/[`irq_restore`] are the
//! bare DAIF accesses with **no** barrier, and [`IrqGuard`] is
//! `irq_save_mask()` **plus an `isb`**. That reproduces all three call sites'
//! existing codegen exactly — the guards keep their barrier, the hot path keeps
//! its absence of one — so this merge is a pure deduplication with no behaviour
//! change to measure.
//!
//! Resolving the divergence is a separate question with a real cost on each
//! side, and it should not ride along on a cleanup:
//!
//! - **Dropping the `isb` from the guards** is very likely correct (AArch64
//!   masks interrupts synchronously on a direct PSTATE write, and Linux's
//!   arm64 `local_irq_disable()` is a bare `msr daifset` with no barrier), and
//!   `irq_save_mask` has run without one under real SMP on the contended
//!   `KernelLock::acquire` path for a long time. But "very likely" is not a
//!   measurement, and the failure mode would be a rare lost-window bug in the
//!   exception path.
//! - **Adding the `isb` to `irq_save_mask`** is the conservative direction, and
//!   it puts a pipeline flush on the BKL acquire path and inside every
//!   `PreemptGuard` — hot enough that Phase 3 of that document deleted a
//!   *spinlocked struct read* from a comparable path for cost.
//!
//! So: one implementation, two documented entry points, and the choice left to
//! whoever measures it.

/// Mask local IRQs (set `DAIF.I`) and return the prior `DAIF` for
/// [`irq_restore`].
///
/// No `isb` — see the module header. Used by `KernelLock::acquire` to make its
/// FIFO ticket wait atomic against local exception nesting, and by
/// `PreemptGuard` to make inner-spinlock critical sections nest-free under a
/// dropped BKL.
///
/// Bare-metal AArch64 only; a no-op returning `0` on host builds
/// (single-threaded tests have no local IRQs).
#[cfg(target_os = "none")]
#[inline(always)]
#[must_use]
pub fn irq_save_mask() -> u64 {
    let daif: u64;
    // SAFETY: reading DAIF and setting the IRQ mask bit have no memory effects.
    unsafe {
        core::arch::asm!("mrs {}, daif", out(reg) daif, options(nomem, nostack));
        core::arch::asm!("msr daifset, #0x2", options(nomem, nostack));
    }
    daif
}

/// Restore `DAIF` saved by [`irq_save_mask`]. Bare-metal AArch64 only; no-op on
/// host.
#[cfg(target_os = "none")]
#[inline(always)]
pub fn irq_restore(daif: u64) {
    // SAFETY: restoring the previously-saved DAIF; no memory effects.
    unsafe { core::arch::asm!("msr daif, {}", in(reg) daif, options(nomem, nostack)) };
}

#[cfg(not(target_os = "none"))]
#[inline(always)]
#[must_use]
pub fn irq_save_mask() -> u64 {
    0
}

#[cfg(not(target_os = "none"))]
#[inline(always)]
pub fn irq_restore(_daif: u64) {}

/// Unmask local IRQs (clear `DAIF.I`) unconditionally, with **no** barrier and
/// no saved state.
///
/// Distinct from [`irq_restore`]: this asserts "IRQs on" rather than putting back
/// whatever was there, so it is only correct where the caller knows IRQs should
/// end up enabled (thread entry, the idle loop, handing control to EL0).
///
/// Six sites open-coded this `msr daifclr, #2`, and the bin crate's
/// `irq::enable_irqs()` — which does exactly this — had **zero callers**. Two of
/// the six are inside `akuma-exec`, which cannot reach the bin crate: the same
/// missing-crate shape as the five stack writers (see `crate::console`).
///
/// Sites that follow the unmask with an `isb` want [`unmask_irqs_sync`] instead —
/// a distinct operation, kept under a distinct name rather than folded in here.
#[cfg(target_os = "none")]
#[inline(always)]
pub fn unmask_irqs() {
    // SAFETY: clearing the DAIF IRQ mask bit has no memory effects.
    unsafe { core::arch::asm!("msr daifclr, #2", options(nomem, nostack)) };
}

#[cfg(not(target_os = "none"))]
#[inline(always)]
pub fn unmask_irqs() {}

/// Unmask local IRQs and then **synchronize the context** (`isb`), so a pending
/// IRQ is taken before the next instruction rather than at some later point.
///
/// Separate from [`unmask_irqs`] on purpose: the barrier is the difference
/// between "IRQs are on from here" and "IRQs are on and any pending one has
/// already been taken", and the three call sites that want it want it for that
/// reason — a secondary core enabling interrupts for the first time
/// (`src/smp_shared.rs`), and the two exit-to-EL0 paths in
/// `akuma-exec/src/process/mod.rs`.
///
/// `#[inline(always)]`, so this emits exactly the two instructions each site
/// used to open-code — the merge removes the duplicated `asm!` without changing
/// a byte of codegen.
#[cfg(target_os = "none")]
#[inline(always)]
pub fn unmask_irqs_sync() {
    // SAFETY: clearing the DAIF IRQ mask bit and synchronizing the context have
    // no memory effects.
    unsafe { core::arch::asm!("msr daifclr, #2", "isb", options(nomem, nostack)) };
}

#[cfg(not(target_os = "none"))]
#[inline(always)]
pub fn unmask_irqs_sync() {}

/// Mask local IRQs and synchronize the context (`isb`), with no saved state.
///
/// The mask-side counterpart to [`unmask_irqs_sync`]. Prefer [`IrqGuard`] — this
/// leaves the caller responsible for a matching [`unmask_irqs`], and "IRQs were
/// already masked when I was called" is not recoverable from it.
#[cfg(target_os = "none")]
#[inline(always)]
pub fn mask_irqs_sync() {
    // SAFETY: setting the DAIF IRQ mask bit and synchronizing the context have
    // no memory effects.
    unsafe { core::arch::asm!("msr daifset, #2", "isb", options(nomem, nostack)) };
}

#[cfg(not(target_os = "none"))]
#[inline(always)]
pub fn mask_irqs_sync() {}

/// Read `DAIF`. `DAIF.I` is bit 7 — set means local IRQs are masked.
///
/// For code that must *observe* the mask rather than change it: `yield_now`
/// checks it because an SGI raised with IRQs masked is never delivered to this
/// core, which turns the yield into a silent no-op and spins the caller.
#[cfg(target_os = "none")]
#[inline(always)]
#[must_use]
pub fn read_daif() -> u64 {
    let daif: u64;
    // SAFETY: reading DAIF has no memory effects.
    unsafe { core::arch::asm!("mrs {}, daif", out(reg) daif, options(nomem, nostack)) };
    daif
}

#[cfg(not(target_os = "none"))]
#[inline(always)]
#[must_use]
pub fn read_daif() -> u64 {
    0
}

/// `DAIF.I` — local IRQs masked. Bit 7.
pub const DAIF_I_MASKED: u64 = 0x80;

/// RAII guard that masks local IRQs on creation and restores `DAIF` on drop.
///
/// Adds an `isb` after the mask, which [`irq_save_mask`] alone does not — see
/// the module header for why that difference is kept rather than resolved.
///
/// On non-`target_os = "none"` builds (host testing) this carries no state and
/// does nothing.
pub struct IrqGuard {
    #[cfg(target_os = "none")]
    saved_daif: u64,
}

impl IrqGuard {
    /// Mask local IRQs until the returned guard drops.
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        #[cfg(target_os = "none")]
        {
            let saved_daif = irq_save_mask();
            // SAFETY: a context synchronization barrier has no memory effects.
            unsafe { core::arch::asm!("isb", options(nomem, nostack)) };
            Self { saved_daif }
        }
        #[cfg(not(target_os = "none"))]
        Self {}
    }
}

impl Default for IrqGuard {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for IrqGuard {
    #[inline]
    fn drop(&mut self) {
        #[cfg(target_os = "none")]
        irq_restore(self.saved_daif);
    }
}

/// Run a closure with local IRQs masked, restoring `DAIF` afterwards.
#[inline]
pub fn with_irqs_disabled<T, F: FnOnce() -> T>(f: F) -> T {
    let _guard = IrqGuard::new();
    f()
}

#[cfg(test)]
mod tests {
    use super::{IrqGuard, irq_restore, irq_save_mask, with_irqs_disabled};

    #[test]
    fn host_build_is_a_no_op_and_round_trips() {
        // On host there are no local IRQs to mask; the contract is only that
        // nothing panics and the guard is zero-state.
        assert_eq!(irq_save_mask(), 0);
        irq_restore(0);
        assert_eq!(core::mem::size_of::<IrqGuard>(), 0);
    }

    #[test]
    fn guards_nest() {
        let outer = IrqGuard::new();
        {
            let inner = IrqGuard::new();
            drop(inner);
        }
        drop(outer);
    }

    #[test]
    fn with_irqs_disabled_returns_the_closure_value() {
        assert_eq!(with_irqs_disabled(|| 7u32 + 1), 8);
    }

    #[test]
    fn unmask_and_read_are_host_no_ops() {
        super::unmask_irqs();
        assert_eq!(super::read_daif(), 0);
        // Host reads 0, so the masked bit reads clear — matching "IRQs are not
        // masked", which is the truth on a host thread.
        assert_eq!(super::read_daif() & super::DAIF_I_MASKED, 0);
    }
}
