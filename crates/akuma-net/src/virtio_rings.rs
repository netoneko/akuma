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
//! # Why the buffers are `static mut` and not struct fields
//!
//! `NetworkState` is *built on the stack* and then moved into the `NETWORK`
//! static (`*NETWORK.lock() = Some(NetworkState { .. })`). Sixteen 2 KB frames
//! inline would push 32 KB through a kernel stack during `init`. Keeping them in
//! dedicated statics — exactly what `SOCKET_STORAGE` already does in
//! `smoltcp_net.rs` — leaves `NetworkState` *smaller* than before, since the
//! rings hold only tokens and indices.
//!
//! There is one network device, initialised once, and every access below happens
//! under the `NETWORK` spinlock, which is what makes the aliasing sound.

use akuma_virtio::VirtioHal;
use core::sync::atomic::{AtomicU64, Ordering};
use virtio_drivers::device::net::VirtIONetRaw;
use akuma_virtio::VirtioTransport;

/// The virtio-net device type this crate binds. `16` is the virtqueue depth.
pub type NetDev = VirtIONetRaw<VirtioHal, VirtioTransport, 16>;

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
static mut RX_BUFS: [[u8; FRAME_BUF]; RX_RING] = [[0; FRAME_BUF]; RX_RING];
/// Transmit frame storage.
static mut TX_BUFS: [[u8; FRAME_BUF]; TX_RING] = [[0; FRAME_BUF]; TX_RING];
/// Write-only discard buffer for a frame that could not get a ring slot.
///
/// smoltcp's `TxToken::consume` contract says the fill closure runs and its
/// value is returned, so a dropped frame still has to be written *somewhere*.
/// Nothing ever reads this back and it is never submitted to the device.
static mut TX_DISCARD: [u8; FRAME_BUF] = [0; FRAME_BUF];

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

/// Pointer to receive slot `slot`.
///
/// # Safety
/// `slot < RX_RING`, and the caller holds `NETWORK` (the lock that serialises
/// every device access).
unsafe fn rx_buf(slot: usize) -> *mut u8 {
    // `&raw mut` rather than `&mut`: taking a reference to a `static mut` is an
    // error in edition 2024, and a raw pointer is all this needs.
    unsafe { (&raw mut RX_BUFS).cast::<u8>().add(slot * FRAME_BUF) }
}

/// Pointer to transmit slot `slot`.
///
/// # Safety
/// As [`rx_buf`], with `slot < TX_RING`.
unsafe fn tx_buf(slot: usize) -> *mut u8 {
    unsafe { (&raw mut TX_BUFS).cast::<u8>().add(slot * FRAME_BUF) }
}

/// A whole frame slot as a mutable slice.
///
/// # Safety
/// `slot < RX_RING` and the caller holds `NETWORK`.
#[must_use]
pub unsafe fn rx_frame(slot: usize) -> &'static mut [u8] {
    unsafe { core::slice::from_raw_parts_mut(rx_buf(slot), FRAME_BUF) }
}

/// The discard buffer for a frame with no slot, as a mutable slice.
///
/// # Safety
/// Caller holds `NETWORK`. The contents are never read back — see
/// [`TX_DISCARD`].
#[must_use]
pub unsafe fn tx_discard() -> &'static mut [u8] {
    unsafe { core::slice::from_raw_parts_mut((&raw mut TX_DISCARD).cast::<u8>(), FRAME_BUF) }
}

/// A whole transmit slot as a mutable slice.
///
/// # Safety
/// `slot < TX_RING` and the caller holds `NETWORK`.
#[must_use]
pub unsafe fn tx_frame(slot: usize) -> &'static mut [u8] {
    unsafe { core::slice::from_raw_parts_mut(tx_buf(slot), FRAME_BUF) }
}

// ============================================================================
// Receive
// ============================================================================

/// Which receive slots are posted to the device, and under what token.
pub struct RxRing {
    /// `Some(token)` when slot `i` is posted and owned by the device; `None`
    /// when it is free to post (or currently borrowed by an `RxToken`).
    tokens: [Option<u16>; RX_RING],
}

impl RxRing {
    #[must_use]
    pub const fn new() -> Self {
        Self { tokens: [None; RX_RING] }
    }

    /// Post every free slot to the device.
    ///
    /// Called at the top of `Device::receive`, which is also what re-posts the
    /// slot released by the previous call — by then smoltcp has finished with
    /// the frame, because `RxToken::consume` has returned.
    ///
    /// Stops at the first failure rather than trying every slot: `receive_begin`
    /// only fails when the virtqueue is full, and the next slot would fail the
    /// same way.
    pub fn refill(&mut self, dev: &mut NetDev) {
        for slot in 0..RX_RING {
            if self.tokens[slot].is_some() {
                continue;
            }
            let t = crate::nicstat::start();
            // SAFETY: `slot < RX_RING`; the caller holds `NETWORK`. The buffer
            // stays untouched by us until `receive_complete` hands it back,
            // which is the borrow `VirtIONetRaw::receive_begin` requires.
            let buf = unsafe { rx_frame(slot) };
            match unsafe { dev.receive_begin(buf) } {
                Ok(token) => {
                    crate::nicstat::record_rx_begin(t);
                    self.tokens[slot] = Some(token);
                }
                Err(_) => return,
            }
        }
    }

    /// Complete the next frame the device has finished, if any.
    ///
    /// Returns `(slot, offset, len)` — the frame is `rx_frame(slot)[offset..
    /// offset + len]`, with `offset` skipping the virtio net header.
    ///
    /// Completion follows the used ring's order (`peek_used` then `pop_used`,
    /// which rejects any other token), so the slot is looked up *by* the token
    /// rather than assumed.
    pub fn take_frame(&mut self, dev: &mut NetDev) -> Option<(usize, usize, usize)> {
        let token = dev.poll_receive()?;
        let Some(slot) = self.tokens.iter().position(|t| *t == Some(token)) else {
            // Not one of ours. Cannot `pop_used` without the matching buffer,
            // so leave it and record it — see `ORPHAN_TOKENS`.
            ORPHAN_TOKENS.fetch_add(1, Ordering::Relaxed);
            return None;
        };

        let t = crate::nicstat::start();
        // SAFETY: `slot` came from our own table, so it is in range, and this is
        // the same buffer that was passed to `receive_begin` for `token` —
        // which is what `receive_complete` requires.
        let buf = unsafe { rx_frame(slot) };
        let (hdr_len, pkt_len) = unsafe { dev.receive_complete(token, buf) }.ok()?;
        self.tokens[slot] = None;

        // A malformed device response could claim a frame longer than the
        // buffer; slicing on that would read (and let smoltcp parse) memory
        // past the slot. This check is why the pre-existing single-buffer path
        // has it too — see the EL1 `EC=0x25` entry in
        // `docs/runbooks/debug-network.md`.
        if hdr_len.saturating_add(pkt_len) > FRAME_BUF {
            return None;
        }
        crate::nicstat::record_rx_packet(t, pkt_len);
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
    /// `Some(token)` while slot `i` is owned by the device.
    inflight: [Option<u16>; TX_RING],
    /// Bytes submitted from slot `i`. `transmit_complete` must be handed the
    /// *same* slice `transmit_begin` was given — `pop_used` walks the
    /// descriptor chain to unshare it — so the length has to be remembered.
    lens: [u16; TX_RING],
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
            inflight: [None; TX_RING],
            lens: [0; TX_RING],
            submitted: [None; TX_RING],
            next: 0,
        }
    }

    /// Return every slot the device has finished with.
    ///
    /// This is the half that replaces the blocking spin: the wait still happens,
    /// but it happens *later*, on somebody else's pass through the poll loop,
    /// instead of inside the `NETWORK` critical section that produced the frame.
    pub fn reap(&mut self, dev: &mut NetDev) -> usize {
        let mut freed = 0;
        while let Some(token) = dev.poll_transmit() {
            let Some(slot) = self.inflight.iter().position(|t| *t == Some(token)) else {
                ORPHAN_TOKENS.fetch_add(1, Ordering::Relaxed);
                break;
            };
            // SAFETY: same slot, same length as the matching `transmit_begin`.
            let buf = unsafe {
                core::slice::from_raw_parts(tx_buf(slot), self.lens[slot] as usize)
            };
            if unsafe { dev.transmit_complete(token, buf) }.is_err() {
                break;
            }
            self.inflight[slot] = None;
            if let Some(s) = self.submitted[slot].take() {
                crate::nicstat::record_tx_complete(s);
            }
            freed += 1;
        }
        freed
    }

    /// Reap, then claim a free slot.
    ///
    /// `None` means every slot was still in flight after [`CLAIM_SPINS`] reap
    /// attempts. The caller drops the frame; TCP retransmits, and a UDP
    /// datagram lost to a saturated NIC is a drop the protocol already allows.
    ///
    /// It must not fall back to `VirtIONetRaw::send` — that is
    /// `add_notify_wait_pop`, which pops the used ring by *head* token and
    /// errors with `WrongToken` when anything else is in flight. Mixing it with
    /// `transmit_begin` would fail the send *and* leak the descriptor chain,
    /// permanently shrinking the queue. The two paths cannot be interleaved, so
    /// the ring owns the send queue exclusively.
    pub fn claim(&mut self, dev: &mut NetDev) -> Option<usize> {
        for attempt in 0..=CLAIM_SPINS {
            self.reap(dev);
            for i in 0..TX_RING {
                let slot = (self.next + i) % TX_RING;
                if self.inflight[slot].is_none() {
                    self.next = (slot + 1) % TX_RING;
                    if attempt > 0 {
                        TX_STALLS.fetch_add(1, Ordering::Relaxed);
                    }
                    return Some(slot);
                }
            }
            core::hint::spin_loop();
        }
        TX_STALLS.fetch_add(1, Ordering::Relaxed);
        None
    }

    /// Submit `total_len` bytes (virtio header included) from `slot`.
    ///
    /// Returns whether the device accepted it. On refusal the slot is left free,
    /// so the next claim can reuse it.
    pub fn submit(&mut self, dev: &mut NetDev, slot: usize, total_len: usize) -> bool {
        // SAFETY: `slot < TX_RING`, caller holds `NETWORK`, and the buffer stays
        // untouched until `reap` completes this token — the borrow
        // `transmit_begin` requires.
        let buf = unsafe { core::slice::from_raw_parts(tx_buf(slot), total_len) };
        match unsafe { dev.transmit_begin(buf) } {
            Ok(token) => {
                self.inflight[slot] = Some(token);
                self.lens[slot] = total_len as u16;
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
