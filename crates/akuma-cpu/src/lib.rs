//! AArch64 instructions that are **safe to execute**, behind safe functions.
//!
//! # Why this crate exists
//!
//! `core::arch::asm!` is unconditionally `unsafe`, so every barrier, cache
//! maintenance op and system-register read in the tree carried an `unsafe` block.
//! Every one of those blocks vouches for the same fact: that executing `dsb ish`
//! cannot violate memory safety.
//!
//! The tree was migrated onto this crate on **2026-08-31**
//! (`docs/archive/INLINE_ASM_CLEANUP.md`): **218 `asm!` sites outside this crate
//! became 35**, and `unsafe` sites fell 645 -> 543 tree-wide (production 518 ->
//! 455), as counted by `scripts/cloc_akuma.py`. What is left outside is
//! the exclusion list below, plus raw MMIO `ldr`/`str`, the GICv3 `ICC_*` writes,
//! the PSCI `hvc`/`smc` conduit, `adrp` symbol loads, and `global_asm!`.
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
//! - `msr tpidr_el1` / `tpidrro_el0` — re-points every per-thread static: the
//!   kernel's own per-thread base, and the thread id `current_tid` indexes every
//!   per-slot static with. (`tpidr_el0` — userspace's TLS base, which the kernel
//!   never reads through — moved *into* the crate as
//!   [`sysreg::set_tpidr_el0`]; see its doc for why the three registers are not
//!   one rule.)
//! - `mov sp, x` and `mov x30, x` — retarget every later stack access, and the
//!   next `ret`. Reading both is in [`reg`]; writing neither is.
//! - `dc zva` — unlike every other `dc`, it **writes** the block it names.
//! - Raw `ldr`/`str` (device MMIO), and the GICv3 `ICC_*` writes that gate
//!   interrupt delivery for a whole PE.
//!
//! Those stay `unsafe` at their call sites, where the argument for them is
//! specific. Reading `TTBR0_EL1` is here; writing it is not — the asymmetry is
//! the point, and mirrors the seam `akuma_primitives::preempt` already documents
//! for `TPIDRRO_EL0` ("reading moves, writing does not").
//!
//! Two modules sit deliberately *inside* the line rather than outside it —
//! [`daif`] (the interrupt mask) and [`vtimer`] (the timer comparator). Both
//! change control flow, so both look excludable; neither is, because the
//! obligation they can break is a **discipline** property of the surrounding
//! code (unmasking inside a critical section, arming a deadline the tick policy
//! did not choose) that no `unsafe` block at the call site could discharge. Each
//! module header states the argument, and names the type that does enforce it.
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

    /// `tlbi aside1` — invalidate every entry for one ASID, this core only.
    ///
    /// The core-local twin of [`aside1is`]: the non-`smp-shared` builds use it
    /// because no peer core is running the address space being invalidated.
    #[inline(always)]
    pub fn aside1(asid: u16) {
        #[cfg(target_os = "none")]
        // SAFETY: as `vmalle1`.
        unsafe {
            core::arch::asm!(
                "tlbi aside1, {}",
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

/// Reads of the two special-purpose general registers.
///
/// Not `mrs` — `mov {}, sp` and `mov {}, x30` — but the same argument: copying a
/// register into a local reads nothing through it, so an arbitrary value coming
/// back is not a safety problem. Both are diagnostic here: every caller in the
/// tree prints the value or range-checks it.
///
/// **Writing** either is deliberately absent, and is the sharpest case of the
/// asymmetry the crate header describes: `mov sp, x` retargets every subsequent
/// stack access, and `mov x30, x` retargets the next `ret`.
pub mod reg {
    /// The current stack pointer (`SP_EL1` at EL1).
    #[inline(always)]
    pub fn sp() -> usize {
        #[cfg(target_os = "none")]
        {
            let v: usize;
            // SAFETY: copies SP into a local; no access is made through it.
            unsafe {
                core::arch::asm!("mov {}, sp", out(reg) v, options(nomem, nostack));
            };
            v
        }
        #[cfg(not(target_os = "none"))]
        0
    }

    /// The link register, `x30` — the caller's return address.
    #[inline(always)]
    pub fn lr() -> u64 {
        #[cfg(target_os = "none")]
        {
            let v: u64;
            // SAFETY: as `sp`.
            unsafe {
                core::arch::asm!("mov {}, x30", out(reg) v, options(nomem, nostack));
            };
            v
        }
        #[cfg(not(target_os = "none"))]
        0
    }
}

/// The `DAIF` interrupt mask.
///
/// # Why masking is in this crate and not a caller's `unsafe`
///
/// Taking an interrupt is a control-flow change, so this looks like it belongs
/// in the header's exclusion list. It does not, for a reason worth stating:
/// `unsafe` cannot express the invariant these instructions can break.
///
/// The danger of `unmask_irq` is unmasking *inside* a critical section that
/// assumed IRQs were off — and that is a lock-discipline property of the
/// surrounding code, not of the instruction. No `unsafe` block at the call site
/// could discharge it, because the block cannot see the section it sits in.
/// What does enforce it is `akuma_primitives::irq::IrqGuard`: a scope whose
/// `Drop` restores the saved `DAIF`. That crate has presented all six of these
/// as **safe functions** since long before this crate existed; what moves here is
/// the `asm!`, not the safety judgement.
///
/// Callers should still reach for `IrqGuard` rather than these directly.
pub mod daif {
    /// Read `DAIF`. Bit 7 (`I`) set means IRQs are masked.
    #[inline(always)]
    pub fn read() -> u64 {
        #[cfg(target_os = "none")]
        {
            let v: u64;
            // SAFETY: reading the mask has no memory effect and changes nothing.
            unsafe {
                core::arch::asm!("mrs {}, daif", out(reg) v, options(nomem, nostack));
            };
            v
        }
        #[cfg(not(target_os = "none"))]
        0
    }

    /// Write `DAIF` wholesale — restoring a value from [`read`].
    #[inline(always)]
    pub fn restore(daif: u64) {
        #[cfg(target_os = "none")]
        // SAFETY: see the module header — the obligation this could break is a
        // lock-discipline one that `unsafe` cannot express.
        unsafe {
            core::arch::asm!("msr daif, {}", in(reg) daif, options(nomem, nostack));
        };
        #[cfg(not(target_os = "none"))]
        let _ = daif;
    }

    /// `msr daifset, #2` — mask IRQs.
    #[inline(always)]
    pub fn mask_irq() {
        #[cfg(target_os = "none")]
        // SAFETY: as `restore`. Masking is the conservative direction besides.
        unsafe {
            core::arch::asm!("msr daifset, #2", options(nomem, nostack));
        };
    }

    /// `msr daifclr, #2` — unmask IRQs.
    #[inline(always)]
    pub fn unmask_irq() {
        #[cfg(target_os = "none")]
        // SAFETY: as `restore`.
        unsafe {
            core::arch::asm!("msr daifclr, #2", options(nomem, nostack));
        };
    }

    /// Mask IRQs and `isb`, so the mask is in force for every instruction after.
    ///
    /// Fused deliberately: `msr daifset` without the barrier leaves a window in
    /// which an already-fetched instruction can still take the interrupt.
    #[inline(always)]
    pub fn mask_irq_sync() {
        #[cfg(target_os = "none")]
        // SAFETY: as `restore`.
        unsafe {
            core::arch::asm!("msr daifset, #2", "isb", options(nomem, nostack));
        };
    }

    /// Unmask IRQs and `isb`. Fused for the same reason as [`mask_irq_sync`].
    #[inline(always)]
    pub fn unmask_irq_sync() {
        #[cfg(target_os = "none")]
        // SAFETY: as `restore`.
        unsafe {
            core::arch::asm!("msr daifclr, #2", "isb", options(nomem, nostack));
        };
    }

    /// Read `DAIF`, then mask IRQs — the acquire half of an `IrqGuard`.
    #[inline(always)]
    pub fn save_and_mask_irq() -> u64 {
        let saved = read();
        mask_irq();
        saved
    }
}

/// The EL1 virtual timer comparator (`CNTV_*_EL0`).
///
/// Arming the comparator schedules an interrupt; like `daif`, that is a
/// control-flow effect with no memory-safety component, and the discipline it
/// can break (arming a deadline the tick policy did not intend) is one `unsafe`
/// cannot state. `akuma-timer` owns the policy; this owns the two instructions.
///
/// Reads of `CNTVCT_EL0` / `CNTFRQ_EL0` are in [`sysreg`] with the other reads.
///
/// # Why these two omit `nomem`
///
/// Every read in [`sysreg`] carries `options(nomem, nostack)`, which is right for
/// them: the value of `ESR_EL1` does not depend on memory, so letting the
/// optimiser move the `mrs` across a load costs nothing. Arming a comparator is
/// not that. It is an **observable device effect** ordered against the memory the
/// tick path publishes alongside it (the deadline it just recorded, the policy
/// state the ISR will read), and `nomem` would license moving the `msr` across
/// exactly those stores.
///
/// The open-coded `asm!` these replaced in `akuma-timer` and `src/timer.rs`
/// carried **no options at all** — the most conservative contract there is. They
/// were first written here with `nomem, nostack` copied from the read macro,
/// which silently weakened that. `nostack` alone restores it.
pub mod vtimer {
    /// `msr cntv_cval_el0` — set the compare value, in counter ticks.
    ///
    /// **No `nomem`**, deliberately — see the module note below.
    #[inline(always)]
    pub fn set_cval(deadline: u64) {
        #[cfg(target_os = "none")]
        // SAFETY: writes a comparator; it dereferences nothing.
        unsafe {
            core::arch::asm!("msr cntv_cval_el0, {}", in(reg) deadline, options(nostack));
        };
        #[cfg(not(target_os = "none"))]
        let _ = deadline;
    }

    /// `msr cntv_ctl_el0` — bit 0 enables, bit 1 **masks**. `1` is armed.
    #[inline(always)]
    pub fn set_ctl(ctl: u64) {
        #[cfg(target_os = "none")]
        // SAFETY: as `set_cval`.
        unsafe {
            core::arch::asm!("msr cntv_ctl_el0, {}", in(reg) ctl, options(nostack));
        };
        #[cfg(not(target_os = "none"))]
        let _ = ctl;
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
    read_only!(/// `TPIDR_EL0` — the thread pointer EL0 runs under. Both halves
        /// of this one are here; see [`set_tpidr_el0`].
        tpidr_el0, "tpidr_el0");

    /// Set `TPIDR_EL0`, the thread pointer **EL0** runs under, and `isb` so the
    /// new base is in effect before the `eret` back to userspace.
    ///
    /// # Why this write is in the crate when the other `msr`s are not
    ///
    /// The admission test is "safe to execute", and this one is. `TPIDR_EL0` is
    /// opaque userspace state to this kernel: it is read in exactly one place in
    /// the tree (`src/exceptions.rs`, to save it into the trap frame) and is
    /// never dereferenced or indexed with. A garbage value faults EL0's own TLS
    /// accesses — in userspace, contained to the process that asked for it — and
    /// cannot touch kernel memory, translation, or privilege.
    ///
    /// **This does not generalise to its neighbours, and the difference is the
    /// point.** `TPIDRRO_EL0` holds the *thread id*:
    /// `akuma_primitives::preempt::current_tid` indexes every per-slot static in
    /// the kernel with it and halts the core if it is out of range. `TPIDR_EL1`
    /// is the kernel's own per-thread base. Writes to either re-point kernel
    /// state the kernel then dereferences, so both stay `unsafe` at their call
    /// site alongside `ttbr0_el1`/`vbar_el1`/`elr_el1`.
    ///
    /// Added 2026-08-31. `INLINE_ASM_CLEANUP.md` §2 had all three registers on
    /// one exclusion row reading "re-points every per-thread static, **or**
    /// userspace's whole TLS"; the first half is the soundness argument and
    /// belongs to the other two, the second half is a blast-radius argument and
    /// was never a reason to make the caller write `unsafe`.
    #[inline(always)]
    pub fn set_tpidr_el0(base: u64) {
        #[cfg(target_os = "none")]
        {
            // SAFETY: writes a register the kernel never reads through. The `isb`
            // orders it ahead of the return to EL0; `nomem` is accurate because no
            // memory is touched by either instruction.
            unsafe {
                core::arch::asm!(
                    "msr tpidr_el0, {}",
                    "isb",
                    in(reg) base,
                    options(nomem, nostack)
                );
            }
        }
        #[cfg(not(target_os = "none"))]
        let _ = base;
    }
    read_only!(/// `FPCR` — floating-point control.
        fpcr, "fpcr");

    /// `CNTVCT_EL0`, preceded by an `isb`.
    ///
    /// **Use this, not [`cntvct_el0`], to time anything.** A bare `mrs
    /// cntvct_el0` is not ordered against the instructions around it, so the
    /// core may read the counter before the work being measured has issued —
    /// measured on this project, that made an 8 KB `copy_to_user` come out at
    /// **0 ns** (`docs/archive/`, "CNTVCT needs isb for timing"). The barrier is
    /// fused into the same `asm!` so no optimiser can separate them.
    ///
    /// It costs a pipeline flush, which is why the unordered read stays
    /// available for callers that only want a coarse timestamp.
    #[inline(always)]
    pub fn cntvct_el0_ordered() -> u64 {
        #[cfg(target_os = "none")]
        {
            let v: u64;
            // SAFETY: a barrier and a read of a counter register.
            unsafe {
                core::arch::asm!("isb", "mrs {}, cntvct_el0", out(reg) v, options(nostack));
            };
            v
        }
        #[cfg(not(target_os = "none"))]
        0
    }
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
        tlb::aside1(0);
        park::wfe();
        park::sev();
        park::nop();
        let _ = reg::sp();
        let _ = reg::lr();
        daif::restore(daif::save_and_mask_irq());
        daif::mask_irq();
        daif::unmask_irq();
        daif::mask_irq_sync();
        daif::unmask_irq_sync();
        vtimer::set_cval(0);
        vtimer::set_ctl(0);
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
        assert_eq!(sysreg::tpidr_el0(), 0);
        assert_eq!(sysreg::fpcr(), 0);
        assert_eq!(sysreg::cntvct_el0_ordered(), 0);
        assert_eq!(daif::read(), 0);
    }
}
