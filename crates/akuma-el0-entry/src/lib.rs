//! The EL0 entry `eret` — `enter_user_mode` and its SPSR-gated safe wrapper
//! `enter_user_mode_checked`.
//!
//! # What this crate is for
//!
//! Extracted from `akuma-exec`'s `process/mod.rs` on 2026-09-02
//! (`AKUMA_EXEC_AUDIT.md` §6) so that `akuma-exec` proper — fork, exec, signals,
//! channels, fds, lifecycle, reclaim — no longer carries the register-load +
//! `eret` `asm!` that drops a freshly-launched or freshly-`execve`'d thread into
//! userspace. It is one `asm!` block plus the one runtime check
//! (`SPSR_EL1.M[3:0] == EL0t`) that makes calling it safe.
//!
//! It **cannot** `#![forbid(unsafe_code)]`: it is a `global`-scope `asm!` that
//! writes `sp_el0`/`elr_el1`/`spsr_el1`/`tpidr_el0` and `eret`s.
//!
//! # It sits BELOW `akuma-exec`
//!
//! It does not name the `Process` type. `enter_user_mode` takes a
//! `&UserContext`, which lives in `akuma-exec-core`. `akuma-exec` re-exports both
//! functions under their original paths
//! (`akuma_exec::process::{enter_user_mode, enter_user_mode_checked}`), so its
//! call sites and `akuma-syscalls-glue`'s one `enter_user_mode_checked` call
//! resolve unchanged.
//!
//! # The one stated contract
//!
//! `enter_user_mode(ctx)` `eret`s to `ctx.pc` at the exception level named by
//! `ctx.spsr`. The caller must guarantee `ctx.spsr` targets EL0 — an `EL1h`
//! context turns the call into "jump to `ctx.pc` **with kernel privilege**".
//! `enter_user_mode_checked` discharges exactly that obligation with a runtime
//! compare and is the safe entry point; the raw `unsafe fn` stays available for
//! the trampolines that have already validated the context upstream.

#![cfg_attr(not(test), no_std)]

use akuma_exec_core::process::UserContext;
#[cfg(target_os = "none")]
use akuma_primitives::safe_print;

/// [`enter_user_mode`], with the one check that makes it safe to call.
///
/// `eret` returns to whatever exception level `SPSR_EL1.M[3:0]` names, so a
/// context claiming `EL1h` turns this call into "jump to an arbitrary address
/// **with kernel privilege**" — that, not the program counter, is what made the
/// raw form `unsafe`. A context that targets EL0 can only misbehave in
/// userspace, where a bad PC or SP faults the process and nothing else.
///
/// Every context this kernel builds sets `spsr = 0` (EL0t), so the check costs
/// one compare on a path taken once per exec.
///
/// A context that fails it does not return either: by the time execve reaches
/// here the old image is already gone, so there is nothing to fall back to. It
/// halts the core with the offending value named, the same choice
/// `akuma_primitives::preempt::current_tid` makes for a corrupt `TPIDRRO_EL0`.
#[cfg(target_os = "none")]
pub fn enter_user_mode_checked(ctx: &UserContext) -> ! {
    const SPSR_M_MASK: u64 = 0b1111;
    const SPSR_M_EL0T: u64 = 0b0000;
    if ctx.spsr & SPSR_M_MASK != SPSR_M_EL0T {
        safe_print!(
            192,
            "[FATAL] enter_user_mode_checked: spsr={:#x} does not target EL0 (M={:#x})\n\
             Refusing to eret — this would return with kernel privilege.\n",
            ctx.spsr,
            ctx.spsr & SPSR_M_MASK
        );
        loop {
            akuma_cpu::park::wfi();
        }
    }
    // SAFETY: the context targets EL0, checked above. An EL0 context cannot
    // return into the kernel; a bad PC/SP inside it faults the process.
    unsafe { enter_user_mode(ctx) }
}

#[cfg(not(target_os = "none"))]
pub fn enter_user_mode_checked(_ctx: &UserContext) -> ! {
    panic!("enter_user_mode_checked on a host build")
}

/// Enter user mode with the given context.
///
/// This sets up the CPU state and performs an ERET to EL0. Does not return.
///
/// # Safety
///
/// `ctx.spsr` must target EL0 (`M[3:0] == 0b0000`). See the crate-level contract;
/// callers that cannot prove it statically must go through
/// [`enter_user_mode_checked`].
#[cfg(target_os = "none")]
#[inline(never)]
pub unsafe fn enter_user_mode(ctx: &UserContext) -> ! {
    // Tripwire for the SMP=4 mixed-EL corruption: refuse silence if this EL0 entry
    // would land in kernel text (poison minted upstream — see update_thread_context).
    if ctx.pc >= 0x4000_0000 {
        safe_print!(128, "[EUM POISON] enter_user_mode pc={:#x} spsr={:#x} tid={}\n",
            ctx.pc, ctx.spsr, akuma_threading::current_thread_id());
    }
    // This `eret` drops to EL0 without returning through the syscall wrapper (initial
    // process launch / execve), so the SVC epilogue's `clear_current_trap_frame` never
    // runs for the trap that got us here. On the execve path that leaves the slot
    // pointing at the abandoned execve trap frame while userspace runs — stale for
    // every reader until the next SVC republishes. No live frame exists at an ERET to
    // user, so clear it unconditionally.
    akuma_threading::clear_current_trap_frame();
    // Real shared-kernel SMP: this `eret` drops to EL0 without returning through the
    // syscall wrapper (initial process launch / execve), so release the BKL here —
    // otherwise it would stay held while running userspace. No-op unless
    // `cfg(kernel_smp_shared)`.
    akuma_bkl::bkl::leave_kernel();
    // SAFETY: This inline asm sets up CPU state and ERETs to user mode.
    // x30 is pinned as the context pointer and loaded last to avoid corruption.
    unsafe {
        core::arch::asm!(
            // Set system registers from named operands (consumed before GP loads)
            "msr sp_el0, {sp_user}",
            "msr elr_el1, {pc}",
            "msr spsr_el1, {spsr}",
            "msr tpidr_el0, {tls}",
            // Load x0-x29 from context struct (x30 = ctx pointer, stable throughout)
            "ldp x0, x1, [x30]",
            "ldp x2, x3, [x30, #16]",
            "ldp x4, x5, [x30, #32]",
            "ldp x6, x7, [x30, #48]",
            "ldp x8, x9, [x30, #64]",
            "ldp x10, x11, [x30, #80]",
            "ldp x12, x13, [x30, #96]",
            "ldp x14, x15, [x30, #112]",
            "ldp x16, x17, [x30, #128]",
            "ldp x18, x19, [x30, #144]",
            "ldp x20, x21, [x30, #160]",
            "ldp x22, x23, [x30, #176]",
            "ldp x24, x25, [x30, #192]",
            "ldp x26, x27, [x30, #208]",
            "ldp x28, x29, [x30, #224]",
            // Load x30 last (overwrites ctx pointer, no longer needed)
            "ldr x30, [x30, #240]",
            "eret",
            in("x30") core::ptr::from_ref::<UserContext>(ctx),
            sp_user = in(reg) ctx.sp,
            pc = in(reg) ctx.pc,
            spsr = in(reg) ctx.spsr,
            tls = in(reg) ctx.tpidr,
            options(noreturn)
        )
    }
}

/// Host-build stub — see the bare-metal definition for the contract.
///
/// # Safety
/// Never satisfiable off bare metal: this always panics.
#[cfg(not(target_os = "none"))]
pub unsafe fn enter_user_mode(_ctx: &UserContext) -> ! {
    panic!("not on bare metal")
}
