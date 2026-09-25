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
    let mut regs = MmioRegs { base };

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

// Bring-up step 2 (runbook): controller reset per HDA 1.0a §4.3 — CRST is
// software-driven: write 0, poll for 0, write 1, poll for 1. Both polls
// are bounded so a hung controller cannot hang the boot; the post-reset
// version re-read separates a decode artifact from a controller answer.
// (M2 assumed CRST self-clears — wrong; see runbook Challenges.)
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
        serial::puts("[HDA] CRST handshake failed - controller not reset\n");
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
    codec_probe(&mut regs);
}

// ===================================================================
// M7: codec command engine — CORB/RIRB (HDA 1.0a §4.4/§4.5).
// Rings are static .bss DMA buffers, xhci dma_buf style; bus address
// via virt_to_phys (no IOMMU on this target). Offsets per Linux ICH6
// map: CORBWP 0x48, CORBRP 0x4A, CORBCTL 0x4C, CORBSIZE 0x4E;
// RIRB 0x50/0x54, RIRBWP 0x58, RINTCNT 0x5A, RIRBCTL 0x5C, SIZE 0x5E.
// ===================================================================
struct CorbRing([u32; 256]);
static mut CORB_RING: CorbRing = CorbRing([0; 256]);
struct RirbRing([u64; 256]);
static mut RIRB_RING: RirbRing = RirbRing([0; 256]);

fn codec_link_init(regs: &mut MmioRegs) {
    unsafe {
        let cb = akuma_primitives::addr::virt_to_phys(
            (&raw mut CORB_RING.0) as *mut [u32; 256] as usize,
        ) as u32;
        let rb = akuma_primitives::addr::virt_to_phys(
            (&raw mut RIRB_RING.0) as *mut [u64; 256] as usize,
        ) as u32;
        regs.w16(0x4C, 0x0000); // CORBCTL: stop while reprogramming
        regs.w16(0x4A, 0x8000); // CORBRP: read-pointer reset
        regs.w16(0x4A, 0x0000); // CORBRP: clear
        regs.w16(0x4E, 0x0002); // CORBSIZE: 256 entries (spec: program size before RUN)
        regs.w16(0x48, 0x0000); // CORBWP = 0
        regs.w16(0x40, (cb & 0xffff) as u16);
        regs.w16(0x42, (cb >> 16) as u16);
        regs.w16(0x44, 0x0000); // upper base = 0 (<4GB)
        regs.w16(0x4E, 0x0002); // CORBSIZE: 256 entries
        regs.w16(0x58, 0x8000); // RIRBWP: reset
        regs.w16(0x58, 0x0000);
        regs.w16(0x50, (rb & 0xffff) as u16);
        regs.w16(0x52, (rb >> 16) as u16);
        regs.w16(0x54, 0x0000);
        regs.w16(0x5A, 0x0001); // RINTCNT = 1
        regs.w16(0x5E, 0x0002); // RIRBSIZE: 256 entries
        regs.w16(0x5C, 0x0002); // RIRBCTL: DMA enable
        regs.w16(0x4C, 0x0002); // CORBCTL: RUN (bit1); bit0 is CORBRPRST pointer-reset - that bug held CORB in reset
        serial::puts("[HDA] postinit corbsize=");
        serial::put_hexn(regs.r16(0x4E) as u64, 4);
        serial::puts(" rirbsize=");
        serial::put_hexn(regs.r16(0x5E) as u64, 4);
        serial::puts(" rintcnt=");
        serial::put_hexn(regs.r16(0x5A) as u64, 4);
        serial::puts(" rirbctl=");
        serial::put_hexn(regs.r16(0x5C) as u64, 4);
        serial::puts(" corbsts=");
        serial::put_hexn(regs.r16(0x4D) as u64, 4);
        serial::puts("\n");
    }
    serial::puts("[HDA] CORB/RIRB enabled\n");
}

/// Send one verb (CAd<<28 | NID<<20 | verb<<8 | payload), poll RIRB.
fn codec_send(regs: &mut MmioRegs, verb: u32) -> Option<u32> {
    unsafe {
        let corb = (&raw mut CORB_RING.0).cast::<u32>();
        let rirb = (&raw mut RIRB_RING.0).cast::<u64>();
        let wp = (regs.r16(0x48) & 0x00ff) as usize;
        let np = (wp % 255) + 1;
        corb.add(np).write_volatile(verb);
        regs.w16(0x48, np as u16);
        let start = regs.r16(0x58) & 0x00ff;
        let mut n = 0;
        loop {
            let rwp = regs.r16(0x58) & 0x00ff;
            if rwp != start {
                let rp = (start as usize % 255) + 1;
                let resp = rirb.add(rp).read_volatile();
                return Some(resp as u32);
            }
            n += 1;
            if n > 2_000_000 {
                return None;
            }
        }
    }
}

/// First conversation with codec 0: vendor ID (0xF00) + root node count (0xF04).
fn codec_probe(regs: &mut MmioRegs) {
    codec_link_init(regs);
    match codec_send(regs, 0x000F_0000) {
        Some(v) => {
            serial::puts("[HDA] codec0 vendor=0x");
            serial::put_hexn(u64::from(v), 8);
        }
        None => serial::puts("[HDA] codec0: no RIRB response to vendor verb"),
    }
    match codec_send(regs, 0x000F_0004) {
        Some(v) => {
            serial::puts(" nodes=0x");
            serial::put_hexn(u64::from(v), 8);
        }
        None => serial::puts("[HDA] codec0: no RIRB response to node-count verb"),
    }
        codec_ici_probe(regs, 0x000F0000);
    serial::puts("\n");
}

/// Immediate command interface probe (M7d): no DMA, bypasses CORB/RIRB
/// entirely — bisects "rings broken" vs "link/codec broken". Also dumps
/// the command-path registers so the capture shows the stall point.
/// Map per Linux ICH6: ICW 0x60 (dword), IRR 0x64 (dword), IRS 0x68
/// (bit0 ICBUSY, bit1 IRV valid).
fn codec_ici_probe(regs: &mut MmioRegs, verb: u32) {
    let corbwp = regs.r16(0x48);
    let corbrp = regs.r16(0x4A);
    let corbctl = regs.r16(0x4C);
    let rirbwp = regs.r16(0x58);
    let rintcnt = regs.r16(0x5A);
    let rirbctl = regs.r16(0x5C);
    let rirbsts = regs.r16(0x5D);
    serial::puts("[HDA] cmdpath corbwp=");
    serial::put_hexn(corbwp as u64, 4);
    serial::puts(" corbrp=");
    serial::put_hexn(corbrp as u64, 4);
    serial::puts(" corbctl=");
    serial::put_hexn(corbctl as u64, 4);
    serial::puts(" rirbwp=");
    serial::put_hexn(rirbwp as u64, 4);
    serial::puts(" rintcnt=");
    serial::put_hexn(rintcnt as u64, 4);
    serial::puts(" rirbctl=");
    serial::put_hexn(rirbctl as u64, 4);
    serial::puts(" rirbsts=");
    serial::put_hexn(rirbsts as u64, 4);
    serial::puts("\n");
    regs.w16(0x60, (verb & 0xffff) as u16);
    regs.w16(0x62, (verb >> 16) as u16);
    regs.w16(0x68, 1);
    let mut tries: usize = 0;
    let mut s = regs.r16(0x68);
    while s & 1 != 0 && tries < 200000 {
        s = regs.r16(0x68);
        tries += 1;
    }
    if s & 1 != 0 {
        serial::puts("[HDA] ICI: timeout waiting for busy clear\n");
    } else if s & 2 != 0 {
        let lo = regs.r16(0x64) as u32;
        let hi = regs.r16(0x66) as u32;
        serial::puts("[HDA] ICI response: ");
        serial::put_hexn(((hi << 16) | lo) as u64, 8);
        serial::puts("\n");
    } else {
        serial::puts("[HDA] ICI: busy cleared, no result valid\n");
    }
}
