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
//! The controller runs with `USBCMD.INTE` **clear**, `IMAN.IE` **clear**, and
//! PCI legacy INTx masked (`pci::enable_full(.., mask_intx=true)`). The event
//! ring is maintained regardless of those bits; the driver polls it. This is
//! not just a simplification — an unmasked INTx from a running controller lands
//! on an unhandled IDT vector (no IOAPIC routing on this target) and takes the
//! machine down. A failed bring-up that left the controller running once wedged
//! the metal box across reboots for exactly this reason; `halt_controller` on
//! every error path is the other half of the fix.
//!
//! # One device
//!
//! On this box `XUSB2PRM = 0`, so xHCI only ever sees SuperSpeed devices, and
//! the USB-to-SATA enclosure is the only one. One slot, one BOT interface, two
//! bulk endpoints — the statics are singular, not arrays.

use core::sync::atomic::{Ordering, compiler_fence};

use akuma_primitives::addr::virt_to_phys;
use akuma_primitives::mmio::MmioReg;
#[cfg(not(feature = "no-tests"))]
use akuma_selftest::Suite;
use akuma_usb::descriptor::{self, TransferType};
use akuma_usb_storage::{Cbw, Csw, CswStatus, Direction, cdb};
use akuma_xhci::context::{self, EndpointConfig, EpType, SlotConfig};
use akuma_xhci::regs::{self, CapabilityRegisters, PortSc, crcr, intr, op, rt, usbcmd, usbsts};
use akuma_xhci::trb::{self, ConsumerRing, ControlDir, Event, ProducerRing, cc};
use akuma_xhci::xcap::ProtocolMap;
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
/// `REQUEST SENSE`'s 18 bytes. Its own buffer so asking a failed command why
/// does not overwrite the data (or the `WRITE(10)` payload) the retry needs.
static mut SENSE_BUF: Aligned64<64> = Aligned64([0; 64]);
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
dma_buf!(sense_buf, sense_buf_mut, SENSE_BUF, 64);
dma_buf!(bounce, bounce_mut, BOUNCE_BUF, BOUNCE_LEN);

fn dcbaa_mut() -> &'static mut [u64] {
    // SAFETY: `DCBAA` is a live `.bss` DMA static of 64 `u64`s; lock held.
    unsafe { core::slice::from_raw_parts_mut((&raw mut DCBAA).cast::<u64>(), 64) }
}

// ===========================================================================
// Timing
// ===========================================================================

/// The TSC rate every budget below is written against.
///
/// `lapic::calibrate` measures it across the same 10 ms PIT gate as the LAPIC
/// count, so this driver's "one second" and the scheduler's agree. When no PIT
/// was there to measure against (Firecracker, `microvm`) the fallback is the
/// **fastest** part this kernel could plausibly meet, so a budget is never
/// shorter than asked — the failure that matters here is a slow drive read as
/// dead, not a dead drive read as slow.
///
/// History: the budgets used to be a bare `1_000_000_000` ticks, assuming a
/// TSC of at least 1 GHz. On the trashcan's 3.2 GHz Haswell that "second" was
/// 0.31 s. A drive waking from standby answers in seconds, so the first
/// command after every idle gap "timed out", the recovery ran against a device
/// that was merely busy, and much of the stall cadence on that box was this
/// constant (`docs/archive/AKUMA_AMD64_USB_XHCI.md` § "Clock finding").
const TSC_HZ_FALLBACK: u64 = 4_000_000_000;

fn tsc_hz() -> u64 {
    match crate::lapic::tsc_hz() {
        0 => TSC_HZ_FALLBACK,
        hz => hz,
    }
}

/// TSC ticks in `ms` milliseconds.
fn ticks_ms(ms: u64) -> u64 {
    tsc_hz() / 1000 * ms
}

/// Busy-wait `us` microseconds.
fn spin_us(us: u64) {
    let target = tsc_hz() / 1_000_000 * us;
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

/// Controller commands and bring-up register polls. The controller itself
/// answers in microseconds; a second is generous and keeps a dead controller
/// from holding the boot.
fn command_budget() -> u64 {
    ticks_ms(1_000)
}

/// EP0 control transfers — descriptors, the BOT Mass Storage Reset,
/// `CLEAR_FEATURE`. The device answers these from its bridge firmware with no
/// media involved, but a bridge mid-reset can take a while to come back.
fn control_budget() -> u64 {
    ticks_ms(5_000)
}

/// Bulk phases carry the SCSI command, and a SCSI command may have to spin a
/// platter up: a 2.5" SATA drive leaving standby needs 3-8 s, and the ASMedia
/// bridge in front of it adds its own link wake. Linux's `sd` gives the whole
/// command 30 s. Ten covers spin-up with margin while still failing a genuinely
/// dead disk in a time a human at the console can wait out.
const BULK_BUDGET_MS: u64 = 10_000;

fn bulk_budget() -> u64 {
    ticks_ms(BULK_BUDGET_MS)
}

fn puthex(label: &str, v: u32) {
    serial::puts(label);
    serial::puts("0x");
    serial::put_hexn(u64::from(v), 8);
    serial::puts("\n");
}

/// Name the step that is about to run, before it runs.
///
/// Every step below can take the machine down in a way that leaves no other
/// trace: a config-space write can reset the box, an MMIO read of an unmapped
/// BAR faults, the BIOS handoff hands control to an SMI, and a wrong DMA
/// address makes the controller scribble on the page tables — after which the
/// next fault is a triple fault and the box simply restarts. None of those
/// reach an exception handler, so the **last line printed is the diagnosis**.
/// A step is therefore announced before the operation, never after it.
///
/// `serial::puts` is a polled 16550 write: it has left the UART before this
/// returns, so there is no buffer to lose in a reset.
fn step(what: &str) {
    serial::puts("  [xhci] .. ");
    serial::puts(what);
    serial::puts("\n");
}

fn putphys(label: &str, v: u64) {
    serial::puts(label);
    serial::puts("0x");
    serial::put_hexn(v, 8);
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
    /// The BOT interface's `bInterfaceNumber` — the `wIndex` the Mass Storage
    /// Reset class request needs.
    bot_if: u8,
    tag: u32,
    /// Last endpoint-context state seen for each bulk endpoint (index 0 =
    /// bulk IN, 1 = bulk OUT; `0xff` = nothing seen yet). `note_ep_state`
    /// prints the transition when it changes — the device-state tracking
    /// that would have exposed the `>> 2` decode bug on its first boot.
    ep_state_seen: [u8; 2],
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
            if tsc().wrapping_sub(start) > command_budget() {
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
    /// up to `budget` TSC ticks for the last TRB's Transfer Event. Returns
    /// `(completion_code, bytes moved)`.
    ///
    /// On `Err` the TD is **still live in the ring**. The caller must not
    /// enqueue behind it: the ring is strictly ordered, so the retry would not
    /// run until the dead TD completes, and a late completion would land the
    /// device's next bytes in whichever TRB is at the dequeue — the 2026-09-12
    /// double-TD bug. `recover` aborts it with Stop Endpoint + Set TR Dequeue
    /// Pointer before any retry.
    fn transfer(
        &mut self,
        ring: Ring,
        pr: RingField,
        dci: u8,
        td: &[[u32; 4]],
        requested: u32,
        budget: u64,
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
        // Any event that does not match (slot, dci, trb_pointer) is discarded,
        // which makes "no event arrived" and "an event arrived and we threw it
        // away" indistinguishable in the log — exactly the ambiguity a stall
        // investigation cannot afford. Print the discarded ones, bounded: four
        // is enough to see what the controller is actually completing.
        let mut unmatched = 0u32;
        loop {
            match self.next_event() {
                Some(Event::Transfer {
                    completion_code, slot, endpoint_dci, residual, trb_pointer, ..
                }) => {
                    if slot == self.slot && endpoint_dci == dci && trb_pointer == last_phys {
                        return Ok((completion_code, requested.saturating_sub(residual)));
                    }
                    if unmatched < 4 {
                        unmatched += 1;
                        serial::puts("  [xhci] discarded transfer event: cc=");
                        serial::put_dec(u64::from(u32::from(completion_code)));
                        serial::puts(" slot=");
                        serial::put_dec(u64::from(u32::from(slot)));
                        serial::puts(" dci=");
                        serial::put_dec(u64::from(u32::from(endpoint_dci)));
                        serial::puts(" trb=");
                        putphys("", trb_pointer);
                    }
                }
                Some(Event::CommandCompletion { completion_code, slot, trb_pointer }) => {
                    if unmatched < 4 {
                        unmatched += 1;
                        serial::puts("  [xhci] discarded command completion during transfer: cc=");
                        serial::put_dec(u64::from(u32::from(completion_code)));
                        serial::puts(" slot=");
                        serial::put_dec(u64::from(u32::from(slot)));
                        serial::puts(" trb=");
                        putphys("", trb_pointer);
                    }
                }
                Some(Event::PortStatusChange { port, completion_code }) => {
                    if unmatched < 4 {
                        unmatched += 1;
                        serial::puts("  [xhci] discarded port event during transfer: port=");
                        serial::put_dec(u64::from(u32::from(port)));
                        serial::puts(" cc=");
                        serial::put_dec(u64::from(u32::from(completion_code)));
                        serial::puts("\n");
                    }
                }
                Some(_) => {}
                None => {}
            }
            if tsc().wrapping_sub(start) > budget {
                serial::puts("  [xhci] transfer timeout: ");
                serial::puts(what);
                serial::puts(" after ");
                serial::put_dec(budget / (tsc_hz() / 1000));
                serial::puts(" ms\n");
                // Diagnostic (trash box, uncommitted): a timeout whose endpoint
                // context reads disabled can never succeed on retry — the ring
                // doorbell on a disabled endpoint is ignored. What is not known
                // is WHO disabled it: nothing in this driver writes DEV_CTX
                // after Configure Endpoint, so a 0 here was written by the
                // controller (or the read is stale/garbage). PORTSC says
                // whether the link is still up when that happened; the raw
                // slot-context dword says whether the whole slot went with it
                // or only the bulk endpoints.
                let psc = PortSc(r32(self.op, op::portsc(self.port)));
                puthex("  [xhci] PORTSC=0x", psc.0);
                puthex("  [xhci] slot ctx dw0=0x", dev_ctx_dw0(self, 0));
                puthex("  [xhci] ep in ctx dw0=0x", dev_ctx_dw0(self, self.bulk_in_dci));
                puthex("  [xhci] ep out ctx dw0=0x", dev_ctx_dw0(self, self.bulk_out_dci));
                note_ep_state(self, self.bulk_in_dci);
                note_ep_state(self, self.bulk_out_dci);
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

        let (code, moved) = self.transfer(
            Ring::Ep0,
            RingField::Ep0,
            1,
            &td[..n],
            u32::from(w_length),
            control_budget(),
            "control",
        )?;
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

    step("find controller");
    let dev = pci::find_class(0x0c, 0x03)
        .filter(|d| d.header.prog_if == 0x30)
        .ok_or("no xHCI controller")?;
    let bar = dev.bars[0].ok_or("xHCI BAR0 missing")?;

    // Sizing a BAR writes all-ones into it. `probe_bar_size` disables the
    // device's decode around that write, but the window is still one where the
    // controller's register file is not where the firmware left it.
    step("probe BAR0 size (writes config space)");
    let (size, _) = pci::probe_bar_size(dev.addr, 0);
    putphys("  [xhci] BAR0 phys=", match bar {
        akuma_pci::Bar::Memory { address, .. } => address,
        _ => 0,
    });
    putphys("  [xhci] BAR0 size=", size);

    step("map BAR0");
    let bar_va = pci::map_bar(bar, size.max(0x1_0000)).ok_or("could not map xHCI BAR")? as usize;
    // Bus-master on (DMA), legacy INTx masked — this driver polls the event ring
    // and no IDT vector is wired for the controller.
    step("enable memory decode + bus master, mask INTx");
    pci::enable_full(dev.addr, true, true);

    // Read the 0x20-byte capability block as eight 32-bit accesses — a
    // controller register file may not answer a sub-dword read. This is the
    // first MMIO touch of the BAR: if the mapping is wrong, it faults here.
    step("read capability block (first MMIO)");
    let mut cap_bytes = [0u8; 0x20];
    for w in 0..8usize {
        let v = r32(bar_va, w * 4).to_le_bytes();
        cap_bytes[w * 4..w * 4 + 4].copy_from_slice(&v);
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

    // Which ports are USB 2.0 and which are SuperSpeed. Read before anything
    // is reset, because `find_and_reset_port` cannot choose a port — or the
    // reset that port accepts — without it.
    step("read supported protocols");
    let protocols = scan_protocols(bar_va, caps.hcc_params1.ext_cap_offset());
    report_protocols(&protocols);

    // Taking the controller from the firmware. On a box whose BIOS is still
    // using it for the USB keyboard this hands control through an SMI, and a
    // firmware that dislikes what it finds resets the machine from inside SMM —
    // where no kernel print can follow it. If the last line on the console is
    // this one, suspect the handoff, not the driver.
    step("BIOS handoff (may enter SMM)");
    bios_handoff(bar_va, caps.hcc_params1.ext_cap_offset());

    // --- reset ---
    step("wait CNR clear");
    wait_cnr_clear(op)?;
    let cmd = r32(op, op::USBCMD);
    if cmd & usbcmd::RS != 0 {
        w32(op, op::USBCMD, cmd & !usbcmd::RS);
        let s = tsc();
        while r32(op, op::USBSTS) & usbsts::HCH == 0 {
            if tsc().wrapping_sub(s) > command_budget() {
                return Err("xHCI would not halt");
            }
            spin_us(100);
        }
    }
    step("HCRST");
    w32(op, op::USBCMD, usbcmd::HCRST);
    let s = tsc();
    loop {
        if r32(op, op::USBCMD) & usbcmd::HCRST == 0 && r32(op, op::USBSTS) & usbsts::CNR == 0 {
            break;
        }
        if tsc().wrapping_sub(s) > command_budget() {
            return Err("xHCI reset timeout");
        }
        spin_us(100);
    }
    serial::puts("  [xhci] reset ok\n");

    // --- lay out DMA memory ---
    // First touch of the big `.bss` DMA statics, and the first place a wrong
    // `virt_to_phys` shows itself. The addresses are printed because a bad one
    // is otherwise silent: the controller happily DMAs to a plausible wrong
    // physical address, and what it lands on — page tables, the GDT — decides
    // whether the box faults, corrupts, or just restarts. Every value below
    // must be under 4 GiB (this image loads at 2 MiB) and non-zero.
    step("lay out DMA memory");
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
    putphys("  [xhci] dcbaa  phys=", dcbaa_phys);
    putphys("  [xhci] cmd    phys=", cmd_phys);
    putphys("  [xhci] event  phys=", events_phys);
    putphys("  [xhci] erst   phys=", erst_phys);
    putphys("  [xhci] bounce phys=", phys_of(bounce().as_ptr()));
    // A DMA address the controller cannot use is worth refusing before it is
    // programmed, not after: `AC64` is set on this controller but the image is
    // below 4 GiB, so anything above that — or a zero — means `virt_to_phys`
    // translated through the wrong window and the rings are not where we think.
    for p in [dcbaa_phys, cmd_phys, events_phys, erst_phys, phys_of(bounce().as_ptr())] {
        if p == 0 || p >= 1 << 32 {
            return Err("DMA static translated outside the low 4 GiB");
        }
    }

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

    // Everything from here on can touch the running controller. If any of it
    // fails, `init` MUST leave the controller halted and reset before returning
    // — otherwise its DMA engine keeps chewing on these `.bss` addresses across
    // the next (kexec-less) reboot and corrupts whatever kernel loads there.
    // That is exactly how a failed bring-up wedged the metal box once.
    let built = (|| -> Result<Xhci, &'static str> {
        step("program DCBAAP / CRCR / ERST");
        w32(op, op::CONFIG, regs::config_max_slots_en(max_slots));
        w64(op, op::DCBAAP, dcbaa_phys);
        w64(op, op::CRCR, (cmd_phys & crcr::PTR_MASK) | crcr::RCS);

        // Interrupter 0's event ring, with interrupts DISABLED (`IMAN.IE` clear):
        // the driver polls the ring. `IMAN_IP` is written to clear any pending
        // bit the firmware left set.
        let intr0 = rt + rt::interrupter(0);
        w32(intr0, intr::ERSTSZ, 1);
        w64(intr0, intr::ERDP, events_phys | intr::ERDP_EHB);
        w64(intr0, intr::ERSTBA, erst_phys);
        w32(intr0, intr::IMAN, intr::IMAN_IP);

        compiler_fence(Ordering::SeqCst);
        // The point of no return: from this write the controller's DMA engine is
        // live against the addresses printed above. Everything after it must
        // reach `halt_controller` on the way out.
        step("USBCMD.RS — controller DMA goes live");
        // Run/Stop only — no `INTE` (polled), no `HSEE` (do not let the
        // controller try to signal host system errors on an un-serviced line).
        w32(op, op::USBCMD, usbcmd::RS);

        let s = tsc();
        while r32(op, op::USBSTS) & usbsts::HCH != 0 {
            if tsc().wrapping_sub(s) > command_budget() {
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
            bot_if: 0,
            tag: 1,
            ep_state_seen: [0xff; 2],
        };

        step("no-op command");
        let (code, _) = x.command(trb::no_op_command(), "no-op")?;
        if code != cc::SUCCESS {
            puthex("  [xhci] no-op cc=", u32::from(code));
            return Err("xHCI command ring dead");
        }
        serial::puts("  [xhci] command ring ok\n");

        step("find + reset port");
        x.port = find_and_reset_port(op, max_ports, &protocols)?;
        let slot_type = protocols.slot_type(x.port);
        step("enumerate device");
        enumerate(&mut x, slot_type)?;
        step("READ CAPACITY");
        read_capacity(&mut x)?;
        Ok(x)
    })();

    let x = match built {
        Ok(x) => x,
        Err(e) => {
            halt_controller(op);
            serial::puts("  [xhci] bring-up failed — controller halted + reset\n");
            return Err(e);
        }
    };

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

/// Stop every xHCI controller on the bus from DMA-ing, using config space only.
///
/// This runs on **every** boot, before the memory map is trusted and long
/// before anything decides whether to drive USB at all — because the thing it
/// defends against is the *previous* boot. A controller left running keeps
/// writing its rings into the physical addresses that boot handed it, which on
/// this target are `.bss` inside a kernel image loaded at 2 MiB. The next
/// kernel is loaded into that same memory with the old DMA still in flight, so
/// it is corrupted before its first instruction — and the box restarts, and
/// does it again. UEFI does not fully re-initialise a controller the OS took
/// over, so a warm reboot does not clear it; only a full power cycle does.
///
/// Clearing `BUS_MASTER` here is what makes that recoverable in software. It
/// costs one config-space write per controller and needs no driver, no BAR and
/// no knowledge of what the previous boot was doing.
///
/// Scoped to xHCI deliberately. The obvious generalisation — quiesce every
/// bus-master device — would take out the GPU, whose framebuffer is this
/// machine's only console.
pub fn quiesce_all() {
    let mut found = 0u32;
    pci::for_each(|d| {
        if d.header.is_class(0x0c, 0x03) && d.header.prog_if == 0x30 {
            pci::quiesce(d.addr);
            found += 1;
        }
    });
    if found > 0 {
        serial::puts("  [xhci] quiesced ");
        serial::put_dec(u64::from(found));
        serial::puts(" controller(s): bus-master off, INTx masked\n");
    }
}

/// Halt the controller this kernel brought up, on the way out of the kernel.
///
/// The error paths in [`init`] already halt; this is the **success** path,
/// which otherwise leaves a fully-configured DMA engine running across a warm
/// reboot — the same crash-loop [`quiesce_all`] describes, just earned by a
/// bring-up that worked. Safe to call when the driver never initialised.
pub fn shutdown() {
    if let Some(x) = XHCI.lock().take() {
        halt_controller(x.op);
    }
    // Belt and braces: `quiesce_all` needs no lock and no successful bring-up,
    // so it also covers a controller that `init` left mid-flight.
    quiesce_all();
}

/// Stop the controller and reset it, so a partially-configured DMA engine
/// cannot act on stale ring pointers after this function returns. Best-effort:
/// every step is time-bounded and failures are swallowed — a wedged controller
/// that will not even reset is a hardware problem a power cycle fixes, and there
/// is nothing more the driver can do about it.
fn halt_controller(op: usize) {
    w32(op, op::USBCMD, 0);
    let s = tsc();
    while r32(op, op::USBSTS) & usbsts::HCH == 0 {
        if tsc().wrapping_sub(s) > command_budget() / 4 {
            break;
        }
        spin_us(100);
    }
    w32(op, op::USBCMD, usbcmd::HCRST);
    let s = tsc();
    while r32(op, op::USBCMD) & usbcmd::HCRST != 0 {
        if tsc().wrapping_sub(s) > command_budget() / 4 {
            break;
        }
        spin_us(100);
    }
}

fn wait_cnr_clear(op: usize) -> Result<(), &'static str> {
    let s = tsc();
    while r32(op, op::USBSTS) & usbsts::CNR != 0 {
        if tsc().wrapping_sub(s) > command_budget() {
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
                    if now.handoff_complete() || tsc().wrapping_sub(s) > command_budget() {
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

/// Walk the extended-capability list and collect every Supported Protocol
/// block (xHCI §7.2), which is what says whether a given root-hub port is USB
/// 2.0 or SuperSpeed.
///
/// Read-only — no writes, nothing that can enter SMM — so unlike the BIOS
/// handoff it is safe to run early, and its answer is available to every step
/// after it.
fn scan_protocols(bar_va: usize, mut off: usize) -> ProtocolMap {
    let mut map = ProtocolMap::default();
    if off == 0 {
        return map;
    }
    for _ in 0..32 {
        let hdr = r32(bar_va, off);
        if xcap::cap_id(hdr) == xcap::CAP_ID_SUPPORTED_PROTOCOL
            && !map.push(xcap::SupportedProtocol::parse(
                hdr,
                r32(bar_va, off + 4),
                r32(bar_va, off + 8),
                r32(bar_va, off + 12),
            ))
        {
            break;
        }
        match xcap::next_cap_offset(off, hdr) {
            Some(n) => off = n,
            None => break,
        }
    }
    map
}

fn report_protocols(map: &ProtocolMap) {
    for b in map.blocks() {
        serial::puts("  [xhci] proto USB");
        serial::put_dec(u64::from(b.major));
        serial::puts(" ports ");
        serial::put_dec(u64::from(b.port_offset));
        serial::puts("..");
        serial::put_dec(u64::from(
            b.port_offset.saturating_add(b.port_count.saturating_sub(1)),
        ));
        serial::puts(" slot_type ");
        serial::put_dec(u64::from(b.slot_type));
        serial::puts("\n");
    }
}

/// Reset one root-hub port and wait for it to enable.
///
/// **Which reset a port accepts depends on its protocol**, and getting that
/// wrong is where the metal bring-up died for a whole session. `PORTSC.WPR` —
/// Warm Port Reset — is a SuperSpeed-only bit and **reserved on a USB 2.0
/// port**: the write is ignored, the port stays in Polling, and the only
/// symptom is the one-second timeout below. The enclosure attaches on USB 2.0
/// port 8 on the reference box, so a driver that could only warm-reset was
/// never going to enumerate it, and said nothing about why.
///
/// Hot reset (`PR`) is valid on both protocols, so it is what is tried first.
/// A warm reset is SuperSpeed link *recovery*; it is worth a second attempt
/// only on a SuperSpeed port whose link did not train, and is skipped outright
/// on USB 2.0 rather than spent as another second of timeout.
fn reset_port(op: usize, port: u8, superspeed: bool) -> Result<(), &'static str> {
    for warm in [false, true] {
        if warm && !superspeed {
            break;
        }
        let psc = PortSc(r32(op, op::portsc(port)));
        if psc.enabled() && !psc.resetting() {
            return Ok(());
        }
        w32(
            op,
            op::portsc(port),
            if warm { psc.with_warm_reset_asserted() } else { psc.with_reset_asserted() },
        );
        let s = tsc();
        loop {
            let now = PortSc(r32(op, op::portsc(port)));
            if now.enabled() && !now.resetting() {
                w32(op, op::portsc(port), now.acknowledging_reset());
                return Ok(());
            }
            if tsc().wrapping_sub(s) > command_budget() {
                break;
            }
            spin_us(1000);
        }
        serial::puts(if warm {
            "  [xhci] warm reset timed out\n"
        } else {
            "  [xhci] hot reset timed out\n"
        });
    }
    Err("xHCI port reset timeout")
}

fn find_and_reset_port(
    op: usize,
    max_ports: u8,
    protocols: &ProtocolMap,
) -> Result<u8, &'static str> {
    // Print **every** connected port, not just the one chosen.
    //
    // A physical SuperSpeed socket is two root-hub ports — a USB 2.0 one and a
    // SuperSpeed one — and a device lands on whichever half its link trained
    // for. The first version of this loop stopped at the first connected port
    // and printed only that: on the metal, one line naming USB 2.0 port 8,
    // with no protocol on it and no hint that ports 16..=21 had never been
    // looked at. The map is the difference between "the reset timed out" and
    // "the reset timed out because that port is USB 2.0".
    let mut chosen: Option<u8> = None;
    for p in 1..=max_ports {
        let psc = PortSc(r32(op, op::portsc(p)));
        if !psc.connected() {
            continue;
        }
        let ss = protocols.is_superspeed(p);
        serial::puts("  [xhci] port ");
        serial::put_dec(u64::from(p));
        serial::puts(" USB");
        serial::put_dec(u64::from(protocols.major(p)));
        serial::puts(" connected PORTSC=0x");
        serial::put_hexn(u64::from(psc.0), 8);
        serial::puts(" PLS=");
        serial::put_dec(u64::from(psc.link_state()));
        serial::puts(if psc.enabled() { " enabled\n" } else { " not-enabled\n" });
        // Prefer SuperSpeed: the same disk over four times the link. A port
        // whose protocol is unknown loses to one known to be SuperSpeed and
        // beats nothing else, which keeps a controller with an unreadable
        // capability list working exactly as before.
        let better = match chosen {
            None => true,
            Some(c) => ss && !protocols.is_superspeed(c),
        };
        if better {
            chosen = Some(p);
        }
    }
    let found = chosen.ok_or("no connected xHCI port")?;

    reset_port(op, found, protocols.is_superspeed(found))?;
    spin_us(20_000); // settle (USB 2.0 §7.1.7.3)

    let psc = PortSc(r32(op, op::portsc(found)));
    serial::puts("  [xhci] port ");
    serial::put_dec(u64::from(found));
    serial::puts(" enabled, speed ");
    serial::put_dec(u64::from(psc.speed_field()));
    serial::puts("\n");
    Ok(found)
}

/// `slot_type` is the Protocol Slot Type from the Supported Protocol
/// capability covering this port (0 for USB on every real part, but the field
/// exists so a controller can define others — and taking it from the
/// capability costs nothing over hardcoding the value it almost always has).
fn enumerate(x: &mut Xhci, slot_type: u8) -> Result<(), &'static str> {
    let psc = PortSc(r32(x.op, op::portsc(x.port)));
    let speed = Speed::from_field(psc.speed_field()).ok_or("unknown port speed")?;
    let ep0_mps = speed.default_ep0_max_packet();

    // --- Enable Slot ---
    let (code, slot) = x.command(trb::enable_slot(slot_type), "enable slot")?;
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

    let (config_value, bin, bout, bot_if) = parse_bot_endpoints(&cfg[..usize::from(want)])?;
    x.bot_if = bot_if;
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
    // A0 (slot) plus the two bulk endpoints — and **not A1**.
    //
    // EP0 belongs to Address Device and Evaluate Context; a Configure Endpoint
    // command that also claims it is rejected outright. The controller's answer
    // is `TRB Error` (cc=5) against the *command*, which reads like a malformed
    // TRB rather than a flag that should not be there, and the whole bring-up
    // stops one step short of a working disk. The EP0 context below is still
    // written — a context with no Add flag is ignored, and leaving it populated
    // is what Linux's `xhci_configure_endpoint` does with the same buffer.
    // `configure_endpoint_add_flags_exclude_ep0` in `akuma-xhci` pins the rule.
    write_input_context(
        x.context_bytes,
        context::configure_endpoint_add_flags(&[x.bulk_in_dci, x.bulk_out_dci]),
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

fn parse_bot_endpoints(cfg: &[u8]) -> Result<(u8, BulkEp, BulkEp, u8), &'static str> {
    let mut config_value = 1u8;
    let mut bot_if = 0u8;
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
                in_bot = false;
                if let Some(i) = descriptor::InterfaceDescriptor::parse(d.bytes)
                    && i.class == 0x08
                    && i.sub_class == 0x06
                    && i.protocol == 0x50
                {
                    in_bot = true;
                    bot_if = i.interface_number;
                }
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
        (Some(i), Some(o)) => Ok((config_value, i, o, bot_if)),
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
    // Through the status layer: a freshly powered enclosure answers its first
    // real command with UNIT ATTENTION, which `scsi_io` clears and retries.
    scsi_io(x, cdb::read_capacity_10(), 8).map_err(|_| "READ CAPACITY failed")?;
    let mut cap = [0u8; 8];
    cap.copy_from_slice(&bounce()[..8]);
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

/// How one BOT command attempt came back. The *decision* — recover and
/// retry, or give up — lives in `akuma_xhci::recovery::retry_decision` and is
/// host-tested against the 2026-09-11 metal incident; this enum only carries
/// what that decision and the recovery need.
enum BotErr {
    /// The controller completed the TD with a completion code the BOT layer
    /// cannot use.
    Stalled { code: u8, phase: Phase, dci: u8 },
    /// The controller never completed the TD within `bulk_budget`. The TD is
    /// still live in the ring and the device may answer late — the photograph
    /// of 2026-09-11 shows a data-phase STALL arriving just past the budget:
    /// indistinguishable from never answering, and deadly when treated that
    /// way, because nothing un-halted.
    TimedOut { phase: Phase, dci: u8 },
    /// The CSW was unparseable or its tag mismatched — pipe desynced.
    Desynced { phase: Phase, dci: u8 },
}

use akuma_xhci::recovery::{recovery_plan, retry_decision, AttemptOutcome, Decision, Phase, RecoveryStep};

impl BotErr {
    fn outcome(&self) -> AttemptOutcome {
        match *self {
            Self::Stalled { code, phase, dci } => AttemptOutcome::Stalled { code, phase, dci },
            Self::TimedOut { phase, dci } => AttemptOutcome::TimedOut { phase, dci },
            Self::Desynced { phase, dci } => AttemptOutcome::Desynced { phase, dci },
        }
    }
}

/// Run one BOT command, recovering once from a stall OR a timeout. Every
/// failure shape `akuma_xhci::recovery` covers gets the class-standard
/// recovery (see [`recover`]) and exactly one retry; a command that fails
/// again afterwards is reported and given up on.
fn bot_run(
    x: &mut Xhci,
    command: akuma_usb_storage::Command,
    data_len: usize,
    data_phys: u64,
) -> Result<(CswStatus, u32), &'static str> {
    for attempt in 0..=2u8 {
        match bot_run_once(x, command, data_len, data_phys) {
            Ok(r) => return Ok(r),
            Err(e) => match retry_decision(attempt, &e.outcome()) {
                Decision::RecoverAndRetry => {
                    if !recover(x, &e) {
                        return Err("bulk transfer error");
                    }
                    if attempt == 0 {
                        serial::puts("  [xhci] stall recovered - retrying the command once\n");
                    }
                }
                Decision::GiveUp => return Err("bulk transfer error"),
                Decision::Dead => {
                    return Err("bulk transfer failed again after recovery");
                }
            },
        }
    }
    unreachable!("retry_decision caps the loop at one retry")
}

/// `data_phys` is the DMA address of the data phase's buffer — `BOUNCE_BUF`
/// for disk I/O, `SENSE_BUF` for the `REQUEST SENSE` that must not overwrite
/// the payload a failed command is about to retry with.
fn bot_run_once(
    x: &mut Xhci,
    command: akuma_usb_storage::Command,
    data_len: usize,
    data_phys: u64,
) -> Result<(CswStatus, u32), BotErr> {
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
        bulk_budget(),
        Phase::Cbw.as_str(),
    )
    .map_err(|_| BotErr::TimedOut { phase: Phase::Cbw, dci: x.bulk_out_dci })?;
    if code != cc::SUCCESS {
        return Err(BotErr::Stalled { code, phase: Phase::Cbw, dci: x.bulk_out_dci });
    }

    let mut moved = 0u32;
    if data_len > 0 && command.direction != Direction::None {
        let n = data_len.min(BOUNCE_LEN) as u32;
        let bp = data_phys;
        let (ring, field, dci, is_in) = match command.direction {
            Direction::In => (Ring::BulkIn, RingField::BulkIn, x.bulk_in_dci, true),
            _ => (Ring::BulkOut, RingField::BulkOut, x.bulk_out_dci, false),
        };
        let phase = Phase::Data;
        let (count, td) = trb::data_trbs(bp, n);
        let (code, m) = x
            .transfer(ring, field, dci, &td[..count], n, bulk_budget(), phase.as_str())
            .map_err(|_| BotErr::TimedOut { phase, dci })?;
        moved = m;
        if code != cc::SUCCESS && !(code == cc::SHORT_PACKET && is_in) {
            return Err(BotErr::Stalled { code, phase, dci });
        }
    }

    let csw_phys = phys_of(csw_buf().as_ptr());
    let (code, _) = x.transfer(
        Ring::BulkIn,
        RingField::BulkIn,
        x.bulk_in_dci,
        &[trb::normal(csw_phys, 13, true)],
        13,
        bulk_budget(),
        Phase::Csw.as_str(),
    )
    .map_err(|_| BotErr::TimedOut { phase: Phase::Csw, dci: x.bulk_in_dci })?;
    if code != cc::SUCCESS && code != cc::SHORT_PACKET {
        return Err(BotErr::Stalled { code, phase: Phase::Csw, dci: x.bulk_in_dci });
    }
    let csw = Csw::parse(&csw_buf()[..13])
        .ok_or(BotErr::Desynced { phase: Phase::Csw, dci: x.bulk_in_dci })?;
    if csw.tag != tag {
        return Err(BotErr::Desynced { phase: Phase::Csw, dci: x.bulk_in_dci });
    }
    Ok((csw.status, moved))
}

/// The endpoint address a bulk dci was built from (`context::dci` is
/// `ep_num * 2 + direction`, EP direction-in is odd).
fn bulk_ep_addr(dci: u8) -> u8 {
    if dci & 1 == 1 {
        0x80 | (dci >> 1)
    } else {
        dci >> 1
    }
}

/// Run one step of [`akuma_xhci::recovery`]'s plan, printing the result.
/// Every step used to be `let _ =`-swallowed; the 2026-09-11 photograph
/// shows recovery announcing success while the very next transfer was dead —
/// if Set TR Dequeue Pointer answers `cc=CONTEXT_STATE` (STDP on a running
/// endpoint) we must see it, not sail past it.
fn recover_step(x: &mut Xhci, what: &str, t: [u32; 4]) {
    match x.command(t, what) {
        Ok((code, _)) if code == cc::SUCCESS => {}
        Ok((code, _)) => {
            puthex("  [xhci] recovery step cc=", u32::from(code));
            serial::puts("  [xhci] step ");
            serial::puts(what);
            serial::puts("\n");
        }
        Err(e) => {
            serial::puts("  [xhci] recovery step timeout: ");
            serial::puts(e);
            serial::puts(" (");
            serial::puts(what);
            serial::puts(")\n");
        }
    }
}

/// Read the EP State of endpoint `dci` from the device context, decoded by
/// the crate (`EpState::decode` — one decode, host-tested; the glue keeps no
/// arithmetic copy).
///
/// Indexing: the **device** context lays out slot ctx at index 0 and EP ctx
/// for `dci` at index `dci` — `dci + 1` is the *input* context's layout
/// (which has the Input Control Context at 0). First version read `dci + 1`
/// here and reported state 0 (disabled) for a perfectly running endpoint,
/// because it was reading the zeroed slot past the last real context.
fn ep_state(x: &Xhci, dci: u8) -> akuma_xhci::device::EpState {
    // History: this read `dw0 >> 2` for a week — RUNNING became "disabled",
    // `halted` was unreachable, Reset Endpoint never once ran on the metal,
    // and the "ep disabled while transfers were working" mystery in
    // `AKUMA_AMD64_USB_XHCI.md` was this arithmetic, not the controller.
    // The decode moved into the crate so it cannot drift again.
    akuma_xhci::device::EpState::decode(dev_ctx_dw0(x, dci))
}

/// Raw first dword of context entry `idx` in the live device context
/// (idx 0 = slot context, idx dci = that endpoint's context). Diagnostic.
fn dev_ctx_dw0(x: &Xhci, idx: u8) -> u32 {
    let base = context::context_offset(usize::from(idx), x.context_bytes);
    // SAFETY: `DEV_CTX` is the live device context (DCBAA slot points at it);
    // the lock is held and this is a read of our own DMA memory.
    unsafe {
        core::ptr::read_unaligned((&raw const DEV_CTX).cast::<u8>().add(base).cast::<u32>())
    }
}

/// Track the bulk endpoints' context states and print every transition —
/// `ep 4 state run->halt->stop->run` is the recovery loop made visible, and
/// a transition the model in `akuma_xhci::device` does not know about is a
/// driver/controller disagreement worth a line. The state byte itself goes
/// through the crate's decode (one decode, host-tested) rather than this
/// file's arithmetic — the `>> 2` bug lived exactly in such a copy.
fn note_ep_state(x: &mut Xhci, dci: u8) {
    let state = ep_state(x, dci);
    let idx = usize::from(dci != x.bulk_in_dci);
    let prev = x.ep_state_seen[idx];
    x.ep_state_seen[idx] = match state {
        akuma_xhci::device::EpState::Disabled => 0,
        akuma_xhci::device::EpState::Running => 1,
        akuma_xhci::device::EpState::Halted => 2,
        akuma_xhci::device::EpState::Stopped => 3,
        akuma_xhci::device::EpState::Error => 4,
    };
    if prev == x.ep_state_seen[idx] {
        return;
    }
    let label = |code: u8| -> &'static str {
        if code == 0xff {
            "?"
        } else {
            akuma_xhci::device::EpState::decode(u32::from(code)).as_str()
        }
    };
    serial::puts("  [xhci] ep ");
    serial::put_dec(u64::from(u32::from(dci)));
    serial::puts(" state ");
    serial::puts(label(prev));
    serial::puts("->");
    serial::puts(label(x.ep_state_seen[idx]));
    serial::puts("\n");
}

/// Class-standard mass-storage recovery for a stalled or timed-out phase.
/// Returns `true` when recovery ran and the caller may retry the command
/// once; `false` for a completion code recovery does not cover.
///
/// The sequence and its ordering come from
/// [`akuma_xhci::recovery::recovery_plan`] (host-tested); this function only
/// performs the steps against the hardware. Controller-side work touches the
/// failed ring ONLY — Reset Endpoint and Set TR Dequeue Pointer are illegal
/// on a running endpoint — while the device-side Mass Storage Reset and the
/// per-endpoint CLEAR_FEATUREs cover either bulk endpoint, because the
/// device may have halted either.
fn recover(x: &mut Xhci, e: &BotErr) -> bool {
    let outcome = e.outcome();
    let (code, phase, dci) = match outcome {
        AttemptOutcome::Stalled { code, phase, dci } => (Some(code), phase, dci),
        AttemptOutcome::TimedOut { phase, dci } | AttemptOutcome::Desynced { phase, dci } => {
            (None, phase, dci)
        }
    };
    if let Some(code) = code
        && code != cc::STALL_ERROR
    {
        puthex("  [xhci] bulk cc=", u32::from(code));
        serial::puts("  [xhci] phase ");
        serial::puts(phase.as_str());
        serial::puts("\n");
        return false;
    }

    match e {
        BotErr::Stalled { code, .. } => {
            puthex("  [xhci] bulk cc=", u32::from(*code));
        }
        BotErr::TimedOut { .. } => {
            serial::puts("  [xhci] transfer timed out - device slow, not halted; retrying\n");
        }
        BotErr::Desynced { .. } => {
            serial::puts("  [xhci] CSW desynced - recovering\n");
        }
    }
    serial::puts("  [xhci] phase ");
    serial::puts(phase.as_str());
    serial::puts("\n");

    // `failed_dci` maps to exactly one bulk ring; the other ring keeps its
    // dequeue. Controller-side steps (Reset Endpoint, Set TR Dequeue Pointer)
    // are legal only on a HALTED endpoint — the 2026-09-11 metal run answered
    // both with `cc=0x13 CONTEXT_STATE_ERROR` because the endpoint was merely
    // slow (a post-idle command completing late, `cc=1` events arriving after
    // the budget), not halted. Resetting a running endpoint is at best noise
    // and at worst deranges a live ring, so a timeout whose endpoint reads
    // not-halted skips them entirely and just retries the command.
    let (failed_ring, failed_field) = if dci == x.bulk_in_dci {
        (Ring::BulkIn, RingField::BulkIn)
    } else {
        (Ring::BulkOut, RingField::BulkOut)
    };
    let halted = ep_state(x, dci) == akuma_xhci::device::EpState::Halted;
    note_ep_state(x, dci);
    let kind = match outcome {
        AttemptOutcome::Stalled { .. } => akuma_xhci::recovery::OutcomeKind::Stalled,
        AttemptOutcome::TimedOut { .. } => akuma_xhci::recovery::OutcomeKind::TimedOut,
        AttemptOutcome::Desynced { .. } => akuma_xhci::recovery::OutcomeKind::Desynced,
    };
    let plan = recovery_plan(kind, halted, dci, bulk_ep_addr(x.bulk_in_dci), bulk_ep_addr(x.bulk_out_dci));
    if !halted && kind == akuma_xhci::recovery::OutcomeKind::TimedOut {
        // The TD is live and the endpoint is running: the only legal way off
        // it is Stop Endpoint (Running -> Stopped), then the dequeue move.
        // Until 2026-09-12 this case retried with the dead TD still queued
        // ahead of the retry — see `akuma_xhci::recovery`'s module doc.
        serial::puts("  [xhci] ep not halted - aborting the live TD (stop ep)\n");
    }
    // The plan encodes spec legality (host-tested against the model in
    // `akuma_xhci::device`); this loop only performs the steps, in order.
    for step in plan {
        match step {
            RecoveryStep::None => {}
            RecoveryStep::ResetEndpoint { dci } => {
                recover_step(x, "reset ep", trb::reset_endpoint(x.slot, dci));
            }
            RecoveryStep::StopEndpoint { dci } => {
                // The controller answers with a Transfer Event (cc=STOPPED /
                // STOPPED_LENGTH_INVALID) for the aborted TD and then the
                // Command Completion; `command` consumes the first, and one
                // that lands late shows up as a `discarded transfer event`
                // on the next transfer — expected, and the proof the abort
                // took.
                recover_step(x, "stop ep", trb::stop_endpoint(x.slot, dci));
            }
            RecoveryStep::SetTrDequeuePointer { dci } => {
                // Resume at the ring's enqueue position with the cycle the
                // next TRB there will carry: everything up to it — the dead
                // TD included — is abandoned.
                let idx = x.producer(failed_field).enqueue_index();
                let cycle = x.producer(failed_field).cycle();
                let dequeue = ring_phys(failed_ring) + (idx as u64) * 16;
                recover_step(
                    x,
                    "set tr dequeue",
                    trb::set_tr_dequeue_pointer(x.slot, dci, dequeue, cycle),
                );
            }
            RecoveryStep::BotMassStorageReset => {
                if x.control(0x21, 0xFF, 0, u16::from(x.bot_if), 0).is_err() {
                    serial::puts("  [xhci] BOT mass storage reset: control transfer failed\n");
                }
            }
            RecoveryStep::ClearHalt { ep_addr } => {
                if x.control(0x02, 0x01, 0, u16::from(ep_addr), 0).is_err() {
                    serial::puts("  [xhci] CLEAR_FEATURE(ENDPOINT_HALT) failed for ep 0x");
                    serial::put_hexn(u64::from(ep_addr), 2);
                    serial::puts("\n");
                }
            }
        }
    }
    note_ep_state(x, dci);
    true
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
    let (status, moved) = bot_run(x, command, command.data_len as usize, phys_of(bounce().as_ptr()))?;
    if command.direction == Direction::In {
        let n = (moved as usize).min(data.len()).min(BOUNCE_LEN);
        data[..n].copy_from_slice(&bounce()[..n]);
    }
    Ok(status)
}

// ===========================================================================
// SCSI status: the layer between "the BOT transport delivered a CSW" and "the
// block device did what it was asked"
// ===========================================================================

/// How long to keep asking a drive that answers `NOT READY`: standby spin-up
/// on a 2.5" SATA drive behind the bridge is 3-8 s. Twelve covers it with
/// margin and still fails a genuinely dead disk in a time a human at the
/// console can wait out.
const NOT_READY_WAIT_MS: u64 = 12_000;
const NOT_READY_POLL_MS: u64 = 250;
/// `UNIT ATTENTION` retries per command. The device raises it once after a
/// reset (ours, or a power event) and clears it on the next command; a device
/// that raises it on every command is broken, not attentive.
const UNIT_ATTENTION_RETRIES: u8 = 4;

/// Run one SCSI command through the BOT transport and interpret its status:
/// `Passed` must have moved exactly `span` bytes, `Failed` is asked why with
/// `REQUEST SENSE` and retried when the answer is "not yet" (a platter
/// spinning up, or the unit-attention every device raises after a reset — the
/// reset *this driver's recovery* just sent included), and a `PhaseError`
/// gets the class-standard reset once.
///
/// This exists because `read_bytes` used to check only `status == Passed` and
/// copied `span` bytes out of the bounce buffer regardless of how many the
/// device had actually delivered. A short data phase reported as success is
/// exactly what the double-TD bug produced, and a 64 KiB read that carried 13
/// real bytes went into the ext2 block cache as a full block.
///
/// The bounce buffer holds the command's data across every retry: `REQUEST
/// SENSE` lands in its own `SENSE_BUF`, so a `WRITE(10)` payload staged
/// before the call is still intact when the command is re-issued.
fn scsi_io(
    x: &mut Xhci,
    command: akuma_usb_storage::Command,
    span: usize,
) -> Result<(), &'static str> {
    let mut not_ready_polls: u64 = 0;
    let mut unit_attentions: u8 = 0;
    let mut phase_errors: u8 = 0;
    loop {
        let (status, moved) = bot_run(x, command, span, phys_of(bounce().as_ptr()))?;
        match status {
            CswStatus::Passed => {
                if moved as usize != span {
                    serial::puts("  [xhci] short transfer: device moved ");
                    serial::put_dec(u64::from(moved));
                    serial::puts(" of ");
                    serial::put_dec(span as u64);
                    serial::puts(" bytes\n");
                    return Err("short bulk transfer");
                }
                return Ok(());
            }
            CswStatus::Failed => {
                let sense = request_sense(x)?;
                match sense.sense_key {
                    // UNIT ATTENTION: "something happened" (0x29 = reset
                    // occurred — including the BOT reset recovery just sent).
                    // Acknowledged by the asking; retry at once.
                    0x06 => {
                        unit_attentions += 1;
                        if unit_attentions > UNIT_ATTENTION_RETRIES {
                            return Err("SCSI unit attention would not clear");
                        }
                        serial::puts("  [xhci] unit attention asc=0x");
                        serial::put_hexn(u64::from(sense.asc), 2);
                        serial::puts(" ascq=0x");
                        serial::put_hexn(u64::from(sense.ascq), 2);
                        serial::puts(" - retrying\n");
                    }
                    // NOT READY: the platter is spinning up (0x04/0x01
                    // "becoming ready", 0x04/0x02 "initializing command
                    // required" on some bridges). Wait, bounded.
                    0x02 => {
                        if not_ready_polls == 0 {
                            serial::puts("  [xhci] not ready asc=0x");
                            serial::put_hexn(u64::from(sense.asc), 2);
                            serial::puts(" ascq=0x");
                            serial::put_hexn(u64::from(sense.ascq), 2);
                            serial::puts(" - waiting for the drive\n");
                        }
                        not_ready_polls += 1;
                        if not_ready_polls * NOT_READY_POLL_MS > NOT_READY_WAIT_MS {
                            return Err("SCSI device not ready");
                        }
                        spin_us(NOT_READY_POLL_MS * 1000);
                    }
                    key => {
                        serial::puts("  [xhci] check condition key=0x");
                        serial::put_hexn(u64::from(key), 1);
                        serial::puts(" asc=0x");
                        serial::put_hexn(u64::from(sense.asc), 2);
                        serial::puts(" ascq=0x");
                        serial::put_hexn(u64::from(sense.ascq), 2);
                        serial::puts("\n");
                        return Err("SCSI command failed");
                    }
                }
            }
            CswStatus::PhaseError => {
                // BOT §6.7.3: the device lost the plot; reset the interface
                // (the Desynced plan is exactly that) and re-issue once.
                phase_errors += 1;
                if phase_errors > 1 {
                    return Err("BOT phase error persists");
                }
                serial::puts("  [xhci] CSW phase error - resetting the interface\n");
                let e = BotErr::Desynced { phase: Phase::Csw, dci: x.bulk_in_dci };
                if !recover(x, &e) {
                    return Err("BOT phase error");
                }
            }
            CswStatus::Unknown(_) => return Err("unknown CSW status"),
        }
    }
}

/// `REQUEST SENSE` into `SENSE_BUF` (never the bounce buffer — the failed
/// command's data or payload is still there and about to be retried).
fn request_sense(x: &mut Xhci) -> Result<akuma_usb_storage::RequestSense, &'static str> {
    let (status, moved) = bot_run(x, cdb::request_sense(), 18, phys_of(sense_buf().as_ptr()))?;
    if status != CswStatus::Passed || moved < 14 {
        return Err("REQUEST SENSE failed");
    }
    akuma_usb_storage::RequestSense::parse(&sense_buf()[..18]).ok_or("bad REQUEST SENSE response")
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

        scsi_io(x, cdb::read_10(lba32, blocks as u16, x.block_len), span)?;
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

        if within != 0 || take != span {
            scsi_io(x, cdb::read_10(lba32, blocks as u16, x.block_len), span)?;
        }
        bounce_mut()[within..within + take].copy_from_slice(&data[done..done + take]);
        scsi_io(x, cdb::write_10(lba32, blocks as u16, x.block_len), span)?;
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

#[cfg(not(feature = "no-tests"))]
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
    //
    // Skipped rather than clamped when the disk is smaller than that: the point
    // of the fixed LBA is that it is inside a partition nothing else uses, and
    // a clamped address on a small disk has no such promise — under QEMU with a
    // synthetic image it would land in the middle of `sda1`. A rig that wants
    // this check covered gives itself a disk with the real layout.
    const SDA2_LBA: u64 = 134_217_728;
    let scratch_lba = SDA2_LBA + 1000;
    if sectors <= scratch_lba {
        t.note("xhci: disk too small for the sda2 scratch write — skipped", sectors);
        return;
    }
    let scratch = scratch_lba * 512;
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
