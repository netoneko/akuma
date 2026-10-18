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

use core::sync::atomic::{AtomicBool, Ordering, compiler_fence};

use akuma_net_rtl8169::desc::Desc;
use akuma_net_rtl8169::ring::RX_BUF_SIZE;
use akuma_net_rtl8169::stall::{self, StallArm};
use akuma_net_rtl8169::{Nic, Regs, Rings};
use akuma_primitives::addr::virt_to_phys;
use akuma_dma::DmaArray;
use akuma_dma::align::{A256, A64};
use akuma_primitives::mmio::MmioWindow;
use smoltcp::phy::DeviceCapabilities;

use crate::counters::C;

/// Descriptors per ring. A power of two, small — this is a bring-up NIC on a
/// polled single-core kernel, not a throughput target.
const RING_LEN: usize = 16;

/// Bytes per frame buffer. `RX_BUF_SIZE` (2048) holds a full frame plus FCS.
const BUF_LEN: usize = RX_BUF_SIZE as usize;

/// Words in one descriptor: `[cmdstat, vlan, buf_lo, buf_hi]` (the field order
/// of [`Desc`]).
const DESC_WORDS: usize = 4;

// One NIC on this target, so one set of statics. Every access is under the
// `NETWORK` spinlock (`Device::receive`/`transmit` run only from
// `iface.poll()`), which serialises the driver; the *chip* only touches a
// descriptor/buffer while its `OWN` bit says it may — the driver's whole
// protocol. The descriptor rings are `u32` cells (256-byte aligned, as the chip
// requires) read and written one volatile word at a time, because the chip
// writes them; the frame buffers are bytes, touched as a slice only over the one
// slot the `OWN` protocol says the CPU holds.
static RX_DESCS: DmaArray<u32, { RING_LEN * DESC_WORDS }, A256> = DmaArray::new();
static TX_DESCS: DmaArray<u32, { RING_LEN * DESC_WORDS }, A256> = DmaArray::new();
static RX_BUFS: DmaArray<u8, { RING_LEN * BUF_LEN }, A64> = DmaArray::new();
static TX_BUFS: DmaArray<u8, { RING_LEN * BUF_LEN }, A64> = DmaArray::new();

/// Whether a successful [`Rtl8169Device::probe`] has claimed the statics
/// above. Set only after a full bring-up, so a failed probe can be retried;
/// set exactly once, ever, because one NIC owns these arrays.
static RTL8169_CLAIMED: AtomicBool = AtomicBool::new(false);

fn read_desc(ring: &DmaArray<u32, { RING_LEN * DESC_WORDS }, A256>, i: usize) -> Desc {
    // `cmdstat` first: if `OWN` is clear the rest of the words are stable.
    let cmdstat = ring.get(i * DESC_WORDS);
    Desc {
        cmdstat,
        vlan: ring.get(i * DESC_WORDS + 1),
        buf_lo: ring.get(i * DESC_WORDS + 2),
        buf_hi: ring.get(i * DESC_WORDS + 3),
    }
}

fn write_desc(ring: &DmaArray<u32, { RING_LEN * DESC_WORDS }, A256>, i: usize, d: Desc) {
    // Everything but the ownership word first...
    ring.set(i * DESC_WORDS + 1, d.vlan);
    ring.set(i * DESC_WORDS + 2, d.buf_lo);
    ring.set(i * DESC_WORDS + 3, d.buf_hi);
    // ...then a fence, then `cmdstat` — the chip must never see `OWN` set over
    // a half-written descriptor.
    compiler_fence(Ordering::SeqCst);
    ring.set(i * DESC_WORDS, d.cmdstat);
}

/// Bytes of the register BAR the driver addresses: the standard registers all
/// sit below 0x100 (`akuma-net-rtl8169`'s map ends at `MISC`, 0xF0).
const REG_WINDOW_LEN: usize = 0x100;

/// MMIO on the mapped register BAR.
struct Rtl8169Regs {
    win: MmioWindow,
}

impl Regs for Rtl8169Regs {
    fn r8(&mut self, off: u16) -> u8 {
        self.win.read::<u8>(off as usize)
    }
    fn r16(&mut self, off: u16) -> u16 {
        self.win.read::<u16>(off as usize)
    }
    fn r32(&mut self, off: u16) -> u32 {
        self.win.read::<u32>(off as usize)
    }
    fn w8(&mut self, off: u16, val: u8) {
        self.win.write::<u8>(off as usize, val);
    }
    fn w16(&mut self, off: u16, val: u16) {
        self.win.write::<u16>(off as usize, val);
    }
    fn w32(&mut self, off: u16, val: u32) {
        self.win.write::<u32>(off as usize, val);
    }
    fn delay_us(&mut self, us: u32) {
        #[cfg(all(target_os = "none", target_arch = "x86_64"))]
        {
            // Assume a >= 1 GHz TSC — every x86_64 part is — so this over-waits
            // a bounded reset/MDIO poll on a fast box rather than ever
            // under-waiting and reporting a false timeout. TSC needs no
            // calibration and always advances.
            let target = u64::from(us) * 1000;
            let start = akuma_cpu::tsc::rdtsc();
            while akuma_cpu::tsc::rdtsc().wrapping_sub(start) < target {
                core::hint::spin_loop();
            }
        }
        // The feature is x86-only in practice; keep the crate compiling (dead)
        // if it is enabled elsewhere.
        #[cfg(not(all(target_os = "none", target_arch = "x86_64")))]
        for _ in 0..us.saturating_mul(1000) {
            core::hint::spin_loop();
        }
    }
}

/// Descriptor rings and frame buffers, with the physical addresses the chip
/// needs.
struct Rtl8169Rings;

impl Rings for Rtl8169Rings {
    fn rx_ring_len(&self) -> usize {
        RING_LEN
    }
    fn tx_ring_len(&self) -> usize {
        RING_LEN
    }
    fn rx_ring_phys(&self) -> u64 {
        virt_to_phys(RX_DESCS.addr()) as u64
    }
    fn tx_ring_phys(&self) -> u64 {
        virt_to_phys(TX_DESCS.addr()) as u64
    }

    fn rx_desc(&self, i: usize) -> Desc {
        read_desc(&RX_DESCS, i)
    }
    fn set_rx_desc(&mut self, i: usize, d: Desc) {
        write_desc(&RX_DESCS, i, d);
    }
    fn tx_desc(&self, i: usize) -> Desc {
        read_desc(&TX_DESCS, i)
    }
    fn set_tx_desc(&mut self, i: usize, d: Desc) {
        write_desc(&TX_DESCS, i, d);
    }

    fn rx_buf_phys(&self, i: usize) -> u64 {
        virt_to_phys(RX_BUFS.addr()) as u64 + (i * BUF_LEN) as u64
    }
    fn tx_buf_phys(&self, i: usize) -> u64 {
        virt_to_phys(TX_BUFS.addr()) as u64 + (i * BUF_LEN) as u64
    }

    fn tx_buf_len(&self, _i: usize) -> usize {
        BUF_LEN
    }

    fn rx_buf_read(&self, i: usize, len: usize, dst: &mut [u8]) -> usize {
        let n = len.min(dst.len()).min(BUF_LEN);
        // The chip released this buffer (its `OWN` bit is clear — the driver
        // checked), so the bytes of slot `i` are stable; the range check keeps
        // `i` inside the ring.
        RX_BUFS.read_range(i * BUF_LEN, n, |src| dst[..n].copy_from_slice(src));
        n
    }
    fn tx_buf_write(&mut self, i: usize, src: &[u8]) -> usize {
        let n = src.len().min(BUF_LEN);
        // This descriptor is not owned by the chip (the driver only writes a
        // buffer before posting it).
        TX_BUFS.write_range(i * BUF_LEN, n, |dst| dst.copy_from_slice(&src[..n]));
        n
    }
    fn tx_buf_zero(&mut self, i: usize, from: usize, to: usize) {
        let (from, to) = (from.min(BUF_LEN), to.min(BUF_LEN));
        if from >= to {
            return;
        }
        TX_BUFS.write_range(i * BUF_LEN + from, to - from, |dst| dst.fill(0));
    }
}

/// The Realtek NIC behind [`ExternalDevice::Rtl8169`](crate::ExternalDevice).
/// Consecutive fruitless receive laps before the stall dump fires — the
/// **fallback** horizon, used only until the clock seam is registered.
///
/// This used to be the only horizon, and the comment it carried ("the poll loop
/// runs into the hundreds of thousands per second, so this is a second or two
/// of genuine silence") stopped being true on the target that has this chip.
/// `amd64`'s `netpoll_daemon` parks for one LAPIC tick whenever a lap moved
/// nothing (`NETPOLL_IDLE_PARK_US`), so an idle receive lap now runs about
/// **100 times a second, not hundreds of thousands** — and 2,000,000 of them is
/// five and a half hours, not two seconds. The recovery below therefore never
/// fired in practice after boot: the one time it was observed
/// (`[rtl] STALL #1 after 2000000 idle laps`, quoted in
/// `docs/archive/AKUMA_AMD64_STREAM_END_STALL.md`) was during *bring-up*, while
/// the loop was still busy-spinning, which is exactly why the calibration
/// breaking afterwards went unnoticed.
///
/// A lap count cannot express "two seconds" on a loop whose rate is a scheduling
/// decision, so the real horizon is [`STALL_QUIET_US`] and this only covers the
/// window before `uptime_us` is available.
const STALL_LAPS: u32 = 2_000_000;

/// [`STALL_LAPS`], for the case where **no frame has arrived since bring-up**.
///
/// Two orders of magnitude smaller, because it is answering a different and
/// much easier question. `STALL_LAPS` has to avoid calling a quiet link broken;
/// this one only fires while the receiver has never produced anything, which no
/// working link on a LAN does for long — broadcast traffic alone climbs the
/// counter within seconds.
const STALL_LAPS_UNSTARTED: u32 = 20_000;

/// How many consecutive frameless laps before the cursor is suspected of having
/// drifted. A desync cannot appear on a lap that delivered a frame, so any
/// non-zero value is safe; this one keeps a busy link from ever scanning.
const RESYNC_AFTER_IDLE_LAPS: u32 = 64;

/// How often to scan once suspected. The scan is at most one ring of
/// descriptor reads, so this bounds the idle cost at a sixty-fourth of that,
/// while repairing within milliseconds instead of [`STALL_QUIET_US`]'s five
/// seconds.
const RESYNC_SCAN_LAPS: u32 = 64;

/// Console reports before the resync goes quiet. The repair itself is never
/// capped — same trade as [`MAX_STALL_REPORTS`], for the same reason.
const MAX_RESYNC_REPORTS: usize = 5;

/// `uptime_us`, or `None` before the runtime seam is registered (early boot and
/// host tests). The stall watch measures against this; `None` means fall back
/// to the lap count, never to a bogus zero.
#[inline]
fn now_us() -> Option<u64> {
    akuma_primitives::net_runtime::try_runtime().map(|rt| (rt.uptime_us)())
}

/// How long receive may stay silent before the recovery fires, in microseconds.
///
/// Wall-clock, so it means the same thing whether the poll loop is spinning
/// through bring-up or parked at one lap per tick — which is the whole point of
/// replacing the lap count.
///
/// Five seconds rather than the two the lap count was meant to express: the
/// kick resets the ring cursor, so a frame the chip has written but the driver
/// has not yet read comes out late — out of ring order — across it, and on a
/// quiet link that is a cost paid for nothing. The crate's own bring-up note —
/// "on any real LAN this climbs within seconds from broadcast traffic alone" —
/// is what makes five seconds still a stall rather than an idle link.
const STALL_QUIET_US: u64 = 5_000_000;

/// How many stalls are reported on the console before the recovery goes quiet.
///
/// The recovery itself is **not** capped — it is what keeps the machine on the
/// network, and a chip that needs restarting every few seconds still needs
/// restarting. Only the printing stops, because a console that scrolls its own
/// diagnosis away is no better than one that never printed it. The running
/// total stays visible in the probe line as `kicks=`.
const MAX_STALL_REPORTS: u32 = 5;

/// How many receive laps between PHY samples. See [`Rtl8169Device::take_rx_frame`].
const LINK_SAMPLE_LAPS: u32 = 1024;

/// How many failed blind recoveries before the whole chip is re-initialised
/// rather than merely kicked. Four, so the cheap recovery gets a fair run
/// (~20 s at [`STALL_QUIET_US`]) before the expensive one is tried.
const BLIND_REINIT_EVERY: u32 = 4;

/// How long a receiver that **has** produced frames may stay silent before it
/// is treated as stalled.
///
/// This is the horizon for the case with no other evidence at all, and the one
/// that had no arm until 2026-09-20: frames arrived, the receiver stopped, and
/// the chip stayed quiet — `rx_seen` had retired [`StallArm::Blind`] for the
/// boot, and a receiver taking nothing off the wire can never raise `RDU`
/// again, so [`StallArm::Backpressure`] could not arm either. The box sat deaf
/// with `kicks=0` and every recovery in this file intact and unreachable.
///
/// Ten seconds. The number is a judgement about *this LAN*, and the crate's own
/// bring-up note is what sets it: broadcast traffic alone climbs the receive
/// counter within seconds on any real network, so ten seconds of total silence
/// is already a fault rather than a quiet moment. It was a minute for one
/// build and that was too generous — the box is off the network for every
/// second of the window, and the escalation to a full re-init is four windows
/// further out again.
///
/// Still twice [`STALL_QUIET_US`], because this arm has no evidence beyond the
/// passage of time while that one has the chip's own `RDU` report. Being wrong
/// costs one restarted receiver — a frame the chip has written and the driver
/// has not yet read — so the floor is "long enough that a working LAN always
/// beats it", not "as short as possible".
const STALL_SILENT_US: u64 = 10_000_000;

/// Silence between *subsequent* [`StallArm::Silent`] firings.
///
/// Slow to first accuse a quiet link of being broken, quick to retry once it
/// has proved itself pathological. Without the asymmetry the recovery cadence
/// would be the accusation threshold, and the escalation to a full re-init
/// (every [`BLIND_REINIT_EVERY`] attempts) would land four minutes after the
/// first one — on a box that is off the network for every second of it.
const STALL_SILENT_RETRY_US: u64 = STALL_QUIET_US;

/// The horizons above, as the shape [`stall::decide`] takes. The numbers stay
/// here because they are statements about this poll loop and this link; the
/// *order* the arms are tested in lives in the crate, where it has tests.
const STALL_POLICY: stall::StallPolicy = stall::StallPolicy {
    quiet_us: STALL_QUIET_US,
    silent_us: STALL_SILENT_US,
    silent_retry_us: STALL_SILENT_RETRY_US,
    laps_unstarted: STALL_LAPS_UNSTARTED,
    laps: STALL_LAPS,
};

/// The transmit watch's horizons (`stall::tx_decide`'s policy): a full ring
/// with no completion for [`STALL_QUIET_US`] gets the doorbell re-rung — the
/// cheap repair, a chip that has in some wedge states merely stopped looking
/// — and a full ring with no completion for thirty seconds, after two failed
/// doorbells, gets the same full re-init the receive side escalates to. The
/// pre-clock lap fallbacks assume the parked loop's ~100 laps/s, like the
/// receive fallbacks above.
///
/// This watch exists because every receive arm is structurally blind to a
/// transmit wedge: a chip that stops draining the TX ring while still
/// receiving leaves `rx_seen` true, raises no `RDU`, and looks healthy to
/// every arm above — while `transmit` returns `Full` forever and the
/// machine is off the network for everything it wants to say
/// (`docs/archive/NETWORKING_AUDIT.md` §3).
const TX_POLICY: stall::TxPolicy = stall::TxPolicy {
    doorbell_us: STALL_QUIET_US,
    reinit_us: 30_000_000,
    doorbell_laps: 500,
    reinit_laps: 3_000,
    doorbells_before_reinit: 2,
};


pub struct Rtl8169Device {
    nic: Nic<Rtl8169Regs, Rtl8169Rings>,
    /// Lap counter for the periodic PHY sample.
    link_poll: u32,
    /// Consecutive laps that produced no frame. Only consulted while
    /// [`Self::last_rx_us`] is `None` — see [`STALL_LAPS`].
    idle_laps: u32,
    /// `uptime_us` at the last frame that actually came off the ring, or `None`
    /// before the clock seam exists / before the first frame. This is what the
    /// stall watch measures against; the lap counter is the fallback.
    last_rx_us: Option<u64>,
    /// Has the chip told us it *could not take a frame* since the last one it
    /// gave us? Set from `INT_RDU`, which the per-lap `take_interrupts()`
    /// already reads — deliberately **not** from `MPC`, which would cost an
    /// MMIO read on a path that must stay free.
    ///
    /// **Silence is not a stall**, and separating the two is the whole point of
    /// this flag. The wall-clock window alone fired on an idle LAN — measured
    /// on the box 2026-09-19, `[rtl] stall #3: kick misc 0x3f -> 0x3f mpc=0`:
    /// no missed packets, and the kick changed nothing, because there was
    /// nothing wrong. A link with nothing to deliver looks exactly like a dead
    /// receiver from the ring's side, and an LLM stream is quiet for tens of
    /// seconds at a time between bursts. Only the chip can tell them apart, and
    /// it does: `RDU` means the ring ran dry with a frame waiting, and `MPC`
    /// counts frames dropped for want of a descriptor.
    rx_backpressure: bool,
    /// How many stalls have been seen. The full ring dump prints on the first
    /// one only — once is a diagnosis, sixteen lines every two seconds is a
    /// screen nobody can read — and the recovery attempt is capped at
    /// [`MAX_KICKS`] so a chip that will not restart cannot bury the log.
    stalls: u32,
    /// What the last PHY sample said about carrier.
    ///
    /// Kept so the periodic sample can see a **down -> up edge**, which is the
    /// one moment this chip needs the receiver re-armed: `init` clears
    /// `MISC_RXDV_GATED` once, before the link is up, and the hardware can
    /// re-gate across the transition. Nothing else re-clears it.
    link_up: bool,
    /// Has a single frame ever come off the ring since bring-up?
    ///
    /// Deliberately separate from [`Self::last_rx_us`], which the stall watch
    /// sets to "now" the first time it sees a clock so that bring-up silence is
    /// not counted against the chip — so it cannot answer this question.
    rx_seen: bool,
    /// How many recoveries the "nothing has ever arrived" suspicion has spent.
    /// Counted for the console line, never used as a limit.
    blind_kicks: u32,
    /// How many times the [`StallArm::Silent`] arm has fired. Separate from
    /// [`Self::blind_kicks`] so the two recoveries stay tellable apart in a log
    /// and escalate independently — attribution between arms has already cost a
    /// session's worth of uncertainty once.
    silent_kicks: u32,
    /// The copy-out receive path's target, handed up to smoltcp as an
    /// `RxToken`. smoltcp may build a reply through a `TxToken` **while that
    /// token is live**, so the transmit path stages in `tx_scratch` instead.
    rx_scratch: [u8; BUF_LEN],
    tx_scratch: [u8; BUF_LEN],
    /// Wall clock of the last lap that reclaimed a transmit descriptor or
    /// found the transmit ring uncongested — the progress that disarms the
    /// transmit stall watch. `None` before the clock seam exists.
    tx_last_progress_us: Option<u64>,
    /// Consecutive full-with-no-reclaim laps, the pre-clock fallback for the
    /// transmit watch.
    tx_stalled_laps: u32,
    /// Doorbell re-rings spent on the current transmit stall.
    tx_doorbells: u32,
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
    /// the returned device; called once — a second *successful* call panics,
    /// because the module's ring statics can serve one `Nic` and two cursors
    /// over the same descriptors is a violation the ownership protocol
    /// cannot recover from.
    pub unsafe fn probe(bar: *mut u8) -> Result<Self, akuma_net_rtl8169::Error> {
        // SAFETY: the caller vouches `bar` is the NIC's device-mapped register BAR
        // for the life of the device (page-aligned); the standard registers all
        // lie within the first `REG_WINDOW_LEN` bytes of it.
        let win = unsafe { MmioWindow::new(bar as usize, REG_WINDOW_LEN) };
        let mut nic = Nic::probe(Rtl8169Regs { win }, Rtl8169Rings)?;
        nic.init()?;
        // Claimed only on success: a failed probe (absent or wrong chip,
        // timed-out reset) leaves the statics untouched, so a corrected
        // re-probe can retry. The guard used to be a sentence in this doc
        // comment; a contract that can be forgotten has to be a bit.
        assert!(
            !RTL8169_CLAIMED.swap(true, Ordering::AcqRel),
            "rtl8169 probe called twice: the ring statics are already claimed",
        );
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
            last_rx_us: None,
            rx_backpressure: false,
            stalls: 0,
            link_up: l.up,
            rx_seen: false,
            blind_kicks: 0,
            silent_kicks: 0,
            rx_scratch: [0; BUF_LEN],
            tx_scratch: [0; BUF_LEN],
            tx_last_progress_us: None,
            tx_stalled_laps: 0,
            tx_doorbells: 0,
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
            ("rx_desc", RX_DESCS.addr(), virt_to_phys(RX_DESCS.addr()) as u64),
            ("tx_desc", TX_DESCS.addr(), virt_to_phys(TX_DESCS.addr()) as u64),
            ("rx_bufs", RX_BUFS.addr(), virt_to_phys(RX_BUFS.addr()) as u64),
            ("tx_bufs", TX_BUFS.addr(), virt_to_phys(TX_BUFS.addr()) as u64),
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
        crate::counters::C.rx_kicks.fetch_add(1, Ordering::Relaxed);

        // The full picture once; after that a single line; after
        // `MAX_STALL_REPORTS`, nothing at all — but the kick still happens.
        if self.stalls > 1 {
            // Cheapest repair first. If the chip and the cursor have merely
            // drifted apart, moving the cursor costs no MMIO, does not stop
            // the receiver and loses nothing in flight — so there is no reason
            // to reach for the kick until it has failed.
            if let Some(i) = self.nic.resync_rx_cursor() {
                if self.stalls <= MAX_STALL_REPORTS {
                    crate::safe_print!(
                        112,
                        "[rtl] stall #{}: resync -> cursor {} (frame was waiting)\n",
                        self.stalls, i
                    );
                }
                return;
            }
            let s = self.nic.snapshot();
            let (before, after) = self.nic.kick_receiver();
            if self.stalls <= MAX_STALL_REPORTS {
                crate::safe_print!(
                    128,
                    "[rtl] stall #{}: kick misc 0x{:08x} -> 0x{:08x} mpc={} cr=0x{:02x} rx={}\n",
                    self.stalls, before, after, s.mpc, s.cr,
                    C.rx_frames_received.load(Ordering::Relaxed)
                );
            }
            return;
        }
        let s = self.nic.snapshot();
        // `rx=` is the number that decides what this fault *is*, and it was
        // missing for the whole 2026-09-20 investigation. `OWN=1` in a dump
        // means both "never filled" and "filled, consumed, re-posted", so a
        // ring of `OWN=1` descriptors cannot say whether the driver consumed a
        // ring's worth or nothing at all:
        //
        //   rx == 0            -> the cursor advanced without ever consuming;
        //                         the bug is in bring-up's post/advance.
        //   rx == ring length  -> the driver consumed a ring's worth and the
        //                         chip then rewound to the ring base under it;
        //                         the bug is in whatever restarted the
        //                         receiver without resynchronising the cursor.
        //
        // The second is what `kick_receiver` already assumes — it resets the
        // cursor precisely because restarting the receiver sends the chip back
        // to slot 0 — and what this file's own note describes: "`rx` climbed to
        // exactly 16 — `RING_LEN`, one ring's worth — and then never moved".
        crate::safe_print!(
            120,
            "[rtl] STALL #1 after {} idle laps (rx={} frames, blind_kicks={})\n",
            STALL_LAPS,
            C.rx_frames_received.load(Ordering::Relaxed),
            self.blind_kicks
        );
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

        // Repair the disagreement before restarting anything. The dump above is
        // the evidence and has already been printed, so trying this first costs
        // nothing diagnostically — and where it works it is the better fix,
        // because `kick_receiver` throws away whatever the chip has in flight.
        if let Some(i) = self.nic.resync_rx_cursor() {
            crate::safe_print!(
                112,
                "[rtl] resync -> cursor {} (a completed descriptor was waiting)\n",
                i
            );
            return;
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
        // harvested on a request/response workload. The count is also the
        // transmit stall watch's progress signal: any reclaim says the wire
        // is moving.
        let reclaimed = self.nic.reclaim_tx();
        if reclaimed > 0 {
            // Completions, not queues: `tx_frames_sent` is bumped when a
            // frame is *accepted*, and until the chip hands the descriptor
            // back that number proves only that we tried. The gap between
            // the two is what a wedged transmitter looks like in the
            // counters (`docs/archive/NETWORKING_AUDIT.md` §3).
            C.tx_frames_completed.fetch_add(reclaimed, Ordering::Relaxed);
        }

        // The transmit stall watch — the half the receive arms cannot do,
        // because receive keeping working is exactly what leaves a wedged
        // transmitter invisible to them. Decision in `stall::tx_decide`,
        // host-tested; the recovery ladder mirrors the receive side's:
        // doorbell first, full re-init once the cheap repair has failed
        // across the long horizon. Never capped — only its printing is
        // ([`MAX_STALL_REPORTS`]), for the same reason as receive.
        let now = now_us();
        let stalled_us = match (now, self.tx_last_progress_us) {
            (Some(now), Some(last)) => Some(now.saturating_sub(last)),
            _ => None,
        };
        match stall::tx_decide(
            &TX_POLICY,
            &stall::TxInputs {
                ring_full: !self.nic.can_transmit(),
                reclaimed,
                stalled_us,
                stalled_laps: self.tx_stalled_laps,
                doorbells: self.tx_doorbells,
            },
        ) {
            stall::TxArm::None => {}
            stall::TxArm::Doorbell => {
                self.tx_doorbells += 1;
                C.tx_stalls.fetch_add(1, Ordering::Relaxed);
                if C.tx_stalls.load(Ordering::Relaxed) <= MAX_STALL_REPORTS as usize {
                    crate::safe_print!(
                        112,
                        "[rtl] tx stall #{}: ring full, no completions - re-ringing doorbell (queued={} completed={})\n",
                        C.tx_stalls.load(Ordering::Relaxed),
                        C.tx_frames_sent.load(Ordering::Relaxed),
                        C.tx_frames_completed.load(Ordering::Relaxed)
                    );
                }
                self.nic.ring_transmitter();
            }
            stall::TxArm::Reinit => {
                C.tx_stalls.fetch_add(1, Ordering::Relaxed);
                let r = self.nic.init();
                crate::safe_print!(
                    104,
                    "[rtl] tx stall: doorbell exhausted - full re-init -> {}\n",
                    if r.is_ok() { "ok" } else { "FAILED" }
                );
                // The re-init repopulates both rings and resets the driver's
                // cursors; the watch restarts with them.
                self.tx_last_progress_us = now;
                self.tx_stalled_laps = 0;
                self.tx_doorbells = 0;
            }
        }
        // Progress disarms the watch; a congested ring with nothing coming
        // back accumulates toward the horizons.
        if reclaimed > 0 || self.nic.can_transmit() {
            self.tx_last_progress_us = now;
            self.tx_stalled_laps = 0;
            self.tx_doorbells = 0;
        } else {
            self.tx_stalled_laps = self.tx_stalled_laps.saturating_add(1);
        }

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
                // The ring ran dry with the chip wanting to hand a frame over.
                // That, not the passage of time, is what makes the quiet below
                // a stall.
                self.rx_backpressure = true;
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

            // A **down -> up edge** re-arms the receiver, and nothing else
            // does. `init` clears `MISC_RXDV_GATED` exactly once, and it runs
            // before the link is up: measured on the HP box 2026-09-20, this
            // part negotiates ~3 s after the PHY is touched (Ubuntu's own
            // `r8169` log: `Link is Down` 63.806 s -> `Link is Up - 1Gbps/Full`
            // 66.736 s), while Akuma reaches `init` far sooner. The hardware
            // can re-gate RXDV across that transition, and the result is a
            // receiver that is off for the rest of the boot while transmit
            // carries on — which is invisible from this side and reads exactly
            // like a dead LAN.
            //
            // One MMIO burst, on the edge only. The per-lap register reads that
            // this path must never regain are what took the box down twice on
            // 2026-09-19 (`AMD64_TRASHCAN_ISSUES.md` §7b).
            if l.up && !self.link_up {
                let (before, after) = self.nic.kick_receiver();
                crate::safe_print!(
                    120,
                    "[rtl] link up: re-armed receiver (misc 0x{:08x} -> 0x{:08x}, gate was {})\n",
                    before, after,
                    if before & akuma_net_rtl8169::regs::MISC_RXDV_GATED != 0 { "SET" } else { "clear" }
                );
            }
            self.link_up = l.up;
        }

        // Stall watch. Receive dying at a ring boundary with no error bit set
        // has now cost three rounds of theorising; this dumps what the chip
        // says about itself the moment it stops, once, and then never again.
        // Repair a drifted cursor *here*, not in the stall handler. The chip
        // resumes from the ring base whenever its receiver restarts, so
        // anything that restarts it without the driver's knowledge leaves the
        // two disagreeing — and `receive` only ever inspects the slot under
        // the cursor, so the disagreement is permanent rather than transient.
        // Reproduced as a host test in
        // `akuma-net-rtl8169/tests/bringup.rs::a_restarted_receiver_desyncs…`.
        //
        // Leaving this to `on_stall` was wrong twice over: it waits out a
        // five-second window, and it only runs if the stall arms at all — which
        // on 2026-09-20 it repeatedly did not, leaving the box deaf for a whole
        // boot with a completed frame one wrap away.
        //
        // Affordable because it reads **descriptors**, not registers: the
        // per-lap cost §7b forbids is MMIO, and there is none here. Still on a
        // cadence, and only once the ring has been quiet for a while, so a busy
        // link never pays for it — a desync cannot appear on a lap that
        // delivered a frame.
        if self.idle_laps >= RESYNC_AFTER_IDLE_LAPS
            && self.idle_laps.is_multiple_of(RESYNC_SCAN_LAPS)
            && let Some(i) = self.nic.resync_rx_cursor()
        {
            C.rx_resyncs.fetch_add(1, Ordering::Relaxed);
            if C.rx_resyncs.load(Ordering::Relaxed) <= MAX_RESYNC_REPORTS {
                crate::safe_print!(
                    112,
                    "[rtl] resync: cursor -> {} after {} idle laps\n",
                    i, self.idle_laps
                );
            }
        }

        let Some(n) = self.nic.receive(&mut self.rx_scratch) else {
            self.idle_laps = self.idle_laps.saturating_add(1);
            // Wall-clock first, lap count only as the pre-clock fallback. The
            // two must not both be able to fire, or a bring-up spin would kick
            // on laps while the clock says receive is healthy.
            let silent_us = match (now_us(), self.last_rx_us) {
                (Some(now), Some(last)) => Some(now.saturating_sub(last)),
                (Some(now), None) => {
                    // Clock up but nothing received yet: start the window here
                    // rather than at boot, so a late-arriving link does not
                    // count its own bring-up as silence.
                    self.last_rx_us = Some(now);
                    Some(0)
                }
                // No clock yet, so there is no silence to measure and
                // `decide` falls back to `idle_laps` — which is why the lap
                // count travels with it. That fallback is why [`STALL_LAPS`]
                // and [`STALL_LAPS_UNSTARTED`] are two orders of magnitude
                // apart: two million laps is quick only while the loop is
                // busy-spinning through bring-up (the two boots that recovered
                // on 2026-09-20 both did so during exactly that spin), and a
                // loop that has settled to one lap per tick would wait hours.
                (None, _) => None,
            };
            // Which arm fires, and in what order, is `akuma_net_rtl8169::stall`
            // — it has host tests, including one for each conflict, because a
            // test that sets one condition at a time passes under every
            // ordering. What stays here are the three rules that constrain
            // *this loop* rather than the decision:
            //
            // **No register reads on this path.** The inputs are `rx_seen`, the
            // `RDU` bit of the `ISR` the loop already harvests every lap, and a
            // clock — all free. `MPC` would be better evidence and reading it
            // here took the box down twice on 2026-09-19: `snapshot()` is
            // eleven MMIO reads and there is no "occasionally" on a per-lap
            // path. Gating it behind the quiet window looked like a fix and was
            // not — the window is open on *every* lap once it passes, so the
            // reads came back permanently five seconds after boot. See
            // `docs/archive/AMD64_TRASHCAN_ISSUES.md` §7b and
            // `AKUMA_NET_ISSUES.md` §11.7.
            //
            // **The recovery is never capped**, only its printing
            // ([`MAX_STALL_REPORTS`]). It is what keeps the machine on the
            // network, and a chip that needs restarting every minute still
            // needs restarting. It was capped at three for one boot on
            // 2026-09-20 and that was wrong: one boot recovered on the first
            // kick and the next did not recover at all, and a cap makes "the
            // recovery does not work" indistinguishable from "the recovery was
            // allowed three tries". On this box the difference is a person
            // walking to the machine.
            //
            // **Not gated on `link_up`.** It was, for one boot on 2026-09-20,
            // and that made the whole thing unreachable: carrier comes from a
            // periodic `PHYSTATUS` sample, and no boot on this box has ever
            // printed the `link up: re-armed receiver` line a `down -> up` edge
            // produces, so there is no evidence the sample reads `up` here at
            // all. Gating a receive recovery on the PHY means trusting the one
            // register whose report is in question. A link that really is down
            // loses nothing by having its ring cursor reset.
            let arm = stall::decide(
                &STALL_POLICY,
                &stall::StallInputs {
                    rx_seen: self.rx_seen,
                    backpressure: self.rx_backpressure,
                    silent_us,
                    idle_laps: self.idle_laps,
                    silent_fired: self.silent_kicks > 0,
                },
            );
            if arm != StallArm::None {
                match arm {
                    StallArm::Blind => self.blind_kicks += 1,
                    StallArm::Silent => self.silent_kicks += 1,
                    _ => {}
                }
                // Its own line, once: a `Silent` recovery and an `RDU` one are
                // different faults with the same repair, and a log that cannot
                // tell them apart is what made this arm take a whole session to
                // find. `on_stall` below prints the shared detail.
                if arm == StallArm::Silent && self.silent_kicks == 1 {
                    crate::safe_print!(
                        120,
                        "[rtl] SILENT: no frame for {} us with no RDU (rx={} frames) - receiver stopped\n",
                        STALL_SILENT_US,
                        C.rx_frames_received.load(Ordering::Relaxed)
                    );
                }
                self.stalls += 1;
                self.idle_laps = 0;
                self.last_rx_us = now_us();
                self.on_stall();

                // Escalation. `kick_receiver` restates `RDSAR` and resets the
                // driver's cursor, which is enough when the two merely drifted
                // apart — measured 2026-09-20, one boot came back on the first
                // one. The next boot did not come back at all, so there is a
                // state it does not reach, and the next bigger hammer is the
                // bring-up sequence itself: `init` resets the chip, repopulates
                // the rings and rewrites both ring bases.
                //
                // Only from the `blind` arm, and only once several kicks have
                // failed: this throws away anything in flight, which is the
                // wrong trade for a link that is passing traffic. It cannot be
                // the wrong trade here, because `!rx_seen` means nothing has
                // arrived since bring-up.
                //
                // It prints its own line so the two recoveries stay tellable
                // apart in a log — attribution between the `RDU` arm and this
                // one already cost a session's worth of uncertainty.
                //
                // `Silent` escalates on the same ladder and for the same
                // reason: it also means nothing is arriving, so there is
                // nothing in flight to throw away.
                let (escalate, which, n) = match arm {
                    StallArm::Blind => (
                        self.blind_kicks.is_multiple_of(BLIND_REINIT_EVERY),
                        "blind",
                        self.blind_kicks,
                    ),
                    StallArm::Silent => (
                        self.silent_kicks.is_multiple_of(BLIND_REINIT_EVERY),
                        "silent",
                        self.silent_kicks,
                    ),
                    _ => (false, "", 0),
                };
                if escalate {
                    let r = self.nic.init();
                    crate::safe_print!(
                        104,
                        "[rtl] {} #{}: full re-init -> {}\n",
                        which,
                        n,
                        if r.is_ok() { "ok" } else { "FAILED" }
                    );
                }
            }
            return None;
        };
        self.idle_laps = 0;
        self.last_rx_us = now_us();
        // A frame came off the ring, so whatever backpressure the chip reported
        // has been relieved; the next stall must produce its own evidence.
        self.rx_backpressure = false;
        // ...and the receiver has demonstrably started, which retires the
        // `blind` arm for the rest of the boot.
        self.rx_seen = true;
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
