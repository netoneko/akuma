#![no_std]
// This crate is unsafe-free by design and stays that way: `forbid` (not
// `deny`) so no module can opt back in with a local `allow`. Cargo cannot
// host this per-crate — `[lints] workspace = true` and crate-local lints are
// mutually exclusive ("cannot override `workspace.lints` in `lints`"), and
// spelling the ban in Cargo.toml would mean duplicating the whole workspace
// lint table into every crate that wants it.
#![forbid(unsafe_code)]
//! Linux `reboot(2)` ABI decode — pure, host-testable.
//!
//! Since 2026-09-01 this crate also *performs* the reboot: [`system_reset`] and
//! [`system_off`] live here, issuing the call through `akuma-psci`. They were in
//! `src/smp_shared.rs`, which owned the conduit because it needed `CPU_ON` for
//! bring-up — so a crate that could decode a reboot had to reach back into the
//! bin crate to do one, and `sc-reboot` had to depend on `smp-shared` to get it.
//! The conduit is now `akuma-psci`, a sibling rather than part of this crate,
//! precisely so the `unsafe` `smc`/`hvc` does not cost this crate its
//! `#![forbid(unsafe_code)]` (`docs/archive/AKUMA_SMP_SHARED_SPLIT.md`).
//!
//! Devbox-only (`sc-reboot` feature, wired into `devbox`/`devbox-smoltcp`):
//! rebuild the kernel in-guest, `dd` it over the file backing both `-kernel`
//! and a virtio-blk drive (`scripts/cargo_runner.sh`'s `KERNEL_DROPOFF`), then
//! call `reboot(2)` to relaunch straight into the freshly built image instead
//! of a host-side extract + manual `cargo run` cycle.

/// First magic Linux's `reboot(2)` requires — constant across all commands.
pub const MAGIC1: u32 = 0xfee1_dead;
/// The magic musl's `reboot()` wrapper actually sends.
pub const MAGIC2: u32 = 0x2812_1969;
// Historical alternates the Linux kernel ABI also accepts for MAGIC2.
pub const MAGIC2A: u32 = 0x0512_1996;
pub const MAGIC2B: u32 = 0x1604_1998;
pub const MAGIC2C: u32 = 0x2011_2000;

pub const CMD_RESTART: u32 = 0x0123_4567;
pub const CMD_HALT: u32 = 0xcdef_0123;
pub const CMD_CAD_ON: u32 = 0x89ab_cdef;
pub const CMD_CAD_OFF: u32 = 0x0000_0000;
pub const CMD_POWER_OFF: u32 = 0x4321_fedc;
pub const CMD_RESTART2: u32 = 0xa1b2_c3d4;

// PSCI SMCCC function IDs the `Action`s below map to. `SYSTEM_OFF`/`SYSTEM_RESET`
// take no 64-bit arguments, so unlike `CPU_ON` there's no separate SMC64 encoding
// — same function ID over either conduit (`smc`/`hvc`). The conduit call itself
// stays in `src/smp_shared.rs`, which already owns SMC/HVC conduit selection.
pub const PSCI_SYSTEM_OFF: u64 = 0x8400_0008;
pub const PSCI_SYSTEM_RESET: u64 = 0x8400_0009;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Restart,
    PowerOff,
    /// `CMD_HALT`/`CMD_CAD_ON`/`CMD_CAD_OFF`: accepted per the Linux ABI, no
    /// hardware effect — there's no ACPI, watchdog, or CAD state here to touch.
    Noop,
}

/// Either magic was wrong — maps to `-EINVAL` at the syscall boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadMagic;

/// Decode a `reboot(2)` call per the Linux ABI.
///
/// `Err(BadMagic)` and `Ok(None)` (a valid-magic but unrecognized `cmd`) both
/// map to `-EINVAL` at the call site — kept distinct here so the caller can log
/// which failure it was.
pub fn decode(magic1: u32, magic2: u32, cmd: u32) -> Result<Option<Action>, BadMagic> {
    if magic1 != MAGIC1 || !matches!(magic2, MAGIC2 | MAGIC2A | MAGIC2B | MAGIC2C) {
        return Err(BadMagic);
    }
    Ok(match cmd {
        CMD_RESTART | CMD_RESTART2 => Some(Action::Restart),
        CMD_POWER_OFF => Some(Action::PowerOff),
        CMD_HALT | CMD_CAD_ON | CMD_CAD_OFF => Some(Action::Noop),
        _ => None,
    })
}

/// Whole-machine PSCI `SYSTEM_RESET`.
///
/// Only built with the `psci` feature (the default) — an AArch64 concept.
/// `amd64/src/reboot.rs` is the x86 counterpart.
///
/// Akuma has no in-kernel park/quiesce dance before this, and needs none: QEMU
/// and firmware tear every core and device back down to the same clean reset
/// state `boot.rs` already assumes, so a plain PSCI reset gets that for free.
///
/// `-kernel` bytes are cached by QEMU at process startup and are **not** re-read
/// on an in-process reset, so this only picks up a freshly built kernel when
/// combined with `-action reboot=shutdown` and a host-side relaunch — see
/// `scripts/cargo_runner.sh`'s `KERNEL_DROPOFF` and
/// `docs/runbooks/selfhost-kernel-build.md`.
///
/// Callers should sync filesystems first; this does not return.
#[cfg(feature = "psci")]
pub fn system_reset() -> ! {
    // Discarded deliberately: on success this does not return at all, so a
    // returned status can only mean the call failed — which the loop below
    // already handles. There is no caller left to report it to.
    let _ = akuma_psci::call(akuma_psci::SYSTEM_RESET, 0, 0, 0);
    // `SYSTEM_RESET` does not return on success, so reaching here means the call
    // itself failed — typically no PSCI conduit. There is nothing sensible left
    // to do: the syscall dispatcher is not set up to receive a return from this
    // path, and the caller has already synced and announced the reboot.
    loop {
        core::hint::spin_loop();
    }
}

/// Whole-machine PSCI `SYSTEM_OFF`. See [`system_reset`] for the shared
/// reasoning; likewise does not return.
#[cfg(feature = "psci")]
pub fn system_off() -> ! {
    let _ = akuma_psci::call(akuma_psci::SYSTEM_OFF, 0, 0, 0);
    loop {
        core::hint::spin_loop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_decodes() {
        assert_eq!(decode(MAGIC1, MAGIC2, CMD_RESTART), Ok(Some(Action::Restart)));
        assert_eq!(decode(MAGIC1, MAGIC2, CMD_RESTART2), Ok(Some(Action::Restart)));
    }

    #[test]
    fn bad_magic1_rejected() {
        assert_eq!(decode(0, MAGIC2, CMD_RESTART), Err(BadMagic));
    }

    #[test]
    fn bad_magic2_rejected() {
        assert_eq!(decode(MAGIC1, 0, CMD_RESTART), Err(BadMagic));
    }

    #[test]
    fn alternate_magic2_values_accepted() {
        for m2 in [MAGIC2A, MAGIC2B, MAGIC2C] {
            assert_eq!(decode(MAGIC1, m2, CMD_RESTART), Ok(Some(Action::Restart)));
        }
    }

    #[test]
    fn power_off_decodes() {
        assert_eq!(decode(MAGIC1, MAGIC2, CMD_POWER_OFF), Ok(Some(Action::PowerOff)));
    }

    #[test]
    fn cad_and_halt_are_noop() {
        assert_eq!(decode(MAGIC1, MAGIC2, CMD_HALT), Ok(Some(Action::Noop)));
        assert_eq!(decode(MAGIC1, MAGIC2, CMD_CAD_ON), Ok(Some(Action::Noop)));
        assert_eq!(decode(MAGIC1, MAGIC2, CMD_CAD_OFF), Ok(Some(Action::Noop)));
    }

    #[test]
    fn unrecognized_cmd_with_good_magic_is_none() {
        assert_eq!(decode(MAGIC1, MAGIC2, 0xdead_beef), Ok(None));
    }
}
