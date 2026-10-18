//! The MMIO / DMA half of the RTL8852CE (ryzen's wifi card) bring-up, over
//! [`akuma_rtw89`].
//!
//! Stages W1 and W2 of `docs/archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md` § 5:
//! power the card on, download its firmware, see it ready (`rtw89`); with
//! `rtw89rx`, replay the rest of Linux's start (`akuma_rtw89::script::UP`) and
//! receive on channel 1 for a few seconds, logging what it heard. `akuma-rtw89` owns the sequence (replayed in host tests
//! against Linux's register trace of this very card); this module owns what
//! cannot be decided from bytes: the PCI function, the mapped BAR, DMA memory
//! with a known bus address, and the firmware file.
//!
//! Opt-in: the `rtw89` boot token, after the root filesystem is up (the
//! firmware is a file on it). Without the token nothing here touches the card.
//!
//! With `rtw89wifi` (stages W3/W4) the card is **kept up**: after the
//! firmware is ready this hands it, as a [`Card`], to the station
//! (`rtw89_sta.rs`), which is `/dev/wifi0`'s backend and replays the recorded
//! join (`script::JOIN1..4`) around the frames it sends itself. The card is
//! then shut down by the reboot path ([`shutdown_for_reset`]) instead of here.
//!
//! # The DMA contract
//!
//! The CH12 ring, the packet slots, the idle ring every stopped TX channel is
//! pointed at, and the two RX rings are page-aligned `.bss` statics,
//! translated with `virt_to_phys`, as `nvme.rs` does. Firmware leaves AMD-Vi
//! off (measured on ryzen 2026-10-06: the IOMMU's control register reads
//! `0x400`, `IommuEn` clear) and this kernel never enables it, so a physical
//! address is a bus address. The card's DMA reaches memory only through its
//! root port, whose Bus Master Enable firmware left **off** —
//! `pci::enable_bridges_above` turns it on; without it the first five metal
//! runs stalled at the firmware header with `HAXI_IDCT = TXMDA_STUCK`. The bring-up runs once, on the boot CPU, before `init` — the only
//! code that ever touches these statics — so the `&'static mut` views below
//! are never aliased. Both RX rings are stocked the way Linux stocks them,
//! every entry with a buffer — the chip treats a ring with equal indices as
//! all free, and the first metal runs, with zeroed entries, stalled at the
//! firmware header with `HAXIDMA_ERR_FLAG` set. As in Linux, every entry has
//! its own buffer (12 KiB for RXQ, 4 KiB for the release reports): the chip
//! may fill all but one entry ahead of the host. x86 is DMA-coherent; the crate fences before each doorbell.
//!
//! # Always ends quiet
//!
//! Success or failure, the card is shut down before this returns (except a
//! successful `rtw89wifi` bring-up, which [`shutdown_for_reset`] ends): firmware CPU
//! stopped, host DMA stopped, MAC powered off, PCI bus mastering off. W1 has
//! nothing to use running firmware for yet, and a bus master left pointing at
//! this kernel's `.bss` across a warm reset is what crash-looped the xHCI box
//! (`docs/archive/AKUMA_AMD64_XHCI_WEDGED_BOX.md`). Linux on the next boot
//! powers the card on from scratch either way.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use akuma_primitives::addr::virt_to_phys;
use akuma_dma::{DmaArray, align::A4K};
use akuma_primitives::mmio::MmioWindow;
use akuma_primitives::OnceCopy;
use akuma_rtw89::bringup::{self, Ch12, Dma, RING_BYTES, SLOT};
use akuma_rtw89::fw::Source;
use akuma_rtw89::{Bus, h2c, rx, script, tx};

use crate::pci;
use crate::polltime::spin_us;
use crate::serial;

/// Diagnostics only: `R_AX_DMAC_ERR_ISR` and the HAXI DMA error indications
/// behind its bit 14 (`R_AX_HAXI_IDCT`: `TXBD_LEN0`, `TXBD_4KBOUND`,
/// `RXMDA_STUCK`, `TXMDA_STUCK`).
const DMAC_ERR_ISR: u32 = 0x8524;
const HAXI_IDCT: u32 = 0x10bc;

const VENDOR: u16 = 0x10ec;
const DEVICE: u16 = 0xc852;
/// The register BAR (BAR0 is a 256-byte I/O BAR the driver never uses).
const REG_BAR: usize = 2;
/// The firmware, newest-format name first: `rtw8852c_fw-1.bin` is what Pop's
/// Linux loaded in the W0 trace; `overlays/ryzen/fetch-firmware.sh` puts
/// Alpine's copy of it (and `-2.bin`, a newer format this parser has not been
/// checked against) on p3.
const FIRMWARE: [&str; 2] = ["/lib/firmware/rtw89/rtw8852c_fw-1.bin", "/lib/firmware/rtw89/rtw8852c_fw.bin"];
/// Packet slots in flight on CH12. Linux keeps all 167 packets in flight; 16
/// is enough for the chip never to wait on the host.
const SLOTS: usize = 16;
/// RX ring entries, each with its own buffer (the chip may fill all but one
/// ahead of the host).
const RX_ENTRIES: usize = 256;
/// Bytes per RXQ buffer: [`h2c::RX_BUF_SIZE`] rounded up to whole pages.
const RX_BUF_BYTES: usize = 12 * 1024;
/// Bytes per RPQ (release report) buffer: reports are 8-byte records.
const RPQ_BUF_BYTES: usize = 4096;

// Each is page-aligned, as the chip needs. `RING`, `SLOT` and `TX` are handed to
// the card for good (`DmaArray::leak_mut`); the RX rings are stocked once and
// the RX buffers are read an entry at a time (`DmaArray::read_range`).
static RING_MEM: DmaArray<u8, 4096, A4K> = DmaArray::new();
static IDLE_MEM: DmaArray<u8, 4096, A4K> = DmaArray::new();
static RXQ_MEM: DmaArray<u8, 4096, A4K> = DmaArray::new();
static RPQ_MEM: DmaArray<u8, 4096, A4K> = DmaArray::new();
static SLOT_MEM: DmaArray<u8, { SLOT * SLOTS }, A4K> = DmaArray::new();
static RXQ_BUF_MEM: DmaArray<u8, { RX_BUF_BYTES * RX_ENTRIES }, A4K> = DmaArray::new();
static RPQ_BUF_MEM: DmaArray<u8, { RPQ_BUF_BYTES * RX_ENTRIES }, A4K> = DmaArray::new();
/// The TX channels the join transmits on (ACH0, ACH3, CH8), back to back:
/// each `tx::CHAN_BYTES` of TXBD ring, WD pages and frame buffers.
static TX_MEM: DmaArray<u8, { 3 * tx::CHAN_BYTES }, A4K> = DmaArray::new();

const _: () = assert!(RING_BYTES <= 4096);
const _: () = assert!(h2c::RX_BUF_SIZE as usize <= RX_BUF_BYTES);

/// BAR2, mapped. Every offset the crate hands in is a register inside the
/// 1 MiB BAR (the largest is `INDIR_ACCESS_ENTRY`, `0x40000`).
pub struct Regs(MmioWindow);

impl Bus for Regs {
    fn read8(&mut self, off: u32) -> u8 {
        self.0.read::<u8>(off as usize)
    }
    fn read16(&mut self, off: u32) -> u16 {
        self.0.read::<u16>(off as usize)
    }
    fn read32(&mut self, off: u32) -> u32 {
        self.0.read::<u32>(off as usize)
    }
    fn write8(&mut self, off: u32, v: u8) {
        self.0.write::<u8>(off as usize, v);
    }
    fn write16(&mut self, off: u32, v: u16) {
        self.0.write::<u16>(off as usize, v);
    }
    fn write32(&mut self, off: u32, v: u32) {
        self.0.write::<u32>(off as usize, v);
    }
    fn delay_us(&mut self, us: u32) {
        spin_us(u64::from(us));
    }
}

/// The firmware file, read in place through the VFS — never loaded whole.
///
/// `path` is the **symlink-resolved** name: `read_at` does not follow links, and
/// a usr-merged root (Gentoo) has `/lib -> usr/lib`, so `/lib/firmware/...` is a
/// path through a link. One `String`, once, at boot.
pub struct File {
    path: alloc::string::String,
    len: u32,
}

impl Source for File {
    fn len(&self) -> u32 {
        self.len
    }
    fn read_at(&mut self, off: u32, buf: &mut [u8]) -> bool {
        let mut done = 0;
        while done < buf.len() {
            match crate::fs::read_at(&self.path, off as usize + done, &mut buf[done..]) {
                Ok(0) | Err(_) => return false,
                Ok(n) => done += n,
            }
        }
        true
    }
}

fn say(what: &str) {
    serial::puts("[rtw] ");
    serial::puts(what);
    serial::puts("\n");
}

fn say_hex(what: &str, v: u64) {
    serial::puts("[rtw] ");
    serial::puts(what);
    serial::puts(" 0x");
    serial::put_hex(v);
    serial::puts("\n");
}

/// `rtw89` on the command line: bring the card up to running firmware, report,
/// and shut it down again. `rtw89rx`: also replay Linux's start and listen on
/// channel 1 (W2). `rtw89wifi`: keep it up for the station (W3/W4).
pub fn init(cmdline: &str) {
    let has = |tok: &str| cmdline.split_ascii_whitespace().any(|t| t == tok);
    let keep = has("rtw89wifi");
    if !(has("rtw89") || has("rtw89rx") || keep) {
        return;
    }
    let mut found = None;
    pci::for_each(|d| {
        if d.header.vendor_id == VENDOR && d.header.device_id == DEVICE {
            found = Some(*d);
        }
    });
    let Some(dev) = found else {
        say("no RTL8852CE (10ec:c852) on the PCI bus");
        return;
    };
    let Some(mut file) = FIRMWARE.iter().find_map(|&path| {
        let path = crate::fs::resolve_symlinks(path);
        let size = crate::fs::metadata(&path).ok()?.size;
        Some(File { path, len: u32::try_from(size).ok()? })
    }) else {
        say("no firmware: /lib/firmware/rtw89/rtw8852c_fw-1.bin missing (overlays/ryzen/fetch-firmware.sh)");
        return;
    };
    serial::puts("[rtw] firmware ");
    serial::puts(&file.path);
    serial::puts(", ");
    serial::put_dec(u64::from(file.len));
    serial::puts(" bytes\n");

    // D0 before anything: MMIO is only guaranteed live there. Only written
    // when PMCSR says otherwise (cycling a D0 device wedged the HDA controller).
    let cfg = pci::config_space(dev.addr);
    for cap in akuma_pci::capabilities(&cfg, dev.header.capabilities_pointer) {
        if cap.id == akuma_pci::capability_id::POWER_MANAGEMENT {
            let pmcsr = pci::read_u16_config(dev.addr, cap.offset + 4);
            if akuma_pci::pm::power_state(pmcsr) != akuma_pci::pm::D0 {
                pci::write_u16_config(dev.addr, cap.offset + 4, pmcsr & !akuma_pci::pm::POWER_STATE_MASK);
                spin_us(10_000);
                say("PCI power state was not D0; wrote D0");
            }
            break;
        }
    }
    let devsta_at = pcie_config(dev.addr, dev.header.capabilities_pointer, &cfg);
    let Some(bar) = dev.bars[REG_BAR] else {
        say("BAR2 not decoded");
        return;
    };
    let (size, _) = pci::probe_bar_size(dev.addr, REG_BAR as u8);
    let Some(win) = pci::map_bar_window(bar, size.max(0x10_0000)) else {
        say("could not map BAR2");
        return;
    };
    // Bus mastering on the root port as well as the card: without the port's,
    // every DMA read the card makes is dropped there (`pci::enable_bridges_above`).
    // INTx masked: this driver polls.
    match pci::enable_bridges_above(dev.addr.bus) {
        Some(cmd) => say_hex("root port command was", u64::from(cmd)),
        None => say("no bridge found above the card"),
    }
    pci::enable_full(dev.addr, true, true);
    let mut regs = Regs(win);
    // What the card arrived in, before a single write.
    say_hex("arrived: IC_PWR_STATE", u64::from(regs.read32(akuma_rtw89::regs::IC_PWR_STATE)));
    say_hex("arrived: WCPU_FW_CTRL", u64::from(regs.read32(akuma_rtw89::regs::WCPU_FW_CTRL)));

    let mut dma = dma_statics();

    let t0 = crate::polltime::tsc();
    let mut ch12 = Ch12::new();
    let result = bringup::bring_up(&mut regs, &mut dma, &mut ch12, &mut file, &mut |what: &'static str, v: u32| {
        say_hex(what, u64::from(v));
    });
    let us = (crate::polltime::tsc() - t0) / (crate::polltime::tsc_hz() / 1_000_000).max(1);
    match result {
        Ok(r) => {
            serial::puts("[rtw] fw ready v");
            for (i, part) in r.version.iter().take(3).enumerate() {
                if i > 0 {
                    serial::puts(".");
                }
                serial::put_dec(u64::from(*part));
            }
            serial::puts(" (cut ");
            serial::put_dec(u64::from(r.cv));
            serial::puts(", ");
            serial::put_dec(u64::from(r.data_packets));
            serial::puts(" packets, FW_CTRL 0x");
            serial::put_hex(u64::from(r.fw_ctrl));
            serial::puts(", ");
            serial::put_dec(us);
            serial::puts(" us)\n");
            if keep {
                // From here the card belongs to the station; the reset path
                // shuts it down.
                LIVE_BAR.set(regs.0);
                LIVE_KEPT.store(true, Ordering::Release);
                LIVE_PCI.store(pack(dev.addr), Ordering::Release);
                let card = Card::new(regs, dma, ch12, file);
                crate::rtw89_sta::adopt(card, has("wifibackoff"));
                return;
            }
            if has("rtw89rx") {
                start_and_listen(&mut regs, &mut dma, &mut ch12);
            }
        }
        Err(e) => {
            report_error(&mut regs, e);
            // Bit 13: a DMA read the card made came back as a master abort.
            say_hex("PCI status", u64::from(pci::read_u16_config(dev.addr, 0x06)));
            if let Some(at) = devsta_at {
                say_hex("DevSta", u64::from(pci::read_u16_config(dev.addr, at)));
            }
        }
    }

    match bringup::shutdown(&mut regs) {
        Ok(()) => say("card shut down"),
        Err(_) => say("power-off handshake failed; DMA stopped regardless"),
    }
    pci::quiesce(dev.addr);
}

/// The DMA statics as a [`Dma`], both RX rings stocked: every entry its own
/// buffer.
///
/// Called once, by [`init`] — the DMA contract: boot CPU, before `init`, the
/// only code that names these statics. `RING_MEM` and `SLOT_MEM` are handed to
/// the card for good, so a second call panics (`DmaArray::leak_mut`) instead of
/// aliasing the first call's slices. A kept card ([`Card`]) carries them from
/// then on.
fn dma_statics() -> Dma<'static> {
    let (ring, slots) = (RING_MEM.leak_mut(), SLOT_MEM.leak_mut());
    let phys = |addr: usize| virt_to_phys(addr) as u64;
    // Stock both RX rings: every entry its own buffer.
    let rxq_phys = phys(RXQ_BUF_MEM.addr());
    let rpq_phys = phys(RPQ_BUF_MEM.addr());
    for (ring_mem, base, stride) in [(&RXQ_MEM, rxq_phys, RX_BUF_BYTES), (&RPQ_MEM, rpq_phys, RPQ_BUF_BYTES)] {
        let mut ring_view = ring_mem.view_mut();
        let ring_bytes = &mut ring_view[..RING_BYTES];
        let size = if stride == RX_BUF_BYTES { h2c::RX_BUF_SIZE } else { RPQ_BUF_BYTES as u16 };
        for (i, entry) in ring_bytes.as_chunks_mut::<{ h2c::BD_LEN }>().0.iter_mut().enumerate() {
            entry.copy_from_slice(&h2c::rx_bd(base + (i * stride) as u64, size));
        }
    }
    Dma {
        ring_phys: phys(RING_MEM.addr()),
        slots_phys: phys(SLOT_MEM.addr()),
        ring,
        slots,
        idle_phys: phys(IDLE_MEM.addr()),
        rx_phys: [phys(RXQ_MEM.addr()), phys(RPQ_MEM.addr())],
        tx_phys: {
            let tx_base = phys(TX_MEM.addr());
            // The address-info format carries the frame's bus address in 32
            // low bits and 4 high ones; this driver only handles the low 32.
            [0, 1, 2].map(|k| {
                let at = tx_base + (k as usize * tx::CHAN_BYTES) as u64;
                assert!(at + tx::CHAN_BYTES as u64 <= 1 << 32, "TX channel memory above 4 GiB");
                at
            })
        },
    }
}

/// Where a kept card's registers are, for [`shutdown_for_reset`]: the mapped
/// BAR window (`LIVE_KEPT` clear = no card kept) and its PCI address (`bus << 16 | dev << 8 | fn`).
static LIVE_BAR: OnceCopy<MmioWindow> = OnceCopy::new();
static LIVE_KEPT: AtomicBool = AtomicBool::new(false);
static LIVE_PCI: AtomicU32 = AtomicU32::new(0);

fn pack(a: akuma_pci::Address) -> u32 {
    u32::from(a.bus) << 16 | u32::from(a.device) << 8 | u32::from(a.function)
}

/// The reset path: power a kept card off and stop its bus mastering — a bus
/// master left pointing at this kernel's `.bss` across a warm reset is what
/// crash-looped the xHCI box. A no-op unless `rtw89wifi` kept one.
///
/// Through its own view of the registers, not the station's [`Card`]: the
/// station may be parked mid-join on this very core and cannot be asked. It
/// is told to stop first ([`crate::rtw89_sta::stop`]); whatever it still does
/// after that is to a card with bus mastering off.
pub fn shutdown_for_reset() {
    if !LIVE_KEPT.swap(false, Ordering::AcqRel) {
        return;
    }
    let Some(bar) = LIVE_BAR.get() else { return };
    crate::rtw89_sta::stop();
    let p = LIVE_PCI.load(Ordering::Acquire);
    let addr = akuma_pci::Address { bus: (p >> 16) as u8, device: (p >> 8) as u8, function: p as u8 };
    match bringup::shutdown(&mut Regs(bar)) {
        Ok(()) => serial::puts("[rtw] card shut down for reset\n"),
        Err(_) => serial::puts("[rtw] power-off handshake failed; DMA stopped regardless\n"),
    }
    pci::quiesce(addr);
}

/// `FIF_BCN_PRBRESP_PROMISC`'s register: the RX filter options
/// (`rtw89_ops_configure_filter`).
const RX_FLTR_OPT: u32 = 0xce20;
const A_A1_MATCH: u32 = 1 << 1;
const A_BC: u32 = 1 << 2;
const A_BCN_CHK_EN: u32 = 1 << 7;

/// The card, kept up after [`init`] for the station (`rtw89wifi`): the
/// registers, the DMA memory and the host halves of every ring on it.
///
/// One owner — the station's daemon — so nothing here locks. The TX channel
/// memory is [`TX_MEM`] split three ways in [`tx::USED`] order.
pub struct Card {
    regs: Regs,
    dma: Dma<'static>,
    ch12: Ch12,
    file: File,
    tx: [tx::Ring; 3],
    tx_mem: [&'static mut [u8]; 3],
    rxq: rx::Ring,
    rpq: rx::Ring,
    /// The RX filter as the replay left it, while [`Card::open_filter`] has it
    /// open.
    saved_filter: Option<u32>,
    /// Release reports drained so far, and the head of the latest — what the
    /// chip says became of the frames sent (diagnostics for the join).
    pub rpq_seen: u32,
    pub rpq_last: [u8; 32],
    /// Release reports by the TX status of their first record
    /// (`rtw89_pci_rpp_fmt` bits 15:13: 0 done, 1 retry limit, 2 lifetime,
    /// 3 dropped).
    pub rpq_status: [u32; 8],
    /// Every RXQ entry taken, by outcome: parsed (by packet type, low 4
    /// bits), unparsable, and 802.11 frames with a bad FCS.
    pub rx_types: [u32; 16],
    pub rx_bad: u32,
    pub rx_crc: u32,
}

/// Index into [`Card`]'s TX channels ([`tx::USED`] order).
pub const TX_DATA: usize = 0;
pub const TX_EAPOL: usize = 1;
pub const TX_MGMT: usize = 2;

impl Card {
    fn new(regs: Regs, dma: Dma<'static>, ch12: Ch12, file: File) -> Self {
        // `TX_MEM` is named by nothing but this slice from here on (the boot code
        // only took its bus address), and the three chunks are disjoint. A second
        // card would panic here rather than alias the first.
        let all = TX_MEM.leak_mut();
        let (a, rest) = all.split_at_mut(tx::CHAN_BYTES);
        let (b, c) = rest.split_at_mut(tx::CHAN_BYTES);
        Self {
            regs,
            dma,
            ch12,
            file,
            tx: [tx::Ring::new(), tx::Ring::new(), tx::Ring::new()],
            tx_mem: [a, b, c],
            rxq: rx::Ring::new(rx::RXQ_IDX, RX_ENTRIES as u16),
            rpq: rx::Ring::new(rx::RPQ_IDX, RX_ENTRIES as u16),
            saved_filter: None,
            rpq_seen: 0,
            rpq_last: [0; 32],
            rpq_status: [0; 8],
            rx_types: [0; 16],
            rx_bad: 0,
            rx_crc: 0,
        }
    }

    /// Power the card off and bring it up again from nothing, firmware and
    /// all — the state the recorded join starts from (Linux leaves idle power
    /// save the same way before it joins). Every host ring index restarts at
    /// 0, as `pre_init` restarts the chip's.
    pub fn restart(&mut self) -> bool {
        let _ = bringup::shutdown(&mut self.regs);
        self.ch12 = Ch12::new();
        self.tx = [tx::Ring::new(), tx::Ring::new(), tx::Ring::new()];
        self.rxq = rx::Ring::new(rx::RXQ_IDX, RX_ENTRIES as u16);
        self.rpq = rx::Ring::new(rx::RPQ_IDX, RX_ENTRIES as u16);
        self.saved_filter = None;
        match bringup::bring_up(&mut self.regs, &mut self.dma, &mut self.ch12, &mut self.file, &mut |_: &'static str, _: u32| {}) {
            Ok(_) => true,
            Err(e) => {
                report_error(&mut self.regs, e);
                false
            }
        }
    }

    /// Replay `seq` with `vars`, logging it as `label`; `false` if it could
    /// not run to the end.
    pub fn replay(&mut self, label: &str, seq: &[u8], vars: &script::Vars) -> bool {
        replay(&mut self.regs, &mut self.dma, &mut self.ch12, label, seq, vars)
    }

    /// Queue `frame` on TX channel `which` ([`TX_DATA`], [`TX_EAPOL`],
    /// [`TX_MGMT`]).
    pub fn send(&mut self, which: usize, frame: &[u8], d: tx::Desc) -> Result<(), tx::Error> {
        let phys = self.dma.tx_phys[which];
        self.tx[which].submit(&mut self.regs, self.tx_mem[which], phys, frame, d)
    }

    /// Hand every received buffer to `f`, then the entries back to the chip;
    /// drain the release reports. Returns how many RXQ buffers there were.
    pub fn poll_rx(&mut self, mut f: impl FnMut(&rx::Packet<'_>)) -> u16 {
        let n = self.rxq.ready(&mut self.regs);
        for _ in 0..n {
            let at = usize::from(self.rxq.wp) * RX_BUF_BYTES;
            // Entry `wp` is one the chip has finished with (it is behind the
            // chip's index) and will not touch again until `release`; the view
            // covers that entry only, not the entries the chip is filling.
            core::sync::atomic::fence(Ordering::Acquire);
            RXQ_BUF_MEM.read_range(at, RX_BUF_BYTES, |buf| match rx::parse(buf) {
                Ok(p) => {
                    self.rx_types[usize::from(p.desc.pkt_type & 0xf)] += 1;
                    if p.desc.pkt_type == rx::kind::WIFI && p.desc.crc32_err {
                        self.rx_crc += 1;
                    }
                    f(&p);
                }
                Err(_) => self.rx_bad += 1,
            });
            self.rxq.advance(1);
        }
        if n > 0 {
            self.rxq.release(&mut self.regs);
        }
        let r = self.rpq.ready(&mut self.regs);
        for _ in 0..r {
            let at = usize::from(self.rpq.wp) * RPQ_BUF_BYTES;
            // As for the RXQ buffer above, on the release-report ring.
            core::sync::atomic::fence(Ordering::Acquire);
            let rpp = RPQ_BUF_MEM.read_range(at, 32, |buf| {
                self.rpq_last.copy_from_slice(buf);
                u32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]])
            });
            self.rpq_seen += 1;
            self.rpq_status[((rpp >> 13) & 7) as usize] += 1;
            self.rpq.advance(1);
        }
        if r > 0 {
            self.rpq.release(&mut self.regs);
        }
        n
    }

    /// TX channel `which`'s index register: host write pointer in bits 15:0,
    /// the chip's read pointer in 27:16.
    pub fn tx_idx(&mut self, which: usize) -> u32 {
        self.regs.read32(tx::idx_reg(tx::USED[which] as u8))
    }

    /// The RX ring's index register: the host's read pointer in bits 15:0,
    /// the chip's write pointer in 27:16.
    pub fn rx_idx(&mut self) -> u32 {
        self.regs.read32(rx::RXQ_IDX)
    }

    /// `(DMAC_ERR_ISR, HAXI_IDCT)`: the DMA engine's error indications.
    pub fn dma_errors(&mut self) -> (u32, u32) {
        (self.regs.read32(DMAC_ERR_ISR), self.regs.read32(HAXI_IDCT))
    }

    /// Hear every beacon on the channel, not only the joined network's — what
    /// mac80211 asks for during a scan (`FIF_BCN_PRBRESP_PROMISC`).
    pub fn open_filter(&mut self) {
        if self.saved_filter.is_none() {
            let f = self.regs.read32(RX_FLTR_OPT);
            self.saved_filter = Some(f);
            self.regs.write32(RX_FLTR_OPT, f & !(A_A1_MATCH | A_BC | A_BCN_CHK_EN));
        }
    }

    /// Put back the filter [`Card::open_filter`] found.
    pub fn close_filter(&mut self) {
        if let Some(f) = self.saved_filter.take() {
            self.regs.write32(RX_FLTR_OPT, f);
        }
    }
}

/// Sends the replay's H2C commands on CH12, after the download's packets.
struct Sink<'a, 'b> {
    ch12: &'a mut Ch12,
    dma: &'a mut Dma<'b>,
}

impl script::H2cSink<Regs> for Sink<'_, '_> {
    fn send(&mut self, bus: &mut Regs, cmd: &[u8]) -> Result<(), script::SendFailed> {
        self.ch12.send_h2c(bus, self.dma, cmd).map_err(|_| script::SendFailed)
    }
}

/// Replay `seq` (with `vars`) and log what the chip did differently, as
/// `[rtw] <label>: ...`. `false` if the replay could not run to the end.
fn replay(regs: &mut Regs, dma: &mut Dma<'_>, ch12: &mut Ch12, label: &str, seq: &[u8], vars: &script::Vars) -> bool {
    let t0 = crate::polltime::tsc();
    let mut st = script::Stats::default();
    let result = script::run(regs, seq, vars, &mut Sink { ch12, dma }, &mut st);
    let us = (crate::polltime::tsc() - t0) / (crate::polltime::tsc_hz() / 1_000_000).max(1);
    let ok = match result {
        Ok(()) => true,
        Err(script::Error::Diverged) => false,
        Err(script::Error::Corrupt(at)) => {
            serial::puts("[rtw] ");
            serial::puts(label);
            say_hex(" FAILED: replay stream corrupt at byte", at as u64);
            return false;
        }
        Err(script::Error::H2c(n)) => {
            serial::puts("[rtw] ");
            serial::puts(label);
            say_hex(" FAILED: could not send H2C number", n as u64);
            return false;
        }
    };
    serial::puts("[rtw] ");
    serial::puts(label);
    serial::puts(if ok { ": " } else { " DIVERGED (too many poll timeouts): " });
    serial::put_dec(st.ops as u64);
    serial::puts(" ops, ");
    serial::put_dec(st.writes as u64);
    serial::puts(" writes, ");
    serial::put_dec(st.h2c as u64);
    serial::puts(" h2c, checks ");
    serial::put_dec(st.checks as u64);
    serial::puts(" (");
    serial::put_dec(st.mismatches as u64);
    serial::puts(" differ), polls ");
    serial::put_dec(st.polls as u64);
    serial::puts(" (");
    serial::put_dec(st.poll_timeouts as u64);
    serial::puts(" timed out), ");
    serial::put_dec(us);
    serial::puts(" us\n");
    for d in &st.first[..st.first_n] {
        serial::puts("[rtw]   op ");
        serial::put_dec(d.op as u64);
        serial::puts(" reg 0x");
        serial::put_hex(u64::from(d.off));
        serial::puts(" want 0x");
        serial::put_hex(u64::from(d.want));
        serial::puts(" got 0x");
        serial::put_hex(u64::from(d.got));
        serial::puts("\n");
    }
    ok
}

/// How long `rtw89rx` listens on channel 1 before shutting the card down.
const LISTEN_MS: u64 = 12_000;
/// Distinct networks remembered while listening.
const MAX_SEEN: usize = 32;

#[derive(Clone, Copy)]
struct Seen {
    bssid: [u8; 6],
    ssid_hash: u32,
    ssid_len: u8,
    channel: u8,
    rsn: u8,
    beacons: u32,
}

/// FNV-1a: SSIDs go to the log as hashes, not names — the log is read
/// off the laptop's disk, and the neighbours' network names are theirs.
pub fn fnv1a(b: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for &x in b {
        h = (h ^ u32::from(x)).wrapping_mul(0x0100_0193);
    }
    h
}

/// Stage W2: the rest of Linux's start replayed (`script::UP`), the RX filter
/// opened to every beacon, then [`LISTEN_MS`] of receiving on channel 1.
fn start_and_listen(regs: &mut Regs, dma: &mut Dma<'_>, ch12: &mut Ch12) {
    if !replay(regs, dma, ch12, "replay", script::UP, &script::Vars::default()) {
        return;
    }

    // `FIF_BCN_PRBRESP_PROMISC`, as mac80211 asks for during a scan: beacons
    // of every BSS, not only our own (`rtw89_ops_configure_filter`).
    let f = regs.read32(RX_FLTR_OPT);
    regs.write32(RX_FLTR_OPT, f & !(A_A1_MATCH | A_BC | A_BCN_CHK_EN));
    say_hex("RX filter", u64::from(regs.read32(RX_FLTR_OPT)));

    let mut rxq = rx::Ring::new(rx::RXQ_IDX, RX_ENTRIES as u16);
    let mut rpq = rx::Ring::new(rx::RPQ_IDX, RX_ENTRIES as u16);
    let mut seen = [Seen { bssid: [0; 6], ssid_hash: 0, ssid_len: 0, channel: 0, rsn: 0, beacons: 0 }; MAX_SEEN];
    let mut nseen = 0;
    let mut counts = [0u32; 16];
    let (mut bad, mut frames, mut crc) = (0u32, 0u32, 0u32);
    let deadline = crate::polltime::tsc() + crate::polltime::ticks_ms(LISTEN_MS);
    while crate::polltime::tsc() < deadline {
        let n = rxq.ready(regs);
        for _ in 0..n {
            let at = usize::from(rxq.wp) * RX_BUF_BYTES;
            // Entry `wp` is one the chip has finished with (it is behind the chip's
            // index) and will not touch again until `release`; the view covers
            // that entry only.
            core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
            RXQ_BUF_MEM.read_range(at, RX_BUF_BYTES, |buf| match rx::parse(buf) {
                Ok(p) => {
                    counts[usize::from(p.desc.pkt_type & 0xf)] += 1;
                    if p.desc.pkt_type == rx::kind::WIFI {
                        frames += 1;
                        if p.desc.crc32_err {
                            crc += 1;
                        } else if let Some(b) = akuma_ieee80211::beacon::parse(p.body, true) {
                            let h = fnv1a(b.ssid);
                            let rsn = b.rsn.map_or(0, |r| 1 | (u8::from(r.akm_psk) << 1) | (u8::from(r.pairwise_ccmp) << 2));
                            if let Some(e) = seen[..nseen].iter_mut().find(|e| e.bssid == b.bssid) {
                                e.beacons += 1;
                            } else if nseen < MAX_SEEN {
                                seen[nseen] = Seen {
                                    bssid: b.bssid,
                                    ssid_hash: h,
                                    ssid_len: b.ssid.len() as u8,
                                    channel: b.channel.unwrap_or(0),
                                    rsn,
                                    beacons: 1,
                                };
                                nseen += 1;
                            }
                        }
                    }
                }
                Err(_) => bad += 1,
            });
            rxq.advance(1);
        }
        if n > 0 {
            rxq.release(regs);
        }
        let n = rpq.ready(regs);
        if n > 0 {
            rpq.advance(n);
            rpq.release(regs);
        }
        spin_us(200);
    }
    serial::puts("[rtw] listened ");
    serial::put_dec(LISTEN_MS);
    serial::puts(" ms: wifi ");
    serial::put_dec(u64::from(frames));
    serial::puts(" (crc err ");
    serial::put_dec(u64::from(crc));
    serial::puts("), ppdu-status ");
    serial::put_dec(u64::from(counts[usize::from(rx::kind::PPDU_STAT)]));
    serial::puts(", c2h ");
    serial::put_dec(u64::from(counts[usize::from(rx::kind::C2H)]));
    serial::puts(", other ");
    let other: u32 = counts.iter().sum::<u32>()
        - counts[usize::from(rx::kind::WIFI)]
        - counts[usize::from(rx::kind::PPDU_STAT)]
        - counts[usize::from(rx::kind::C2H)];
    serial::put_dec(u64::from(other));
    serial::puts(", unparsable ");
    serial::put_dec(u64::from(bad));
    serial::puts(", networks ");
    serial::put_dec(nseen as u64);
    serial::puts("\n");
    for e in &seen[..nseen] {
        // The OUI only: enough to tell access points apart from each other
        // in a log, not to name a neighbour's.
        serial::puts("[rtw] bss ");
        for (i, b) in e.bssid[..3].iter().enumerate() {
            if i > 0 {
                serial::puts(":");
            }
            serial::put_hexn(u64::from(*b), 2);
        }
        serial::puts(":xx:xx:xx ch ");
        serial::put_dec(u64::from(e.channel));
        serial::puts(" ssid#");
        serial::put_hexn(u64::from(e.ssid_hash), 8);
        serial::puts(" len ");
        serial::put_dec(u64::from(e.ssid_len));
        serial::puts(if e.rsn & 1 == 0 { " open" } else if e.rsn & 6 == 6 { " wpa2-psk-ccmp" } else { " rsn-other" });
        serial::puts(" beacons ");
        serial::put_dec(u64::from(e.beacons));
        serial::puts("\n");
    }
}

/// The PCIe device-control state Linux runs the card with, from config space.
///
/// - `DevCtl.NoSnoop` **off**: Linux's `lspci` shows `NoSnoop-` (the spec's
///   reset value is on). Firmware on ryzen already leaves it off; this makes
///   it a precondition rather than a coincidence.
/// - `DevCtl2` completion-timeout disable **on**: `rtw89_pci_cpl_timeout_cfg`.
///
/// Returns the config offset of `DevSta`, cleared here (write-1-to-clear) so a
/// later read shows only what the bring-up caused.
fn pcie_config(addr: akuma_pci::Address, caps: u8, cfg: &[u8; 256]) -> Option<u8> {
    const DEVCTL: u8 = 0x08;
    const DEVSTA: u8 = 0x0a;
    const DEVCTL2: u8 = 0x28;
    const NOSNOOP_EN: u16 = 1 << 11;
    const COMP_TMOUT_DIS: u16 = 1 << 4;
    let Some(cap) = akuma_pci::capabilities(cfg, caps).find(|c| c.id == akuma_pci::capability_id::PCI_EXPRESS)
    else {
        say("no PCIe capability");
        return None;
    };
    let devctl = pci::read_u16_config(addr, cap.offset + DEVCTL);
    pci::write_u16_config(addr, cap.offset + DEVCTL, devctl & !NOSNOOP_EN);
    let devctl2 = pci::read_u16_config(addr, cap.offset + DEVCTL2);
    pci::write_u16_config(addr, cap.offset + DEVCTL2, devctl2 | COMP_TMOUT_DIS);
    pci::write_u16_config(addr, cap.offset + DEVSTA, 0x000f);
    Some(cap.offset + DEVSTA)
}

/// The failure, with what Linux's `rtw89_fw_dl_fail_dump` would show, plus the
/// DMA engine's own error indications — `HAXI_IDCT` is what named the stuck
/// TX DMA in 2026-10-06's runs.
fn report_error(regs: &mut Regs, e: bringup::Error) {
    use bringup::Error as E;
    match e {
        E::Poll { stage, reg, last } => {
            serial::puts("[rtw] FAILED waiting at ");
            serial::puts(stage_name(stage));
            serial::puts(": reg 0x");
            serial::put_hex(u64::from(reg));
            serial::puts(" last read 0x");
            serial::put_hex(u64::from(last));
            serial::puts("\n");
        }
        E::MacOff(v) => say_hex("FAILED: DMAC not enabled after power-on, DMAC_FUNC_EN", u64::from(v)),
        E::CpuRunning => say("FAILED: firmware CPU already running"),
        E::Firmware(_) => say("FAILED: firmware file refused (not an rtw89 container, or no image for this cut)"),
        E::Read { packet } => say_hex("FAILED: firmware read failed at packet", u64::from(packet)),
        E::Rejected(st) => {
            serial::puts("[rtw] FAILED: ");
            serial::puts(bringup::rejected_reason(st));
            serial::puts(" (status ");
            serial::put_dec(u64::from(st));
            serial::puts(")\n");
        }
        E::DmaTooSmall => say("FAILED: DMA memory too small"),
    }
    say_hex("fwdl 0x1E0 =", u64::from(regs.read32(akuma_rtw89::regs::WCPU_FW_CTRL)));
    say_hex("fwdl 0x83F0 =", u64::from(regs.read32(akuma_rtw89::regs::BOOT_DBG)));
    say_hex("CH12_TXBD_IDX =", u64::from(regs.read32(akuma_rtw89::regs::CH12_TXBD_IDX)));
    say_hex("DMAC_ERR_ISR =", u64::from(regs.read32(DMAC_ERR_ISR)));
    say_hex("HAXI_IDCT (8 len0, 4 4k-bound, 2 rx stuck, 1 tx stuck) =", u64::from(regs.read32(HAXI_IDCT)));
}

const fn stage_name(s: bringup::Stage) -> &'static str {
    use bringup::Stage as S;
    match s {
        S::PowerOn => "power-on",
        S::PowerOff => "power-off",
        S::DmacPreInit => "DMAC pre-init",
        S::DleInit => "DLE init",
        S::HfcInit => "HCI flow control",
        S::PciPreInit => "PCI pre-init",
        S::Firmware => "firmware",
        S::CpuEnable => "CPU enable",
        S::H2cPathReady => "H2C path ready",
        S::HeaderSent => "firmware header send",
        S::FwdlPathReady => "firmware header accepted",
        S::Data => "firmware data",
        S::FwReady => "firmware ready",
        S::H2c => "firmware command",
    }
}
