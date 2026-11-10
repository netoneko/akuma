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
#[cfg(target_os = "none")]
pub mod exceptions;
#[cfg(all(target_os = "none", kernel_smp_shared))]
pub mod smp_shared;
#[cfg(target_os = "none")]
pub mod hooked;

/// The absolute symbols `linker.ld` exports, in `akuma-boot-aarch64` beside the
/// assembly that consumes them since 2026-11-04.
#[cfg(target_os = "none")]
pub use akuma_boot_aarch64::linker_syms;
