//! Static RX/TX buffer rings for virtio-net (`net-noalloc`).
//!
//! Replaces the device's **one** 2 KB receive buffer and **one** 2 KB transmit
//! buffer with a fixed ring of each, held in BSS. That single change is what
//! makes an asynchronous transmit path possible, and asynchronous transmit is
//! the point.
//!
//! # What was wrong with one buffer
//!
//! `VirtIONetRaw::send` — the only thing a single TX buffer permits — is
//! `VirtQueue::add_notify_wait_pop`:
//!
//! ```text
//! add() -> notify() -> while !can_pop() { spin_loop() } -> pop_used()
//! ```
//!
//! It cannot return until the host has consumed the descriptor, because the
//! buffer it was handed must be free again by the time the caller gets control
//! back. And it runs from `TxToken::consume`, inside `iface.poll()`, inside the
//! `NETWORK` spinlock, inside a `PreemptGuard` that masks local IRQs. Measured
//! on the devbox: **20-26 us per packet, 194 us worst case**, during which no
//! core can enter the network stack and this core cannot take an interrupt.
//!
//! On receive the cost is smaller but the same shape: one buffer means one
//! `receive_begin` — an MMIO notify, so a vmexit — **per packet** (7-11 us
//! measured), and a burst can never be drained without a full notify/complete
//! round trip per frame.
//!
//! Full measurements: `docs/archive/AKUMA_NET_ISSUES.md` §3.2, §3.3.
//!
//! # What this does instead
//!
//! - **RX**: every free slot is posted up front, so the device always has
//!   somewhere to DMA. `take_frame` completes whatever the used ring offers and
//!   the slot is re-posted on the next `refill` — the notify amortises over the
//!   ring instead of falling on every packet.
//! - **TX**: `transmit_begin` submits and returns immediately; completions are
//!   reaped on a later pass ([`TxRing::reap`]), which is called before every
//!   claim. The spin is gone from the common path entirely.
//!
//! # Why the buffers are statics and not struct fields
//!
//! `NetworkState` is *built on the stack* and then moved into the `NETWORK`
//! static (`*NETWORK.lock() = Some(NetworkState { .. })`). Sixteen 2 KB frames
//! inline would push 32 KB through a kernel stack during `init`. Keeping them in
//! dedicated statics leaves `NetworkState` *smaller* than before, since the
//! rings hold only tokens and leases.
//!
//! They are [`FrameArena`]s rather than `static mut` arrays since 2026-08-30.
//! The rings used to hold slot indices and reach the bytes through `unsafe fn`
//! accessors whose `slot < RING` contract nothing enforced; they now hold
//! [`FrameLease`]es, so a slot cannot be addressed out of range and cannot be
//! handed to the device twice. What that buys, precisely: the window the lease
//! covers is the window the device owns the buffer — `receive_begin` to
//! `receive_complete`, `transmit_begin` to `transmit_complete` — which is
//! exactly the obligation `crate::nic` exists to discharge.

use crate::frames::{FrameArena, FrameLease};
use crate::nic::Nic;
use core::sync::atomic::{AtomicU64, Ordering};

/// Bytes per frame slot: the 1514-byte MTU plus the virtio net header, rounded
/// up. Also the minimum `VirtIONetRaw` will accept for a receive buffer
/// (`MIN_BUFFER_LEN` is 1526).
pub const FRAME_BUF: usize = 2048;

/// Receive slots posted to the device.
///
/// Half the 16-descriptor virtqueue, which leaves headroom for the driver's own
/// bookkeeping and is far more than the 1 this replaces. Deeper would buy
/// nothing here: the netpoll loop drains up to 64 frames per lap, so the ring
/// only has to cover one host-side burst, not a backlog.
///
/// **This path has no inbound networking under Firecracker.** Firecracker will
/// not read a frame from the tap until the driver has posted 65562 bytes of
/// receive capacity in total (see `smoltcp_net::RX_BUFFER_LEN`), and this ring
/// offers `RX_RING * FRAME_BUF` = 16 KB. Reaching the threshold from a
/// 16-descriptor queue needs `FRAME_BUF` of at least 4098, and `FRAME_BUF` is
/// shared with `TX_ARENA`, so it is a resize of both rings rather than a
/// constant bump — not done, because nothing enables `net-noalloc` (it measured
/// worse; see this crate's `[features]`).
pub const RX_RING: usize = 8;

/// Transmit slots that may be in flight simultaneously.
pub const TX_RING: usize = 8;

/// Reap attempts [`TxRing::claim`] makes before giving up and dropping the
/// frame.
///
/// This is the one place the old blocking behaviour survives, so it is
/// deliberately short: the whole point of the ring is that `NETWORK` is not
/// held for a host round trip. A frame that cannot get a slot within this many
/// polls is better dropped — smoltcp will retransmit — than held onto while
/// every other core is locked out of the network stack.
const CLAIM_SPINS: usize = 64;

/// Receive frame storage. See the module header for why this is not a field.
pub static RX_ARENA: FrameArena<RX_RING, FRAME_BUF> = FrameArena::new();
/// Transmit frame storage.
pub static TX_ARENA: FrameArena<TX_RING, FRAME_BUF> = FrameArena::new();
/// Write-only discard buffer for a frame that could not get a ring slot.
///
/// smoltcp's `TxToken::consume` contract says the fill closure runs and its
/// value is returned, so a dropped frame still has to be written *somewhere*.
/// Nothing ever reads this back and it is never submitted to the device.
pub static TX_DISCARD_ARENA: FrameArena<1, FRAME_BUF> = FrameArena::new();

/// The single slot of [`TX_DISCARD_ARENA`].
pub const TX_DISCARD_SLOT: usize = 0;

/// Frames the device completed with a token no slot claims.
///
/// Should be impossible: every posted buffer is ours. Counted rather than
/// panicked because a wedged network stack is worse than a dropped frame, and
/// because a non-zero value here is a precise signal that the token/slot map
/// has desynchronised from the used ring.
static ORPHAN_TOKENS: AtomicU64 = AtomicU64::new(0);

/// Times [`TxRing::claim`] found every slot in flight and had to spin waiting
/// for the device to return one. A steadily climbing value means `TX_RING` is
/// too shallow for the offered load — the async path is not keeping up and the
/// old per-packet spin is back for those frames.
static TX_STALLS: AtomicU64 = AtomicU64::new(0);

/// `(orphan_tokens, tx_stalls)` since boot. Both should stay at zero on a
/// healthy system; see the statics above for what a non-zero value means.
#[must_use]
pub fn ring_health() -> (u64, u64) {
    (
        ORPHAN_TOKENS.load(Ordering::Relaxed),
        TX_STALLS.load(Ordering::Relaxed),
    )
}

/// A receive slot the device owns: its token and the lease proving nothing else
/// touches the buffer while DMA is live.
type PostedRx = (u16, FrameLease<'static, RX_RING, FRAME_BUF>);

/// A transmit slot the device owns: token, lease, and the length that was
/// submitted (`transmit_complete` must be given the same slice).
type InflightTx = (u16, FrameLease<'static, TX_RING, FRAME_BUF>, u16);

// ============================================================================
// Receive
// ============================================================================

/// Which receive slots are posted to the device, and under what token.
pub struct RxRing {
    /// `Some((token, lease))` when slot `i` is posted and owned by the device;
    /// `None` when it is free to post.
    posted: [Option<PostedRx>; RX_RING],
    /// The lease for the frame most recently handed up to smoltcp as an
    /// `RxToken`. Released at the top of [`Self::refill`] — by then
    /// `RxToken::consume` has returned, which is the same reasoning the
    /// pre-lease code used to justify re-posting the slot there.
    handed_up: Option<FrameLease<'static, RX_RING, FRAME_BUF>>,
}

impl RxRing {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            posted: [const { None }; RX_RING],
            handed_up: None,
        }
    }

    /// Post every free slot to the device.
    ///
    /// Called at the top of `Device::receive`, which is also what re-posts the
    /// slot released by the previous call — by then smoltcp has finished with
    /// the frame, because `RxToken::consume` has returned.
    ///
    /// Stops at the first failure rather than trying every slot: `post_rx`
    /// only fails when the virtqueue is full (or the slot is still leased, which
    /// the release above has just ruled out), and the next slot would fail the
    /// same way.
    pub fn refill(&mut self, nic: &mut Nic) {
        // smoltcp has finished with the previous frame; free its slot before
        // trying to re-post it.
        self.handed_up = None;
        for (slot, entry) in self.posted.iter_mut().enumerate() {
            if entry.is_some() {
                continue;
            }
            let t = crate::nicstat::start();
            let Some(posted) = nic.post_rx(&RX_ARENA, slot) else {
                return;
            };
            crate::nicstat::record_rx_begin(t);
            *entry = Some(posted);
        }
    }

    /// Complete the next frame the device has finished, if any.
    ///
    /// Returns `(slot, offset, len)` — the frame is `RX_ARENA.slot_ptr(slot)`
    /// at `[offset .. offset + len]`, with `offset` skipping the virtio net
    /// header. The slot stays leased (as `handed_up`) until the next
    /// [`Self::refill`], covering the returned pointer's use as an `RxToken`.
    ///
    /// Completion follows the used ring's order (`peek_used` then `pop_used`,
    /// which rejects any other token), so the slot is looked up *by* the token
    /// rather than assumed.
    pub fn take_frame(&mut self, nic: &mut Nic) -> Option<(usize, usize, usize)> {
        let token = nic.poll_receive()?;
        let Some(slot) = self
            .posted
            .iter()
            .position(|p| matches!(p, Some((t, _)) if *t == token))
        else {
            // Not one of ours. Cannot `pop_used` without the matching buffer,
            // so leave it and record it — see `ORPHAN_TOKENS`.
            ORPHAN_TOKENS.fetch_add(1, Ordering::Relaxed);
            return None;
        };

        let t = crate::nicstat::start();
        let (_, lease) = self.posted[slot].take()?;
        // `complete_rx` bounds-checks `hdr_len + pkt_len` against the slot: a
        // malformed device response claiming a frame longer than the buffer
        // would otherwise have smoltcp parse memory past it. See the EL1
        // `EC=0x25` entry in `docs/runbooks/debug-network.md`.
        let (hdr_len, pkt_len) = nic.complete_rx(token, lease)?;
        crate::nicstat::record_rx_packet(t, pkt_len);
        // Re-take the slot for the RxToken's life. The device released it at
        // `complete_rx`; this hands it to smoltcp instead.
        self.handed_up = RX_ARENA.lease(slot);
        Some((slot, hdr_len, pkt_len))
    }
}

impl Default for RxRing {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Transmit
// ============================================================================

/// Transmit slots and the frames in flight in them.
pub struct TxRing {
    /// `Some((token, lease, len))` while slot `i` is owned by the device. The
    /// length is remembered because `transmit_complete` must be handed the
    /// *same* slice `transmit_begin` was given — `pop_used` walks the
    /// descriptor chain to unshare it.
    inflight: [Option<InflightTx>; TX_RING],
    /// When slot `i` was submitted, so `reap` can charge the host's consume
    /// latency to `tx_flight_us`. `transmit_begin` stopped making the caller
    /// wait for that; it did not make it go away, and it is the only way to see
    /// whether the device is picking async submissions up promptly.
    submitted: [Option<crate::nicstat::Started>; TX_RING],
    /// Round-robin hint, so a burst spreads across slots instead of hammering
    /// slot 0 and stalling on it.
    next: usize,
}

impl TxRing {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            inflight: [const { None }; TX_RING],
            submitted: [None; TX_RING],
            next: 0,
        }
    }

    /// Return every slot the device has finished with.
    ///
    /// This is the half that replaces the blocking spin: the wait still happens,
    /// but it happens *later*, on somebody else's pass through the poll loop,
    /// instead of inside the `NETWORK` critical section that produced the frame.
    pub fn reap(&mut self, nic: &mut Nic) -> usize {
        let mut freed = 0;
        while let Some(token) = nic.poll_transmit() {
            let Some(slot) = self
                .inflight
                .iter()
                .position(|e| matches!(e, Some((t, _, _)) if *t == token))
            else {
                ORPHAN_TOKENS.fetch_add(1, Ordering::Relaxed);
                break;
            };
            let Some((_, lease, len)) = self.inflight[slot].take() else {
                break;
            };
            if let Err(lease) = nic.complete_tx(token, lease, len as usize) {
                // Refused: the device may still own the buffer, so put the entry
                // back rather than freeing the slot. One slot is leaked; a slot
                // reused under live DMA would be corruption.
                self.inflight[slot] = Some((token, lease, len));
                break;
            }
            if let Some(s) = self.submitted[slot].take() {
                crate::nicstat::record_tx_complete(s);
            }
            freed += 1;
        }
        freed
    }

    /// Reap, then claim a free slot, returning its lease.
    ///
    /// `None` means every slot was still in flight after [`CLAIM_SPINS`] reap
    /// attempts. The caller drops the frame; TCP retransmits, and a UDP
    /// datagram lost to a saturated NIC is a drop the protocol already allows.
    ///
    /// It must not fall back to `Nic::send_blocking` — that is
    /// `add_notify_wait_pop`, which pops the used ring by *head* token and
    /// errors with `WrongToken` when anything else is in flight. Mixing it with
    /// `submit_tx` would fail the send *and* leak the descriptor chain,
    /// permanently shrinking the queue. The two paths cannot be interleaved, so
    /// the ring owns the send queue exclusively.
    pub fn claim(&mut self, nic: &mut Nic) -> Option<FrameLease<'static, TX_RING, FRAME_BUF>> {
        for attempt in 0..=CLAIM_SPINS {
            self.reap(nic);
            for i in 0..TX_RING {
                let slot = (self.next + i) % TX_RING;
                if self.inflight[slot].is_some() {
                    continue;
                }
                // A free `inflight` entry whose lease will not take is a slot
                // still held elsewhere; skip it rather than spinning on it.
                if let Some(lease) = TX_ARENA.lease(slot) {
                    self.next = (slot + 1) % TX_RING;
                    if attempt > 0 {
                        TX_STALLS.fetch_add(1, Ordering::Relaxed);
                    }
                    return Some(lease);
                }
            }
            core::hint::spin_loop();
        }
        TX_STALLS.fetch_add(1, Ordering::Relaxed);
        None
    }

    /// Submit `total_len` bytes (virtio header included) from a claimed slot.
    ///
    /// Returns whether the device accepted it. On refusal the lease is dropped,
    /// which frees the slot for the next claim — the device never took it.
    pub fn submit(
        &mut self,
        nic: &mut Nic,
        lease: FrameLease<'static, TX_RING, FRAME_BUF>,
        total_len: usize,
    ) -> bool {
        let slot = lease.slot();
        match nic.submit_tx(lease, total_len) {
            Ok((token, lease)) => {
                self.inflight[slot] = Some((token, lease, total_len as u16));
                self.submitted[slot] = Some(crate::nicstat::start());
                // Unconditional: `transmit_begin`'s own notify is suppressible
                // and nothing here spins to wait the suppression out. See
                // `smoltcp_net::nic_kick_tx`.
                crate::smoltcp_net::nic_kick_tx();
                true
            }
            Err(_) => false,
        }
    }
}

impl Default for TxRing {
    fn default() -> Self {
        Self::new()
    }
}
