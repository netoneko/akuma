//! The MMIO / DMA half of the xHCI driver, over [`akuma_xhci`].
//!
//! `akuma-xhci` is the pure logic — register offsets, TRB encode/decode, the
//! ring cycle-bit bookkeeping, the context builders — host-tested against
//! register values read off `00:14.0` on the reference machine. This module is
//! the two things that logic cannot be: the `unsafe` MMIO on a mapped BAR, and
//! the DMA memory with a known physical address.
//!
//! # The DMA contract
//!
//! Every structure the controller reads or writes — the DCBAA, the scratchpad
//! array and its pages, the command ring, the event ring and its segment table,
//! the device and input contexts, the three transfer rings, and the transfer
//! buffers — lives in a `.bss` static here, the same place the virtio and
//! rtl8169 paths keep their DMA memory, translated the same way
//! (`akuma_primitives::addr::virt_to_phys`, which knows the kernel-image
//! window). There is no IOMMU on this target, so a physical address is a bus
//! address. The kernel image is loaded below 4 GiB, so every one of these
//! addresses fits 32 bits even though `HCCPARAMS1.AC64` is 1.
//!
//! Every access to that memory goes through the typed accessors below, so the
//! `unsafe` and the aliasing obligation are stated once, not at each call site.
//! All accessors are reached under the `XHCI` lock and bring-up is
//! single-threaded, so the `&'static mut` they hand out is not actually aliased
//! — the same discipline `akuma-net-nic`'s rtl8169 glue keeps.
//!
//! Ownership words (a TRB's cycle bit, a doorbell) are written **last**, after a
//! `compiler_fence(SeqCst)`. x86 is cache-coherent with DMA, so no flushes.
//!
//! # Polled, not interrupt-driven
//!
//! `USBCMD.INTE` and `IMAN.IE` are set so the event ring is maintained, but the
//! driver polls it — taking the MSI needs IOAPIC routing, which `blk.rs` also
//! defers, and none of it is needed to read a sector.
//!
//! # One device
//!
//! On this box `XUSB2PRM = 0`, so xHCI only ever sees SuperSpeed devices, and
//! the USB-to-SATA enclosure is the only one. One slot, one BOT interface, two
//! bulk endpoints — the statics are singular, not arrays.

use core::sync::atomic::{Ordering, compiler_fence};

use akuma_primitives::addr::virt_to_phys;
use akuma_primitives::mmio::MmioReg;
use akuma_selftest::Suite;
use akuma_usb::descriptor::{self, TransferType};
use akuma_usb_storage::{Cbw, Csw, CswStatus, Direction, cdb};
use akuma_xhci::context::{self, EndpointConfig, EpType, SlotConfig};
use akuma_xhci::regs::{self, CapabilityRegisters, PortSc, crcr, intr, op, rt, usbcmd, usbsts};
use akuma_xhci::trb::{self, ConsumerRing, ControlDir, Event, ProducerRing, cc};
use akuma_xhci::{Speed, xcap};
use spinning_top::Spinlock;

use crate::pci;
use crate::serial;

// ===========================================================================
// DMA memory
// ===========================================================================

const CMD_TRBS: usize = 16;
const EVENT_TRBS: usize = 64;
const XFER_TRBS: usize = 16;
/// Max bytes per BOT data phase — 128 512-byte sectors. 64 KiB, aligned to
/// 64 KiB so a single Normal TRB over the whole buffer never crosses the 64 KiB
/// boundary xHCI §4.11.2.3 forbids.
const BOUNCE_LEN: usize = 64 * 1024;
/// Reserved scratchpad pages — must be `>= HCSPARAMS2.MaxScratchpadBufs` (16 on
/// the box).
const SCRATCH_PAGES: usize = 32;

#[repr(C, align(4096))]
struct Aligned4K<const N: usize>([u8; N]);
#[repr(C, align(64))]
struct Aligned64<const N: usize>([u8; N]);
#[repr(C, align(64))]
struct Trbs<const N: usize>([[u32; 4]; N]);
#[repr(C, align(64))]
struct U64s<const N: usize>([u64; N]);

static mut DCBAA: U64s<64> = U64s([0; 64]);
static mut SCRATCH_ARR: Aligned64<{ SCRATCH_PAGES * 8 }> = Aligned64([0; SCRATCH_PAGES * 8]);
static mut SCRATCH_MEM: Aligned4K<{ SCRATCH_PAGES * 4096 }> = Aligned4K([0; SCRATCH_PAGES * 4096]);
static mut CMD_RING: Trbs<CMD_TRBS> = Trbs([[0; 4]; CMD_TRBS]);
static mut EVENT_RING: Trbs<EVENT_TRBS> = Trbs([[0; 4]; EVENT_TRBS]);
static mut ERST: Trbs<1> = Trbs([[0; 4]; 1]);
static mut DEV_CTX: Aligned64<2048> = Aligned64([0; 2048]);
static mut INPUT_CTX: Aligned64<2048> = Aligned64([0; 2048]);
static mut EP0_RING: Trbs<XFER_TRBS> = Trbs([[0; 4]; XFER_TRBS]);
static mut BULK_IN_RING: Trbs<XFER_TRBS> = Trbs([[0; 4]; XFER_TRBS]);
static mut BULK_OUT_RING: Trbs<XFER_TRBS> = Trbs([[0; 4]; XFER_TRBS]);
static mut CTRL_BUF: Aligned4K<512> = Aligned4K([0; 512]);
static mut CBW_BUF: Aligned64<64> = Aligned64([0; 64]);
static mut CSW_BUF: Aligned64<64> = Aligned64([0; 64]);
/// 4 KiB-aligned; the data phase splits at the 64 KiB boundary it may straddle
/// (`trb::data_trbs`), so page alignment is enough — `.bss` cannot promise more.
static mut BOUNCE_BUF: Aligned4K<BOUNCE_LEN> = Aligned4K([0; BOUNCE_LEN]);

fn phys_of<T>(p: *const T) -> u64 {
    virt_to_phys(p as usize) as u64
}

/// Which TRB-array static a ring accessor / phys lookup refers to.
#[derive(Clone, Copy)]
enum Ring {
    Cmd,
    Event,
    Erst,
    Ep0,
    BulkIn,
    BulkOut,
}

fn ring_ptr(r: Ring) -> (*mut [u32; 4], usize) {
    match r {
        Ring::Cmd => ((&raw mut CMD_RING).cast(), CMD_TRBS),
        Ring::Event => ((&raw mut EVENT_RING).cast(), EVENT_TRBS),
        Ring::Erst => ((&raw mut ERST).cast(), 1),
        Ring::Ep0 => ((&raw mut EP0_RING).cast(), XFER_TRBS),
        Ring::BulkIn => ((&raw mut BULK_IN_RING).cast(), XFER_TRBS),
        Ring::BulkOut => ((&raw mut BULK_OUT_RING).cast(), XFER_TRBS),
    }
}

fn ring_mut(r: Ring) -> &'static mut [[u32; 4]] {
    let (p, n) = ring_ptr(r);
    // SAFETY: `p` is a live `.bss` DMA static of `[[u32; 4]; n]`; lock held.
    unsafe { core::slice::from_raw_parts_mut(p, n) }
}

fn ring_phys(r: Ring) -> u64 {
    phys_of(ring_ptr(r).0)
}

macro_rules! dma_buf {
    ($name:ident, $name_mut:ident, $static:ident, $len:expr) => {
        fn $name() -> &'static [u8] {
            // SAFETY: `$static` is a live `.bss` DMA static of `$len` bytes.
            unsafe { core::slice::from_raw_parts((&raw const $static).cast::<u8>(), $len) }
        }
        #[allow(dead_code)]
        fn $name_mut() -> &'static mut [u8] {
            // SAFETY: as `$name`; lock held.
            unsafe { core::slice::from_raw_parts_mut((&raw mut $static).cast::<u8>(), $len) }
        }
    };
}

dma_buf!(input_ctx, input_ctx_mut, INPUT_CTX, 2048);
dma_buf!(scratch_arr, scratch_arr_mut, SCRATCH_ARR, SCRATCH_PAGES * 8);
dma_buf!(ctrl_buf, ctrl_buf_mut, CTRL_BUF, 512);
dma_buf!(cbw_buf, cbw_buf_mut, CBW_BUF, 64);
dma_buf!(csw_buf, csw_buf_mut, CSW_BUF, 64);
dma_buf!(bounce, bounce_mut, BOUNCE_BUF, BOUNCE_LEN);

fn dcbaa_mut() -> &'static mut [u64] {
    // SAFETY: `DCBAA` is a live `.bss` DMA static of 64 `u64`s; lock held.
    unsafe { core::slice::from_raw_parts_mut((&raw mut DCBAA).cast::<u64>(), 64) }
}

// ===========================================================================
// Timing
// ===========================================================================

/// Busy-wait `us` microseconds. Assumes a >= 1 GHz TSC (every x86_64 part), so
/// it over-waits on a fast box rather than under-waiting a reset poll.
fn spin_us(us: u64) {
    let target = us * 1000;
    // SAFETY: RDTSC is unprivileged and present on all x86_64.
    let start = unsafe { core::arch::x86_64::_rdtsc() };
    while unsafe { core::arch::x86_64::_rdtsc() }.wrapping_sub(start) < target {
        core::hint::spin_loop();
    }
}

fn tsc() -> u64 {
    // SAFETY: as `spin_us`.
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// One-second poll budget, in TSC ticks (assuming >= 1 GHz).
const BUDGET: u64 = 1_000_000_000;

fn puthex(label: &str, v: u32) {
    serial::puts(label);
    serial::puts("0x");
    serial::put_hexn(u64::from(v), 8);
    serial::puts("\n");
}

// ===========================================================================
// The controller
// ===========================================================================

struct Xhci {
    op: usize,
    db: usize,
    rt: usize,
    context_bytes: usize,

    cmd: ProducerRing,
    events: ConsumerRing,
    events_phys: u64,

    slot: u8,
    port: u8,

    ep0: ProducerRing,
    bulk_in: ProducerRing,
    bulk_in_dci: u8,
    bulk_out: ProducerRing,
    bulk_out_dci: u8,

    block_len: u32,
    block_count: u64,
    tag: u32,
}

static XHCI: Spinlock<Option<Xhci>> = Spinlock::new(None);

fn r32(base: usize, off: usize) -> u32 {
    // SAFETY: `base` is the mapped xHCI BAR window; `off` is inside a register
    // block bounded by CAPLENGTH / DBOFF / RTSOFF.
    unsafe { MmioReg::<u32>::new(base + off).read() }
}
fn w32(base: usize, off: usize, v: u32) {
    // SAFETY: as `r32`.
    unsafe { MmioReg::<u32>::new(base + off).write(v) }
}
fn w64(base: usize, off: usize, v: u64) {
    // SAFETY: as `r32`; DCBAAP / CRCR / ERSTBA / ERDP take one 64-bit access.
    unsafe { MmioReg::<u64>::new(base + off).write(v) }
}

impl Xhci {
    /// Enqueue a command TRB, ring the command doorbell, wait for its
    /// completion event. Returns `(completion_code, event slot id)`.
    fn command(&mut self, t: [u32; 4], what: &str) -> Result<(u8, u8), &'static str> {
        let e = self.cmd.enqueue(t, ring_phys(Ring::Cmd));
        let r = ring_mut(Ring::Cmd);
        r[e.index] = e.trb;
        if let Some(link) = e.link {
            r[CMD_TRBS - 1] = link;
        }
        compiler_fence(Ordering::SeqCst);
        w32(self.db, regs::db::doorbell(0), regs::db::COMMAND_RING_TARGET);

        let start = tsc();
        loop {
            match self.next_event() {
                Some(Event::CommandCompletion { completion_code, slot, trb_pointer }) => {
                    if trb_pointer == e.trb_phys {
                        return Ok((completion_code, slot));
                    }
                }
                Some(Event::PortStatusChange { .. }) | None => {}
                Some(_) => {}
            }
            if tsc().wrapping_sub(start) > BUDGET {
                serial::puts("  [xhci] timeout: ");
                serial::puts(what);
                serial::puts("\n");
                return Err("xhci command timeout");
            }
            spin_us(20);
        }
    }

    /// Dequeue one event, advancing `ERDP`.
    fn next_event(&mut self) -> Option<Event> {
        let idx = self.events.dequeue_index();
        let raw = ring_mut(Ring::Event)[idx];
        let ev = self.events.poll(raw)?;
        let new_idx = self.events.dequeue_index();
        let erdp = (self.events_phys + (new_idx as u64) * 16) | intr::ERDP_EHB;
        w64(self.rt + rt::interrupter(0), intr::ERDP, erdp);
        Some(ev)
    }

    /// Push `td` onto a transfer ring, ring the slot doorbell for `dci`, wait
    /// for the last TRB's Transfer Event. Returns `(completion_code, bytes moved)`.
    fn transfer(
        &mut self,
        ring: Ring,
        pr: RingField,
        dci: u8,
        td: &[[u32; 4]],
        requested: u32,
        what: &str,
    ) -> Result<(u8, u32), &'static str> {
        let base = ring_phys(ring);
        let mut last_phys = 0u64;
        for &t in td {
            let e = self.producer(pr).enqueue(t, base);
            last_phys = e.trb_phys;
            let r = ring_mut(ring);
            r[e.index] = e.trb;
            if let Some(link) = e.link {
                r[XFER_TRBS - 1] = link;
            }
        }
        compiler_fence(Ordering::SeqCst);
        w32(self.db, regs::db::doorbell(usize::from(self.slot)), regs::db::endpoint_target(dci));

        let start = tsc();
        loop {
            if let Some(Event::Transfer {
                completion_code, slot, endpoint_dci, residual, trb_pointer, ..
            }) = self.next_event()
                && slot == self.slot
                && endpoint_dci == dci
                && trb_pointer == last_phys
            {
                return Ok((completion_code, requested.saturating_sub(residual)));
            }
            if tsc().wrapping_sub(start) > BUDGET {
                serial::puts("  [xhci] transfer timeout: ");
                serial::puts(what);
                serial::puts("\n");
                return Err("xhci transfer timeout");
            }
            spin_us(10);
        }
    }

    fn producer(&mut self, f: RingField) -> &mut ProducerRing {
        match f {
            RingField::Ep0 => &mut self.ep0,
            RingField::BulkIn => &mut self.bulk_in,
            RingField::BulkOut => &mut self.bulk_out,
        }
    }

    /// A control transfer on EP0. For an IN transfer the result is left in
    /// `CTRL_BUF`; returns the bytes moved.
    fn control(
        &mut self,
        bm_request_type: u8,
        b_request: u8,
        w_value: u16,
        w_index: u16,
        w_length: u16,
    ) -> Result<u32, &'static str> {
        let dir = if w_length == 0 {
            ControlDir::NoData
        } else if bm_request_type & 0x80 != 0 {
            ControlDir::In
        } else {
            ControlDir::Out
        };
        let pkt = trb::setup_packet(bm_request_type, b_request, w_value, w_index, w_length);
        let mut td: [[u32; 4]; 3] = [[0; 4]; 3];
        let mut n = 0;
        td[n] = trb::setup_stage(pkt, dir);
        n += 1;
        if w_length != 0 {
            let buf_phys = phys_of(ctrl_buf().as_ptr());
            td[n] = trb::data_stage(buf_phys, u32::from(w_length), dir, false);
            n += 1;
        }
        td[n] = trb::status_stage(dir, true);
        n += 1;

        let (code, moved) =
            self.transfer(Ring::Ep0, RingField::Ep0, 1, &td[..n], u32::from(w_length), "control")?;
        if code != cc::SUCCESS && code != cc::SHORT_PACKET {
            puthex("  [xhci] control cc=", u32::from(code));
            return Err("control transfer error");
        }
        Ok(moved)
    }
}

#[derive(Clone, Copy)]
enum RingField {
    Ep0,
    BulkIn,
    BulkOut,
}

// ===========================================================================
// Bring-up
// ===========================================================================

/// Probe, reset, run the controller, enumerate the one SuperSpeed device, and
/// configure its bulk endpoints. `Ok(())` leaves the block device usable.
pub fn init() -> Result<(), &'static str> {
    if XHCI.lock().is_some() {
        return Ok(());
    }

    let dev = pci::find_class(0x0c, 0x03)
        .filter(|d| d.header.prog_if == 0x30)
        .ok_or("no xHCI controller")?;
    let bar = dev.bars[0].ok_or("xHCI BAR0 missing")?;
    let (size, _) = pci::probe_bar_size(dev.addr, 0);
    let bar_va = pci::map_bar(bar, size.max(0x1_0000)).ok_or("could not map xHCI BAR")? as usize;
    pci::enable(dev.addr, true);

    let mut cap_bytes = [0u8; 0x20];
    for (i, b) in cap_bytes.iter_mut().enumerate() {
        // SAFETY: `bar_va` is the mapped BAR; byte `i` is in the cap block.
        *b = unsafe { MmioReg::<u8>::new(bar_va + i).read() };
    }
    let caps = CapabilityRegisters::parse(&cap_bytes).ok_or("xHCI cap block unreadable")?;
    let op = bar_va + caps.operational_base();
    let db = bar_va + caps.db_offset as usize;
    let rt = bar_va + caps.rts_offset as usize;
    let context_bytes = caps.hcc_params1.context_bytes();
    let max_slots = caps.hcs_params1.max_slots();
    let max_ports = caps.hcs_params1.max_ports();
    let scratch_needed = caps.hcs_params2.max_scratchpad_buffers() as usize;

    serial::puts("  [xhci] v");
    serial::put_hexn(u64::from(caps.hci_version), 4);
    serial::puts(" slots=");
    serial::put_dec(u64::from(max_slots));
    serial::puts(" ports=");
    serial::put_dec(u64::from(max_ports));
    serial::puts(" ctx=");
    serial::put_dec(context_bytes as u64);
    serial::puts("B scratch=");
    serial::put_dec(scratch_needed as u64);
    serial::puts("\n");
    if scratch_needed > SCRATCH_PAGES {
        return Err("xHCI wants more scratchpad than reserved");
    }

    bios_handoff(bar_va, caps.hcc_params1.ext_cap_offset());

    // --- reset ---
    wait_cnr_clear(op)?;
    let cmd = r32(op, op::USBCMD);
    if cmd & usbcmd::RS != 0 {
        w32(op, op::USBCMD, cmd & !usbcmd::RS);
        let s = tsc();
        while r32(op, op::USBSTS) & usbsts::HCH == 0 {
            if tsc().wrapping_sub(s) > BUDGET {
                return Err("xHCI would not halt");
            }
            spin_us(100);
        }
    }
    w32(op, op::USBCMD, usbcmd::HCRST);
    let s = tsc();
    loop {
        if r32(op, op::USBCMD) & usbcmd::HCRST == 0 && r32(op, op::USBSTS) & usbsts::CNR == 0 {
            break;
        }
        if tsc().wrapping_sub(s) > BUDGET {
            return Err("xHCI reset timeout");
        }
        spin_us(100);
    }
    serial::puts("  [xhci] reset ok\n");

    // --- lay out DMA memory ---
    dcbaa_mut().fill(0);
    for r in [Ring::Cmd, Ring::Event, Ring::Erst, Ring::Ep0, Ring::BulkIn, Ring::BulkOut] {
        for t in ring_mut(r) {
            *t = [0; 4];
        }
    }
    input_ctx_mut().fill(0);
    // SAFETY: `DEV_CTX` is a live `.bss` DMA static; lock held.
    unsafe { core::slice::from_raw_parts_mut((&raw mut DEV_CTX).cast::<u8>(), 2048).fill(0) };

    let dcbaa_phys = phys_of(dcbaa_mut().as_ptr());
    let cmd_phys = ring_phys(Ring::Cmd);
    let events_phys = ring_phys(Ring::Event);
    let erst_phys = ring_phys(Ring::Erst);

    if scratch_needed > 0 {
        for i in 0..scratch_needed {
            // SAFETY: `SCRATCH_MEM` is a live `.bss` static; `i < SCRATCH_PAGES`.
            let page = phys_of(unsafe { (&raw const SCRATCH_MEM).cast::<u8>().add(i * 4096) });
            scratch_arr_mut()[i * 8..i * 8 + 8].copy_from_slice(&page.to_le_bytes());
        }
        dcbaa_mut()[0] = phys_of(scratch_arr().as_ptr());
    }

    ring_mut(Ring::Cmd)[CMD_TRBS - 1] = trb::link(cmd_phys, true);
    ring_mut(Ring::Erst)[0] = regs::erst_entry(events_phys, EVENT_TRBS as u16);
    ring_mut(Ring::Ep0)[XFER_TRBS - 1] = trb::link(ring_phys(Ring::Ep0), true);
    ring_mut(Ring::BulkIn)[XFER_TRBS - 1] = trb::link(ring_phys(Ring::BulkIn), true);
    ring_mut(Ring::BulkOut)[XFER_TRBS - 1] = trb::link(ring_phys(Ring::BulkOut), true);

    // --- program & run ---
    w32(op, op::CONFIG, regs::config_max_slots_en(max_slots));
    w64(op, op::DCBAAP, dcbaa_phys);
    w64(op, op::CRCR, (cmd_phys & crcr::PTR_MASK) | crcr::RCS);

    let intr0 = rt + rt::interrupter(0);
    w32(intr0, intr::ERSTSZ, 1);
    w64(intr0, intr::ERDP, events_phys | intr::ERDP_EHB);
    w64(intr0, intr::ERSTBA, erst_phys);
    w32(intr0, intr::IMAN, intr::IMAN_IE | intr::IMAN_IP);

    compiler_fence(Ordering::SeqCst);
    w32(op, op::USBCMD, usbcmd::RS | usbcmd::INTE | usbcmd::HSEE);

    let s = tsc();
    while r32(op, op::USBSTS) & usbsts::HCH != 0 {
        if tsc().wrapping_sub(s) > BUDGET {
            return Err("xHCI would not start");
        }
        spin_us(100);
    }
    serial::puts("  [xhci] running\n");

    let mut x = Xhci {
        op,
        db,
        rt,
        context_bytes,
        cmd: ProducerRing::new(CMD_TRBS),
        events: ConsumerRing::new(EVENT_TRBS),
        events_phys,
        slot: 0,
        port: 0,
        ep0: ProducerRing::new(XFER_TRBS),
        bulk_in: ProducerRing::new(XFER_TRBS),
        bulk_in_dci: 0,
        bulk_out: ProducerRing::new(XFER_TRBS),
        bulk_out_dci: 0,
        block_len: 512,
        block_count: 0,
        tag: 1,
    };

    // --- prove the command / event loop ---
    let (code, _) = x.command(trb::no_op_command(), "no-op")?;
    if code != cc::SUCCESS {
        puthex("  [xhci] no-op cc=", u32::from(code));
        return Err("xHCI command ring dead");
    }
    serial::puts("  [xhci] command ring ok\n");

    x.port = find_and_reset_port(op, max_ports)?;
    enumerate(&mut x)?;
    read_capacity(&mut x)?;

    serial::puts("  [xhci] disk: ");
    serial::put_dec(x.block_count);
    serial::puts(" x ");
    serial::put_dec(u64::from(x.block_len));
    serial::puts("B = ");
    serial::put_dec(x.block_count * u64::from(x.block_len) / (1024 * 1024));
    serial::puts(" MiB\n");

    *XHCI.lock() = Some(x);
    Ok(())
}

fn wait_cnr_clear(op: usize) -> Result<(), &'static str> {
    let s = tsc();
    while r32(op, op::USBSTS) & usbsts::CNR != 0 {
        if tsc().wrapping_sub(s) > BUDGET {
            return Err("xHCI CNR never cleared");
        }
        spin_us(100);
    }
    Ok(())
}

fn bios_handoff(bar_va: usize, mut off: usize) {
    if off == 0 {
        return;
    }
    for _ in 0..32 {
        // SAFETY: `bar_va` mapped; `off` is a dword offset inside the BAR.
        let hdr = unsafe { MmioReg::<u32>::new(bar_va + off).read() };
        if xcap::cap_id(hdr) == xcap::CAP_ID_LEGACY_SUPPORT {
            let leg = xcap::UsbLegSup(hdr);
            if !leg.handoff_complete() {
                // SAFETY: as above.
                unsafe { MmioReg::<u32>::new(bar_va + off).write(leg.claiming_for_os()) };
                let s = tsc();
                loop {
                    // SAFETY: as above.
                    let now = xcap::UsbLegSup(unsafe { MmioReg::<u32>::new(bar_va + off).read() });
                    if now.handoff_complete() || tsc().wrapping_sub(s) > BUDGET {
                        break;
                    }
                    spin_us(1000);
                }
            }
            let ctl = bar_va + off + xcap::USBLEGCTLSTS_OFFSET;
            // SAFETY: as above.
            let v = unsafe { MmioReg::<u32>::new(ctl).read() };
            // SAFETY: as above.
            unsafe { MmioReg::<u32>::new(ctl).write(xcap::usblegctlsts_disable_all(v)) };
            serial::puts("  [xhci] BIOS handoff ok\n");
            return;
        }
        match xcap::next_cap_offset(off, hdr) {
            Some(n) => off = n,
            None => return,
        }
    }
}

fn find_and_reset_port(op: usize, max_ports: u8) -> Result<u8, &'static str> {
    let mut found = 0u8;
    for p in 1..=max_ports {
        let psc = PortSc(r32(op, op::portsc(p)));
        if psc.connected() {
            serial::puts("  [xhci] port ");
            serial::put_dec(u64::from(p));
            puthex(" PORTSC=", psc.0);
            found = p;
            break;
        }
    }
    if found == 0 {
        return Err("no connected xHCI port");
    }

    let psc = PortSc(r32(op, op::portsc(found)));
    if !psc.enabled() {
        w32(op, op::portsc(found), psc.with_warm_reset_asserted());
        let s = tsc();
        loop {
            let now = PortSc(r32(op, op::portsc(found)));
            if now.enabled() && !now.resetting() {
                break;
            }
            if tsc().wrapping_sub(s) > BUDGET {
                return Err("xHCI port reset timeout");
            }
            spin_us(1000);
        }
        let now = PortSc(r32(op, op::portsc(found)));
        w32(op, op::portsc(found), now.acknowledging_reset());
    }
    spin_us(20_000); // settle (USB 2.0 §7.1.7.3)

    let psc = PortSc(r32(op, op::portsc(found)));
    serial::puts("  [xhci] port ");
    serial::put_dec(u64::from(found));
    serial::puts(" enabled, speed ");
    serial::put_dec(u64::from(psc.speed_field()));
    serial::puts("\n");
    Ok(found)
}

fn enumerate(x: &mut Xhci) -> Result<(), &'static str> {
    let psc = PortSc(r32(x.op, op::portsc(x.port)));
    let speed = Speed::from_field(psc.speed_field()).ok_or("unknown port speed")?;
    let ep0_mps = speed.default_ep0_max_packet();

    // --- Enable Slot ---
    let (code, slot) = x.command(trb::enable_slot(0), "enable slot")?;
    if code != cc::SUCCESS || slot == 0 || u32::from(slot) >= 64 {
        return Err("Enable Slot failed");
    }
    x.slot = slot;
    serial::puts("  [xhci] slot ");
    serial::put_dec(u64::from(slot));
    serial::puts("\n");
    // `DEV_CTX` is a live `.bss` DMA static; `&raw` needs no `unsafe`.
    dcbaa_mut()[usize::from(slot)] = phys_of((&raw const DEV_CTX).cast::<u8>());

    // --- Address Device (add slot ctx + EP0) ---
    write_input_context(
        x.context_bytes,
        context::add_flag(0) | context::add_flag(1),
        SlotConfig { route_string: 0, speed: speed as u8, root_hub_port: x.port, context_entries: 1 },
        &[(
            1,
            EndpointConfig {
                ep_type: EpType::Control,
                max_packet_size: ep0_mps,
                max_burst: 0,
                tr_dequeue_phys: ring_phys(Ring::Ep0),
                dequeue_cycle: true,
                average_trb_length: 8,
            },
        )],
        0,
    );
    let input_phys = phys_of(input_ctx().as_ptr());
    let (code, _) = x.command(trb::address_device(input_phys, slot, false), "address device")?;
    if code != cc::SUCCESS {
        puthex("  [xhci] Address Device cc=", u32::from(code));
        return Err("Address Device failed");
    }
    serial::puts("  [xhci] addressed\n");

    // --- descriptors ---
    x.control(0x80, 6, 0x0100, 0, 18)?;
    if descriptor::DeviceDescriptor::parse(&ctrl_buf()[..18]).is_none() {
        return Err("bad device descriptor");
    }
    x.control(0x80, 6, 0x0200, 0, 9)?;
    let total = {
        let b = ctrl_buf();
        u16::from_le_bytes([b[2], b[3]])
    };
    let want = total.min(512);
    x.control(0x80, 6, 0x0200, 0, want)?;
    let mut cfg = [0u8; 512];
    cfg[..usize::from(want)].copy_from_slice(&ctrl_buf()[..usize::from(want)]);

    let (config_value, bin, bout) = parse_bot_endpoints(&cfg[..usize::from(want)])?;
    serial::puts("  [xhci] BOT ep IN=0x");
    serial::put_hexn(u64::from(bin.address), 2);
    serial::puts(" OUT=0x");
    serial::put_hexn(u64::from(bout.address), 2);
    serial::puts("\n");

    x.control(0x00, 9, u16::from(config_value), 0, 0)?;

    // --- Configure Endpoint ---
    x.bulk_in_dci = context::dci(bin.address);
    x.bulk_out_dci = context::dci(bout.address);
    let max_dci = x.bulk_in_dci.max(x.bulk_out_dci);
    write_input_context(
        x.context_bytes,
        context::add_flag(0)
            | context::add_flag(1)
            | context::add_flag(x.bulk_in_dci)
            | context::add_flag(x.bulk_out_dci),
        SlotConfig {
            route_string: 0,
            speed: speed as u8,
            root_hub_port: x.port,
            context_entries: max_dci,
        },
        &[
            (
                1,
                EndpointConfig {
                    ep_type: EpType::Control,
                    max_packet_size: ep0_mps,
                    max_burst: 0,
                    tr_dequeue_phys: ring_phys(Ring::Ep0),
                    dequeue_cycle: true,
                    average_trb_length: 8,
                },
            ),
            (
                x.bulk_in_dci,
                EndpointConfig {
                    ep_type: EpType::BulkIn,
                    max_packet_size: bin.max_packet,
                    max_burst: bin.max_burst,
                    tr_dequeue_phys: ring_phys(Ring::BulkIn),
                    dequeue_cycle: true,
                    average_trb_length: 3072,
                },
            ),
            (
                x.bulk_out_dci,
                EndpointConfig {
                    ep_type: EpType::BulkOut,
                    max_packet_size: bout.max_packet,
                    max_burst: bout.max_burst,
                    tr_dequeue_phys: ring_phys(Ring::BulkOut),
                    dequeue_cycle: true,
                    average_trb_length: 3072,
                },
            ),
        ],
        config_value,
    );
    let (code, _) =
        x.command(trb::configure_endpoint(input_phys, slot, false), "configure endpoint")?;
    if code != cc::SUCCESS {
        puthex("  [xhci] Configure Endpoint cc=", u32::from(code));
        return Err("Configure Endpoint failed");
    }
    serial::puts("  [xhci] endpoints configured\n");
    Ok(())
}

struct BulkEp {
    address: u8,
    max_packet: u16,
    max_burst: u8,
}

fn parse_bot_endpoints(cfg: &[u8]) -> Result<(u8, BulkEp, BulkEp), &'static str> {
    let mut config_value = 1u8;
    let mut in_bot = false;
    let mut bin: Option<BulkEp> = None;
    let mut bout: Option<BulkEp> = None;
    let mut pending: Option<u8> = None;

    for d in descriptor::descriptors(cfg) {
        match d.descriptor_type {
            0x02 => {
                if let Some(c) = descriptor::ConfigurationDescriptor::parse(d.bytes) {
                    config_value = c.configuration_value;
                }
            }
            0x04 => {
                in_bot = descriptor::InterfaceDescriptor::parse(d.bytes).is_some_and(|i| {
                    i.class == 0x08 && i.sub_class == 0x06 && i.protocol == 0x50
                });
            }
            0x05 if in_bot => {
                if let Some(e) = descriptor::EndpointDescriptor::parse(d.bytes)
                    && e.transfer_type() == TransferType::Bulk
                {
                    let mps = u16::from(d.bytes.get(4).copied().unwrap_or(0))
                        | (u16::from(d.bytes.get(5).copied().unwrap_or(0)) << 8);
                    let ep = BulkEp { address: e.address, max_packet: mps & 0x7ff, max_burst: 0 };
                    pending = Some(e.address);
                    if e.direction_in() {
                        bin = Some(ep);
                    } else {
                        bout = Some(ep);
                    }
                }
            }
            0x30 => {
                if let (Some(addr), Some(&burst)) = (pending, d.bytes.get(2)) {
                    for ep in [bin.as_mut(), bout.as_mut()].into_iter().flatten() {
                        if ep.address == addr {
                            ep.max_burst = burst;
                        }
                    }
                    pending = None;
                }
            }
            _ => {}
        }
    }

    match (bin, bout) {
        (Some(i), Some(o)) => Ok((config_value, i, o)),
        _ => Err("BOT interface has no bulk in/out pair"),
    }
}

/// Input Control Context at index 0, Slot Context at index 1, then each
/// `(dci, ep)` at index `dci + 1`.
fn write_input_context(
    context_bytes: usize,
    add_flags: u32,
    slot: SlotConfig,
    eps: &[(u8, EndpointConfig)],
    config_value: u8,
) {
    let ic = input_ctx_mut();
    ic.fill(0);
    let mut write = |i: usize, words: [u32; 8]| {
        let base = context::context_offset(i, context_bytes);
        for (w, word) in words.iter().enumerate() {
            ic[base + w * 4..base + w * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
    };
    write(0, context::input_control_context(add_flags, 0, config_value));
    write(1, slot.build());
    for &(dci, ep) in eps {
        write(usize::from(dci) + 1, ep.build());
    }
}

fn read_capacity(x: &mut Xhci) -> Result<(), &'static str> {
    for _ in 0..20 {
        match bot_small(x, cdb::test_unit_ready(), &mut []) {
            Ok(CswStatus::Passed) => break,
            Ok(_) => {
                let _ = bot_small(x, cdb::request_sense(), &mut [0u8; 18]);
                spin_us(200_000);
            }
            Err(e) => return Err(e),
        }
    }
    let mut cap = [0u8; 8];
    if bot_small(x, cdb::read_capacity_10(), &mut cap)? != CswStatus::Passed {
        return Err("READ CAPACITY failed");
    }
    let rc = akuma_usb_storage::ReadCapacity10::parse(&cap).ok_or("bad READ CAPACITY response")?;
    x.block_len = rc.block_len;
    x.block_count = rc.block_count();
    if x.block_len == 0 || x.block_count == 0 {
        return Err("READ CAPACITY reported an empty device");
    }
    Ok(())
}

// ===========================================================================
// BOT transport
// ===========================================================================

/// Run one BOT command whose data phase, if any, occupies the first `data_len`
/// bytes of `BOUNCE_BUF`. Returns the CSW status and bytes moved in the data
/// phase. The caller stages BOUNCE before an OUT and reads it after an IN.
fn bot_run(
    x: &mut Xhci,
    command: akuma_usb_storage::Command,
    data_len: usize,
) -> Result<(CswStatus, u32), &'static str> {
    let tag = x.tag;
    x.tag = x.tag.wrapping_add(1).max(1);
    let cbw = Cbw { tag, command, lun: 0 };

    cbw_buf_mut()[..31].copy_from_slice(&cbw.encode());
    let cbw_phys = phys_of(cbw_buf().as_ptr());
    let (code, _) = x.transfer(
        Ring::BulkOut,
        RingField::BulkOut,
        x.bulk_out_dci,
        &[trb::normal(cbw_phys, 31, true)],
        31,
        "CBW",
    )?;
    if code != cc::SUCCESS {
        return Err(recover(x, code, "CBW"));
    }

    let mut moved = 0u32;
    if data_len > 0 && command.direction != Direction::None {
        let n = data_len.min(BOUNCE_LEN) as u32;
        let bp = phys_of(bounce().as_ptr());
        let (ring, field, dci, is_in) = match command.direction {
            Direction::In => (Ring::BulkIn, RingField::BulkIn, x.bulk_in_dci, true),
            _ => (Ring::BulkOut, RingField::BulkOut, x.bulk_out_dci, false),
        };
        let (count, td) = trb::data_trbs(bp, n);
        let (code, m) = x.transfer(ring, field, dci, &td[..count], n, "data")?;
        moved = m;
        if code != cc::SUCCESS && !(code == cc::SHORT_PACKET && is_in) {
            return Err(recover(x, code, "data"));
        }
    }

    let csw_phys = phys_of(csw_buf().as_ptr());
    let (code, _) = x.transfer(
        Ring::BulkIn,
        RingField::BulkIn,
        x.bulk_in_dci,
        &[trb::normal(csw_phys, 13, true)],
        13,
        "CSW",
    )?;
    if code != cc::SUCCESS && code != cc::SHORT_PACKET {
        return Err(recover(x, code, "CSW"));
    }
    let csw = Csw::parse(&csw_buf()[..13]).ok_or("CSW signature mismatch — pipe desynced")?;
    if csw.tag != tag {
        return Err("CSW tag mismatch");
    }
    Ok((csw.status, moved))
}

/// A small command whose data fits a caller buffer (`INQUIRY`, `READ CAPACITY`,
/// `REQUEST SENSE`, `TEST UNIT READY`): staged through BOUNCE.
fn bot_small(
    x: &mut Xhci,
    command: akuma_usb_storage::Command,
    data: &mut [u8],
) -> Result<CswStatus, &'static str> {
    if command.direction == Direction::Out {
        let n = data.len().min(command.data_len as usize).min(BOUNCE_LEN);
        bounce_mut()[..n].copy_from_slice(&data[..n]);
    }
    let (status, moved) = bot_run(x, command, command.data_len as usize)?;
    if command.direction == Direction::In {
        let n = (moved as usize).min(data.len()).min(BOUNCE_LEN);
        data[..n].copy_from_slice(&bounce()[..n]);
    }
    Ok(status)
}

/// Diagnostics + best-effort STALL recovery; always returns an error string.
fn recover(x: &mut Xhci, code: u8, phase: &str) -> &'static str {
    puthex("  [xhci] bulk cc=", u32::from(code));
    serial::puts("  [xhci] phase ");
    serial::puts(phase);
    serial::puts("\n");
    if code == cc::STALL_ERROR {
        let _ = x.command(trb::reset_endpoint(x.slot, x.bulk_in_dci), "reset ep in");
        let _ = x.command(trb::reset_endpoint(x.slot, x.bulk_out_dci), "reset ep out");
    }
    "bulk transfer error"
}

// ===========================================================================
// Public block-device surface (mirrors akuma_virtio::block)
// ===========================================================================

#[must_use]
pub fn is_initialized() -> bool {
    XHCI.lock().is_some()
}

/// Capacity in 512-byte sectors (the block device's own unit).
#[must_use]
pub fn capacity_sectors() -> Option<u64> {
    let g = XHCI.lock();
    let x = g.as_ref()?;
    Some(x.block_count * u64::from(x.block_len) / 512)
}

/// Read `buf.len()` bytes from byte `offset` on the whole disk.
pub fn read_bytes(offset: u64, buf: &mut [u8]) -> Result<(), &'static str> {
    let mut g = XHCI.lock();
    let x = g.as_mut().ok_or("xHCI not initialised")?;
    let bl = x.block_len as usize;
    if bl == 0 {
        return Err("no block length");
    }
    let max_blocks = (BOUNCE_LEN / bl).min(u16::MAX as usize);

    let mut done = 0usize;
    while done < buf.len() {
        let cur = offset + done as u64;
        let lba = cur / bl as u64;
        let within = (cur % bl as u64) as usize;
        let remaining = buf.len() - done;
        let blocks = (within + remaining).div_ceil(bl).clamp(1, max_blocks);
        let span = blocks * bl;
        let lba32 = u32::try_from(lba).map_err(|_| "LBA exceeds 32 bits")?;

        if bot_run(x, cdb::read_10(lba32, blocks as u16, x.block_len), span)?.0 != CswStatus::Passed {
            return Err("READ(10) failed");
        }
        let take = (span - within).min(remaining);
        buf[done..done + take].copy_from_slice(&bounce()[within..within + take]);
        done += take;
    }
    Ok(())
}

/// Write `data.len()` bytes at byte `offset` on the whole disk. A partial block
/// at either end is read-modify-written.
pub fn write_bytes(offset: u64, data: &[u8]) -> Result<(), &'static str> {
    let mut g = XHCI.lock();
    let x = g.as_mut().ok_or("xHCI not initialised")?;
    let bl = x.block_len as usize;
    if bl == 0 {
        return Err("no block length");
    }
    let max_blocks = (BOUNCE_LEN / bl).min(u16::MAX as usize);

    let mut done = 0usize;
    while done < data.len() {
        let cur = offset + done as u64;
        let lba = cur / bl as u64;
        let within = (cur % bl as u64) as usize;
        let remaining = data.len() - done;
        let blocks = (within + remaining).div_ceil(bl).clamp(1, max_blocks);
        let span = blocks * bl;
        let lba32 = u32::try_from(lba).map_err(|_| "LBA exceeds 32 bits")?;
        let take = (span - within).min(remaining);

        if (within != 0 || take != span)
            && bot_run(x, cdb::read_10(lba32, blocks as u16, x.block_len), span)?.0
                != CswStatus::Passed
        {
            return Err("RMW read failed");
        }
        bounce_mut()[within..within + take].copy_from_slice(&data[done..done + take]);
        if bot_run(x, cdb::write_10(lba32, blocks as u16, x.block_len), span)?.0 != CswStatus::Passed
        {
            return Err("WRITE(10) failed");
        }
        done += take;
    }
    Ok(())
}

// ===========================================================================
// Self-test
// ===========================================================================

/// `sda1` starts at LBA 2048 (the `fdisk` / `mke2fs` default) — 1 MiB.
pub const SDA1_OFFSET: u64 = 2048 * 512;

/// `true` if a 512-byte MBR sector has the boot signature and partition 1 starts
/// at LBA 2048 — the sanity check before trusting [`SDA1_OFFSET`].
#[must_use]
pub fn mbr_looks_right(sector: &[u8]) -> bool {
    sector.len() >= 512
        && sector[510] == 0x55
        && sector[511] == 0xAA
        && u32::from_le_bytes([sector[454], sector[455], sector[456], sector[457]]) == 2048
}

pub fn smoke_test(t: &mut Suite, present: bool) {
    if !present {
        t.note("xhci: no controller on this machine", 0);
        return;
    }
    if !t.check("xhci: controller + enumeration + BOT bring-up", init().is_ok()) {
        return;
    }
    t.check("xhci: driver registered", is_initialized());
    let sectors = capacity_sectors().unwrap_or(0);
    t.check("xhci: READ CAPACITY reports a non-empty disk", sectors > 0);
    t.note("xhci: disk sectors", sectors);

    let mut mbr = [0u8; 512];
    if t.check("xhci: read the MBR at LBA 0", read_bytes(0, &mut mbr).is_ok()) {
        t.check("xhci: MBR signature + sda1 @ LBA 2048", mbr_looks_right(&mbr));
    }

    let mut sb = [0u8; 512];
    if t.check("xhci: read the sda1 ext2 superblock", read_bytes(SDA1_OFFSET + 1024, &mut sb).is_ok())
    {
        let magic = u16::from_le_bytes([sb[56], sb[57]]);
        t.check_eq("xhci: sda1 superblock magic 0xEF53", u64::from(magic), 0xEF53);
    }

    // WRITE(10) round trip to a scratch LBA well inside sda2 (starts at LBA
    // 134217728) — never sda1.
    let scratch = (134_217_728u64 + 1000) * 512;
    let mut pattern = [0u8; 512];
    for (i, b) in pattern.iter_mut().enumerate() {
        *b = (i as u8) ^ 0x5a;
    }
    if t.check("xhci: WRITE(10) to a scratch LBA in sda2", write_bytes(scratch, &pattern).is_ok()) {
        let mut back = [0u8; 512];
        if t.check("xhci: read the scratch LBA back", read_bytes(scratch, &mut back).is_ok()) {
            t.check("xhci: scratch LBA round-trips", back == pattern);
        }
    }
}
