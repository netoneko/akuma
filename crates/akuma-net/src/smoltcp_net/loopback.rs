//! `LoopbackAwareDevice` — the ring that keeps 127.x.x.x off the wire.

use super::*;

// Loopback-Aware Device Wrapper
// ============================================================================

/// Check if an Ethernet frame is destined for loopback (127.x.x.x).
///
/// Inspects the `EtherType` and the relevant IP address field:
/// - ARP (0x0806): target protocol address at bytes [38:42]
/// - IPv4 (0x0800): destination IP at bytes [30:34]
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
    virtio: VirtioSmoltcpDevice,
    loopback: LoopbackRing,
}

impl LoopbackAwareDevice {
    #[must_use]
    pub const fn new(virtio: VirtioSmoltcpDevice) -> Self {
        Self {
            virtio,
            loopback: LoopbackRing::new(),
        }
    }

    #[must_use] 
    pub fn mac_address(&self) -> [u8; 6] {
        self.virtio.mac_address()
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
            let (ptr, len) = self.virtio.take_rx_frame()?;
            FrameSource::Virtio(ptr, len)
        };

        let tx = LoopbackAwareTxToken {
            virtio: &mut self.virtio,
            loopback: &mut self.loopback,
        };
        let rx = match source {
            FrameSource::Loopback(frame, len) => LoopbackAwareRxToken::Loopback(frame, len),
            // SAFETY: `take_rx_frame` returned a live L2 frame of `len` bytes in
            // storage this device owns — with `net-noalloc` a ring slot the ring
            // has already released and will not re-post until the next
            // `receive`, otherwise the single rx buffer. The token's lifetime is
            // bounded by the `&mut self` borrow.
            FrameSource::Virtio(ptr, len) => LoopbackAwareRxToken::Virtio(unsafe {
                core::slice::from_raw_parts_mut(ptr, len)
            }),
        };
        Some((rx, tx))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(LoopbackAwareTxToken {
            virtio: &mut self.virtio,
            loopback: &mut self.loopback,
        })
    }

    fn capabilities(&self) -> smoltcp::phy::DeviceCapabilities {
        self.virtio.capabilities()
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
    Virtio(*mut u8, usize),
}

pub enum LoopbackAwareRxToken<'a> {
    /// A frame that was looped back internally: its arena lease and length.
    Loopback(LoopbackLease, usize),
    /// A borrowed frame received from `VirtIO`.
    Virtio(&'a mut [u8]),
}

impl smoltcp::phy::RxToken for LoopbackAwareRxToken<'_> {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        match self {
            Self::Loopback(lease, len) => f(&lease[..len]),
            Self::Virtio(buf) => f(buf),
        }
    }
}

pub struct LoopbackAwareTxToken<'a> {
    virtio: &'a mut VirtioSmoltcpDevice,
    loopback: &'a mut LoopbackRing,
}

impl smoltcp::phy::TxToken for LoopbackAwareTxToken<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let ring = self.loopback;
        self.virtio.emit_frame(len, f, |frame| {
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
