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
#[cfg(not(feature = "no-tests"))]
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

    // Media barrier before anything else: the ext2 write-back cache out, then
    // the disk's own volatile cache (`SYNCHRONIZE CACHE`). A drive that has
    // only *acknowledged* our WRITE(10)s loses them in its own order when the
    // reset cuts power — which is how 2026-10-05's `reboot -f` lost renames
    // and half-persisted a freed inode (corruption doc §11). Failures print
    // and the reset proceeds anyway: a wedged disk must not wedge the reboot
    // itself.
    serial::puts("[reboot] flushing filesystems\n");
    // Per-mount failures print inside ext2's flush path where they can; here
    // one line suffices. A failed sync must not strand the reboot — the data
    // is e2fsck-recoverable, and a wedged disk must not wedge the reset.
    if akuma_vfs_glue::sync_all_filesystems().is_err() {
        serial::puts("[reboot] fs sync failed; disk may need e2fsck\n");
    }
    let _ = crate::xhci::flush();
    // NVMe: flush, normal shutdown, bus mastering off — see `nvme::shutdown`.
    crate::nvme::shutdown();

    // Before the reset, not after: a reset does not stop a bus-master device.
    // An xHCI controller still running here keeps writing its rings into this
    // kernel's `.bss` while the firmware POSTs and the loader unpacks the next
    // kernel into the very same memory — which is how a box ends up
    // crash-looping on a kernel that is itself fine. UEFI does not re-initialise
    // a controller the OS claimed, so nothing downstream undoes this for us.
    crate::xhci::shutdown();
    // The wifi card, if `rtw89wifi` kept it up: the same bus-master hazard.
    crate::rtw89::shutdown_for_reset();

    // 1. Reset-control register. 0x02 selects "system reset" (vs. just CPU),
    //    0x0E requests a full hard reset.
    // The hardware watchdog must not outlive this kernel: still counting, it
    // would reset the firmware's POST or the next OS's boot. Stopped last, so
    // a reset that hangs before this point is still caught by it.
    crate::watchdog::stop();
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

/// Who asked for the reset — `reboot-trace` feature, off by default.
///
/// A reset is silent: `Ok(Some(Action::Restart))` goes straight to `0xCF9`, the
/// in-memory `dmesg` ring dies with the machine, and the only evidence left is
/// the *next* kernel's "software wrote 0xE" line, which names no one. This
/// records the caller (pid / tgid / name / argv, then each ancestor up to
/// [`TRACE_DEPTH`]) to the console — so the ring and any serial capture have
/// it — and appends it to [`TRACE_PATH`] so it survives the reset.
///
/// Allocation: none of its own — the line is rendered into a stack buffer and
/// names are copied out under the `image` lock. `fs::write_file` allocates
/// inside the VFS; that is one write on a terminal, once-per-boot path and the
/// feature is a diagnostic build, not a default one. Best-effort: a failed
/// write is reported on the console and the reset proceeds regardless.
#[cfg(feature = "reboot-trace")]
mod trace {
    use crate::serial;

    pub const TRACE_PATH: &str = "/var/log/reboot-trace.log";
    const TRACE_DEPTH: usize = 6;
    const CAP: usize = 768;
    /// Longest `argv` rendering kept per process.
    const ARGV_MAX: usize = 96;

    struct Buf {
        b: [u8; CAP],
        n: usize,
    }

    impl Buf {
        fn push_bytes(&mut self, s: &[u8]) {
            let room = CAP - self.n;
            let k = s.len().min(room);
            self.b[self.n..self.n + k].copy_from_slice(&s[..k]);
            self.n += k;
        }
        fn push_str(&mut self, s: &str) {
            self.push_bytes(s.as_bytes());
        }
        fn push_dec(&mut self, mut v: u64) {
            let mut tmp = [0u8; 20];
            let mut i = tmp.len();
            loop {
                i -= 1;
                tmp[i] = b'0' + (v % 10) as u8;
                v /= 10;
                if v == 0 {
                    break;
                }
            }
            self.push_bytes(&tmp[i..]);
        }
        fn as_str(&self) -> &str {
            core::str::from_utf8(&self.b[..self.n]).unwrap_or("[reboot-trace] <non-utf8>\n")
        }
    }

    fn describe(buf: &mut Buf, p: &akuma_exec::process::Process) {
        buf.push_str(" pid=");
        buf.push_dec(u64::from(p.pid));
        buf.push_str(" tgid=");
        buf.push_dec(u64::from(p.tgid));
        buf.push_str(" ppid=");
        buf.push_dec(u64::from(p.parent_pid));
        let img = p.image.lock();
        buf.push_str(" name=");
        buf.push_str(&img.name);
        buf.push_str(" argv=");
        let start = buf.n;
        for (i, a) in img.args.iter().enumerate() {
            if i != 0 {
                buf.push_str(" ");
            }
            buf.push_str(a);
            if buf.n - start >= ARGV_MAX {
                buf.n = start + ARGV_MAX;
                buf.push_str("...");
                break;
            }
        }
    }

    /// `cmd` is the raw `reboot(2)` command word.
    pub fn record(cmd: u64) {
        let mut buf = Buf { b: [0; CAP], n: 0 };
        buf.push_str("[reboot-trace] cmd=");
        buf.push_dec(cmd);
        match crate::usermode::current_process() {
            None => buf.push_str(" caller=<kernel/no process>"),
            Some(me) => {
                buf.push_str(" caller:");
                describe(&mut buf, me);
                let mut ppid = me.parent_pid;
                for _ in 0..TRACE_DEPTH {
                    if ppid == 0 {
                        break;
                    }
                    let Some(par) = akuma_exec::process::lookup_process_shared(ppid) else {
                        break;
                    };
                    buf.push_str("\n[reboot-trace]   <-");
                    describe(&mut buf, par);
                    if par.parent_pid == ppid {
                        break;
                    }
                    ppid = par.parent_pid;
                }
            }
        }
        buf.push_str("\n");
        serial::puts(buf.as_str());

        // Append: size, then write at the end. A missing file is created.
        let res = match akuma_vfs_glue::fs::file_size(TRACE_PATH) {
            Ok(sz) => akuma_vfs_glue::fs::write_at(TRACE_PATH, sz as usize, buf.as_str().as_bytes())
                .map(|_| ()),
            Err(_) => akuma_vfs_glue::fs::write_file(TRACE_PATH, buf.as_str().as_bytes()),
        };
        if res.is_err() {
            serial::puts("[reboot-trace] could not write /var/log/reboot-trace.log\n");
        }
    }
}

/// `reboot(magic1, magic2, cmd, arg)` — x86_64 syscall 169.
///
/// `arg` (the `LINUX_REBOOT_CMD_RESTART2` string) is ignored: there is no
/// bootloader command to hand it to.
pub fn sys_reboot(magic1: u64, magic2: u64, cmd: u64, _arg: u64) -> u64 {
    let decoded = decode(magic1 as u32, magic2 as u32, cmd as u32);
    // Only a request that will actually act is worth a record: a rejected or
    // no-op call neither resets nor halts.
    #[cfg(feature = "reboot-trace")]
    if matches!(decoded, Ok(Some(Action::Restart | Action::PowerOff))) {
        trace::record(cmd);
    }
    match decoded {
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

#[cfg(not(feature = "no-tests"))]
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
