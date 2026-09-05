//! The Realtek RTL8169/8168 as an [`ExternalDevice`](crate::ExternalDevice).
//!
//! `akuma-net-rtl8169` is the driver — pure logic over a [`Regs`] and a
//! [`Rings`], host-tested against a `FakeChip`. This module is the two things
//! that logic cannot be: the `unsafe` MMIO on a mapped BAR, and the DMA memory
//! with a known physical address. It is x86-only in practice (the amd64
//! bare-metal target is the one machine with the part), behind the `rtl8169`
//! feature so the aarch64 build never sees it.
//!
//! # The memory
//!
//! Two descriptor rings and their frame buffers, all in `.bss` statics — the
//! same place the virtio path keeps its arenas, translated the same way
//! (`akuma_primitives::addr::virt_to_phys`, which knows the kernel-image
//! window). No IOMMU on this target, so a physical address is a bus address.
//! The rings are 256-byte aligned: the chip ignores the low bits of the base
//! rather than faulting.
//!
//! A descriptor is four little-endian words. When handing one to the chip the
//! ownership word (`cmdstat`, holding `OWN`) is written **last**, after a
//! compiler fence, so the chip can never see `OWN` set over a stale buffer
//! address.
//!
//! # No zero-copy
//!
//! Unlike virtio-net the driver copies: `receive(dst)` fills a caller buffer,
//! `transmit(frame)` sends one. So the `Device` glue here is short — an rx
//! scratch, a separate tx scratch — with none of the lease dance `device.rs`
//! needs.

use core::sync::atomic::{Ordering, compiler_fence};

use akuma_net_rtl8169::desc::Desc;
use akuma_net_rtl8169::ring::RX_BUF_SIZE;
use akuma_net_rtl8169::{Nic, Regs, Rings};
use akuma_primitives::addr::virt_to_phys;
use akuma_primitives::mmio::MmioReg;
use smoltcp::phy::DeviceCapabilities;

use crate::counters::C;

/// Descriptors per ring. A power of two, small — this is a bring-up NIC on a
/// polled single-core kernel, not a throughput target.
const RING_LEN: usize = 16;

/// Bytes per frame buffer. `RX_BUF_SIZE` (2048) holds a full frame plus FCS.
const BUF_LEN: usize = RX_BUF_SIZE as usize;

/// One descriptor as its four raw words: `[cmdstat, vlan, buf_lo, buf_hi]`
/// (the field order of [`Desc`]).
type RawDesc = [u32; 4];

/// A 256-byte-aligned array of descriptors, in DMA-reachable `.bss`.
#[repr(C, align(256))]
struct DescRing([RawDesc; RING_LEN]);

/// Frame storage for one ring.
#[repr(C, align(64))]
struct BufRing([[u8; BUF_LEN]; RING_LEN]);

// One NIC on this target, so one set of statics. Every access is under the
// `NETWORK` spinlock (`Device::receive`/`transmit` run only from
// `iface.poll()`), which serialises the driver; the *chip* only touches a
// descriptor/buffer while its `OWN` bit says it may — the driver's whole
// protocol.
static mut RX_DESCS: DescRing = DescRing([[0; 4]; RING_LEN]);
static mut TX_DESCS: DescRing = DescRing([[0; 4]; RING_LEN]);
static mut RX_BUFS: BufRing = BufRing([[0; BUF_LEN]; RING_LEN]);
static mut TX_BUFS: BufRing = BufRing([[0; BUF_LEN]; RING_LEN]);

fn desc_word(ring: *const DescRing, i: usize, w: usize) -> MmioReg<u32> {
    // SAFETY (of `new`): a live, aligned `u32` inside a `.bss` static for the
    // kernel's lifetime — which is all `MmioReg` actually needs; it is not a
    // device register but volatile access to it is well-defined and is what we
    // want against memory the chip also writes.
    let addr = ring as usize + i * core::mem::size_of::<RawDesc>() + w * 4;
    unsafe { MmioReg::<u32>::new(addr) }
}

fn read_desc(ring: *const DescRing, i: usize) -> Desc {
    // `cmdstat` first: if `OWN` is clear the rest of the words are stable.
    let cmdstat = desc_word(ring, i, 0).read();
    Desc {
        cmdstat,
        vlan: desc_word(ring, i, 1).read(),
        buf_lo: desc_word(ring, i, 2).read(),
        buf_hi: desc_word(ring, i, 3).read(),
    }
}

fn write_desc(ring: *mut DescRing, i: usize, d: Desc) {
    // Everything but the ownership word first...
    desc_word(ring, i, 1).write(d.vlan);
    desc_word(ring, i, 2).write(d.buf_lo);
    desc_word(ring, i, 3).write(d.buf_hi);
    // ...then a fence, then `cmdstat` — the chip must never see `OWN` set over
    // a half-written descriptor.
    compiler_fence(Ordering::SeqCst);
    desc_word(ring, i, 0).write(d.cmdstat);
}

/// MMIO on the mapped register BAR.
struct Rtl8169Regs {
    base: usize,
}

// `base` is a device-mapped BAR; only ever accessed under `NETWORK`, from the
// one core in `iface.poll()`.
unsafe impl Send for Rtl8169Regs {}

impl Rtl8169Regs {
    fn reg<T: Copy>(&self, off: u16) -> MmioReg<T> {
        // SAFETY: `base` is the NIC's register BAR, mapped `MemAttr::Device`;
        // `off` is one of the < 256 standard-register offsets, of width `T` per
        // `akuma-net-rtl8169`'s register map, which keeps each register at its
        // natural alignment.
        unsafe { MmioReg::<T>::new(self.base + off as usize) }
    }
}

impl Regs for Rtl8169Regs {
    fn r8(&mut self, off: u16) -> u8 {
        self.reg::<u8>(off).read()
    }
    fn r16(&mut self, off: u16) -> u16 {
        self.reg::<u16>(off).read()
    }
    fn r32(&mut self, off: u16) -> u32 {
        self.reg::<u32>(off).read()
    }
    fn w8(&mut self, off: u16, val: u8) {
        self.reg::<u8>(off).write(val);
    }
    fn w16(&mut self, off: u16, val: u16) {
        self.reg::<u16>(off).write(val);
    }
    fn w32(&mut self, off: u16, val: u32) {
        self.reg::<u32>(off).write(val);
    }
    fn delay_us(&mut self, us: u32) {
        #[cfg(target_arch = "x86_64")]
        {
            // Assume a >= 1 GHz TSC — every x86_64 part is — so this over-waits
            // a bounded reset/MDIO poll on a fast box rather than ever
            // under-waiting and reporting a false timeout. TSC needs no
            // calibration and always advances.
            let target = u64::from(us) * 1000;
            // SAFETY: RDTSC is unprivileged and present on all x86_64.
            let start = unsafe { core::arch::x86_64::_rdtsc() };
            while unsafe { core::arch::x86_64::_rdtsc() }.wrapping_sub(start) < target {
                core::hint::spin_loop();
            }
        }
        // The feature is x86-only in practice; keep the crate compiling (dead)
        // if it is enabled elsewhere.
        #[cfg(not(target_arch = "x86_64"))]
        for _ in 0..us.saturating_mul(1000) {
            core::hint::spin_loop();
        }
    }
}

/// Descriptor rings and frame buffers, with the physical addresses the chip
/// needs.
struct Rtl8169Rings;

// All statics; every method runs under `NETWORK`.
unsafe impl Send for Rtl8169Rings {}

impl Rings for Rtl8169Rings {
    fn rx_ring_len(&self) -> usize {
        RING_LEN
    }
    fn tx_ring_len(&self) -> usize {
        RING_LEN
    }
    fn rx_ring_phys(&self) -> u64 {
        virt_to_phys(&raw const RX_DESCS as usize) as u64
    }
    fn tx_ring_phys(&self) -> u64 {
        virt_to_phys(&raw const TX_DESCS as usize) as u64
    }

    fn rx_desc(&self, i: usize) -> Desc {
        read_desc(&raw const RX_DESCS, i)
    }
    fn set_rx_desc(&mut self, i: usize, d: Desc) {
        write_desc(&raw mut RX_DESCS, i, d);
    }
    fn tx_desc(&self, i: usize) -> Desc {
        read_desc(&raw const TX_DESCS, i)
    }
    fn set_tx_desc(&mut self, i: usize, d: Desc) {
        write_desc(&raw mut TX_DESCS, i, d);
    }

    fn rx_buf_phys(&self, i: usize) -> u64 {
        virt_to_phys(&raw const RX_BUFS as usize) as u64 + (i * BUF_LEN) as u64
    }
    fn tx_buf_phys(&self, i: usize) -> u64 {
        virt_to_phys(&raw const TX_BUFS as usize) as u64 + (i * BUF_LEN) as u64
    }

    fn rx_buf_read(&self, i: usize, len: usize, dst: &mut [u8]) -> usize {
        let n = len.min(dst.len()).min(BUF_LEN);
        let base = (&raw const RX_BUFS).cast::<u8>();
        // SAFETY: `i < RING_LEN`; `n <= BUF_LEN`; the chip released this buffer
        // (its `OWN` bit is clear — the driver checked), so the bytes are
        // stable RAM.
        let src = unsafe { core::slice::from_raw_parts(base.add(i * BUF_LEN), n) };
        dst[..n].copy_from_slice(src);
        n
    }
    fn tx_buf_write(&mut self, i: usize, src: &[u8]) -> usize {
        let n = src.len().min(BUF_LEN);
        let base = (&raw mut TX_BUFS).cast::<u8>();
        // SAFETY: `i < RING_LEN`; `n <= BUF_LEN`. This descriptor is not owned
        // by the chip (the driver only writes a buffer before posting it).
        let dst = unsafe { core::slice::from_raw_parts_mut(base.add(i * BUF_LEN), n) };
        dst.copy_from_slice(&src[..n]);
        n
    }
    fn tx_buf_zero(&mut self, i: usize, from: usize, to: usize) {
        let (from, to) = (from.min(BUF_LEN), to.min(BUF_LEN));
        if from >= to {
            return;
        }
        let base = (&raw mut TX_BUFS).cast::<u8>();
        // SAFETY: as `tx_buf_write`; `i * BUF_LEN + from .. + to` is inside the
        // buffer array.
        unsafe { core::ptr::write_bytes(base.add(i * BUF_LEN + from), 0, to - from) };
    }
}

/// The Realtek NIC behind [`ExternalDevice::Rtl8169`](crate::ExternalDevice).
/// Consecutive fruitless receive laps before the stall dump fires. The poll
/// loop runs into the hundreds of thousands per second, so this is a second or
/// two of genuine silence — long enough not to fire on an idle link between
/// broadcasts, short enough to catch the failure while it is fresh.
const STALL_LAPS: u32 = 2_000_000;

/// How many times the receiver is restarted before giving up and staying quiet.
/// A chip that comes back on the first kick answers the question; one that
/// needs five has answered a different one, and either way the log stays
/// readable.
const MAX_KICKS: u32 = 5;

/// How many receive laps between PHY samples. See [`Rtl8169Device::take_rx_frame`].
const LINK_SAMPLE_LAPS: u32 = 1024;

pub struct Rtl8169Device {
    nic: Nic<Rtl8169Regs, Rtl8169Rings>,
    /// Lap counter for the periodic PHY sample.
    link_poll: u32,
    /// Consecutive laps that produced no frame.
    idle_laps: u32,
    /// How many stalls have been seen. The full ring dump prints on the first
    /// one only — once is a diagnosis, sixteen lines every two seconds is a
    /// screen nobody can read — and the recovery attempt is capped at
    /// [`MAX_KICKS`] so a chip that will not restart cannot bury the log.
    stalls: u32,
    /// The copy-out receive path's target, handed up to smoltcp as an
    /// `RxToken`. smoltcp may build a reply through a `TxToken` **while that
    /// token is live**, so the transmit path stages in `tx_scratch` instead.
    rx_scratch: [u8; BUF_LEN],
    tx_scratch: [u8; BUF_LEN],
}

impl Rtl8169Device {
    /// Probe and bring the chip up on a mapped register BAR.
    ///
    /// # Errors
    /// The chip did not respond, is a family member the driver has never run
    /// on, or the reset timed out — see [`akuma_net_rtl8169::Error`].
    ///
    /// # Safety
    /// `bar` is the NIC's device-mapped register BAR, valid for the life of
    /// the returned device; called once (it claims the module's ring statics).
    pub unsafe fn probe(bar: *mut u8) -> Result<Self, akuma_net_rtl8169::Error> {
        let mut nic = Nic::probe(Rtl8169Regs { base: bar as usize }, Rtl8169Rings)?;
        nic.init()?;
        // Publish the link once at bring-up, so a probe that reads it before the
        // first receive lap sees a real answer rather than "never sampled".
        let l = nic.link();
        let mbit = match l.speed {
            akuma_net_rtl8169::Speed::Mb10 => 10,
            akuma_net_rtl8169::Speed::Mb100 => 100,
            akuma_net_rtl8169::Speed::Mb1000 => 1000,
            akuma_net_rtl8169::Speed::Unknown => 0,
        };
        crate::counters::set_link_state(l.up, mbit, l.full_duplex);
        Ok(Self {
            nic,
            link_poll: 0,
            idle_laps: 0,
            stalls: 0,
            rx_scratch: [0; BUF_LEN],
            tx_scratch: [0; BUF_LEN],
        })
    }

    #[must_use]
    pub fn mac_address(&self) -> [u8; 6] {
        self.nic.mac().0
    }

    /// Where this driver's DMA memory actually is: `(name, virtual, physical)`
    /// for each of the four `.bss` arrays the chip is told about.
    ///
    /// Printed at bring-up because a wrong `virt_to_phys` here is invisible
    /// until it is catastrophic: the chip writes descriptors and frames at an
    /// address the driver never reads, so the ring looks permanently
    /// chip-owned, receive stops dead, and whatever *does* live at that
    /// physical address is quietly overwritten. Four numbers on the console
    /// settle in one boot what is otherwise inferred from wreckage.
    #[must_use]
    pub fn dma_layout() -> [(&'static str, usize, u64); 4] {
        [
            ("rx_desc", &raw const RX_DESCS as usize, virt_to_phys(&raw const RX_DESCS as usize) as u64),
            ("tx_desc", &raw const TX_DESCS as usize, virt_to_phys(&raw const TX_DESCS as usize) as u64),
            ("rx_bufs", &raw const RX_BUFS as usize, virt_to_phys(&raw const RX_BUFS as usize) as u64),
            ("tx_bufs", &raw const TX_BUFS as usize, virt_to_phys(&raw const TX_BUFS as usize) as u64),
        ]
    }

    /// Print what the chip says about itself, plus the whole receive ring.
    ///
    /// The one number that matters is `rdsar` against the `rx_desc pa=` printed
    /// at bring-up: equal means the chip is looking where we put the ring and
    /// the fault is in the descriptors; different means it is not, and the
    /// completions landing in unrelated kernel memory are simply it writing
    /// where it thinks the ring is.
    fn on_stall(&mut self) {
        let s = self.nic.snapshot();
        // The full picture once; after that the kick line alone, which is the
        // part that changes.
        if self.stalls > 1 {
            let (before, after) = self.nic.kick_receiver();
            crate::safe_print!(
                120,
                "[rtl] stall #{}: kick misc 0x{:08x} -> 0x{:08x} mpc={} cr=0x{:02x}\n",
                self.stalls, before, after, s.mpc, s.cr
            );
            return;
        }
        crate::safe_print!(96, "[rtl] STALL #1 after {} idle laps\n", STALL_LAPS);
        crate::safe_print!(
            128,
            "[rtl] cr=0x{:02x} isr=0x{:04x} imr=0x{:04x} rcr=0x{:08x} mpc={} misc=0x{:08x} rxdv_gated={} cursor={}/{}\n",
            s.cr, s.isr, s.imr, s.rcr, s.mpc, s.misc,
            u8::from(s.misc & akuma_net_rtl8169::regs::MISC_RXDV_GATED != 0),
            s.cursor, s.ring_len
        );
        crate::safe_print!(
            120,
            "[rtl] rdsar=0x{:016x} tnpds=0x{:016x} (expect rx_desc pa from bring-up)\n",
            s.rdsar, s.tnpds
        );
        for i in 0..s.ring_len.min(RING_LEN) {
            let d = self.nic.rx_desc_at(i);
            crate::safe_print!(
                104,
                "[rtl]  rx[{}] cmdstat=0x{:08x} buf=0x{:08x}{:08x}\n",
                i, d.cmdstat, d.buf_hi, d.buf_lo
            );
        }

        // Then try to restart it, and say whether the gate was actually set.
        // A recovery that works tells us what was wrong; one that does not
        // rules the gate out, which is worth almost as much.
        let (before, after) = self.nic.kick_receiver();
        crate::safe_print!(
            120,
            "[rtl] kick: misc 0x{:08x} -> 0x{:08x} (gate was {})\n",
            before, after,
            if before & akuma_net_rtl8169::regs::MISC_RXDV_GATED != 0 { "SET" } else { "clear" }
        );
    }

    #[allow(clippy::unused_self)] // symmetry with the other `ExternalDevice` arms
    pub(crate) fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = akuma_net_rtl8169::ring::MAX_FRAME;
        caps
    }

    pub(crate) fn take_rx_frame(&mut self) -> Option<(*mut u8, usize)> {
        // Reap finished transmits each poll lap — the only place they get
        // harvested on a request/response workload.
        self.nic.reclaim_tx();

        // **Acknowledge `ISR` every lap, even though nothing here takes
        // interrupts.** `ISR` latches independently of `IMR`: the mask decides
        // whether a bit raises an interrupt, not whether it is recorded. On a
        // polled path the bits therefore accumulate from `init`'s one clear
        // onwards and are never written back — and `RDU` (receive descriptor
        // unavailable) is not an idle status, it is a **stall**. The chip
        // raises it the moment the ring runs dry and does not resume taking
        // frames until it is cleared, which `take_interrupts` does by writing
        // the observed bits straight back.
        //
        // Measured on the HP box before this existed: `rx` climbed to exactly
        // 16 — `RING_LEN`, one ring's worth — and then never moved again for
        // the rest of the boot, while transmit carried on happily. Sixteen is
        // the tell; a receive path that dies on a round number died at a ring
        // boundary.
        let isr = self.nic.take_interrupts();
        if isr != 0 {
            C.rx_isr_seen.fetch_or(u32::from(isr), Ordering::Relaxed);
            if isr & akuma_net_rtl8169::regs::INT_RDU != 0 {
                C.rx_ring_dry.fetch_add(1, Ordering::Relaxed);
            }
        }

        // Sample the PHY every so often and publish it (`counters::link_state`).
        // Periodically rather than per lap because reading it is an MDIO
        // transaction, which is slow enough to matter on a poll loop that runs
        // thousands of times a second — and the link does not change that fast.
        //
        // This exists because the bare-metal bring-up had no way to tell "the
        // cable is not carrying" from "the driver is not receiving": both look
        // like an interface that is UP with nothing arriving. One is a
        // five-second fix at the switch and the other is a driver bug.
        self.link_poll = self.link_poll.wrapping_add(1);
        if self.link_poll.is_multiple_of(LINK_SAMPLE_LAPS) {
            let l = self.nic.link();
            let mbit = match l.speed {
                akuma_net_rtl8169::Speed::Mb10 => 10,
                akuma_net_rtl8169::Speed::Mb100 => 100,
                akuma_net_rtl8169::Speed::Mb1000 => 1000,
                akuma_net_rtl8169::Speed::Unknown => 0,
            };
            crate::counters::set_link_state(l.up, mbit, l.full_duplex);
        }

        // Stall watch. Receive dying at a ring boundary with no error bit set
        // has now cost three rounds of theorising; this dumps what the chip
        // says about itself the moment it stops, once, and then never again.
        let Some(n) = self.nic.receive(&mut self.rx_scratch) else {
            self.idle_laps = self.idle_laps.saturating_add(1);
            if self.idle_laps >= STALL_LAPS && self.stalls < MAX_KICKS {
                self.stalls += 1;
                self.idle_laps = 0;
                self.on_stall();
            }
            return None;
        };
        self.idle_laps = 0;
        // Only reached when a frame really came off the ring, so this counts
        // wire arrivals. The virtio path bumps the same counter in `device.rs`;
        // until 2026-09-05 this one bumped nothing, so `rx_counters()` read a
        // flat zero on the only target that has this chip — the exact number
        // the bring-up needed.
        C.rx_frames_received.fetch_add(1, Ordering::Relaxed);
        // The pointer's provenance is `rx_scratch` (a field), not `self`
        // broadly — the borrow checker cannot see that through
        // `from_raw_parts`, which is why the virtio path hands back a raw
        // pointer too. The caller uses it and drops it before the next call.
        Some((self.rx_scratch.as_mut_ptr(), n))
    }

    pub(crate) fn emit_frame<R>(
        &mut self,
        len: usize,
        fill: impl FnOnce(&mut [u8]) -> R,
        divert: impl FnOnce(&[u8]) -> bool,
    ) -> R {
        let end = len.min(BUF_LEN);
        let res = fill(&mut self.tx_scratch[..end]);
        if divert(&self.tx_scratch[..end]) {
            return res;
        }
        if self.nic.transmit(&self.tx_scratch[..end]).is_err() {
            C.tx_drop_count.fetch_add(1, Ordering::Relaxed);
        } else {
            C.tx_frames_sent.fetch_add(1, Ordering::Relaxed);
        }
        res
    }
}
