//! The MMIO / DMA half of the NVMe driver, over [`akuma_nvme`].
//!
//! `akuma-nvme` decides everything that can be decided from bytes — register
//! decode, submission entries, completions and their phase bit, Identify, PRPs,
//! chunking, the partition window, the GPT. This module is the part that
//! cannot be: the `unsafe` register accesses on a mapped BAR, and DMA memory
//! with a known physical address. Written for ryzen's SK hynix SSD
//! (`1c5c:1d59`, `overlays/ryzen/`) and rehearsed against QEMU's `nvme` device
//! under OVMF before it ever touched that disk.
//!
//! # What it touches, and what it refuses to
//!
//! The disk this drives holds the user's operating system. Nothing here reads
//! or writes a byte outside the partition [`open_partition`] selected: every
//! byte offset is checked against the [`Window`] before a command is built, and
//! every command's LBA run is translated through [`Window::absolute`], which
//! refuses a run with any block outside. The window itself comes only from a
//! GPT whose header CRC **and** entry-array CRC both check. Before a partition
//! is open, only LBA 1 and the entry array are ever read, and nothing is
//! written.
//!
//! # The DMA contract
//!
//! Every structure the controller reads or writes — the admin and I/O queues,
//! the Identify page, the PRP list and the bounce buffer — is a page-aligned
//! `.bss` static here, translated with `akuma_primitives::addr::virt_to_phys`,
//! as `xhci.rs` and the rtl8169 glue do. There is no IOMMU in the translation
//! path (firmware leaves AMD-Vi off at handoff and this kernel never enables
//! it), so a physical address is a bus address. All of it is reached under the
//! `NVME` lock, one command in flight at a time, so the `&'static mut` views
//! below are never aliased.
//!
//! Completion entries are read with volatile loads straight from the queue
//! memory — the controller writes them behind the compiler's back. Submission
//! entries are written with volatile stores, then a `compiler_fence`, then the
//! doorbell. x86 is DMA-coherent; no cache maintenance.
//!
//! # Polled, and fail-stop
//!
//! Interrupts are masked at the controller (`INTMS`) and at PCI (`INTx`
//! disable): an unhandled legacy interrupt would land on a vector nobody wired
//! (see `xhci.rs`). A command that does not complete within its budget means
//! the controller may still DMA into the bounce buffer whenever it gets round
//! to it — into memory the next command is using. So a timeout is not retried:
//! the controller is disabled, its bus mastering switched off, and every later
//! call fails. Losing the disk is recoverable; corrupting it is not.
//!
//! # Taking the controller from the firmware
//!
//! UEFI's own NVMe driver leaves the controller enabled with its queues in
//! firmware memory that is now ordinary RAM. [`init`] clears `CC.EN` and waits
//! for `CSTS.RDY` to drop before programming anything, which is the reset the
//! spec requires before the admin queue registers may change.
//!
//! # Before a reset
//!
//! [`shutdown`] flushes the volatile write cache and performs a normal
//! shutdown (`CC.SHN`), then switches bus mastering off — the same reason
//! `xhci::shutdown` exists: a reset does not stop a bus master, and the next
//! kernel is loaded into this one's `.bss`.

use core::sync::atomic::{Ordering, compiler_fence};

use akuma_nvme::chunk;
use akuma_nvme::cmd::{self, CqHead, Cqe, SqTail, Sqe};
use akuma_nvme::gpt::Header;
use akuma_nvme::identify::{self, Controller, Namespace};
use akuma_nvme::prp;
use akuma_nvme::regs::{self, Cap, Csts};
use akuma_nvme::window::Window;
use akuma_primitives::addr::virt_to_phys;
use akuma_primitives::mmio::MmioReg;
use spinning_top::Spinlock;

use crate::pci;
use crate::polltime::{spin_us, ticks_ms, tsc};
use crate::serial;

/// Entries per queue. One command is ever in flight; 16 is the spec's
/// comfortable minimum and every controller supports it.
const QUEUE_ENTRIES: u16 = 16;
/// Bytes one command can move, through the bounce buffer.
const BOUNCE_LEN: usize = 128 * 1024;
/// The namespace this driver uses. Every consumer SSD met so far has exactly one.
const NSID: u32 = 1;
const ADMIN_QID: u16 = 0;
const IO_QID: u16 = 1;
/// Budget for one command. Flushes and first writes after idle are the slow
/// ones; a healthy SSD answers in milliseconds.
const COMMAND_BUDGET_MS: u64 = 10_000;

#[repr(C, align(4096))]
struct Page<const N: usize>([u8; N]);

static mut ASQ_MEM: Page<4096> = Page([0; 4096]);
static mut ACQ_MEM: Page<4096> = Page([0; 4096]);
static mut IOSQ_MEM: Page<4096> = Page([0; 4096]);
static mut IOCQ_MEM: Page<4096> = Page([0; 4096]);
static mut IDENT_MEM: Page<4096> = Page([0; 4096]);
static mut PRP_LIST_MEM: Page<4096> = Page([0; 4096]);
static mut BOUNCE_MEM: Page<BOUNCE_LEN> = Page([0; BOUNCE_LEN]);

/// Which DMA static an accessor refers to.
#[derive(Clone, Copy)]
enum Buf {
    Asq,
    Acq,
    Iosq,
    Iocq,
    Ident,
    PrpList,
    Bounce,
}

fn buf_ptr(b: Buf) -> (*mut u8, usize) {
    match b {
        Buf::Asq => ((&raw mut ASQ_MEM).cast(), 4096),
        Buf::Acq => ((&raw mut ACQ_MEM).cast(), 4096),
        Buf::Iosq => ((&raw mut IOSQ_MEM).cast(), 4096),
        Buf::Iocq => ((&raw mut IOCQ_MEM).cast(), 4096),
        Buf::Ident => ((&raw mut IDENT_MEM).cast(), 4096),
        Buf::PrpList => ((&raw mut PRP_LIST_MEM).cast(), 4096),
        Buf::Bounce => ((&raw mut BOUNCE_MEM).cast(), BOUNCE_LEN),
    }
}

fn buf_phys(b: Buf) -> u64 {
    virt_to_phys(buf_ptr(b).0 as usize) as u64
}

/// The buffer as a byte slice, for the CPU's side of a transfer.
fn buf_mut(b: Buf) -> &'static mut [u8] {
    let (p, len) = buf_ptr(b);
    // SAFETY: a `.bss` static of exactly `len` bytes. Reached only under the
    // `NVME` lock with no command in flight on it (the module's DMA contract),
    // so neither the controller nor another caller is touching it.
    unsafe { core::slice::from_raw_parts_mut(p, len) }
}

/// The PRP list page, as the controller reads it.
// `Page` is `align(4096)`: the cast cannot misalign.
#[allow(clippy::cast_ptr_alignment)]
fn prp_list() -> &'static mut [u64] {
    let (p, _) = buf_ptr(Buf::PrpList);
    // SAFETY: as `buf_mut`; 4096 bytes, page-aligned, so 512 aligned u64s.
    unsafe { core::slice::from_raw_parts_mut(p.cast::<u64>(), 512) }
}

fn r32(bar: usize, off: usize) -> u32 {
    // SAFETY: `bar` is the mapped NVMe BAR0; `off` is a controller register or
    // doorbell offset inside it (doorbells for queues 0 and 1 only).
    unsafe { MmioReg::<u32>::new(bar + off).read() }
}

fn w32(bar: usize, off: usize, v: u32) {
    // SAFETY: as `r32`.
    unsafe { MmioReg::<u32>::new(bar + off).write(v) }
}

/// A 64-bit register as two dword writes, low first — what the spec permits
/// and what every controller accepts, where a single 64-bit store is not
/// guaranteed to be.
fn w64(bar: usize, off: usize, v: u64) {
    w32(bar, off, v as u32);
    w32(bar, off + 4, (v >> 32) as u32);
}

struct Nvme {
    bar: usize,
    cap: Cap,
    pci: akuma_pci::Address,
    asq: SqTail,
    acq: CqHead,
    iosq: SqTail,
    iocq: CqHead,
    next_cid: u16,
    lba_bytes: u32,
    ns_lbas: u64,
    max_blocks: u32,
    window: Option<Window>,
    /// A command timed out or the controller reported fatal status; the
    /// controller has been disabled and nothing more is issued.
    dead: bool,
}

static NVME: Spinlock<Option<Nvme>> = Spinlock::new(None);

impl Nvme {
    /// Submit `e` on the admin or I/O queue and wait for its completion.
    // Queue memory is a `Page` (`align(4096)`): the u32 casts cannot misalign.
    #[allow(clippy::cast_ptr_alignment)]
    fn submit(&mut self, admin: bool, mut e: Sqe, what: &str) -> Result<Cqe, &'static str> {
        if self.dead {
            return Err("NVMe controller disabled after an earlier failure");
        }
        let cid = self.next_cid;
        self.next_cid = self.next_cid.wrapping_add(1);
        e[0] = (e[0] & 0xffff) | (u32::from(cid) << 16);
        let (sq_buf, cq_buf, qid) = if admin {
            (Buf::Asq, Buf::Acq, ADMIN_QID)
        } else {
            (Buf::Iosq, Buf::Iocq, IO_QID)
        };
        let (mut sq, mut cq) = if admin { (self.asq, self.acq) } else { (self.iosq, self.iocq) };

        let base = buf_ptr(sq_buf).0.cast::<u32>();
        for (i, d) in e.iter().enumerate() {
            // SAFETY: slot < QUEUE_ENTRIES, 16 dwords per entry, inside the
            // 4096-byte queue page; the controller does not read a slot past
            // the tail, and the tail moves only below.
            unsafe { base.add(usize::from(sq.slot()) * 16 + i).write_volatile(*d) };
        }
        compiler_fence(Ordering::SeqCst);
        let tail = sq.advance();
        w32(self.bar, self.cap.sq_tail_doorbell(qid), u32::from(tail));

        let cq_base = buf_ptr(cq_buf).0.cast::<u32>();
        let start = tsc();
        let result = loop {
            let mut raw = [0u32; 4];
            for (i, d) in raw.iter_mut().enumerate() {
                // SAFETY: as above, 4 dwords per 16-byte completion entry.
                *d = unsafe { cq_base.add(usize::from(cq.slot()) * 4 + i).read_volatile() };
            }
            let c = Cqe::decode(raw);
            if cq.is_new(&c) {
                compiler_fence(Ordering::SeqCst);
                let head = cq.advance();
                w32(self.bar, self.cap.cq_head_doorbell(qid), u32::from(head));
                if c.cid != cid {
                    // One command in flight means this cannot happen; if it
                    // does, the entry is not ours — keep waiting for ours.
                    serial::puts("  [nvme] stray completion cid=");
                    serial::put_dec(u64::from(c.cid));
                    serial::puts("\n");
                    continue;
                }
                if !c.status.ok() {
                    serial::puts("  [nvme] ");
                    serial::puts(what);
                    serial::puts(" failed: sct=");
                    serial::put_dec(u64::from(c.status.sct));
                    serial::puts(" sc=0x");
                    serial::put_hex(u64::from(c.status.sc));
                    serial::puts("\n");
                    break Err("NVMe command failed");
                }
                break Ok(c);
            }
            let csts = r32(self.bar, regs::CSTS);
            if Csts::is_absent(csts) || Csts::decode(csts).fatal {
                serial::puts("  [nvme] controller fatal status during ");
                serial::puts(what);
                serial::puts("\n");
                self.fail();
                break Err("NVMe controller fatal status");
            }
            if tsc().wrapping_sub(start) > ticks_ms(COMMAND_BUDGET_MS) {
                serial::puts("  [nvme] timeout: ");
                serial::puts(what);
                serial::puts(" — disabling the controller\n");
                self.fail();
                break Err("NVMe command timeout");
            }
            spin_us(5);
        };
        if admin {
            (self.asq, self.acq) = (sq, cq);
        } else {
            (self.iosq, self.iocq) = (sq, cq);
        }
        result
    }

    /// Stop the controller and its DMA for good. See "Polled, and fail-stop".
    fn fail(&mut self) {
        self.dead = true;
        let cc = r32(self.bar, regs::CC);
        w32(self.bar, regs::CC, cc & !regs::CC_EN);
        pci::quiesce(self.pci);
    }

    /// Move `blocks` logical blocks at **absolute** `lba` between the disk and
    /// the bounce buffer at `bounce_off`. Callers translate through the
    /// partition window first; the only absolute callers are the GPT reads.
    fn io(&mut self, write: bool, lba: u64, blocks: u32, bounce_off: usize) -> Result<(), &'static str> {
        let len = blocks as usize * self.lba_bytes as usize;
        if blocks == 0 || blocks > self.max_blocks || bounce_off + len > BOUNCE_LEN {
            return Err("NVMe transfer larger than the bounce buffer");
        }
        if lba.checked_add(u64::from(blocks)).is_none_or(|end| end > self.ns_lbas) {
            return Err("NVMe transfer past the end of the namespace");
        }
        let (prp1, prp2, _) = prp::plan(buf_phys(Buf::Bounce) + bounce_off as u64, len, buf_phys(Buf::PrpList), prp_list())
            .map_err(|_| "NVMe PRP plan refused")?;
        let what = if write { "write" } else { "read" };
        self.submit(false, cmd::rw(write, 0, NSID, lba, blocks, prp1, prp2), what).map(|_| ())
    }

    fn window(&self) -> Result<Window, &'static str> {
        self.window.ok_or("no NVMe partition open")
    }
}

/// `[nvme] <what> <ms> ms` — where a bring-up's time went, for the log a
/// metal boot leaves on disk (ryzen's first metal boot ran ~18 s long with no
/// way to say why).
fn took(what: &str, since: u64) {
    serial::puts("  nvme: ");
    serial::puts(what);
    serial::puts(" ");
    serial::put_dec(tsc().wrapping_sub(since) / ticks_ms(1).max(1));
    serial::puts(" ms\n");
}

fn wait_ready(bar: usize, cap: Cap, want: bool) -> Result<(), &'static str> {
    let start = tsc();
    loop {
        let raw = r32(bar, regs::CSTS);
        if Csts::is_absent(raw) {
            return Err("NVMe controller not responding (CSTS reads all-ones)");
        }
        let s = Csts::decode(raw);
        if s.ready == want {
            return Ok(());
        }
        if want && s.fatal {
            return Err("NVMe controller fatal status while enabling");
        }
        if tsc().wrapping_sub(start) > ticks_ms(cap.ready_timeout_ms()) {
            return Err(if want { "NVMe controller did not become ready" } else { "NVMe controller did not disable" });
        }
        spin_us(100);
    }
}

/// Find the NVMe controller, take it from the firmware, identify it and
/// namespace 1, and create one I/O queue pair. Reads nothing from the medium.
pub fn init() -> Result<(), &'static str> {
    let mut g = NVME.lock();
    if g.is_some() {
        return Ok(());
    }
    let dev = pci::find_class(0x01, 0x08)
        .filter(|d| d.header.prog_if == 0x02)
        .ok_or("no NVMe controller")?;
    serial::puts("  nvme: ");
    serial::put_hex(u64::from(dev.header.vendor_id));
    serial::puts(":");
    serial::put_hex(u64::from(dev.header.device_id));
    serial::puts("\n");
    pci::enable_full(dev.addr, true, true);
    let bar = dev.bars[0].ok_or("NVMe BAR0 not decoded")?;
    let (size, _) = pci::probe_bar_size(dev.addr, 0);
    // Registers to 0x40 plus the four doorbells used; 8 KiB covers any stride.
    let va = pci::map_bar(bar, size.max(0x2000)).ok_or("could not map NVMe BAR0")? as usize;

    let cap = Cap::decode(u64::from(r32(va, regs::CAP)) | (u64::from(r32(va, regs::CAP + 4)) << 32));
    serial::puts("  nvme: CAP.TO ");
    serial::put_dec(cap.ready_timeout_ms());
    serial::puts(" ms, MQES ");
    serial::put_dec(u64::from(cap.max_queue_entries()));
    serial::puts(", DSTRD ");
    serial::put_dec(u64::from(cap.dstrd));
    serial::puts(", firmware left CC=0x");
    serial::put_hex(u64::from(r32(va, regs::CC)));
    serial::puts(" CSTS=0x");
    serial::put_hex(u64::from(r32(va, regs::CSTS)));
    serial::puts("\n");
    let t0 = tsc();
    if !cap.css_nvm || !cap.supports_4k_pages() {
        return Err("NVMe controller lacks the NVM command set or 4 KiB pages");
    }
    if cap.max_queue_entries() < u32::from(QUEUE_ENTRIES) {
        return Err("NVMe controller queues too small");
    }

    // From the firmware: disable, wait for RDY to drop, then reprogram.
    let cc = r32(va, regs::CC);
    if cc & regs::CC_EN != 0 {
        w32(va, regs::CC, cc & !regs::CC_EN);
    }
    wait_ready(va, cap, false)?;
    took("disabled in", t0);

    for b in [Buf::Asq, Buf::Acq, Buf::Iosq, Buf::Iocq] {
        buf_mut(b).fill(0);
    }
    w32(va, regs::INTMS, u32::MAX);
    w32(va, regs::AQA, regs::aqa(QUEUE_ENTRIES, QUEUE_ENTRIES));
    w64(va, regs::ASQ, buf_phys(Buf::Asq));
    w64(va, regs::ACQ, buf_phys(Buf::Acq));
    compiler_fence(Ordering::SeqCst);
    w32(va, regs::CC, regs::cc_enable());
    let t1 = tsc();
    wait_ready(va, cap, true)?;
    took("ready in", t1);

    let mut n = Nvme {
        bar: va,
        cap,
        pci: dev.addr,
        asq: SqTail::new(QUEUE_ENTRIES),
        acq: CqHead::new(QUEUE_ENTRIES),
        iosq: SqTail::new(QUEUE_ENTRIES),
        iocq: CqHead::new(QUEUE_ENTRIES),
        next_cid: 1,
        lba_bytes: 0,
        ns_lbas: 0,
        max_blocks: 0,
        window: None,
        dead: false,
    };

    n.submit(true, cmd::identify(0, cmd::cns::CONTROLLER, 0, buf_phys(Buf::Ident)), "identify controller")?;
    let ctrl = Controller::parse(buf_mut(Buf::Ident)).ok_or("Identify Controller unreadable")?;
    serial::puts("  nvme: ");
    serial::puts(identify::text(&ctrl.model));
    serial::puts(" fw ");
    serial::puts(identify::text(&ctrl.firmware));
    serial::puts(", mdts ");
    serial::put_dec(ctrl.max_transfer_bytes().unwrap_or(0) / 1024);
    serial::puts(" KiB, vwc ");
    serial::puts(if ctrl.vwc { "yes" } else { "no" });
    serial::puts("\n");

    n.submit(true, cmd::identify(0, cmd::cns::NAMESPACE, NSID, buf_phys(Buf::Ident)), "identify namespace")?;
    let ns = Namespace::parse(buf_mut(Buf::Ident)).ok_or("namespace 1 inactive or unusable format")?;
    if ns.metadata_bytes != 0 {
        return Err("NVMe namespace format carries metadata");
    }
    n.lba_bytes = ns.lba_bytes;
    n.ns_lbas = ns.size_lbas;
    let per_cmd = ctrl.max_transfer_bytes().unwrap_or(u64::MAX).min(BOUNCE_LEN as u64);
    n.max_blocks = u32::try_from((per_cmd / u64::from(ns.lba_bytes)).clamp(1, 65_536)).unwrap_or(1);
    serial::puts("  nvme: ns1 ");
    serial::put_dec(ns.size_lbas * u64::from(ns.lba_bytes) / (1024 * 1024 * 1024));
    serial::puts(" GiB in ");
    serial::put_dec(u64::from(ns.lba_bytes));
    serial::puts("-byte blocks, ");
    serial::put_dec(u64::from(n.max_blocks));
    serial::puts(" blocks per command\n");

    n.submit(true, cmd::create_io_cq(0, IO_QID, QUEUE_ENTRIES, buf_phys(Buf::Iocq)), "create I/O CQ")?;
    n.submit(true, cmd::create_io_sq(0, IO_QID, QUEUE_ENTRIES, buf_phys(Buf::Iosq), IO_QID), "create I/O SQ")?;
    took("init total", t0);
    *g = Some(n);
    Ok(())
}

/// Select GPT partition `number` (Linux numbering: `nvme0n1p3` is 3) as the
/// only range later reads and writes may touch. Reads LBA 1 and the entry
/// array; refuses unless both CRCs check.
pub fn open_partition(number: u32) -> Result<(), &'static str> {
    let mut g = NVME.lock();
    let n = g.as_mut().ok_or("NVMe not initialised")?;
    let bl = n.lba_bytes as usize;
    n.io(false, 1, 1, 0)?;
    let hdr = Header::parse(&buf_mut(Buf::Bounce)[..bl]).map_err(|_| "no valid primary GPT")?;
    let blocks = hdr.entry_bytes().div_ceil(bl);
    if blocks * bl > BOUNCE_LEN {
        return Err("GPT entry array larger than the bounce buffer");
    }
    let mut done = 0;
    while done < blocks {
        let step = (blocks - done).min(n.max_blocks as usize);
        n.io(false, hdr.entries_lba + done as u64, step as u32, done * bl)?;
        done += step;
    }
    let entries = &buf_mut(Buf::Bounce)[..blocks * bl];
    hdr.check_entries(entries).map_err(|_| "GPT entry array CRC mismatch")?;
    let e = hdr.partition(entries, number).ok_or("no such GPT partition")?;
    let w = Window::new(e.first_lba, e.last_lba, n.ns_lbas, n.lba_bytes).ok_or("GPT partition outside the namespace")?;
    let mut name = [0u8; 36];
    let len = e.name_ascii(&mut name);
    serial::puts("  nvme: p");
    serial::put_dec(u64::from(number));
    serial::puts(" = LBA ");
    serial::put_dec(e.first_lba);
    serial::puts("..=");
    serial::put_dec(e.last_lba);
    serial::puts(" (");
    serial::put_dec(w.bytes() / (1024 * 1024 * 1024));
    serial::puts(" GiB) \"");
    serial::puts(core::str::from_utf8(&name[..len]).unwrap_or("?"));
    serial::puts("\"\n");
    n.window = Some(w);
    Ok(())
}

/// Read `buf.len()` bytes at byte `offset` **within the open partition**.
pub fn read_bytes(offset: u64, buf: &mut [u8]) -> Result<(), &'static str> {
    if buf.is_empty() {
        return Ok(());
    }
    let mut g = NVME.lock();
    let n = g.as_mut().ok_or("NVMe not initialised")?;
    let w = n.window()?;
    if !w.fits(offset, buf.len()) {
        return Err("read outside the NVMe partition");
    }
    let mut done = 0;
    while done < buf.len() {
        let c = chunk::next(offset, done, buf.len(), n.lba_bytes, n.max_blocks);
        let abs = w.absolute(c.lba, c.blocks).ok_or("read outside the NVMe partition")?;
        n.io(false, abs, c.blocks, 0)?;
        buf[done..done + c.take].copy_from_slice(&buf_mut(Buf::Bounce)[c.within..c.within + c.take]);
        done += c.take;
    }
    Ok(())
}

/// Write `data` at byte `offset` **within the open partition**. A partial block
/// at either end is read-modify-written.
pub fn write_bytes(offset: u64, data: &[u8]) -> Result<(), &'static str> {
    if data.is_empty() {
        return Ok(());
    }
    let mut g = NVME.lock();
    let n = g.as_mut().ok_or("NVMe not initialised")?;
    let w = n.window()?;
    if !w.fits(offset, data.len()) {
        return Err("write outside the NVMe partition");
    }
    let mut done = 0;
    while done < data.len() {
        let c = chunk::next(offset, done, data.len(), n.lba_bytes, n.max_blocks);
        let abs = w.absolute(c.lba, c.blocks).ok_or("write outside the NVMe partition")?;
        if c.partial(n.lba_bytes) {
            n.io(false, abs, c.blocks, 0)?;
        }
        buf_mut(Buf::Bounce)[c.within..c.within + c.take].copy_from_slice(&data[done..done + c.take]);
        n.io(true, abs, c.blocks, 0)?;
        done += c.take;
    }
    Ok(())
}

/// Flush the controller's volatile write cache. A no-op success when the
/// driver was never brought up.
pub fn flush() -> Result<(), &'static str> {
    let mut g = NVME.lock();
    let Some(n) = g.as_mut() else { return Ok(()) };
    n.submit(false, cmd::flush(0, NSID), "flush").map(|_| ())
}

/// Flush, normal shutdown, bus mastering off. For the reset path; the
/// controller is unusable afterwards. A no-op when never brought up.
pub fn shutdown() {
    let mut g = NVME.lock();
    let Some(n) = g.as_mut() else { return };
    if !n.dead {
        let _ = n.submit(false, cmd::flush(0, NSID), "flush before shutdown");
        let cc = r32(n.bar, regs::CC);
        w32(n.bar, regs::CC, (cc & !regs::CC_SHN_MASK) | regs::CC_SHN_NORMAL);
        let start = tsc();
        while Csts::decode(r32(n.bar, regs::CSTS)).shutdown != regs::SHST_COMPLETE
            && tsc().wrapping_sub(start) < ticks_ms(n.cap.ready_timeout_ms())
        {
            spin_us(100);
        }
    }
    pci::quiesce(n.pci);
    n.dead = true;
}
