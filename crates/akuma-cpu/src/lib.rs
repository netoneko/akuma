//! AArch64 instructions that are **safe to execute**, behind safe functions.
//!
//! # Why this crate exists
//!
//! `core::arch::asm!` is unconditionally `unsafe`, so every barrier, cache
//! maintenance op and system-register read in the tree carried an `unsafe` block
//! — about 160 of the ~230 `asm!` sites, spread across `akuma-mmu` (33),
//! `src/exceptions.rs` (48), `akuma-pmm`, `akuma-timer`, `akuma-exec` and
//! `akuma-primitives`. Every one of those blocks vouches for the same fact: that
//! executing `dsb ish` cannot violate memory safety.
//!
//! It cannot. Neither can `isb`, `wfi`, `ic iallu`, a `dc cvau` on any address,
//! or reading `ESR_EL1`. These are `unsafe` for a *syntactic* reason, not a
//! semantic one, and an `unsafe` block that is always trivially discharged is
//! worse than none: it trains the eye to skip exactly the construct that is
//! supposed to stop it.
//!
//! This is the same argument `akuma_primitives::mmio` makes for device registers
//! — "`unsafe` marking the wrong thing" — applied to the instruction set. The
//! obligation is discharged **once, here**, instead of at every call site.
//!
//! # What is deliberately NOT here
//!
//! Instructions whose effect is a change of control flow or address space, where
//! the caller genuinely does have a proof obligation:
//!
//! - `msr ttbr0_el1` — swaps the address space out from under live pointers.
//! - `msr elr_el1` / `msr spsr_el1` — the resolve-and-retry mechanism; writing
//!   these redirects where the CPU returns to.
//! - `msr vbar_el1` — installs the vector table.
//! - `msr tpidr_el1` / `tpidrro_el0` — re-points every per-thread static.
//! - `mov sp, x` and raw `ldr`/`str`.
//!
//! Those stay `unsafe` at their call sites, where the argument for them is
//! specific. Reading `TTBR0_EL1` is here; writing it is not — the asymmetry is
//! the point, and mirrors the seam `akuma_primitives::preempt` already documents
//! for `TPIDRRO_EL0` ("reading moves, writing does not").
//!
//! # Inlining
//!
//! Every function is `#[inline(always)]`. These wrap a single instruction on the
//! hottest paths in the kernel (the fault handler, the TLB shootdown, the idle
//! loop); a call would cost more than the instruction. Cross-crate that needs the
//! attribute, not a hope — the same lesson `akuma_mmu::as_trace` learned when it
//! stopped folding after moving to another crate.
//!
//! # Host builds
//!
//! Off `target_os = "none"` every function is a no-op (or returns 0), so host
//! unit tests in consuming crates link and run.
//!
//! **The gate is `target_os`, not `target_arch`.** This was written as
//! `cfg(target_arch = "aarch64")` first, which is a trap on this project: the
//! development host *is* `aarch64-apple-darwin`, so the gate was true under
//! `cargo test` and the wrappers really executed — `tlbi`, `dc cvau` and
//! `mrs esr_el1` are EL1 instructions, and the first host test died with
//! `SIGILL`. Every other bare-metal gate in the tree keys on `target_os = "none"`
//! for exactly this reason (`akuma_primitives::preempt::current_tid` among them).

#![cfg_attr(not(test), no_std)]
#![allow(clippy::inline_always, clippy::must_use_candidate)]

/// Barriers.
///
/// Ordering instructions. They constrain when *this* core's accesses become
/// visible; they never dereference anything.
pub mod barrier {
    /// `dsb ish` — full data synchronisation barrier, inner-shareable.
    #[inline(always)]
    pub fn dsb_ish() {
        #[cfg(target_os = "none")]
        // SAFETY: a barrier orders accesses; it touches no memory itself.
        unsafe {
            core::arch::asm!("dsb ish", options(nostack, preserves_flags));
        };
    }

    /// `dsb ishst` — store-only data synchronisation barrier, inner-shareable.
    #[inline(always)]
    pub fn dsb_ishst() {
        #[cfg(target_os = "none")]
        // SAFETY: as `dsb_ish`.
        unsafe {
            core::arch::asm!("dsb ishst", options(nostack, preserves_flags));
        };
    }

    /// `dsb sy` — full system data synchronisation barrier.
    #[inline(always)]
    pub fn dsb_sy() {
        #[cfg(target_os = "none")]
        // SAFETY: as `dsb_ish`.
        unsafe {
            core::arch::asm!("dsb sy", options(nostack, preserves_flags));
        };
    }

    /// `isb` — instruction synchronisation barrier: flush the pipeline so
    /// context-changing operations before it are seen by instructions after it.
    #[inline(always)]
    pub fn isb() {
        #[cfg(target_os = "none")]
        // SAFETY: flushing the pipeline cannot violate memory safety. Note this
        // makes a *prior* unsafe change (a TTBR write) take effect — the
        // obligation belongs to that write, which is not in this crate.
        unsafe {
            core::arch::asm!("isb", options(nostack, preserves_flags));
        };
    }
}

/// Cache maintenance.
///
/// Every one of these takes a virtual address, and none of them dereferences it:
/// a `dc`/`ic` on an unmapped or garbage address faults or is ignored per the
/// architecture, it does not read or write through the pointer. That is what
/// makes an arbitrary `usize` a safe argument.
pub mod cache {
    /// `dc cvau` — clean one data cache line to the point of unification.
    #[inline(always)]
    pub fn dc_cvau(va: usize) {
        #[cfg(target_os = "none")]
        // SAFETY: cache maintenance by VA does not access the line's contents.
        unsafe {
            core::arch::asm!("dc cvau, {}", in(reg) va, options(nostack, preserves_flags));
        };
        #[cfg(not(target_os = "none"))]
        let _ = va;
    }

    /// `dc cvac` — clean one data cache line to the point of coherency.
    #[inline(always)]
    pub fn dc_cvac(va: usize) {
        #[cfg(target_os = "none")]
        // SAFETY: as `dc_cvau`.
        unsafe {
            core::arch::asm!("dc cvac, {}", in(reg) va, options(nostack, preserves_flags));
        };
        #[cfg(not(target_os = "none"))]
        let _ = va;
    }

    /// `ic ivau` — invalidate one instruction cache line by VA.
    #[inline(always)]
    pub fn ic_ivau(va: usize) {
        #[cfg(target_os = "none")]
        // SAFETY: as `dc_cvau`.
        unsafe {
            core::arch::asm!("ic ivau, {}", in(reg) va, options(nostack, preserves_flags));
        };
        #[cfg(not(target_os = "none"))]
        let _ = va;
    }

    /// `ic iallu` — invalidate the entire instruction cache to PoU.
    #[inline(always)]
    pub fn ic_iallu() {
        #[cfg(target_os = "none")]
        // SAFETY: discarding cached instructions is always safe; the next fetch
        // re-reads memory.
        unsafe {
            core::arch::asm!("ic iallu", options(nostack, preserves_flags));
        };
    }

    /// Cache line size in bytes, from `CTR_EL0`.
    #[inline(always)]
    pub fn line_size() -> usize {
        #[cfg(target_os = "none")]
        {
            let ctr: u64;
            // SAFETY: `CTR_EL0` is a read-only ID register.
            unsafe {
                core::arch::asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack));
            };
            // DminLine (bits 19:16) is log2 of words; a word is 4 bytes.
            4 << ((ctr >> 16) & 0xf)
        }
        #[cfg(not(target_os = "none"))]
        64
    }
}

/// TLB maintenance.
///
/// Invalidating a translation only forces a re-walk of the page tables. It can
/// never make an access succeed that would otherwise fault, so no argument is
/// unsafe.
pub mod tlb {
    /// `tlbi vmalle1` — invalidate all stage-1 entries for EL1.
    #[inline(always)]
    pub fn vmalle1() {
        #[cfg(target_os = "none")]
        // SAFETY: invalidation forces a re-walk; it cannot grant access.
        unsafe {
            core::arch::asm!("tlbi vmalle1", options(nostack, preserves_flags));
        };
    }

    /// `tlbi vmalle1is` — the same, broadcast to the inner-shareable domain.
    #[inline(always)]
    pub fn vmalle1is() {
        #[cfg(target_os = "none")]
        // SAFETY: as `vmalle1`.
        unsafe {
            core::arch::asm!("tlbi vmalle1is", options(nostack, preserves_flags));
        };
    }

    /// `tlbi vaae1` — invalidate one VA, all ASIDs, EL1.
    ///
    /// Takes the architectural operand (VA >> 12), not a byte address.
    #[inline(always)]
    pub fn vaae1(va_page: u64) {
        #[cfg(target_os = "none")]
        // SAFETY: as `vmalle1`.
        unsafe {
            core::arch::asm!("tlbi vaae1, {}", in(reg) va_page, options(nostack, preserves_flags));
        };
        #[cfg(not(target_os = "none"))]
        let _ = va_page;
    }

    /// `tlbi vaae1is` — the same, broadcast to the inner-shareable domain.
    #[inline(always)]
    pub fn vaae1is(va_page: u64) {
        #[cfg(target_os = "none")]
        // SAFETY: as `vmalle1`.
        unsafe {
            core::arch::asm!("tlbi vaae1is, {}", in(reg) va_page, options(nostack, preserves_flags));
        };
        #[cfg(not(target_os = "none"))]
        let _ = va_page;
    }

    /// `tlbi aside1is` — invalidate every entry for one ASID, inner-shareable.
    #[inline(always)]
    pub fn aside1is(asid: u16) {
        #[cfg(target_os = "none")]
        // SAFETY: as `vmalle1`.
        unsafe {
            core::arch::asm!(
                "tlbi aside1is, {}",
                in(reg) (u64::from(asid) << 48),
                options(nostack, preserves_flags)
            );
        };
        #[cfg(not(target_os = "none"))]
        let _ = asid;
    }
}

/// Core parking and event signalling.
pub mod park {
    /// `wfi` — wait for interrupt. Returns when one is taken (or spuriously).
    #[inline(always)]
    pub fn wfi() {
        #[cfg(target_os = "none")]
        // SAFETY: a hint that stops fetching until an interrupt. Whether it is
        // *wise* to park here is a scheduling question, not a safety one.
        unsafe {
            core::arch::asm!("wfi", options(nostack, preserves_flags));
        };
    }

    /// `wfe` — wait for event.
    #[inline(always)]
    pub fn wfe() {
        #[cfg(target_os = "none")]
        // SAFETY: as `wfi`.
        unsafe {
            core::arch::asm!("wfe", options(nostack, preserves_flags));
        };
    }

    /// `sev` — signal event to all cores.
    #[inline(always)]
    pub fn sev() {
        #[cfg(target_os = "none")]
        // SAFETY: sets a per-core event flag; touches no memory.
        unsafe {
            core::arch::asm!("sev", options(nostack, preserves_flags));
        };
    }

    /// `nop` — one architecturally-defined no-op, for spin backoff.
    #[inline(always)]
    pub fn nop() {
        #[cfg(target_os = "none")]
        // SAFETY: it is a nop.
        unsafe {
            core::arch::asm!("nop", options(nomem, nostack, preserves_flags));
        };
    }
}

/// Read-only system registers.
///
/// Reads only. Every one of these is `nomem, nostack` — the value is diagnostic
/// or identifying, and obtaining it changes nothing. **Writing** any of these is
/// deliberately absent; see the crate header.
pub mod sysreg {
    /// Build a `pub fn` reading one system register into a `u64`.
    macro_rules! read_only {
        ($(#[$m:meta])* $name:ident, $reg:literal) => {
            $(#[$m])*
            #[inline(always)]
            pub fn $name() -> u64 {
                #[cfg(target_os = "none")]
                {
                    let v: u64;
                    // SAFETY: a read of a read-only system register: no memory
                    // access, no side effect.
                    unsafe {
                        core::arch::asm!(
                            concat!("mrs {}, ", $reg),
                            out(reg) v,
                            options(nomem, nostack)
                        );
                    };
                    v
                }
                #[cfg(not(target_os = "none"))]
                0
            }
        };
    }

    read_only!(/// `MPIDR_EL1` — core affinity.
        mpidr_el1, "mpidr_el1");
    read_only!(/// `ESR_EL1` — syndrome of the current exception.
        esr_el1, "esr_el1");
    read_only!(/// `FAR_EL1` — faulting virtual address.
        far_el1, "far_el1");
    read_only!(/// `ELR_EL1` — the address the exception will return to.
        elr_el1, "elr_el1");
    read_only!(/// `SPSR_EL1` — saved processor state.
        spsr_el1, "spsr_el1");
    read_only!(/// `TTBR0_EL1` — the user translation table base. Reading is safe;
        /// **writing is not**, and is deliberately not in this crate.
        ttbr0_el1, "ttbr0_el1");
    read_only!(/// `TTBR1_EL1` — the kernel translation table base.
        ttbr1_el1, "ttbr1_el1");
    read_only!(/// `TPIDRRO_EL0` — the current thread id.
        tpidrro_el0, "tpidrro_el0");
    read_only!(/// `TPIDR_EL1` — per-core kernel pointer.
        tpidr_el1, "tpidr_el1");
    read_only!(/// `SP_EL0` — the EL0 stack pointer.
        sp_el0, "sp_el0");
    read_only!(/// `CNTVCT_EL0` — virtual counter.
        cntvct_el0, "cntvct_el0");
    read_only!(/// `CNTFRQ_EL0` — counter frequency in Hz.
        cntfrq_el0, "cntfrq_el0");
    read_only!(/// `CTR_EL0` — cache type register.
        ctr_el0, "ctr_el0");
    read_only!(/// `DCZID_EL0` — `DC ZVA` block size.
        dczid_el0, "dczid_el0");
    read_only!(/// `SCTLR_EL1` — system control.
        sctlr_el1, "sctlr_el1");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The host shims must be callable, so a consuming crate's unit tests link.
    /// This is the whole of what can be asserted off the target: the point of the
    /// crate is that these are *safe to call*, and "it compiled and returned" is
    /// exactly that claim.
    #[test]
    fn every_wrapper_is_callable_on_the_host() {
        barrier::dsb_ish();
        barrier::dsb_ishst();
        barrier::dsb_sy();
        barrier::isb();
        cache::dc_cvau(0);
        cache::dc_cvac(0);
        cache::ic_ivau(0);
        cache::ic_iallu();
        tlb::vmalle1();
        tlb::vmalle1is();
        tlb::vaae1(0);
        tlb::vaae1is(0);
        tlb::aside1is(0);
        park::wfe();
        park::sev();
        park::nop();
        // `park::wfi()` is deliberately not called: on the host it is a no-op, but
        // a future port could make it real and hang the test runner.
    }

    /// Cache-line size must be a sane power of two on every build, because
    /// `dc_cvau` loops step by it — a zero would spin forever, and the host shim
    /// is what those loops use under `cargo test`.
    #[test]
    fn line_size_is_a_sane_power_of_two() {
        let n = cache::line_size();
        assert!(n >= 16 && n <= 2048, "implausible cache line size {n}");
        assert!(n.is_power_of_two(), "cache line size {n} is not a power of two");
    }

    #[test]
    fn sysreg_reads_return_zero_on_the_host() {
        assert_eq!(sysreg::mpidr_el1(), 0);
        assert_eq!(sysreg::esr_el1(), 0);
        assert_eq!(sysreg::cntfrq_el0(), 0);
    }
}
