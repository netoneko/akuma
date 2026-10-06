//! The MMIO / DMA half of the RTL8852CE (ryzen's wifi card) bring-up, over
//! [`akuma_rtw89`].
//!
//! Stage W1 of `docs/archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md` § 5: power the
//! card on, download its firmware, and see the firmware report itself ready —
//! nothing more. `akuma-rtw89` owns the sequence (replayed in host tests
//! against Linux's register trace of this very card); this module owns what
//! cannot be decided from bytes: the PCI function, the mapped BAR, DMA memory
//! with a known bus address, and the firmware file.
//!
//! Opt-in: the `rtw89` boot token, after the root filesystem is up (the
//! firmware is a file on it). Without the token nothing here touches the card.
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
//! firmware header with `HAXIDMA_ERR_FLAG` set. Linux gives each entry its
//! own 11 KiB buffer; here the 256 entries of a ring share [`RX_BUFS`]
//! buffers in turn, because nothing reads RX yet and 5.6 MiB of `.bss` would
//! buy nothing. x86 is DMA-coherent; the crate fences before each doorbell.
//!
//! # Always ends quiet
//!
//! Success or failure, the card is shut down before this returns: firmware CPU
//! stopped, host DMA stopped, MAC powered off, PCI bus mastering off. W1 has
//! nothing to use running firmware for yet, and a bus master left pointing at
//! this kernel's `.bss` across a warm reset is what crash-looped the xHCI box
//! (`docs/archive/AKUMA_AMD64_XHCI_WEDGED_BOX.md`). Linux on the next boot
//! powers the card on from scratch either way.

use akuma_primitives::addr::virt_to_phys;
use akuma_primitives::mmio::MmioReg;
use akuma_rtw89::bringup::{self, Dma, RING_BYTES, SLOT};
use akuma_rtw89::fw::Source;
use akuma_rtw89::{Bus, h2c};

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
/// Distinct RX buffers behind each RX ring's 256 entries.
const RX_BUFS: usize = 8;
/// Bytes per RX buffer: [`h2c::RX_BUF_SIZE`] rounded up to whole pages.
const RX_BUF_BYTES: usize = 12 * 1024;

#[repr(C, align(4096))]
struct Page<const N: usize>([u8; N]);

static mut RING_MEM: Page<4096> = Page([0; 4096]);
static mut IDLE_MEM: Page<4096> = Page([0; 4096]);
static mut RXQ_MEM: Page<4096> = Page([0; 4096]);
static mut RPQ_MEM: Page<4096> = Page([0; 4096]);
static mut SLOT_MEM: Page<{ SLOT * SLOTS }> = Page([0; SLOT * SLOTS]);
/// RXQ's buffers, then RPQ's.
static mut RX_BUF_MEM: Page<{ RX_BUF_BYTES * RX_BUFS * 2 }> = Page([0; RX_BUF_BYTES * RX_BUFS * 2]);

const _: () = assert!(RING_BYTES <= 4096);
const _: () = assert!(h2c::RX_BUF_SIZE as usize <= RX_BUF_BYTES);

/// BAR2, mapped. Every offset the crate hands in is a register inside the
/// 1 MiB BAR (the largest is `INDIR_ACCESS_ENTRY`, `0x40000`).
struct Regs(usize);

impl Regs {
    fn at<T: Copy>(&self, off: u32) -> MmioReg<T> {
        // SAFETY: `self.0` is the device-mapped BAR2 of the RTL8852CE (mapped by
        // `init` for the BAR's full probed size), and `off` is one of the
        // register offsets `akuma_rtw89::regs` names, all inside it and
        // naturally aligned for their width.
        unsafe { MmioReg::<T>::new(self.0 + off as usize) }
    }
}

impl Bus for Regs {
    fn read8(&mut self, off: u32) -> u8 {
        self.at::<u8>(off).read()
    }
    fn read16(&mut self, off: u32) -> u16 {
        self.at::<u16>(off).read()
    }
    fn read32(&mut self, off: u32) -> u32 {
        self.at::<u32>(off).read()
    }
    fn write8(&mut self, off: u32, v: u8) {
        self.at::<u8>(off).write(v);
    }
    fn write16(&mut self, off: u32, v: u16) {
        self.at::<u16>(off).write(v);
    }
    fn write32(&mut self, off: u32, v: u32) {
        self.at::<u32>(off).write(v);
    }
    fn delay_us(&mut self, us: u32) {
        spin_us(u64::from(us));
    }
}

/// The firmware file, read in place through the VFS — never loaded whole.
struct File {
    path: &'static str,
    len: u32,
}

impl Source for File {
    fn len(&self) -> u32 {
        self.len
    }
    fn read_at(&mut self, off: u32, buf: &mut [u8]) -> bool {
        let mut done = 0;
        while done < buf.len() {
            match crate::fs::read_at(self.path, off as usize + done, &mut buf[done..]) {
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
/// and shut it down again.
pub fn init(cmdline: &str) {
    if !cmdline.split_ascii_whitespace().any(|t| t == "rtw89") {
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
        let size = crate::fs::metadata(path).ok()?.size;
        Some(File { path, len: u32::try_from(size).ok()? })
    }) else {
        say("no firmware: /lib/firmware/rtw89/rtw8852c_fw-1.bin missing (overlays/ryzen/fetch-firmware.sh)");
        return;
    };
    serial::puts("[rtw] firmware ");
    serial::puts(file.path);
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
    let Some(va) = pci::map_bar(bar, size.max(0x10_0000)) else {
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
    let mut regs = Regs(va as usize);
    // What the card arrived in, before a single write.
    say_hex("arrived: IC_PWR_STATE", u64::from(regs.read32(akuma_rtw89::regs::IC_PWR_STATE)));
    say_hex("arrived: WCPU_FW_CTRL", u64::from(regs.read32(akuma_rtw89::regs::WCPU_FW_CTRL)));

    // SAFETY: the DMA contract above — boot CPU only, once, before `init`;
    // nothing else names these statics.
    let (ring, slots) = unsafe {
        (
            core::slice::from_raw_parts_mut((&raw mut RING_MEM).cast::<u8>(), 4096),
            core::slice::from_raw_parts_mut((&raw mut SLOT_MEM).cast::<u8>(), SLOT * SLOTS),
        )
    };
    let phys = |p: *const u8| virt_to_phys(p as usize) as u64;
    // Stock both RX rings: entry i of queue q points at buffer i % RX_BUFS.
    let bufs_phys = phys((&raw const RX_BUF_MEM).cast());
    for (q, ring_mem) in [(&raw mut RXQ_MEM), (&raw mut RPQ_MEM)].into_iter().enumerate() {
        // SAFETY: as for `RING_MEM` above.
        let ring_bytes = unsafe { core::slice::from_raw_parts_mut(ring_mem.cast::<u8>(), RING_BYTES) };
        for (i, entry) in ring_bytes.as_chunks_mut::<{ h2c::BD_LEN }>().0.iter_mut().enumerate() {
            let buf = bufs_phys + ((q * RX_BUFS + i % RX_BUFS) * RX_BUF_BYTES) as u64;
            entry.copy_from_slice(&h2c::rx_bd(buf, h2c::RX_BUF_SIZE));
        }
    }
    let mut dma = Dma {
        ring_phys: phys(ring.as_ptr()),
        slots_phys: phys(slots.as_ptr()),
        ring,
        slots,
        idle_phys: phys((&raw const IDLE_MEM).cast()),
        rx_phys: [phys((&raw const RXQ_MEM).cast()), phys((&raw const RPQ_MEM).cast())],
    };

    let t0 = crate::polltime::tsc();
    let result = bringup::bring_up(&mut regs, &mut dma, &mut file, &mut |what: &'static str, v: u32| {
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
    }
}
