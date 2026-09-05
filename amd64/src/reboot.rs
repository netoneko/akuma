//! `reboot(2)` — the ABI decode is shared, the machine reset is x86-specific.
//!
//! `akuma-boot` already turns `(magic1, magic2, cmd)` into an [`Action`], and
//! host-tests that against the values musl actually sends — the aarch64
//! kernel's `sc-reboot` uses it, and so does this. What `akuma-boot` also has,
//! `system_reset`, is a PSCI `smc`: an AArch64 firmware call that does nothing
//! on x86. So the *action* is shared and the *effect* is here.
//!
//! Three ways to reset an x86 PC, tried in order — each is a fallback for the
//! last failing silently:
//!
//! 1. **`0xCF9`, the reset-control register.** `0x0E` = full reset. Every Intel
//!    PCH and most others honour it; it is what the reference machine needs.
//! 2. **The i8042 pulse.** `0xFE` to port `0x64` pulses the CPU's RESET line —
//!    the fallback from the AT.
//! 3. **A triple fault.** Load a zero-length IDT and raise `#BP`; with no
//!    handler and no way to escalate, the CPU resets. Always works.
//!
//! Verified without rebooting by [`smoke_test`] (the decode + the syscall's
//! `EINVAL` path); the reset itself is checked by hand on the box through a
//! GRUB entry that runs `busybox reboot`.

use akuma_boot::{Action, decode};
use akuma_selftest::Suite;

use crate::fd::errno;
use crate::port;
use crate::serial;

/// A 10-byte `IDTR` operand describing an empty interrupt descriptor table.
#[repr(C, packed)]
struct Idtr {
    limit: u16,
    base: u64,
}

static EMPTY_IDT: Idtr = Idtr { limit: 0, base: 0 };

/// Reset the machine. Never returns.
pub fn perform_reset() -> ! {
    serial::puts("\n[reboot] resetting\n");

    // 1. Reset-control register. 0x02 selects "system reset" (vs. just CPU),
    //    0x0E requests a full hard reset.
    // SAFETY: `0xCF9` is the architectural reset-control port on every PC
    //   chipset since ICH; these two writes are its documented reset request.
    unsafe {
        port::outb(0xCF9, 0x02);
        port::outb(0xCF9, 0x0E);
    }
    spin(100_000);

    // 2. i8042 pulse. Drain the input buffer first so the command is accepted.
    // SAFETY: `0x64` is the architectural i8042 command port; `0xFE` is
    //   "pulse output line 0" (the RESET line).
    unsafe {
        for _ in 0..100_000 {
            if port::inb(0x64) & 0x02 == 0 {
                break;
            }
        }
        port::outb(0x64, 0xFE);
    }
    spin(100_000);

    // 3. Triple fault.
    serial::puts("[reboot] port resets did not take; triple-faulting\n");
    // SAFETY: intentionally loading an empty IDT and raising a breakpoint so
    //   the CPU cannot deliver or escalate the exception and resets. This is a
    //   terminal operation; nothing runs after it.
    unsafe {
        core::arch::asm!(
            "lidt [{idtr}]",
            "int3",
            idtr = in(reg) &raw const EMPTY_IDT,
            options(noreturn, nostack),
        );
    }
}

fn spin(iterations: u32) {
    for _ in 0..iterations {
        core::hint::spin_loop();
    }
}

/// `reboot(magic1, magic2, cmd, arg)` — x86_64 syscall 169.
///
/// `arg` (the `LINUX_REBOOT_CMD_RESTART2` string) is ignored: there is no
/// bootloader command to hand it to.
pub fn sys_reboot(magic1: u64, magic2: u64, cmd: u64, _arg: u64) -> u64 {
    match decode(magic1 as u32, magic2 as u32, cmd as u32) {
        Err(_) | Ok(None) => errno::EINVAL,
        Ok(Some(Action::Noop)) => 0,
        Ok(Some(Action::PowerOff)) => {
            // No ACPI PM block on this target, so an honest power-off is a
            // halt — same choice `akuma-boot::Action` documents for aarch64's
            // `CMD_HALT`.
            serial::puts("\n[reboot] power-off requested — no ACPI here; halting\n");
            crate::halt();
        }
        Ok(Some(Action::Restart)) => perform_reset(),
    }
}

/// Verify the decode and the syscall's rejection path without rebooting.
pub fn smoke_test(t: &mut Suite) {
    use akuma_boot::{
        CMD_CAD_ON, CMD_HALT, CMD_POWER_OFF, CMD_RESTART, MAGIC1, MAGIC2, MAGIC2A,
    };

    t.check(
        "reboot: musl magic + CMD_RESTART -> Restart",
        decode(MAGIC1, MAGIC2, CMD_RESTART) == Ok(Some(Action::Restart)),
    );
    t.check(
        "reboot: an alternate magic2 is accepted",
        decode(MAGIC1, MAGIC2A, CMD_POWER_OFF) == Ok(Some(Action::PowerOff)),
    );
    t.check("reboot: bad magic1 is rejected", decode(0, MAGIC2, CMD_RESTART).is_err());
    t.check(
        "reboot: CMD_HALT is a no-op action",
        decode(MAGIC1, MAGIC2, CMD_HALT) == Ok(Some(Action::Noop)),
    );
    t.check(
        "reboot: an unknown cmd with good magic is None",
        matches!(decode(MAGIC1, MAGIC2, 0xdead_beef), Ok(None)),
    );

    // The syscall wiring: bad magic must be EINVAL and must not reset.
    t.check_eq(
        "reboot: sys_reboot rejects bad magic with EINVAL",
        sys_reboot(0, 0, u64::from(CMD_RESTART), 0),
        errno::EINVAL,
    );
    // A no-op command is safe to actually dispatch.
    t.check_eq(
        "reboot: sys_reboot(CMD_CAD_ON) returns 0",
        sys_reboot(u64::from(MAGIC1), u64::from(MAGIC2), u64::from(CMD_CAD_ON), 0),
        0,
    );
}
