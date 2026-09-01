//! The PSCI conduit — the `smc`/`hvc` instruction, and which of the two to use.
//!
//! Moved out of `src/smp_shared.rs` on 2026-09-01
//! (`docs/archive/AKUMA_SMP_SHARED_SPLIT.md`), where it was the **last**
//! `unsafe` block in that file.
//!
//! # Why this is not in `akuma-boot`
//!
//! `akuma-boot` decodes `reboot(2)` into an [`Action`] and carries
//! `#![forbid(unsafe_code)]`. Absorbing an `smc` would have cost it that ban, so
//! the conduit is a sibling instead — the same split `akuma-net` /
//! `akuma-net-nic` makes, for the same reason: keep the crate that can forbid
//! forbidding. `akuma-boot` decides *what a reboot means*; this crate is the
//! only thing that can *perform* one.
//!
//! [`Action`]: https://docs.rs/akuma-boot
//!
//! # Why this is not in `akuma-cpu`
//!
//! `akuma-cpu` is AArch64 instructions that are **safe to execute**, exposed as
//! safe functions. `smc #0` is the opposite: with [`SYSTEM_OFF`] it halts the
//! machine, with [`SYSTEM_RESET`] it reboots it, and with [`CPU_ON`] it starts
//! another core executing at an address you supply. A safe wrapper for that in a
//! crate every module depends on would let any safe code in the tree power the
//! box off. It stays `unsafe`, in the crate that owns it.
//!
//! # The contract
//!
//! Calling into firmware or the hypervisor is only sound when the arguments form
//! a valid SMCCC call. The asm clobbers `x1`–`x17` (the caller-saved range SMCCC
//! permits an implementation to trash) and returns `x0`. Nothing else about the
//! machine is assumed: the calls take `options(nostack)` because they touch no
//! stack, and are deliberately **not** `nomem` — firmware may observe or modify
//! memory (`CPU_ON` reads the entry point's page tables).
//!
//! # These functions may not return
//!
//! [`SYSTEM_RESET`] and [`SYSTEM_OFF`] do not return on success. Callers must
//! treat a return as the failure path — typically "no PSCI conduit" — and have
//! somewhere to go that is not "fall through".

#![no_std]

use core::sync::atomic::{AtomicBool, Ordering};

/// PSCI SMC64 `CPU_ON`. Starts a secondary at a physical entry point.
///
/// SMC64-specific encoding, unlike the two below: it takes 64-bit arguments
/// (the target MPIDR, the entry PA and a context id).
pub const CPU_ON: u64 = 0xC400_0003;

/// PSCI `SYSTEM_OFF`. Takes no 64-bit arguments, so there is no separate SMC64
/// encoding — the same function ID works over either conduit.
pub const SYSTEM_OFF: u64 = 0x8400_0008;

/// PSCI `SYSTEM_RESET`. Same encoding note as [`SYSTEM_OFF`].
pub const SYSTEM_RESET: u64 = 0x8400_0009;

/// Which instruction reaches the PSCI implementation on this machine.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Conduit {
    /// `smc #0` — firmware at EL3.
    Smc,
    /// `hvc #0` — a hypervisor at EL2. QEMU `virt` and Firecracker.
    Hvc,
}

/// The conduit, as `/psci`'s `method` property reported it.
///
/// Defaults to [`Conduit::Hvc`], which is what both supported machines use, so a
/// board that never calls [`set_conduit`] still boots. Stored rather than passed
/// because the DTB is parsed once at boot, long before `bringup_secondaries` or
/// a `reboot(2)` needs it — and on large-RAM configs the heap can land on the
/// DTB, so re-reading the tree later is not an option.
static IS_HVC: AtomicBool = AtomicBool::new(true);

/// Record the conduit the device tree reported. Call once, from DTB probe.
pub fn set_conduit(c: Conduit) {
    IS_HVC.store(c == Conduit::Hvc, Ordering::Relaxed);
}

/// The conduit currently selected.
#[must_use]
pub fn conduit() -> Conduit {
    if IS_HVC.load(Ordering::Relaxed) { Conduit::Hvc } else { Conduit::Smc }
}

/// Issue a PSCI call over the selected conduit. Returns `x0`.
///
/// See the module docs: this **may not return** for [`SYSTEM_RESET`] /
/// [`SYSTEM_OFF`].
#[must_use]
pub fn call(func: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    match conduit() {
        Conduit::Hvc => hvc_call(func, a1, a2, a3),
        Conduit::Smc => smc_call(func, a1, a2, a3),
    }
}

/// Issue the call over `hvc #0`, regardless of the selected conduit.
#[must_use]
pub fn hvc_call(func: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    let ret: i64;
    // SAFETY: a standard SMCCC call; we clobber the caller-saved GPR range
    // (x1-x17) that SMCCC permits an implementation to trash.
    unsafe {
        core::arch::asm!(
            "hvc #0",
            inout("x0") func => ret,
            in("x1") a1, in("x2") a2, in("x3") a3,
            lateout("x4") _, lateout("x5") _, lateout("x6") _, lateout("x7") _,
            lateout("x8") _, lateout("x9") _, lateout("x10") _, lateout("x11") _,
            lateout("x12") _, lateout("x13") _, lateout("x14") _, lateout("x15") _,
            lateout("x16") _, lateout("x17") _,
            options(nostack),
        );
    }
    ret
}

/// Issue the call over `smc #0`, regardless of the selected conduit.
#[must_use]
pub fn smc_call(func: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    let ret: i64;
    // SAFETY: as `hvc_call` — a standard SMCCC call clobbering x1-x17.
    unsafe {
        core::arch::asm!(
            "smc #0",
            inout("x0") func => ret,
            in("x1") a1, in("x2") a2, in("x3") a3,
            lateout("x4") _, lateout("x5") _, lateout("x6") _, lateout("x7") _,
            lateout("x8") _, lateout("x9") _, lateout("x10") _, lateout("x11") _,
            lateout("x12") _, lateout("x13") _, lateout("x14") _, lateout("x15") _,
            lateout("x16") _, lateout("x17") _,
            options(nostack),
        );
    }
    ret
}

#[cfg(test)]
mod tests {
    use super::*;

    // The asm cannot run on the host — an `smc` from userspace traps. What is
    // testable is the conduit selection, which is what actually varied by
    // machine and what a wrong DTB parse would silently flip.

    // ONE test, not two: `IS_HVC` is a `static`, and `cargo test` runs tests on
    // a thread pool — a separate "defaults to hvc" test would race whichever
    // test flips the conduit and fail intermittently. Same rule as
    // `akuma_bkl::policy`'s opt-out bitmap tests.
    #[test]
    fn conduit_defaults_to_hvc_and_round_trips() {
        // Both supported machines (QEMU `virt`, Firecracker) use `hvc`, and a
        // board that never probes must still reach PSCI.
        assert_eq!(conduit(), Conduit::Hvc, "default conduit must be hvc");

        set_conduit(Conduit::Smc);
        assert_eq!(conduit(), Conduit::Smc);
        set_conduit(Conduit::Hvc);
        assert_eq!(conduit(), Conduit::Hvc);
    }
}
