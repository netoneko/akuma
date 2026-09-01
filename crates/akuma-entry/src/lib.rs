#![no_std]
// This crate deliberately does NOT carry `#![forbid(unsafe_code)]`, and never
// will. It exists so that everything above it can.
//
// Nothing here is an `unsafe` *operation* in the usual sense — there is not one
// `unsafe {}` block in the crate. What it holds is the three constructs the
// `unsafe_code` lint rejects on sight, all of them link-level rather than
// memory-level:
//
//   * `core::arch::global_asm!`   — `_boot`, and the secondary trampoline
//   * `#[unsafe(no_mangle)]`      — the symbols that assembly branches to
//   * `unsafe extern "C" { … }`   — the symbols assembly and `linker.ld` define
//
// `src/smp_shared.rs` reached zero `unsafe` blocks on 2026-09-01 and still could
// not take the ban, because of exactly those three. Splitting them out is the
// same trade the tree already made twice: `akuma-psci` holds the `smc`/`hvc` so
// `akuma-boot` can forbid, and `akuma-net-nic` holds the DMA so `akuma-net` can.
// Here the beneficiary is `akuma-kernel-glue` (~2.7k lines of `kernel_main` and
// the rump proxy), which took `#![forbid(unsafe_code)]` the moment these two
// modules left it.
//
// The contract, stated once for the whole crate: every symbol declared in an
// `unsafe extern` block below is defined either by `linker.ld` or by a
// `global_asm!` block in this crate, and every `#[unsafe(no_mangle)]` function
// here is branched to only by that assembly. Nothing here dereferences a
// caller-supplied pointer.
//
// **Do not add a module that merely *calls* this code.** The crate is sized to
// its contract on purpose; the place for logic that runs after the trampoline is
// `akuma-kernel-glue`, above.
//! Kernel entry points: boot assembly, the secondary-core trampoline, and the
//! linker symbols describing the loaded image.
//!
//! Extracted from `akuma-kernel-glue` on 2026-09-01. `boot` and `smp_shared`
//! moved verbatim; the linker-symbol block below came out of `kernel_main`,
//! where it was an `unsafe extern "C"` declaration inline in the boot path.

#[cfg(target_os = "none")]
pub mod boot;
#[cfg(all(target_os = "none", kernel_smp_shared))]
pub mod smp_shared;

/// The absolute symbols `linker.ld` exports to describe the loaded image and the
/// boot stack reserved above it.
///
/// These were declared inline in `kernel_main` (and, separately and identically,
/// in `src/process_tests.rs`) until this crate existed. Reading a linker symbol's
/// *address* is a safe operation — `&raw const` needs no `unsafe` block — so the
/// only reason those call sites were unsafe at all was the `unsafe extern` block
/// required to name the symbols. Naming them once here makes every consumer safe
/// and stops the two declarations from drifting.
///
/// The values auto-track the binary: `linker.ld` derives the stack reservation
/// from the actual linked size, so there is no per-profile `IMAGE_SIZE` constant
/// to keep in lockstep.
#[cfg(target_os = "none")]
pub mod linker_syms {
    unsafe extern "C" {
        static _kernel_phys_end: u8;
        static STACK_BOTTOM: u8;
        static STACK_TOP: u8;
    }

    /// One past the last byte of the linked kernel image.
    #[must_use]
    pub fn kernel_phys_end() -> usize {
        &raw const _kernel_phys_end as usize
    }

    /// First page of the boot stack — the guard-adjacent low end, not the SP.
    #[must_use]
    pub fn stack_bottom() -> usize {
        &raw const STACK_BOTTOM as usize
    }

    /// Initial SP: one past the top of the boot stack.
    #[must_use]
    pub fn stack_top() -> usize {
        &raw const STACK_TOP as usize
    }
}
