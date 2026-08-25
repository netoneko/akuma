//! `reboot(2)` — `sc-reboot`. ABI decode lives in `akuma_boot` (host-testable);
//! this just matches the decoded `Action` onto the PSCI call `smp_shared`
//! already knows how to issue.

use super::*;

/// Whole-machine PSCI `SYSTEM_RESET`/`SYSTEM_OFF` takes every box down with it,
/// not just the caller's — strictly worse than the mount-table tampering
/// `caller_may_mount` (`src/syscall/container.rs`) already restricts to box 0,
/// so the same restriction applies here. `is_none_or` matches that helper: no
/// current process (host-side/early-boot caller) is treated as box 0.
fn caller_may_reboot() -> bool {
    akuma_exec::process::current_process_shared().is_none_or(|p| p.box_id == 0)
}

pub(super) fn sys_reboot(magic1: u32, magic2: u32, cmd: u32) -> u64 {
    if !caller_may_reboot() {
        return EPERM;
    }
    match akuma_boot::decode(magic1, magic2, cmd) {
        Ok(Some(akuma_boot::Action::Restart)) => {
            crate::safe_print!(48, "[reboot] PSCI SYSTEM_RESET requested\n");
            crate::smp_shared::system_reset();
        }
        Ok(Some(akuma_boot::Action::PowerOff)) => {
            crate::safe_print!(48, "[reboot] PSCI SYSTEM_OFF requested\n");
            crate::smp_shared::system_off();
        }
        Ok(Some(akuma_boot::Action::Noop)) => 0,
        Ok(None) | Err(akuma_boot::BadMagic) => EINVAL,
    }
}
