//! Intel HDA bring-up, step 1 (the runbook's "Discovery"): find the
//! controller, map BAR0, print GCAP, gate on the version register.
//!
//! The register decode is `akuma-hda`'s (host-tested, no MMIO); this module
//! is the metal half — the PCI walk, the BAR map and one console line:
//! `[HDA] 8086:8c20 bar0=0x… version=1.0 oss=N iss=N bss=N`.
//! Nothing else happens here in M1; the userspace seam still points at
//! virtio.

use crate::serial;
use akuma_hda::Regs16;
use core::sync::atomic::{AtomicBool, Ordering};

/// Intel's vendor id. The NVIDIA HDMI audio function is class 04:03 too, so
/// the walk matches class **and** vendor or it brings up the wrong chip.
const INTEL: u16 = 0x8086;

/// Step 1 outcome, for the M2 boot-suite check.
static DISCOVERED: AtomicBool = AtomicBool::new(false);

/// MMIO window over BAR0 — the `Regs16` the pure crate decodes through.
struct MmioRegs {
    base: *mut u8,
}

impl Regs16 for MmioRegs {
    fn r16(&self, offset: usize) -> u16 {
        // SAFETY: `base` is the BAR0 mapping from `pci::map_bar` (16 KiB;
        // this reads three halfwords at 0x00/0x02/0x04), volatile only.
        unsafe { (self.base.add(offset) as *const u16).read_volatile() }
    }
}

/// Whether discovery found and decoded the controller (M2's suite hook).
#[allow(dead_code)] // wired into the boot suite with the next milestone
pub fn discovered() -> bool {
    DISCOVERED.load(Ordering::Relaxed)
}

/// Walk the bus for the Intel HDA controller and, if found, map and print.
pub fn init() {
    // Census: every 04:03 function, so the console shows the NVIDIA HDMI
    // function was seen — and that skipping it was a choice, not blindness.
    crate::pci::for_each(|d| {
        if d.header.is_audio() {
            serial::puts("  [HDA] audio class ");
            serial::put_hexn(u64::from(d.header.vendor_id), 4);
            serial::puts(":");
            serial::put_hexn(u64::from(d.header.device_id), 4);
            serial::puts("\n");
        }
    });

    // `find_class` returns the first 04:03 in scan order — on this box the
    // Intel controller at 00:1b.0 walks before the GPU's HDMI at 01:00.1.
    // The census above is what proves that if it ever stops being true.
    let Some(dev) = crate::pci::find_class(
        akuma_pci::class::MULTIMEDIA,
        akuma_pci::subclass::AUDIO,
    ) else {
        serial::puts("[HDA] no audio-class function on the bus\n");
        return;
    };
    let addr = dev.addr;
    let (vid, did) = (dev.header.vendor_id, dev.header.device_id);
    if vid != INTEL {
        serial::puts("[HDA] first 04:03 is not Intel (HDMI) — walk order changed, fix me\n");
        return;
    }
    crate::pci::enable(addr, true);
    let Some(bar) = dev.bars.into_iter().next().flatten() else {
        serial::puts("[HDA] BAR0 absent\n");
        return;
    };
    let bar_addr = match bar {
        akuma_pci::Bar::Memory { address, .. } => address,
        akuma_pci::Bar::Io { .. } => {
            serial::puts("[HDA] BAR0 decoded as I/O ports — not an HDA MMIO BAR\n");
            return;
        }
    };
    let (size, _) = crate::pci::probe_bar_size(addr, 0);
    let Some(base) = crate::pci::map_bar(bar, size.max(0x4000)) else {
        serial::puts("[HDA] BAR0 map failed\n");
        return;
    };

    serial::puts("[HDA] ");
    serial::put_hexn(u64::from(vid), 4);
    serial::puts(":");
    serial::put_hexn(u64::from(did), 4);
    serial::puts(" bar0=0x");
    serial::put_hex(bar_addr);
    let regs = MmioRegs { base };
    if let Some(info) = akuma_hda::discover(&regs) {
        serial::puts(" version=");
        serial::put_dec(u64::from(info.vmaj));
        serial::puts(".");
        serial::put_dec(u64::from(info.vmin));
        serial::puts(" oss=");
        serial::put_dec(u64::from(info.gcap.output_streams()));
        serial::puts(" iss=");
        serial::put_dec(u64::from(info.gcap.input_streams()));
        serial::puts(" bss=");
        serial::put_dec(u64::from(info.gcap.bidirectional_streams()));
        serial::puts("\n");
        DISCOVERED.store(true, Ordering::Relaxed);
    } else {
        serial::puts(" version=0xffff — BAR0 not decoded; everything after is noise\n");
    }
}
