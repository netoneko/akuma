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

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use akuma_primitives::{OnceCopy, mmio::MmioWindow};

use crate::paging;
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

/// The watchdog's register page, once armed.
static WDT: OnceCopy<MmioWindow> = OnceCopy::new();
/// Armed and counting. Cleared by [`stop`], which leaves [`WDT`] set: it is a
/// mapping, not a state.
static WDT_ARMED: AtomicBool = AtomicBool::new(false);
/// The timeout armed with, for the `wdttest` message.
static TIMEOUT_S: AtomicU64 = AtomicU64::new(0);

/// Map the page holding `pa` as device memory and name it as a register window.
fn map(pa: u64) -> Option<MmioWindow> {
    paging::map_device_window(pa, 4096)
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
    if acpi.read::<u8>(PM + PM_ISACONTROL) & PM_ISACONTROL_MMIOEN == 0 {
        serial::puts("  wdt:  ACPI MMIO not decoded (ISACONTROL.MMIOEN = 0) — not armed\n");
        return;
    }
    acpi.write::<u8>(PM + PM_DECODEEN, acpi.read::<u8>(PM + PM_DECODEEN) | PM_DECODEEN_WDT_TMREN);
    let d3 = acpi.read::<u8>(PM + PM_DECODEEN3);
    acpi.write::<u8>(PM + PM_DECODEEN3, (d3 & !PM_DECODEEN3_WATCHDOG_DISABLE) | PM_DECODEEN3_RES_1S);

    let ctl = wdt.read::<u32>(WDT_CONTROL);
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
    wdt.write::<u32>(WDT_CONTROL, base);
    wdt.write::<u32>(WDT_COUNT, timeout);
    let base = base & !CTL_FIRED;
    wdt.write::<u32>(WDT_CONTROL, base | CTL_RUN);
    wdt.write::<u32>(WDT_CONTROL, base | CTL_RUN | CTL_TRIGGER);
    TIMEOUT_S.store(u64::from(timeout), Ordering::Relaxed);
    WDT.set(wdt);
    WDT_ARMED.store(true, Ordering::Release);
    serial::puts("  wdt:  AMD FCH watchdog armed, ");
    serial::put_dec(u64::from(timeout));
    serial::puts(" s, reset on expiry; CONTROL=0x");
    serial::put_hex(u64::from(wdt.read::<u32>(WDT_CONTROL)));
    serial::puts("\n");
}

/// Ticks between reloads: one second at the 100 Hz tick.
const TICKS_PER_PET: u64 = 100;

/// The one-second bucket the last reload happened in; `u64::MAX` = none yet.
static LAST_PET_BUCKET: AtomicU64 = AtomicU64::new(u64::MAX);

/// Whether a reload is due at `tick`, given the bucket the last one happened in.
///
/// Decided by *which second the count is in*, not by whether it lands on a
/// multiple of 100. `lapic::credit_shot_fired` / `settle_idle_shot` add the
/// ticks an idle long shot replaced in one jump (`idlesleep`), so the count can
/// step over every multiple of 100 — and with a steady idle wake pattern it
/// does so every second, forever, locked to one phase. `tick % 100 == 0` then
/// never fires, nothing reloads the count, and the watchdog resets a healthy,
/// merely idle machine after its timeout (60 s by default).
#[must_use]
const fn pet_due(last_bucket: u64, tick: u64) -> bool {
    tick / TICKS_PER_PET != last_bucket
}

/// Reload the count. From the BSP's timer tick (and from `settle_idle_shot`,
/// after it credits a long sleep); cheap enough to call every tick — it
/// touches the hardware at most once per second.
pub fn pet(tick: u64) {
    if !WDT_ARMED.load(Ordering::Relaxed) {
        return;
    }
    let Some(wdt) = WDT.get() else { return };
    let bucket = tick / TICKS_PER_PET;
    if !pet_due(LAST_PET_BUCKET.load(Ordering::Relaxed), tick) {
        return;
    }
    LAST_PET_BUCKET.store(bucket, Ordering::Relaxed);
    wdt.write::<u32>(WDT_CONTROL, wdt.read::<u32>(WDT_CONTROL) | CTL_TRIGGER);
}

/// Stop the countdown — before an orderly reset, so the watchdog cannot fire
/// during the firmware's POST or the next OS's boot.
pub fn stop() {
    if !WDT_ARMED.swap(false, Ordering::AcqRel) {
        return;
    }
    let Some(wdt) = WDT.get() else { return };
    wdt.write::<u32>(WDT_CONTROL, wdt.read::<u32>(WDT_CONTROL) & !CTL_RUN);
}

/// `wdttest`: wedge this core with interrupts off, which stops [`pet`], so the
/// only way out is the watchdog's reset. Says so first, on every output.
///
/// **Refuses unless the watchdog is armed**: wedging on a machine where arming
/// failed would leave it hung until someone power-cycles it, which is the
/// very thing this module exists to prevent. Returns, and the boot goes on.
pub fn wedge_for_test() {
    if !WDT_ARMED.load(Ordering::Acquire) {
        serial::puts("  wdt:  wdttest refused — the watchdog is not armed; booting normally\n");
        return;
    }
    serial::puts("  wdt:  wdttest — wedging with interrupts off; expect a reset in ");
    serial::put_dec(TIMEOUT_S.load(Ordering::Relaxed));
    serial::puts(" s\n");
    // Masking interrupts on this core is the point.
    akuma_cpu::daif::mask_irq();
    loop {
        core::hint::spin_loop();
    }
}
