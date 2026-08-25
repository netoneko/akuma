//! `reboot(2)` — devbox-only (`sc-reboot`). ABI decode lives in `akuma_boot`
//! (host-testable); this just matches the decoded `Action` onto the PSCI call
//! `smp_shared` already knows how to issue.

use super::*;

pub(super) fn sys_reboot(magic1: u32, magic2: u32, cmd: u32) -> u64 {
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
