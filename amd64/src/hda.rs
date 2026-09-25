//! Intel HDA bring-up, step 1 (the runbook's "Discovery"): find the
//! controller, map BAR0, print GCAP, gate on the version register.
//!
//! The register decode is `akuma-hda`'s (host-tested, no MMIO); this module
//! is the metal half — the PCI walk, the BAR map and one console line:
//! `[HDA] 8086:8c20 bar0=0x… version=1.0 oss=N iss=N bss=N`.
//! Nothing else happens here in M1; the userspace seam still points at
//! virtio.

use crate::serial;
use akuma_hda::{Regs16, RegsW16};
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


impl RegsW16 for MmioRegs {
    fn w16(&self, offset: usize, value: u16) {
        // SAFETY: BAR0 mapping as above; the reset path writes GCTL only.
        unsafe { (self.base.add(offset) as *mut u16).write_volatile(value) }
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

    // Bring-up step 3 (runbook): power. The controller boots in D0
    // normally, but a warm reboot can leave D3hot, where MMIO reads
    // answer 0xff — exactly the version=255.0 shape seen in M2. Find
    // the PCI PM capability (id 0x01), read PMCSR, and if it is not
    // D0, write D0 back and wait out the 10 ms D3hot->D0 transition
    // before anyone touches BAR0.
    let cfg = crate::pci::config_space(addr);
    for cap in akuma_pci::capabilities(&cfg, dev.header.capabilities_pointer) {
        if cap.id == akuma_pci::capability_id::POWER_MANAGEMENT {
            let pmcsr = crate::pci::read_u16_config(addr, cap.offset + 4);
            serial::puts("[HDA] PMCSR=");
            serial::put_dec(u64::from(akuma_pci::pm::power_state(pmcsr)));
            // M4 taught: a forced D3hot round-trip on a D0 device wedges
            // it fully (every BAR0 byte then answers 0xff). Cycle only
            // when PMCSR actually says otherwise.
            if akuma_pci::pm::power_state(pmcsr) != akuma_pci::pm::D0 {
                crate::pci::write_u16_config(addr, cap.offset + 4,
                    pmcsr & !akuma_pci::pm::POWER_STATE_MASK);
                for _ in 0..20_000_000 { core::hint::spin_loop(); }
                serial::puts(" -> D0 written");
            }
            serial::puts("\n");
            break;
        }
    }
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

    // M4 diagnostic: the raw first 32 bytes of BAR0, before anything
    // else interprets them — which bytes answer 0xff is the whole
    // question when one register decodes and its neighbour does not.
    for line_off in [0x0usize, 0x10] {
        serial::puts("[HDA] dump");
        serial::put_hexn(line_off as u64, 2);
        serial::puts(":");
        for i in 0..8 {
            let w = regs.r16(line_off + i * 2);
            serial::puts(" ");
            serial::put_hexn(u64::from(w & 0xff), 2);
            serial::puts(" ");
            serial::put_hexn(u64::from(w >> 8), 2);
        }
        serial::puts("\n");
    }
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

    // Bring-up step 2 (runbook): controller reset. CRST self-clears; the
    // wait is a bounded spin, because the boot suite runs before the tick
    // and a hung poll must not hang the boot. The post-reset re-read of the
    // version is the M2 deliverable — it separates a decode artifact from a
    // controller answer.
    let spin = || {
        for _ in 0..2000 {
            core::hint::spin_loop();
        }
    };
    if akuma_hda::reset(&regs, 1 << 20, spin) {
        if let Some(post) = akuma_hda::discover(&regs) {
            serial::puts("[HDA] post-reset version=");
            serial::put_dec(u64::from(post.vmaj));
            serial::puts(".");
            serial::put_dec(u64::from(post.vmin));
            serial::puts("\n");
        }
    } else {
        serial::puts("[HDA] CRST did not self-clear - controller not reset\n");
    }
    serial::puts("[HDA] dump-post00:");
    for i in 0..8 {
        let w = regs.r16(i * 2);
        serial::puts(" ");
        serial::put_hexn(u64::from(w & 0xff), 2);
        serial::puts(" ");
        serial::put_hexn(u64::from(w >> 8), 2);
    }
    serial::puts("\n");
}
