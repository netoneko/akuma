//! `LoopbackAwareDevice` — the ring that keeps 127.x.x.x off the wire.

use core::sync::atomic::{AtomicUsize, Ordering};
use smoltcp::phy::{Device, DeviceCapabilities};
use smoltcp::time::Instant;
use akuma_primitives::net_runtime::runtime;
use crate::counters::C;
use crate::device::VirtioSmoltcpDevice;
use crate::nicstat;
use crate::frames::{FrameArena, FrameLease};

/// The external (wire) side of a [`LoopbackAwareDevice`].
///
/// `LoopbackAwareDevice` used to hard-wire a [`VirtioSmoltcpDevice`]. It no
/// longer does: the amd64 bare-metal target has no virtio-net at all — its NIC
/// is a Realtek on PCI — and a kernel with no NIC still wants a socket layer
/// (loopback, and the `ifconfig`/`SIOCGIF*` surface). This enum is the seam.
///
/// It is an enum rather than a `dyn Device` because `NetworkState` holds it by
/// value in a `static` and a trait object there would cost a `Box` and a
/// vtable hop per frame; and rather than a generic because that would ripple a
/// type parameter through `NETWORK`, `poll()` and every socket call.
// The `Virtio` variant carries a ~2 KB inline staging buffer that
// `LoopbackAwareDevice` held by value before this enum existed; there is
// exactly one `ExternalDevice` in the system, built once at boot into a
// `static`, so boxing it would add a boot-path allocation and a pointer chase
// per frame for nothing.
#[allow(clippy::large_enum_variant)]
pub enum ExternalDevice {
    /// virtio-net (every VMM target).
    Virtio(VirtioSmoltcpDevice),
    /// The Realtek RTL8169/8168 (`amd64` bare metal).
    #[cfg(feature = "rtl8169")]
    Rtl8169(crate::rtl8169::Rtl8169Device),
    /// No wire — loopback only.
    Absent,
    /// A scripted wire for host tests: frames queued here are what
    /// `take_rx_frame` hands up, in order.
    #[cfg(test)]
    Scripted(tests::Wire),
}

/// One-slot staging buffer for [`ExternalDevice::Absent`]: `TxToken::consume`
/// must be handed a `&mut [u8]` of the length smoltcp asked for even when there
/// is nowhere to send it, and a loopback frame is still diverted out of it.
static ABSENT_TX_SCRATCH: FrameArena<1, LOOPBACK_FRAME_BUF> = FrameArena::new();

impl ExternalDevice {
    /// Probe and bring up the Realtek RTL8169/8168 at register BAR `bar`.
    ///
    /// # Errors
    /// A short static string naming why the chip did not come up.
    ///
    /// # Safety
    /// `bar` must be the NIC's device-mapped register BAR, valid for the life
    /// of the returned device; called once.
    #[cfg(feature = "rtl8169")]
    pub unsafe fn probe_rtl8169(bar: *mut u8) -> Result<Self, &'static str> {
        // SAFETY: forwarded to the caller.
        unsafe { crate::rtl8169::Rtl8169Device::probe(bar) }
            .map(Self::Rtl8169)
            .map_err(|e| match e {
                akuma_net_rtl8169::Error::ResetTimeout => "reset timed out (chip wedged or absent)",
                akuma_net_rtl8169::Error::RingMisaligned { .. } => "descriptor ring misaligned",
                akuma_net_rtl8169::Error::RingLength { .. } => "descriptor ring length not a power of two",
                akuma_net_rtl8169::Error::UnknownChip(_) => "unrecognised RTL816x family member",
                akuma_net_rtl8169::Error::ImplausibleMac(_) => "implausible MAC — chip not responding",
            })
    }

    /// The station MAC, or all-zero when there is no wire.
    #[must_use]
    pub fn mac_address(&self) -> [u8; 6] {
        match self {
            Self::Virtio(d) => d.mac_address(),
            #[cfg(feature = "rtl8169")]
            Self::Rtl8169(d) => d.mac_address(),
            Self::Absent => [0; 6],
            #[cfg(test)]
            Self::Scripted(_) => [0x02, 0, 0, 0, 0, 1],
        }
    }

    pub(crate) fn capabilities(&self) -> DeviceCapabilities {
        match self {
            Self::Virtio(d) => d.capabilities(),
            #[cfg(feature = "rtl8169")]
            Self::Rtl8169(d) => d.capabilities(),
            Self::Absent => {
                let mut caps = DeviceCapabilities::default();
                caps.max_transmission_unit = 1514;
                caps
            }
            #[cfg(test)]
            Self::Scripted(_) => DeviceCapabilities::default(),
        }
    }

    pub(crate) fn take_rx_frame(&mut self) -> Option<(*mut u8, usize)> {
        match self {
            Self::Virtio(d) => d.take_rx_frame(),
            #[cfg(feature = "rtl8169")]
            Self::Rtl8169(d) => d.take_rx_frame(),
            Self::Absent => None,
            #[cfg(test)]
            Self::Scripted(w) => w.take(),
        }
    }

    pub(crate) fn emit_frame<R>(
        &mut self,
        len: usize,
        fill: impl FnOnce(&mut [u8]) -> R,
        divert: impl FnOnce(&[u8]) -> bool,
    ) -> R {
        match self {
            Self::Virtio(d) => d.emit_frame(len, fill, divert),
            #[cfg(feature = "rtl8169")]
            Self::Rtl8169(d) => d.emit_frame(len, fill, divert),
            Self::Absent => {
                let end = len.min(LOOPBACK_FRAME_BUF);
                // SAFETY: single slot, `NETWORK` held so this function is not
                // re-entered, bounds are the arena's; the buffer is write-only
                // staging that nothing reads back.
                let scratch = unsafe { &mut *ABSENT_TX_SCRATCH.first_slot_ptr() };
                let res = fill(&mut scratch[..end]);
                if divert(&scratch[..end]) {
                    return res;
                }
                // Nowhere to send a non-loopback frame; smoltcp will retry.
                C.tx_drop_count.fetch_add(1, Ordering::Relaxed);
                res
            }
            #[cfg(test)]
            Self::Scripted(_) => fill(&mut alloc::vec![0u8; len]),
        }
    }
}

// Loopback-Aware Device Wrapper
// ============================================================================

/// Check if an Ethernet frame carries a loopback (127.x.x.x) address.
///
/// Inspects the `EtherType` and the relevant IP address fields:
/// - ARP (0x0806): sender protocol address at byte 28, target at byte 38
/// - IPv4 (0x0800): source IP at byte 26, destination at byte 30
///
/// Used in both directions: on TX a match is diverted into the internal ring,
/// and on RX a match *from the wire* is a martian and is dropped.
fn is_loopback_frame(frame: &[u8]) -> bool {
    if frame.len() < 14 {
        return false;
    }
    let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
    match ethertype {
        // ARP: match if either sender (bytes 28) or target (bytes 38) IP is 127.x.x.x
        0x0806 => frame.len() >= 42 && (frame[28] == 127 || frame[38] == 127),
        // IPv4: match if either source (byte 26) or dest (byte 30) IP is 127.x.x.x
        0x0800 => frame.len() >= 34 && (frame[26] == 127 || frame[30] == 127),
        _ => false,
    }
}

/// A lease on one [`LOOPBACK_ARENA`] slot. Named because it appears in three
/// signatures and the const-generic form is unreadable inline.
type LoopbackLease = FrameLease<'static, LOOPBACK_RING, LOOPBACK_FRAME_BUF>;

/// Bytes per loopback frame slot. Loopback frames are pure L2 (no virtio
/// header, MTU 1514 — see `capabilities()`), but this matches
/// `virtio_rings::FRAME_BUF` and every other frame buffer in this file for
/// the same reason: one size means one set of bounds to reason about.
const LOOPBACK_FRAME_BUF: usize = 2048;

/// Loopback frames that may be queued at once. Deliberately the same order of
/// magnitude as `virtio_rings::RX_RING`/`TX_RING`: enough to cover one
/// TCP-handshake-shaped burst between two `poll()` calls, not a backlog.
const LOOPBACK_RING: usize = 32;

/// Frame storage for the loopback ring. Not a `LoopbackAwareDevice` field —
/// `NetworkState` (which owns the device) is built on the stack before being
/// moved into the `NETWORK` static, and `LOOPBACK_RING * LOOPBACK_FRAME_BUF`
/// (64 KiB) inline would push that far past a comfortable kernel stack frame.
/// Same reasoning as `virtio_rings`' arenas.
static LOOPBACK_ARENA: FrameArena<LOOPBACK_RING, LOOPBACK_FRAME_BUF> = FrameArena::new();

/// Loopback frames dropped because the ring was full, or (should be
/// impossible — `capabilities()` caps the MTU well under this) too large for
/// a slot. `docs/archive/FREEZE_INSTRUMENTATION_PLAN.md` F5 flagged the old
/// `VecDeque` for growing without bound instead of ever hitting this path.
static LOOPBACK_DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

#[must_use]
pub fn loopback_drop_count() -> usize {
    LOOPBACK_DROP_COUNT.load(Ordering::Relaxed)
}

/// Frames from the wire dropped as martians: a 127/8 source or destination
/// (IPv4), or a 127/8 sender or target (ARP), on the external device.
///
/// Nothing legitimate produces one — `127/8` never leaves a host — so a
/// non-zero count is a misconfigured peer or someone probing. Always on,
/// unlike `nicstat`, because it is the evidence the filter is doing its job.
static MARTIAN_DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

#[must_use]
pub fn martian_drop_count() -> usize {
    MARTIAN_DROP_COUNT.load(Ordering::Relaxed)
}

/// Martians [`LoopbackAwareDevice::receive`] discards in one call before
/// reporting "nothing to receive" instead. Each discard consumes a real frame
/// from the device, so the loop already ends when the wire goes quiet; this
/// only stops a sustained stream of them from holding `NETWORK` for a whole
/// ring's worth of drops per call. The frames behind it are taken next lap.
const MARTIAN_DROPS_PER_RECEIVE: usize = 16;

/// A fixed-capacity ring of loopback frames, replacing what used to be a
/// `VecDeque<Vec<u8>>`.
///
/// The old queue paid a zeroing heap allocation and a copy for every loopback
/// frame (`docs/archive/AKUMA_NET_ISSUES.md` §"one per-packet allocation
/// remains", `docs/archive/BENCHMARK_PERFORMANCE_ATTEMPT_0.md` §6) and had no
/// capacity bound, which `docs/archive/SCHEDULING_INVESTIGATION.md` flagged as
/// an unbounded-queue-without-backpressure smell. This ring bounds depth at
/// [`LOOPBACK_RING`] and drops (counted) on overflow instead of growing —
/// the same backpressure a real NIC's finite ring already gives external
/// traffic.
///
/// # Why holding a borrow into a shared static is sound
///
/// Every push and pop happens under the `NETWORK` spinlock (`push` from
/// `TxToken::consume` during egress, `pop` from `Device::receive` during
/// ingress), so there is exactly one thread touching the ring at a time. A
/// slot popped by `pop` is only reused by a `push` after `LOOPBACK_RING`
/// further pushes advance `tail` all the way back around to it — and by then
/// the `RxToken::consume` call that borrowed it has long since returned,
/// because `receive()`/`consume()` are synchronous and non-reentrant on this
/// slot (the one case where a `push` runs "inside" an outstanding `pop` —
/// smoltcp generating an immediate reply, e.g. an ICMP echo, from within the
/// rx closure it was handed alongside the tx token — targets `tail`, a
/// different slot from the `head` slot still being read, as long as
/// `LOOPBACK_RING >= 2`).
///
/// Since 2026-08-30 that argument is *checked* rather than asserted: slots come
/// from [`LOOPBACK_ARENA`], `pop` hands out a [`FrameLease`] that holds the slot
/// until the `RxToken` is consumed, and a `push` that lands on a slot still
/// leased is refused and counted as a drop instead of overwriting a frame
/// smoltcp is reading.
pub(crate) struct LoopbackRing {
    /// Length of the frame in slot `i`, valid only while that slot is queued.
    lens: [u16; LOOPBACK_RING],
    /// Next slot to pop.
    head: usize,
    /// Next slot to push into.
    tail: usize,
    /// Frames currently queued.
    count: usize,
}

impl LoopbackRing {
    const fn new() -> Self {
        Self {
            lens: [0; LOOPBACK_RING],
            head: 0,
            tail: 0,
            count: 0,
        }
    }

    /// Copy `frame` into the next free slot. Drops and counts it
    /// (`LOOPBACK_DROP_COUNT`) if the ring is full or the frame does not fit
    /// a slot — the latter should be unreachable given the MTU, but a
    /// malformed frame must not overrun `LOOPBACK_BUFS`.
    fn push(&mut self, frame: &[u8]) {
        if self.count == LOOPBACK_RING || frame.len() > LOOPBACK_FRAME_BUF {
            LOOPBACK_DROP_COUNT.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let slot = self.tail;
        // A `None` here means the slot is still leased by an `RxToken` smoltcp
        // has not finished with — the case the struct-level argument says
        // cannot happen. Drop and count it rather than overwriting a frame
        // that is being read.
        let copied = LOOPBACK_ARENA.with_slot(slot, |dst| {
            dst[..frame.len()].copy_from_slice(frame);
        });
        if copied.is_none() {
            LOOPBACK_DROP_COUNT.fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.lens[slot] = frame.len() as u16;
        self.tail = (self.tail + 1) % LOOPBACK_RING;
        self.count += 1;
        // A loopback frame never touches virtio, so unlike a real packet it
        // has no interrupt of its own to end a parked core's `wfi`/
        // `blocking_relax` halt — without this it rides the periodic timer
        // tick, the exact cost `AKUMA_NET_ISSUES.md` §3.1 removed for
        // external traffic. See `NetRuntime::wake_netpoll` and
        // `docs/archive/LOOPBACK_RING_CONVERSION.md`.
        (runtime().wake_netpoll)();
    }

    /// Hand back the oldest queued frame, if any, as a lease on its
    /// [`LOOPBACK_ARENA`] slot plus the frame's length.
    ///
    /// The lease is what keeps the slot out of `push`'s reach until the
    /// `RxToken` carrying it has been consumed.
    fn pop(&mut self) -> Option<(LoopbackLease, usize)> {
        if self.count == 0 {
            return None;
        }
        let slot = self.head;
        let len = self.lens[slot] as usize;
        let lease = LOOPBACK_ARENA.lease(slot)?;
        self.head = (self.head + 1) % LOOPBACK_RING;
        self.count -= 1;
        Some((lease, len))
    }
}

/// A composite device that wraps `VirtIO` for external traffic and an internal
/// ring for loopback (127.x.x.x) traffic.
///
/// Outgoing frames destined for
/// loopback addresses are intercepted in `TxToken::consume()` and queued
/// internally rather than being sent through `VirtIO`. `receive()` checks
/// the loopback ring first, then falls back to `VirtIO`.
pub struct LoopbackAwareDevice {
    external: ExternalDevice,
    loopback: LoopbackRing,
}

impl LoopbackAwareDevice {
    #[must_use]
    pub const fn new(external: ExternalDevice) -> Self {
        Self {
            external,
            loopback: LoopbackRing::new(),
        }
    }

    #[must_use]
    pub fn mac_address(&self) -> [u8; 6] {
        self.external.mac_address()
    }
}

impl Device for LoopbackAwareDevice {
    type RxToken<'a> = LoopbackAwareRxToken<'a>;
    type TxToken<'a> = LoopbackAwareTxToken<'a>;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        // Choose the frame BEFORE building the tx token. Doing it in this order
        // is what lets the two tokens be plain disjoint field borrows
        // (`&mut self.virtio` and `&mut self.loopback`) instead of the
        // `&raw mut` aliasing this used to need: the loopback pop wants the
        // ring mutably, and the tx token wants to keep it.
        let source = if let Some((frame, len)) = self.loopback.pop() {
            // An internally queued frame is already in hand — no device round
            // trip, and `receive` drains these ahead of the wire.
            FrameSource::Loopback(frame, len)
        } else {
            let mut dropped = 0;
            loop {
                let (ptr, len) = self.external.take_rx_frame()?;
                // Drop martians. The interface owns `127.0.0.1/8` alongside the
                // NIC's address, so smoltcp accepts a frame addressed to it
                // whichever device it arrived on; Linux refuses one on a
                // non-loopback device (`ip_route_input`, absent
                // `route_localnet`), and until 2026-09-25 this handed it up
                // unchecked — so `127.0.0.1` as a peer or bound address proved
                // nothing about where a connection came from
                // (`docs/archive/AKUMA_NET_LOOPBACK_BIND_EXPOSED.md`).
                //
                // SAFETY: the same frame `FrameSource::External` below hands
                // up, under the same contract; the borrow ends at the end of
                // this statement, before the next `take_rx_frame` releases the
                // slot.
                let martian = is_loopback_frame(unsafe { core::slice::from_raw_parts(ptr, len) });
                if !martian {
                    break FrameSource::External(ptr, len);
                }
                MARTIAN_DROP_COUNT.fetch_add(1, Ordering::Relaxed);
                dropped += 1;
                if dropped == MARTIAN_DROPS_PER_RECEIVE {
                    return None;
                }
            }
        };

        let tx = LoopbackAwareTxToken {
            external: &mut self.external,
            loopback: &mut self.loopback,
        };
        let rx = match source {
            FrameSource::Loopback(frame, len) => LoopbackAwareRxToken::Loopback(frame, len),
            // SAFETY: `take_rx_frame` returned a live L2 frame of `len` bytes in
            // storage this device owns — with `net-noalloc` a ring slot the ring
            // has already released and will not re-post until the next
            // `receive`, otherwise the single rx buffer. The token's lifetime is
            // bounded by the `&mut self` borrow.
            FrameSource::External(ptr, len) => LoopbackAwareRxToken::External(unsafe {
                core::slice::from_raw_parts_mut(ptr, len)
            }),
        };
        Some((rx, tx))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(LoopbackAwareTxToken {
            external: &mut self.external,
            loopback: &mut self.loopback,
        })
    }

    fn capabilities(&self) -> smoltcp::phy::DeviceCapabilities {
        self.external.capabilities()
    }
}

/// Where the frame `receive` is about to hand up came from.
///
/// Exists so the decision can be made before either token is built — see
/// `LoopbackAwareDevice::receive`.
enum FrameSource {
    /// A frame popped off the internal loopback ring, borrowed `'static` out
    /// of `LOOPBACK_BUFS`.
    Loopback(LoopbackLease, usize),
    /// A pointer to the L2 frame in device-owned storage, and its length.
    External(*mut u8, usize),
}

pub enum LoopbackAwareRxToken<'a> {
    /// A frame that was looped back internally: its arena lease and length.
    Loopback(LoopbackLease, usize),
    /// A borrowed frame received from the external device.
    External(&'a mut [u8]),
}

impl smoltcp::phy::RxToken for LoopbackAwareRxToken<'_> {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        match self {
            Self::Loopback(lease, len) => f(&lease[..len]),
            Self::External(buf) => f(buf),
        }
    }
}

pub struct LoopbackAwareTxToken<'a> {
    external: &'a mut ExternalDevice,
    loopback: &'a mut LoopbackRing,
}

impl smoltcp::phy::TxToken for LoopbackAwareTxToken<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let ring = self.loopback;
        self.external.emit_frame(len, f, |frame| {
            // Frames addressed to 127.x never reach the wire: copy them into the
            // internal ring, which `receive` drains ahead of the device.
            if !is_loopback_frame(frame) {
                return false;
            }
            ring.push(frame);
            nicstat::record_loopback(frame.len());
            true
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{martian_drop_count, ExternalDevice, LoopbackAwareDevice, MARTIAN_DROPS_PER_RECEIVE};
    use smoltcp::phy::{Device, RxToken};
    use smoltcp::time::Instant;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// A wire that delivers the frames queued on it, in order. `held` is the
    /// frame last handed up, kept alive until the next `take` the way a real
    /// NIC's ring slot is.
    pub struct Wire {
        queue: VecDeque<Vec<u8>>,
        held: Vec<u8>,
    }

    impl Wire {
        pub(crate) fn take(&mut self) -> Option<(*mut u8, usize)> {
            self.held = self.queue.pop_front()?;
            Some((self.held.as_mut_ptr(), self.held.len()))
        }
    }

    /// `MARTIAN_DROP_COUNT` is a process-wide static and `cargo test` runs
    /// tests on threads, so every test that reads a delta holds this.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn device(frames: impl IntoIterator<Item = Vec<u8>>) -> LoopbackAwareDevice {
        LoopbackAwareDevice::new(ExternalDevice::Scripted(Wire {
            queue: frames.into_iter().collect(),
            held: Vec::new(),
        }))
    }

    /// A minimal Ethernet + IPv4 header, enough for `is_loopback_frame`.
    fn ipv4(src: [u8; 4], dst: [u8; 4]) -> Vec<u8> {
        let mut f = vec![0u8; 34];
        f[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
        f[26..30].copy_from_slice(&src);
        f[30..34].copy_from_slice(&dst);
        f
    }

    fn arp(sender: [u8; 4], target: [u8; 4]) -> Vec<u8> {
        let mut f = vec![0u8; 42];
        f[12..14].copy_from_slice(&0x0806u16.to_be_bytes());
        f[28..32].copy_from_slice(&sender);
        f[38..42].copy_from_slice(&target);
        f
    }

    /// What `receive` hands up next, as bytes.
    fn next(dev: &mut LoopbackAwareDevice) -> Option<Vec<u8>> {
        let (rx, _tx) = dev.receive(Instant::ZERO)?;
        Some(rx.consume(<[u8]>::to_vec))
    }

    const LAN: [u8; 4] = [192, 168, 1, 50];
    const NIC: [u8; 4] = [192, 168, 1, 120];

    /// The defect's second layer: a frame from the wire addressed to (or
    /// claiming to come from) 127/8 was handed up unchecked, and smoltcp
    /// accepted it because the interface owns `127.0.0.1/8`.
    #[test]
    fn a_loopback_frame_from_the_wire_is_dropped_and_counted() {
        let _g = SERIAL.lock().unwrap();
        let before = martian_drop_count();
        let good = ipv4(LAN, NIC);
        let mut dev = device([
            ipv4(LAN, [127, 0, 0, 1]),
            ipv4([127, 0, 0, 1], NIC),
            arp(LAN, [127, 0, 0, 1]),
            arp([127, 0, 0, 1], NIC),
            good.clone(),
        ]);
        assert_eq!(next(&mut dev), Some(good), "the martians are skipped, not the frame behind them");
        assert_eq!(martian_drop_count() - before, 4);
        assert_eq!(next(&mut dev), None);
    }

    #[test]
    fn ordinary_traffic_is_untouched() {
        let _g = SERIAL.lock().unwrap();
        let before = martian_drop_count();
        let frames = [ipv4(LAN, NIC), arp(LAN, NIC), ipv4(LAN, [255, 255, 255, 255])];
        let mut dev = device(frames.clone());
        for f in frames {
            assert_eq!(next(&mut dev), Some(f));
        }
        assert_eq!(martian_drop_count(), before);
    }

    /// A sustained stream of martians gives up the call rather than draining
    /// the whole ring under `NETWORK`; the frame behind them arrives next call.
    #[test]
    fn a_burst_of_martians_is_bounded_per_call() {
        let _g = SERIAL.lock().unwrap();
        let good = ipv4(LAN, NIC);
        let mut frames: Vec<_> =
            (0..MARTIAN_DROPS_PER_RECEIVE).map(|_| ipv4(LAN, [127, 0, 0, 1])).collect();
        frames.push(good.clone());
        let mut dev = device(frames);
        assert_eq!(next(&mut dev), None);
        assert_eq!(next(&mut dev), Some(good));
    }
}
