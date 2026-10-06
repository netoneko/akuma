//! The AMD FCH hardware watchdog — what turns a wedged kernel back into a
//! machine that boots, with nobody at the power button.
//!
//! # Why
//!
//! ryzen's reboot loop (`overlays/ryzen/`) is unattended: systemd-boot's
//! one-shot boots Akuma once, and *any* reset lands back in Pop. Akuma's own
//! `autoreboot` service provides that reset when userspace is healthy. A kernel
//! that wedges — a spin with interrupts off, a fault loop — never gets there,
//! and before this module that meant someone holding the power button. This
//! timer resets the chipset on its own if the kernel stops petting it.
//!
//! # The hardware
//!
//! The "EFCH" watchdog of AMD family 17h+ chipsets, the block Linux drives with
//! `sp5100_tco` (`drivers/watchdog/sp5100_tco.[ch]`, efch path). Measured on
//! ryzen 2026-10-06 with `overlays/ryzen/wdt-probe.py`: firmware leaves it
//! decoded-off (`DECODEEN.WDT_TMREN = 0`) and stopped, but **not** locked
//! (`DECODEEN3.WATCHDOG_DISABLE = 0`); Linux's driver then initialised it
//! ("heartbeat=60 sec") at `0xFEB00000`. That is exactly what [`init`] does:
//!
//! 1. `PM.DECODEEN |= WDT_TMREN` — decode the watchdog's MMIO at `0xFEB00000`;
//! 2. `PM.DECODEEN3`: clear `WATCHDOG_DISABLE`, resolution = 1 s;
//! 3. `CONTROL`: action = reset (bit 2 clear), `COUNT` = timeout, `RUN`, then
//!    `TRIGGER` to load the count.
//!
//! The PM registers are reached through the ACPI MMIO block at `0xFED80000`
//! (`ISACONTROL.MMIOEN`, set by firmware here).
//!
//! # Petting, stopping, testing
//!
//! [`pet`] runs from the BSP's timer interrupt, once a second. So the reset
//! fires when the BSP stops taking timer interrupts for the timeout — a spin
//! with interrupts off, a halted core, a fault loop — and not for a userspace
//! hang (that is `autoreboot`'s job). [`stop`] runs right before an orderly
//! reset: a watchdog still counting through the firmware and into Pop's boot
//! would reset Pop. `wdttest` on the command line deliberately wedges the
//! kernel with interrupts off ([`wedge_for_test`]) to prove the reset fires;
//! after it, the `CONTROL.FIRED` bit tells Pop (or the next Akuma boot) that
//! the last reset was this watchdog.
//!
//! Opt-in (`wdt` / `wdt=<seconds>` on the command line) and gated on the exact
//! chipset (`1022:790b`, revision ≥ `0x51`, the test Linux uses for "EFCH"):
//! the same registers on an older or Intel chipset mean something else.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::paging::{self, MemAttr, PteProt};
use crate::phys::DEVMAP_BASE;
use crate::serial;

/// The ACPI MMIO block; the PM registers are at +0x300.
const ACPI_MMIO: u64 = 0xFED8_0000;
const PM: usize = 0x300;
const PM_DECODEEN: usize = 0x00;
const PM_DECODEEN_WDT_TMREN: u8 = 1 << 7;
const PM_DECODEEN3: usize = 0x03;
const PM_DECODEEN3_WATCHDOG_DISABLE: u8 = 0b11 << 2;
const PM_DECODEEN3_RES_1S: u8 = 0b11;
const PM_ISACONTROL: usize = 0x04;
const PM_ISACONTROL_MMIOEN: u8 = 1 << 1;

/// Where `WDT_TMREN` decodes the watchdog (the address Linux reported).
const WDT_MMIO: u64 = 0xFEB0_0000;
const WDT_CONTROL: usize = 0x00;
const WDT_COUNT: usize = 0x04;
const CTL_RUN: u32 = 1 << 0;
const CTL_FIRED: u32 = 1 << 1;
/// Set = power off on expiry; clear = reset. Always cleared here.
const CTL_ACTION_POWEROFF: u32 = 1 << 2;
const CTL_DISABLED: u32 = 1 << 3;
const CTL_TRIGGER: u32 = 1 << 7;

/// The FCH SMBus function that identifies the chipset (`00:14.0`).
const FCH_VENDOR: u16 = 0x1022;
const FCH_SMBUS: u16 = 0x790B;
const EFCH_MIN_REVISION: u8 = 0x51;

const DEFAULT_TIMEOUT_S: u32 = 60;

/// The watchdog's mapped base once armed; 0 = not armed.
static WDT_VA: AtomicU64 = AtomicU64::new(0);
/// The timeout armed with, for the `wdttest` message.
static TIMEOUT_S: AtomicU64 = AtomicU64::new(0);

fn map(pa: u64) -> Option<usize> {
    let va = DEVMAP_BASE + pa;
    paging::map_page(va as usize, pa, PteProt::KERNEL_RW, MemAttr::Device).then_some(va as usize)
}

fn r8(va: usize) -> u8 {
    // SAFETY: `va` is a byte register in the ACPI MMIO page `map` mapped as
    // device memory; reads have no side effects on these registers.
    unsafe { (va as *const u8).read_volatile() }
}

fn w8(va: usize, v: u8) {
    // SAFETY: as `r8`; only the PM decode/config bytes named above are written.
    unsafe { (va as *mut u8).write_volatile(v) }
}

fn r32(va: usize) -> u32 {
    // SAFETY: `va` is the watchdog's CONTROL or COUNT register, in a page
    // `map` mapped as device memory.
    unsafe { (va as *const u32).read_volatile() }
}

fn w32(va: usize, v: u32) {
    // SAFETY: as `r32`.
    unsafe { (va as *mut u32).write_volatile(v) }
}

/// The timeout `wdt` / `wdt=<s>` asks for, or `None` if the watchdog was not
/// requested.
fn requested(cmdline: &str) -> Option<u32> {
    cmdline.split_ascii_whitespace().find_map(|t| match t {
        "wdt" | "wdttest" => Some(DEFAULT_TIMEOUT_S),
        _ => t.strip_prefix("wdt=").and_then(|s| s.parse::<u32>().ok()).map(|s| s.clamp(10, 0xffff)),
    })
}

/// Arm the watchdog if the command line asks for it and this is the chipset it
/// was written for. Needs the page tables (after `mem::init`) and the PCI scan.
pub fn init(cmdline: &str) {
    let Some(timeout) = requested(cmdline) else { return };
    let mut efch = false;
    crate::pci::for_each(|d| {
        if d.header.vendor_id == FCH_VENDOR && d.header.device_id == FCH_SMBUS && d.header.revision >= EFCH_MIN_REVISION {
            efch = true;
        }
    });
    if !efch {
        serial::puts("  wdt:  no AMD EFCH (1022:790b rev >= 0x51) — not armed\n");
        return;
    }
    let (Some(acpi), Some(wdt)) = (map(ACPI_MMIO), map(WDT_MMIO)) else {
        serial::puts("  wdt:  could not map the FCH registers — not armed\n");
        return;
    };
    let pm = acpi + PM;
    if r8(pm + PM_ISACONTROL) & PM_ISACONTROL_MMIOEN == 0 {
        serial::puts("  wdt:  ACPI MMIO not decoded (ISACONTROL.MMIOEN = 0) — not armed\n");
        return;
    }
    w8(pm + PM_DECODEEN, r8(pm + PM_DECODEEN) | PM_DECODEEN_WDT_TMREN);
    let d3 = r8(pm + PM_DECODEEN3);
    w8(pm + PM_DECODEEN3, (d3 & !PM_DECODEEN3_WATCHDOG_DISABLE) | PM_DECODEEN3_RES_1S);

    let ctl = r32(wdt + WDT_CONTROL);
    if ctl == u32::MAX {
        serial::puts("  wdt:  watchdog MMIO reads all-ones — not armed\n");
        return;
    }
    if ctl & CTL_DISABLED != 0 {
        serial::puts("  wdt:  disabled by firmware (CONTROL.DISABLED) — not armed\n");
        return;
    }
    if ctl & CTL_FIRED != 0 {
        serial::puts("  wdt:  the previous reset was this watchdog (CONTROL.FIRED)\n");
    }
    // Stopped, action = reset, FIRED cleared (write-1-to-clear).
    let base = (ctl & !(CTL_RUN | CTL_ACTION_POWEROFF | CTL_TRIGGER)) | CTL_FIRED;
    w32(wdt + WDT_CONTROL, base);
    w32(wdt + WDT_COUNT, timeout);
    let base = base & !CTL_FIRED;
    w32(wdt + WDT_CONTROL, base | CTL_RUN);
    w32(wdt + WDT_CONTROL, base | CTL_RUN | CTL_TRIGGER);
    TIMEOUT_S.store(u64::from(timeout), Ordering::Relaxed);
    WDT_VA.store(wdt as u64, Ordering::Release);
    serial::puts("  wdt:  AMD FCH watchdog armed, ");
    serial::put_dec(u64::from(timeout));
    serial::puts(" s, reset on expiry; CONTROL=0x");
    serial::put_hex(u64::from(r32(wdt + WDT_CONTROL)));
    serial::puts("\n");
}

/// Reload the count. From the BSP's timer tick; cheap enough to call every
/// tick, but `tick` is passed so it can run once a second.
pub fn pet(tick: u64) {
    let va = WDT_VA.load(Ordering::Relaxed);
    if va == 0 || !tick.is_multiple_of(100) {
        return;
    }
    let va = va as usize;
    w32(va + WDT_CONTROL, r32(va + WDT_CONTROL) | CTL_TRIGGER);
}

/// Stop the countdown — before an orderly reset, so the watchdog cannot fire
/// during the firmware's POST or the next OS's boot.
pub fn stop() {
    let va = WDT_VA.swap(0, Ordering::AcqRel);
    if va == 0 {
        return;
    }
    let va = va as usize;
    w32(va + WDT_CONTROL, r32(va + WDT_CONTROL) & !CTL_RUN);
}

/// `wdttest`: wedge this core with interrupts off, which stops [`pet`], so the
/// only way out is the watchdog's reset. Says so first, on every output.
///
/// **Refuses unless the watchdog is armed**: wedging on a machine where arming
/// failed would leave it hung until someone power-cycles it, which is the
/// very thing this module exists to prevent. Returns, and the boot goes on.
pub fn wedge_for_test() {
    if WDT_VA.load(Ordering::Acquire) == 0 {
        serial::puts("  wdt:  wdttest refused — the watchdog is not armed; booting normally\n");
        return;
    }
    serial::puts("  wdt:  wdttest — wedging with interrupts off; expect a reset in ");
    serial::put_dec(TIMEOUT_S.load(Ordering::Relaxed));
    serial::puts(" s\n");
    // SAFETY: `cli` only masks interrupts on this core; that is the point.
    unsafe { core::arch::asm!("cli", options(nomem, nostack)) };
    loop {
        core::hint::spin_loop();
    }
}
