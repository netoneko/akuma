//! `reboot(2)` — the `sc-reboot` syscall.
//!
//! ABI decode lives in `akuma_boot` (host-testable); this just matches the
//! decoded `Action` onto the PSCI call `smp_shared` already knows how to issue.

use super::*;

pub(super) fn sys_reboot(magic1: u32, magic2: u32, cmd: u32) -> u64 {
    // Whole-machine PSCI `SYSTEM_RESET`/`SYSTEM_OFF` takes every box down with it, and Linux asks for `CAP_SYS_BOOT`:
    // uid 0 in the host box. The token is what `system_reset`/`system_off` take.
    let Some(root) = akuma_exec::process::host_root() else {
        return EPERM;
    };
    match akuma_boot::decode(magic1, magic2, cmd) {
        Ok(Some(akuma_boot::Action::Restart)) => {
            akuma_primitives::safe_print!(48, "[reboot] PSCI SYSTEM_RESET requested\n");
            // Write-back caches make reboot(2) the clean-shutdown path: push
            // every dirty block out before the machine dies. Best-effort —
            // a failed sync must not strand a rebooting box, and the data is
            // e2fsck-recoverable — but say so on the console.
            if akuma_vfs_glue::sync_all_filesystems().is_err() {
                akuma_primitives::safe_print!(48, "[reboot] fs sync failed; disk may need e2fsck\n");
            }
            akuma_boot::system_reset(root);
        }
        Ok(Some(akuma_boot::Action::PowerOff)) => {
            akuma_primitives::safe_print!(48, "[reboot] PSCI SYSTEM_OFF requested\n");
            if akuma_vfs_glue::sync_all_filesystems().is_err() {
                akuma_primitives::safe_print!(48, "[reboot] fs sync failed; disk may need e2fsck\n");
            }
            akuma_boot::system_off(root);
        }
        Ok(Some(akuma_boot::Action::Noop)) => 0,
        Ok(None) | Err(akuma_boot::BadMagic) => EINVAL,
    }
}
