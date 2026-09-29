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
    fn w32(&self, offset: usize, value: u32) {
        unsafe { (self.base.add(offset) as *mut u32).write_volatile(value) }
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
static mut RPOS: usize = 0;

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
        regs.w32(0x44, 0x00000000); // CORBUBASE: full 32-bit write (16-bit halves don't stick!)
        regs.w16(0x4E, 0x0002); // CORBSIZE: 256 entries
        regs.w16(0x58, 0x8000); // RIRBWP: reset
        regs.w16(0x58, 0x0000);
        regs.w16(0x50, (rb & 0xffff) as u16);
        regs.w16(0x52, (rb >> 16) as u16);
        regs.w32(0x54, 0x00000000); // RIRBUBASE: full 32-bit write
        regs.w16(0x5A, 0x0001); // RINTCNT = 1
        regs.w16(0x5E, 0x0002); // RIRBSIZE: 256 entries
        regs.w16(0x5C, 0x0003); // RIRBCTL: RIRBDMAEN(bit0) + RINTCTL(bit1); 0x0002 alone left DMA off - no response could ever land
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
        serial::puts(" corbbase=");
        serial::put_hexn(cb as u64, 8);
        serial::puts(" rirbbase=");
        serial::put_hexn(rb as u64, 8);
        serial::puts("\n");
        serial::puts("\n");
    }
    serial::puts("[HDA] CORB/RIRB enabled\n");
}

/// Send one verb (CAd<<28 | NID<<20 | verb<<8 | payload), poll RIRB.
fn codec_send(regs: &mut MmioRegs, verb: u32) -> Option<u32> {
    unsafe {
        let corb = (&raw mut CORB_RING.0).cast::<u32>();
        let rirb = (&raw mut RIRB_RING.0).cast::<u64>();
        // The RIRB read pointer is bytes 0-1 of the 4-byte RIRBWP; we track
        // how many entries we've consumed (incl. unsolicited) in RPOS.
        let old_rp = RPOS;
        let wp = (regs.r16(0x48) & 0x00ff) as usize;
        let np = (wp % 255) + 1;
        corb.add(np).write_volatile(verb);
        regs.w16(0x48, np as u16); // publish; controller bumps CORBRP and DMAs the verb
        let mut dbg = 0u32;
        let _ = &mut dbg;
        let mut n = 0u32;
        loop {
            let rwp = (regs.r16(0x58) & 0x00ff) as usize; // RIRBWP: hw write pos
            if rwp != old_rp {
                // consume every new entry in order; ours is the last one
                let mut k = (old_rp + 1) % 256;
                loop {
                    let e = rirb.add(k).read_volatile();
                    let resp = e as u32;
                    let ex = (e >> 32) as u32;
                    let cad = (ex >> 28) & 0x0f; // EX dword bits [31:28] (was [3:0] - wrong!)
                    let unsol = (ex >> 4) & 0x01; // EX bit [4]: unsolicited response
                    if unsol == 1 {
                        // stray/unsolicited entry: skip it, it is NOT our answer
                        // (this off-by-one zeroed every readback after the first!)
                        if k == rwp { RPOS = rwp; return None; }
                        k = (k + 1) % 256;
                        continue;
                    }
                    if k == rwp && cad == (verb >> 28) & 0x0f {
                        RPOS = rwp;
                        return Some(resp);
                    }
                    if k == rwp {
                        // reached hw WP without a matching CAd - treat as no answer
                        RPOS = rwp;
                        return None;
                    }
                    k = (k + 1) % 256;
                }
            }
            n += 1;
            if n > 50_000_000 {
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
            unsafe {
                serial::puts(" corbrp-after=0x");
                serial::put_hexn(regs.r16(0x4A) as u64, 4);
                serial::puts(" rirbwp-after=0x");
                serial::put_hexn(regs.r16(0x58) as u64, 4);
            }
        }
        None => serial::puts("[HDA] codec0: no RIRB response to vendor verb"),
    }
    match codec_send(regs, 0x000F_0400) {
        Some(v) => {
            serial::puts(" nodes=0x");
            serial::put_hexn(u64::from(v), 8);
        }
        None => serial::puts("[HDA] codec0: no RIRB response to node-count verb"),
    }
        codec_ici_probe(regs, 0x000F0000);
        hda_scan2(regs);
        codec_ici_probe(regs, 0x000F0400);
        codec_scan_widgets(regs); // M7m: node-count via proven fn - emulator verb-coverage test
        codec_ring_dump(regs);
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
    serial::puts(" rirblbase=");
    serial::put_hexn(((regs.r16(0x52) as u64) << 16) | (regs.r16(0x50) as u64), 8);
    serial::puts(" corblbase=");
    serial::put_hexn(((regs.r16(0x42) as u64) << 16) | (regs.r16(0x40) as u64), 8);
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

// M7g probe: discriminate response-writeback vs verb-fetch vs late-delivery.
// 1) dump ring memory (are our verbs in the DRAM the controller reads? did ANY
//    response land in RIRB memory without RIRBWP moving?), 2) late re-poll of
//    rirbwp/rirbsts after ~100ms, 3) CAd1 verb: unresponsive codec - does the
//    controller auto-generate a no-response entry?
fn codec_ring_dump(regs: &mut MmioRegs) {
    unsafe {
        serial::puts("[HDA] ringdump corb[1..3]:");
        let corb = (&raw mut CORB_RING).cast::<u32>();
        let mut i = 1usize;
        while i < 4 {
            serial::puts(" ");
            serial::put_hexn(corb.add(i).read_volatile() as u64, 8);
            i += 1;
        }
        serial::puts("\n[HDA] ringdump rirb[0..2] hi:lo:");
        let rirb = (&raw mut RIRB_RING).cast::<u64>();
        let mut j = 0usize;
        while j < 2 {
            let e = rirb.add(j).read_volatile();
            serial::puts(" ");
            serial::put_hexn(e >> 32, 8);
            serial::puts(":");
            serial::put_hexn(e & 0xFFFF_FFFF, 8);
            j += 1;
        }
        serial::puts("\n");
        let mut n = 0usize;
        while n < 20_000_000 { core::hint::spin_loop(); n += 1; }
        serial::puts("[HDA] late rirbwp=");
        serial::put_hexn(regs.r16(0x58) as u64, 4);
        serial::puts(" rirbsts=");
        serial::put_hexn(regs.r16(0x5D) as u64, 4);
        serial::puts("\n");
        match codec_send(regs, 0x100F_0000) {
            Some(r) => {
                serial::puts("[HDA] cad1 response: ");
                serial::put_hexn(r as u64, 8);
                serial::puts("\n[HDA] after-cad1 rirbwp=");
            }
            None => serial::puts("[HDA] cad1: no RIRB response\n[HDA] after-cad1 rirbwp="),
        }
        serial::put_hexn(regs.r16(0x58) as u64, 4);
        serial::puts("\n");
    }
}

/// M7i: ICI-backed command path — the immediate command interface answers
/// synchronously on this silicon (two independent 10ec0662 confirmations),
/// while emulated RIRB delivery latency exceeds every poll budget tried
/// (2M, 50M). CORB/RIRB engine stays for the production DMA pass; verbs
/// route through ICI until then. Layout: ICW 0x60, IRR 0x64, IRS 0x68
/// (bit0 BUSY, bit1 VALID).
fn codec_send_ici(regs: &mut MmioRegs, verb: u32) -> Option<u32> {
    // QEMU's intel-hda never executes ICI verbs (ICW ignored, IRR reads 0);
    // real silicon does. CORB/RIRB works on both - route everything there.
    codec_send(regs, verb)
}

// M8: brute widget/pin scan — emulator answers verbs but 0xF04 says no subnodes,
// so walk nid range directly and look for anything that answers nonzero.
fn codec_scan_widgets(regs: &mut MmioRegs) {
    serial::puts("[HDA] M8 scan: pin caps (0xF0C) nid 2..0x20\n");
    let mut nid = 2u32;
    while nid <= 0x20 {
        let verb = (nid << 20) | (0xF0C << 8);
        if let Some(v) = codec_read_ici(regs, verb) {
            if v != 0 {
                serial::puts("[HDA] M8 nid 0x");
                serial::put_hexn(nid as u64, 2);
                serial::puts(" pin_caps=0x");
                serial::put_hexn(v as u64, 8);
                serial::puts("\n");
                let cfg = codec_read_ici(regs, (nid << 20) | (0xF1C << 8));
                if let Some(c) = cfg {
                    serial::puts("[HDA] M8 nid 0x");
                    serial::put_hexn(nid as u64, 2);
                    serial::puts(" cfg_default=0x");
                    serial::put_hexn(c as u64, 8);
                    serial::puts("\n");
                }
            }
        }
        nid += 1;
    }
    serial::puts("[HDA] M8 scan done\n");
    unsafe { codec_raw_scan(regs); }
    m9_beep(regs);
    m9a5_bcis(regs);
        m8_raw_scan(regs);
}

fn codec_read_ici(regs: &mut MmioRegs, verb: u32) -> Option<u32> {
    codec_send(regs, verb)
}
fn hda_scan2(regs: &mut MmioRegs) {
    serial::puts("[HDA] M8raw: pin caps 0xF0C raw, nid 2..0x20\n");
    let mut nid = 2u32;
    while nid <= 0x20 {
        let verb = (nid << 20) | 0x000F_0C00;
        let r = codec_read_ici(regs, verb);
        serial::puts("[HDA] M8raw nid=");
        serial::put_hexn(nid as u64, 2);
        serial::puts(" resp=");
        match r {
            Some(v) => serial::put_hexn(v as u64, 8),
            None => serial::puts("none"),
        }
        serial::puts("\n");
        nid += 1;
    }
    serial::puts("[HDA] M8raw done\n");
}

// M8b: raw unfiltered dump - every nid's responses, no zeros filtered out.
fn m8_raw_scan(regs: &mut MmioRegs) {
    serial::puts("[HDA] M8b raw scan nid 1..0x1f (0xF0C | 0xF1C)\n");
    let mut nid = 1u32;
    while nid <= 0x1f {
        let caps = codec_read_ici(regs, nid << 20 | 0xF0C00);
        let cfg = codec_read_ici(regs, nid << 20 | 0xF1C00);
        serial::puts("[HDA] nid ");
        serial::put_hexn(nid as u64, 2);
        serial::puts(" caps=");
        match caps {
            Some(v) => serial::put_hexn(v as u64, 8),
            None => serial::puts("none"),
        }
        serial::puts(" cfg=");
        match cfg {
            Some(v) => serial::put_hexn(v as u64, 8),
            None => serial::puts("none"),
        }
        serial::puts("\n");
        nid += 1;
    }
    serial::puts("[HDA] M8b raw scan done\n");
}
// M8b: raw scan, no filter - print every nid response to find what the emulator actually implements
unsafe fn codec_raw_scan(regs: &mut MmioRegs) {
    serial::puts("[HDA] M8b raw scan nid 1..0x20: caps(0xF0C)= type(0xF08)= cfg(0xF1C)=\n");
    let mut nid = 1usize;
    while nid <= 0x20 {
        let verb = 0x000F_0C00 | ((nid as u32) << 20);
        match codec_read_ici(regs, verb) {
            Some(v) => {
                serial::puts("    nid ");
                serial::put_hexn(nid as u64, 2);
                serial::puts(": caps=");
                serial::put_hexn(v as u64, 8);
                let vt = 0x000F_0800 | ((nid as u32) << 20);
                let t = codec_read_ici(regs, vt).unwrap_or(0xDEAD_BEEF);
                serial::puts(" type=");
                serial::put_hexn(t as u64, 8);
                let vc = 0x000F_1C00 | ((nid as u32) << 20);
                let c = codec_read_ici(regs, vc).unwrap_or(0xDEAD_BEEF);
                serial::puts(" cfg=");
                serial::put_hexn(c as u64, 8);
                serial::puts("\n");
            }
            None => {
                serial::puts("    nid ");
                serial::put_hexn(nid as u64, 2);
                serial::puts(": None\n");
            }
        }
        nid += 1;
    }
    serial::puts("[HDA] M8b raw scan done\n");
}

// ---- M9a: hardcoded ALC662 + stream DMA beep (root block 36078) ----
static mut BEEP_BUF: [u8; 192000] = [0; 192000]; // 1s of 48k/16-bit/stereo
#[repr(align(128))]
struct BeepBdl([u64; 4]); // 2 BDL entries x 16 bytes; only entry 0 used
static mut BEEP_BDL: BeepBdl = BeepBdl([0; 4]);

pub fn m9_beep(regs: &mut MmioRegs) {
    unsafe { HDA_BASE = regs.base as usize; } // capture base for /dev/dsp backend
    unsafe {
        // 440Hz square, 48k frames/s stereo; half-period in frames
        let half = 54444u32 / 440; // ~123 frames? no: 48000/440/2 = 54
        let halff = 48000u32 / 440 / 2;
        let mut i = 0usize;
        while i < 192000 {
            let frame = (i / 4) as u32;
            let s: i16 = if frame % halff < halff / 2 { 8000 } else { -8000 };
            let b = s.to_le_bytes();
            BEEP_BUF[i] = b[0];
            BEEP_BUF[i + 1] = b[1];
            BEEP_BUF[i + 2] = b[0];
            BEEP_BUF[i + 3] = b[1];
            i += 4;
        }
        let _ = half;
        let buf_phys = akuma_primitives::addr::virt_to_phys((&raw mut BEEP_BUF) as usize) as u64;
        let bdl_phys = akuma_primitives::addr::virt_to_phys((&raw mut BEEP_BDL) as usize) as u64;
        // BDL entry 0 (16 bytes = 2 u64): [addr_lo|addr_hi, len|IOC<<63]
        BEEP_BDL.0[0] = buf_phys;
        BEEP_BDL.0[1] = 192000u64;
        // M9c: hardcoded ALC662 verbs — canonical (nid<<20)|(verb<<8)|payload.
        // Old block was nibble-short (nid field = 0) + a GET instead of SET +
        // amp payload had the MUTE bit set: codec sat factory-muted with
        // stream tag 0, i.e. guaranteed silence on real silicon.
        codec_send_ici(regs, 0x02202011); // SET_CONV_FMT nid2: 48k/16/stereo
        codec_send_ici(regs, 0x02270610); // SET_CONV_STREAM nid2: stream=1 ch=0 (nid was 0!)
        // Amp verbs: V=0xB, payload = (in?0x4000)|(L?0x2000)|(R?0x1000)|(idx<<8)|(mute0x80|gain).
        // alsa-info ground truth: DAC 0x02 amp nsteps=0x57, Ubuntu audible at 0x38;
        // gain 0 on any amp = MAX attenuation (silent). Mixer 0x0c input amp feeds
        // HP pin 0x1b at connection index 0 (conn list 0x0c* 0x0d 0x0e).
        codec_send_ici(regs, 0x0023b138); // SET_AMP nid2 out: L+R unmute, gain 0x38 (Ubuntu level)
        codec_send_ici(regs, 0x00c3b138); // SET_AMP nid0c in(idx0): L+R unmute, gain 0x38 (mixer stage)
        // Pin ctrl: OUT+HP enable (0xC0) | VREF 0x3 → 0xC3, plus EAPD BTLR=0x3
        // (SET_EAPD 0x70C) on both pins — ALC boards keep the jack amp
        // tri-stated without EAPD, even when everything else reads correct.
        codec_send_ici(regs, 0x01b707c3); // SET_PIN_CTRL nid1b: 0xC3
        codec_send_ici(regs, 0x014707c3); // SET_PIN_CTRL nid14: 0xC3 (line-out companion)
        codec_send_ici(regs, 0x01b70c03); // SET_EAPD nid1b: BTLR=0x3
        codec_send_ici(regs, 0x01470c03); // SET_EAPD nid14: BTLR=0x3
        let gf = codec_send_ici(regs, 0x002a0000); // GET_CONV_FMT nid2 (12-bit verb 0xA)
        serial::puts("[HDA] M9c GET_CONV_FMT nid2=0x"); serial::put_hexn(gf.unwrap_or(0) as u64, 4); serial::puts("\n");
        let gs2 = codec_send_ici(regs, 0x002f0600); // re-GET stream tag AFTER SD0 setup (proves bind stuck)
        serial::puts("[HDA] M9c GET_STREAM nid2 late=0x"); serial::put_hexn(gs2.unwrap_or(0) as u64, 4); serial::puts("\n");
        // Jack detection: plug is in nid 0x1b (cfg 0221401f: HP-out, 3.5mm, present).
        // (alsa-info: 0x1b actually hangs off mixer 0x0c, conn idx 0 - the old 0x0e route was wrong).
        // GET readbacks: the boot log itself proves the state took (spec 7.3.3).
        let ga = codec_send_ici(regs, 0x002b0100); // GET_AMP nid2 out
        serial::puts("[HDA] M9c GET_AMP nid2 out=0x"); serial::put_hexn(ga.unwrap_or(0) as u64, 2); serial::puts("\n");
        let gs = codec_send_ici(regs, 0x002f0600); // GET_CONV_STREAM nid2
        serial::puts("[HDA] M9c GET_STREAM nid2=0x"); serial::put_hexn(gs.unwrap_or(0) as u64, 4); serial::puts("\n");
        let gp = codec_send_ici(regs, 0x01bf0700); // GET_PIN_CTRL nid1b (expect 0xc3)
        serial::puts("[HDA] M9c GET_PINCTRL nid1b=0x"); serial::put_hexn(gp.unwrap_or(0) as u64, 2); serial::puts("\n");
        // Pin 0x1b has NO output amp (ALC662 jack pins are ampless) — readback 0x00 is
        // correct and NOT the problem; the mute chain is DAC(0x02)+mixer(0x0c), both set.
        let gi = codec_send_ici(regs, 0x01b3b000); // GET_AMP nid1b out (V=0xB nid1b)
        serial::puts("[HDA] M9c GET_AMP nid1b out=0x"); serial::put_hexn(gi.unwrap_or(0) as u64, 2); serial::puts("\n");
        let gc = codec_send_ici(regs, 0x01bf50c0); // GET_CFG_DEFAULT nid1b (jack presence!)
        serial::puts("[HDA] M9c GET_CFG nid1b=0x"); serial::put_hexn(gc.unwrap_or(0) as u64, 8); serial::puts("\n");
        let gx = codec_send_ici(regs, 0x01bf0c00); // GET_CONNECT_SEL nid1b: which mixer input feeds the pin
        serial::puts("[HDA] M9c GET_CONNSEL nid1b=0x"); serial::put_hexn(gx.unwrap_or(0) as u64, 2); serial::puts("\n");
        serial::puts("[HDA] M9: verbs sent, SD0 setup\n");
        // SDI0 @0x100: stop+reset stream first
        regs.w16(0x100, 0);
        regs.w16(0x102, 0x2000); // SRST=1 (bit13 high half)
        for _ in 0..1000 { core::hint::spin_loop(); }
        regs.w16(0x102, 0x0010); // STRM=1 (bits 23:20 high half), SRST=0
        // CBL = 192000 = BDL entry len (was 176400, mismatch → DESE)
        regs.w16(0x108, 0xee00);
        regs.w16(0x10a, 0x0002);
        regs.w16(0x110, 0x0000); // LVI = entries-1 = 0 (0x10C was CBL hi!)
        regs.w16(0x118, (bdl_phys & 0xffff) as u16);
        regs.w16(0x11a, ((bdl_phys >> 16) & 0xffff) as u16);
        regs.w16(0x11c, ((bdl_phys >> 32) & 0xffff) as u16);
        regs.w16(0x11e, 0);
        let lp0 = regs.r16(0x104);
        regs.w16(0x114, 0x4011); // SDFMT @0x114: 44.1k/16-bit/stereo (0x112 = RO FIFOW!)
        regs.w16(0x100, 0x12); // CTL: STRM=1 (bits7:4) | RUN (was 2 = stream 0!)
        serial::puts("[HDA] M9a4 lvi@10C="); serial::put_hexn(regs.r16(0x10C) as u64, 4); serial::puts(" fifow@10E="); serial::put_hexn(regs.r16(0x10E) as u64, 4); serial::puts(" fmt@112="); serial::put_hexn(regs.r16(0x112) as u64, 4); serial::puts("\n");
        serial::puts("[HDA] SD0 dump: ctl=0x");
        serial::put_hexn(regs.r16(0x100) as u64, 4);
        serial::puts(" sts=0x");
        serial::put_hexn(regs.r16(0x102) as u64, 4);
        serial::puts(" cbl=0x");
        serial::put_hexn((regs.r16(0x108) as u64) | ((regs.r16(0x10A) as u64) << 16), 8);
        serial::puts(" lvi=0x");
        serial::put_hexn(regs.r16(0x110) as u64, 4);
        serial::puts(" bdl=0x");
        serial::put_hexn((regs.r16(0x118) as u64) | ((regs.r16(0x11A) as u64) << 16), 8);
        serial::puts(" lpib=0x");
        serial::put_hexn(regs.r16(0x104) as u64, 4);
        serial::puts("\n");
        for _ in 0..4_000_000 { core::hint::spin_loop(); }
        let lp1 = regs.r16(0x104);
        serial::puts("[HDA] M9 LPIB: 0x");
        serial::put_hexn(lp0 as u64, 4);
        serial::puts(" -> 0x");
        serial::put_hexn(lp1 as u64, 4);
        serial::puts("\n");
        if lp1 != lp0 { serial::puts("[HDA] M9: DMA ALIVE - stream running\n"); }
        else { serial::puts("[HDA] M9a4 sts-after="); serial::put_hexn(regs.r16(0x102) as u64, 4); serial::puts("\n");
        serial::puts("[HDA] M9: LPIB frozen\n"); }
    }
}

// M9a5: BCIS counting probe - proves CONTINUOUS dma playback (not one completion)
fn m9a5_bcis(regs: &mut MmioRegs) {
    let mut n: u32 = 0;
    let mut i: u32 = 0;
    while i < 10 {
        let mut d: u32 = 0;
        while d < 8000000 { d += 1; }
        let sts = regs.r16(0x103);
        if sts & 0x0004 != 0 {
            n += 1;
            regs.w16(0x103, 0x0004); // w1c BCIS
        }
        serial::puts("[HDA] M9a5 t=");
        serial::put_hexn(i as u64, 1);
        serial::puts(" sts=");
        serial::put_hexn(sts as u64, 4);
        serial::puts(" lpib=");
        serial::put_hexn(((regs.r16(0x104) as u64) | ((regs.r16(0x106) as u64) << 16)), 8);
        unsafe {
            serial::puts(" buf=");
            serial::put_hexn(BEEP_BUF[0] as u64, 2);
            serial::put_hexn(BEEP_BUF[96000] as u64, 2);
        }
        serial::puts("\n");
        i += 1;
    }
    serial::puts("[HDA] M9a5 BCIS count=");
    serial::put_hexn(n as u64, 2);
    serial::puts("\n");
}

// ===== Stage 1: /dev/dsp kernel API (feeds the proven SD0 engine) =====
// Global MMIO base captured by init()/m9_beep(); a small config record for
// the glue layer to read; and dsp_write(): blocking single-buffer playback.
#[allow(dead_code)]
#[allow(dead_code)]
pub static mut HDA_BASE: usize = 0;
#[allow(dead_code)]
#[allow(dead_code)]
pub struct DspInfo { pub rate: u32, pub channels: u16, pub fmt: u16 }
#[allow(dead_code)]
#[allow(dead_code)]
pub static mut HDA_DSP: DspInfo = DspInfo { rate: 48000, channels: 2, fmt: 0x0011 };

#[allow(dead_code)]
#[allow(dead_code)]
pub fn hda_dsp_available() -> bool {
    unsafe { HDA_BASE != 0 }
}

// Configure stream format (rate only matters to userspace here; the codec
// was already programmed to 48k/16/2 by the beep bring-up).
#[allow(dead_code)]
#[allow(dead_code)]
pub fn hda_dsp_set_rate(rate: u32) { unsafe { HDA_DSP.rate = rate; } }

// Blocking write: copy into BEEP_BUF (64KiB), arm BDL entry 0 (len = n,
// IOC on last), ensure RUN, wait for BCIS, w1c. Buffer larger than 64KiB
// is truncated by the caller (fd layer re-chunks into periods).

// --- /dev/dsp backend trampolines (meow): registered into akuma_virtio::audio
pub unsafe extern "Rust" fn hda_dsp_tramp_write(p: *const u8, n: usize) -> usize {
    hda_dsp_write(core::slice::from_raw_parts(p, n))
}
pub unsafe extern "Rust" fn hda_dsp_tramp_stop() {
    unsafe { if HDA_BASE != 0 { MmioRegs { base: HDA_BASE as *mut u8 }.w16(0x100, 0); } } // RUN off (was: infinite recursion!)
}
#[allow(dead_code)]
pub unsafe fn hda_dsp_write(data: &[u8]) -> usize {
    // M9-blocking: pace playback at real time. data is 24-bit stereo PCM
    // (44100 Hz, 3-byte LE samples from the WAV) -> convert to 16-bit and
    // chunk it into the 192000-byte ring, waiting real microseconds between
    // chunks so the file plays at true duration.
    unsafe {
        let b = HDA_BASE;
        if b == 0 { return 0; }
        let mut ctl = MmioRegs { base: b as *mut u8 };
        // M9c: re-point codec+SD0 at 44.1k (m9_beep left the 48k pair).
        ctl.w16(0x100, 0); // RUN off while changing FMT
        codec_send_ici(&mut ctl, 0x02204011); // SET_CONV_FMT nid2: 44.1k/16/stereo
        codec_send_ici(&mut ctl, 0x02270610); // re-bind stream 1 after FMT switch
        ctl.w16(0x114, 0x4011); // SDFMT @0x114: 44.1k/16-bit/stereo (0x112 = RO FIFOW!)
        ctl.w16(0x100, 0x12); // STRM=1 (bits 7:4) | RUN (plain 2 = untagged!)
        // 16-bit stereo @44100Hz: 88200 bytes/s. Chunk = full ring.
        const CHUNK: usize = 192000; // bytes per DMA round (even)
        const RATE: u64 = 176400; // 24-bit source bytes/s (out is 16-bit)     // bytes per second
        let mut off = 0usize;
        let t0 = crate::lapic::tsc_uptime_us().unwrap_or(0);
        let mut played_us: u64 = 0;
        while off < data.len() {
            let src = &data[off..];
            // convert 24-bit LE -> 16-bit LE into BEEP_BUF
            let mut o = 0usize;
            let mut i = 0usize;
            while i + 3 <= src.len() && o + 2 <= CHUNK {
                let s24 = (src[i] as u32) | ((src[i+1] as u32) << 8) | ((src[i+2] as u32) << 16);
                // sign-extend 24 -> 32 then take top 16 (simple, quiet-ish but valid)
                let s32 = ((s24 << 8) as i32) >> 16;
                BEEP_BUF[o] = (s32 & 0xff) as u8;
                BEEP_BUF[o+1] = ((s32 >> 8) & 0xff) as u8;
                o += 2; i += 3;
            }
            let n = o;
            if n == 0 { break; }
            // (re)arm single-entry BDL
            ctl.w16(0x100, 0); // RUN off
            let pa = (&raw const BEEP_BUF) as *const u8 as usize;
            let ph = akuma_primitives::addr::virt_to_phys(pa);
            BEEP_BDL.0[0] = ph as u64;
            BEEP_BDL.0[1] = n as u64; // same encoding as the working beep: len in low dword
            ctl.w16(0x118, (ph & 0xffff) as u16);
            ctl.w16(0x11A, ((ph >> 16) & 0xffff) as u16);
            ctl.w16(0x110, 0);   // LVI=0: one entry (0x10E was RO FIFOW hi!)
            ctl.w16(0x108, (n & 0xffff) as u16); // CBL = this chunk's bytes
            ctl.w16(0x10A, (n >> 16) as u16);
            ctl.w16(0x103, 4);   // w1c BCIS
            ctl.w16(0x100, 0x12); // STRM=1 | RUN
            codec_send_ici(&mut ctl, 0x02270610); // keep converter bound (RUN cycling can drop it)
            // wait real time for this chunk: n bytes at RATE bytes/s
            let want_us = (n as u64) * 1_000_000 / RATE;
            let target = t0 + played_us + want_us;
            loop {
                let now = crate::lapic::tsc_uptime_us().unwrap_or(target);
                if now >= target { break; }
                let sts = ctl.r16(0x103);
                if sts & 4 != 0 { ctl.w16(0x103, 4); }
            }
            played_us += want_us;
            off += i; // consumed i source bytes (24-bit)
        }
        ctl.w16(0x100, 0); // RUN off at end
        data.len()
    }
}
