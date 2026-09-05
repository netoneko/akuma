//! `VirtioSmoltcpDevice` — smoltcp's `Device` over virtio-net.

use core::sync::atomic::Ordering;
use smoltcp::phy::Device;
use smoltcp::time::Instant;
// The single-buffer receive path and its counters exist only without
// `net-noalloc`; with it, `virtio_rings` owns the buffers and the accounting.
#[cfg(not(feature = "net-noalloc"))]
use crate::counters::C;
#[cfg(not(feature = "net-noalloc"))]
use crate::frames::{FrameArena, FrameLease};

use crate::nic::Nic;
use crate::nicstat;

// VirtIO Smoltcp Device Wrapper
// ============================================================================

/// Bytes of receive buffer posted to the device at a time.
///
/// 2 KB holds an Ethernet frame and is all QEMU ever asks for. Firecracker is
/// different, and silently so: its virtio-net device will not read a single
/// frame from the host tap until the *total* capacity of the receive
/// descriptors the driver has posted reaches `MAX_BUFFER_SIZE` = 65562 bytes
/// (`src/vmm/src/devices/virtio/net/device.rs`, `read_from_mmds_or_tap`). One
/// 2 KB buffer is 2048 of the 65562, so the gate never opens: every inbound
/// frame is dropped and counted in the `no_rx_avail_buffer` metric, with no
/// error anywhere the guest can see. It presents as a NIC that transmits
/// perfectly and receives absolutely nothing.
///
/// So the default is one buffer past that threshold — 65562 rounded up to a
/// multiple of 8 — on every platform rather than under a Firecracker `cfg`.
/// QEMU does not care how large the buffer is, and one size means the receive
/// path that gets exercised daily is the one Firecracker needs.
/// `VIRTIO_NET_F_MRG_RXBUF` would let the device chain several small buffers to
/// the same total, but `virtio-drivers` does not offer that feature, so the
/// capacity has to come from a single descriptor.
///
/// `extreme-size` keeps 2 KB: it boots in 4 MB of RAM, where 64 KB of BSS is
/// 1.6% of the machine, and it is a QEMU target — its acceptance test is the
/// 4 MB floor (`acceptance/05`). Running *that* profile under Firecracker would
/// have no inbound networking.
///
/// See `docs/archive/AKUMA_FIRECRACKER_KVM.md` §5.1.
pub const RX_BUFFER_LEN: usize = if cfg!(kernel_profile_extreme) {
    2048
} else {
    65_568
};

/// The single receive buffer of the pre-`net-noalloc` path.
///
/// In BSS rather than a [`VirtioSmoltcpDevice`] field, for the same reason
/// `virtio_rings` keeps its frame storage there: `NetworkState` is built on the
/// kernel stack by [`init`] and then moved into `NETWORK`, so a field this size
/// would be a 64 KB stack temporary on a 96 KB system stack.
///
/// A one-slot [`FrameArena`] rather than a `static mut` since 2026-08-30. What
/// used to be an aliasing obligation on every caller ("hold `NETWORK`, take no
/// second borrow") is now the arena's borrow flag, and the window it has to
/// cover — `receive_begin` until `receive_complete`, when the device owns the
/// buffer by DMA — is exactly the life of the lease
/// `VirtioSmoltcpDevice::rx_posted` holds. See `frames.rs`.
#[cfg(not(feature = "net-noalloc"))]
pub(crate) static RX_ARENA: FrameArena<1, RX_BUFFER_LEN> = FrameArena::new();

/// The arena slot the single-buffer path uses. There is only one.
#[cfg(not(feature = "net-noalloc"))]
const RX_SLOT: usize = 0;

pub struct VirtioSmoltcpDevice {
    inner: Nic,
    /// The single transmit buffer of the pre-`net-noalloc` path. Also the
    /// saturation-fallback staging buffer's counterpart.
    #[cfg(not(feature = "net-noalloc"))]
    tx_buffer: [u8; 2048],
    /// The receive buffer currently posted to the device: its token, and the
    /// arena lease that keeps the slot exclusively ours for as long as the
    /// device owns it by DMA.
    ///
    /// `VirtIO` requires buffers to be posted via `receive_begin()` before the
    /// device can DMA into them, so the token is needed to call
    /// `receive_complete()` once `poll_receive()` says it has been filled. The
    /// lease is the other half: holding it is what makes "untouched until
    /// completion" a fact rather than a comment.
    #[cfg(not(feature = "net-noalloc"))]
    rx_posted: Option<(u16, FrameLease<'static, 1, RX_BUFFER_LEN>)>,
    /// The lease for a completed frame that has been handed up to smoltcp as an
    /// `RxToken` and not yet finished with.
    ///
    /// Released at the top of the next [`Self::take_rx_frame`], which is the
    /// same point the `net-noalloc` path re-posts a released slot and for the
    /// same reason: by the time smoltcp asks for another frame, `consume` has
    /// returned on the previous one.
    #[cfg(not(feature = "net-noalloc"))]
    rx_handed_up: Option<FrameLease<'static, 1, RX_BUFFER_LEN>>,
    /// Receive slots posted to the device. Buffers live in BSS, not here — see
    /// `virtio_rings`.
    #[cfg(feature = "net-noalloc")]
    rx: crate::virtio_rings::RxRing,
    /// Transmit slots in flight.
    #[cfg(feature = "net-noalloc")]
    tx: crate::virtio_rings::TxRing,
}

impl VirtioSmoltcpDevice {
    #[must_use]
    pub const fn new(inner: Nic) -> Self {
        Self {
            inner,
            #[cfg(not(feature = "net-noalloc"))]
            tx_buffer: [0u8; 2048],
            #[cfg(not(feature = "net-noalloc"))]
            rx_posted: None,
            #[cfg(not(feature = "net-noalloc"))]
            rx_handed_up: None,
            #[cfg(feature = "net-noalloc")]
            rx: crate::virtio_rings::RxRing::new(),
            #[cfg(feature = "net-noalloc")]
            tx: crate::virtio_rings::TxRing::new(),
        }
    }

    #[must_use]
    pub fn mac_address(&self) -> [u8; 6] {
        self.inner.mac_address()
    }

    /// Take the next received frame, if the device has one ready.
    ///
    /// Returns a pointer to the L2 frame (virtio header already skipped) and its
    /// length. A raw pointer rather than a slice because the caller owns the
    /// lifetime: smoltcp's `RxToken` borrows it for exactly as long as
    /// `consume` runs, which is not a lifetime this function can name.
    ///
    /// The two implementations differ in one thing that matters: with
    /// `net-noalloc` the device always has a *ring* of buffers posted, so a
    /// burst drains without an MMIO notify per frame. Without it there is one
    /// buffer, and every single packet costs a fresh `receive_begin`.
    /// `pub(crate)` for `loopback::LoopbackAwareDevice`, which wraps this device
    /// and drives its receive path directly. That wrapper is the only caller.
    pub(crate) fn take_rx_frame(&mut self) -> Option<(*mut u8, usize)> {
        #[cfg(feature = "net-noalloc")]
        {
            // Reap first: this runs once per poll lap, which is the only place
            // TX completions get harvested promptly. Leaving it to the next
            // `claim` means a slot stays in flight for as long as nothing is
            // transmitted — which, on a request/response workload, is exactly
            // the gap between requests.
            self.tx.reap(&mut self.inner);
            // Re-post whatever the previous call released. Safe to do here and
            // not earlier: an outstanding `RxToken` has been consumed by the
            // time smoltcp asks for another frame.
            self.rx.refill(&mut self.inner);
            let Some((slot, hdr, len)) = self.rx.take_frame(&mut self.inner) else {
                nicstat::record_rx_empty();
                return None;
            };
            // Bounds-checked by the arena; `hdr + len <= FRAME_BUF` was checked
            // by `take_frame`. No intermediate reference: the caller wants a
            // pointer, so go pointer-to-pointer and never mint a `&mut` that
            // could alias. The slot stays leased by the ring until the next
            // `take_frame` releases it, which covers the RxToken's life.
            let base = crate::virtio_rings::RX_ARENA.slot_ptr(slot)?;
            // SAFETY: `hdr < FRAME_BUF` per the check above, so this stays
            // inside the slot the arena just bounds-checked.
            Some((unsafe { base.cast::<u8>().add(hdr) }, len))
        }
        #[cfg(not(feature = "net-noalloc"))]
        {
            // Release the frame handed up last time. smoltcp has finished with
            // it — `RxToken::consume` returned before it asked for another —
            // and the slot has to be free before phase 1 can re-post it.
            self.rx_handed_up = None;

            // Phase 1: ensure a receive buffer is posted to the device.
            if self.rx_posted.is_none() {
                let t = nicstat::start();
                // NOTE: do not log from here. `Device::receive` is called by
                // `iface.poll()` from inside the `NETWORK` critical section, which
                // runs with preemption disabled — see the comment above the
                // deferred `DhcpReport` emission in `poll()`. A print here spins on
                // `CONSOLE_LOCK`, and on a single-vCPU guest a holder that has been
                // preempted can never run to release it. Counters
                // (`C.rx_buffers_posted` below) are safe; console I/O is not.
                let Some(posted) = self.inner.post_rx(&RX_ARENA, RX_SLOT) else {
                    C.rx_begin_failures.fetch_add(1, Ordering::Relaxed);
                    return None;
                };
                nicstat::record_rx_begin(t);
                self.rx_posted = Some(posted);
                C.rx_buffers_posted.fetch_add(1, Ordering::Relaxed);
            }
            // Phase 2: has the device filled it?
            if self.inner.poll_receive().is_some() {
                // Phase 1 above sets `rx_posted` before we ever get here, so
                // this is `Some` by construction — but this is the per-packet
                // receive path, and a panic is not how it should report a broken
                // invariant. Drop the frame and let the next poll retry.
                let (token, lease) = self.rx_posted.take()?;
                let t = nicstat::start();
                // `complete_rx` validates `hdr_len + pkt_len` against the slot,
                // so a malformed device response cannot hand smoltcp a frame
                // that runs off the end of it.
                let (hdr_len, pkt_len) = self.inner.complete_rx(token, lease)?;
                nicstat::record_rx_packet(t, pkt_len);
                C.rx_frames_received.fetch_add(1, Ordering::Relaxed);
                // Re-lease the slot for the RxToken's life. The device released
                // it at `complete_rx`; this hands it to smoltcp instead, and the
                // release at the top of the next call is what ends it.
                let lease = RX_ARENA.lease(RX_SLOT)?;
                let base = RX_ARENA.slot_ptr(RX_SLOT)?;
                self.rx_handed_up = Some(lease);
                // SAFETY: `hdr_len + pkt_len <= RX_BUFFER_LEN` was checked by
                // `complete_rx`, so this stays inside the slot.
                return Some((unsafe { base.cast::<u8>().add(hdr_len) }, pkt_len));
            }
            nicstat::record_rx_empty();
            None
        }
    }

    /// Fill one outbound frame and dispose of it.
    ///
    /// `fill` writes the L2 frame into the staging region. `divert` is then
    /// handed the filled frame and returns `true` if it must **not** reach the
    /// wire — that is how loopback traffic is intercepted without this function
    /// needing to know what loopback is.
    ///
    /// With `net-noalloc` the frame is staged directly in a ring slot and
    /// submitted with `transmit_begin`, which returns immediately; the device's
    /// completion is reaped on a later pass. Without it, every frame goes
    /// through `VirtIONetRaw::send`, which spins until the host consumes the
    /// descriptor — 20-26 us per packet with `NETWORK` held and IRQs masked
    /// (`docs/archive/AKUMA_NET_ISSUES.md` §3.2).
    /// `pub(crate)` for the same reason as [`Self::take_rx_frame`] — the
    /// loopback wrapper diverts frames through it.
    pub(crate) fn emit_frame<R>(
        &mut self,
        len: usize,
        fill: impl FnOnce(&mut [u8]) -> R,
        divert: impl FnOnce(&[u8]) -> bool,
    ) -> R {
        #[cfg(feature = "net-noalloc")]
        {
            use crate::virtio_rings::{FRAME_BUF, TX_DISCARD_ARENA};
            if let Some(mut frame) = self.tx.claim(&mut self.inner) {
                // `transmit_begin` sends whatever is in the buffer verbatim, so
                // the virtio header has to be written into it here — unlike
                // `send`, which prepends its own.
                let hdr = self.inner.fill_buffer_header(&mut frame);
                let end = hdr.saturating_add(len).min(FRAME_BUF);
                let res = fill(&mut frame[hdr..end]);
                if divert(&frame[hdr..end]) {
                    // Never submitted, so the slot was never marked in flight.
                    // Dropping the lease here is what releases it.
                    return res;
                }
                let t = nicstat::start();
                let ok = self.tx.submit(&mut self.inner, frame, end);
                nicstat::record_tx(t, end - hdr, ok);
                if ok {
                    C.tx_frames_sent.fetch_add(1, Ordering::Relaxed);
                } else {
                    C.tx_drop_count.fetch_add(1, Ordering::Relaxed);
                }
                return res;
            }
            // Every slot was still in flight after `CLAIM_SPINS` reaps. The
            // frame is dropped — smoltcp retransmits — but `consume`'s contract
            // still requires the fill closure to run, so it writes into a buffer
            // nothing reads. Falling back to `VirtIONetRaw::send` here would be
            // a bug, not a slow path: see `TxRing::claim`.
            //
            // A diverted (loopback) frame is unaffected by NIC saturation, so it
            // is still delivered.
            let end = len.min(FRAME_BUF);
            // SAFETY: the discard buffer is write-only garbage — nothing ever
            // reads it back and it is never submitted to the device — and
            // `NETWORK` is held, so no other core is in this function. The
            // bounds are the arena's. Deliberately NOT `with_slot`: a refusal
            // there would leave no correct way to honour `consume`'s contract
            // that the fill closure runs against a buffer of the length smoltcp
            // asked for, and handing it a shorter one can panic inside smoltcp.
            let discard = unsafe { &mut *TX_DISCARD_ARENA.first_slot_ptr() };
            let res = fill(&mut discard[..end]);
            if divert(&discard[..end]) {
                return res;
            }
            C.tx_drop_count.fetch_add(1, Ordering::Relaxed);
            res
        }
        #[cfg(not(feature = "net-noalloc"))]
        {
            let end = len.min(self.tx_buffer.len());
            let res = fill(&mut self.tx_buffer[..end]);
            if divert(&self.tx_buffer[..end]) {
                return res;
            }
            let t = nicstat::start();
            let ok = self.inner.send_blocking(&self.tx_buffer[..end]);
            nicstat::record_tx(t, end, ok);
            if ok {
                C.tx_frames_sent.fetch_add(1, Ordering::Relaxed);
            } else {
                C.tx_drop_count.fetch_add(1, Ordering::Relaxed);
            }
            res
        }
    }
}

impl Device for VirtioSmoltcpDevice {
    type RxToken<'a> = VirtioRxToken<'a>;
    type TxToken<'a> = VirtioTxToken<'a>;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let (ptr, len) = self.take_rx_frame()?;
        // Build the tx token FIRST, for the same reason `LoopbackAwareDevice`
        // below chooses its frame before building one: it is what lets both
        // tokens be plain borrows instead of the `&mut *(&raw mut *self)`
        // self-aliasing this used to need — two live `&mut` to one place, which
        // is UB by the language's rules whether or not the device races them.
        //
        // It works because `take_rx_frame` returns a **raw pointer** whose
        // provenance is the BSS frame arena, NOT `self`: once it has returned,
        // `self` is unborrowed and the frame does not alias it. Keep that
        // signature — handing back a `&mut [u8]` would tie the frame to the
        // `&mut self` borrow and the aliasing would be real again.
        let tx = VirtioTxToken { dev: self };
        // SAFETY: `take_rx_frame` returned a live L2 frame of `len` bytes in
        // arena storage the device owns until the next `receive`, and the
        // token's lifetime is bounded by the `&mut self` borrow.
        let rx = VirtioRxToken { buffer: unsafe { core::slice::from_raw_parts_mut(ptr, len) } };
        Some((rx, tx))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(VirtioTxToken { dev: self })
    }

    fn capabilities(&self) -> smoltcp::phy::DeviceCapabilities {
        let mut caps = smoltcp::phy::DeviceCapabilities::default();
        caps.max_transmission_unit = 1514;
        caps
    }
}

pub struct VirtioRxToken<'a> {
    buffer: &'a mut [u8],
}

impl smoltcp::phy::RxToken for VirtioRxToken<'_> {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(self.buffer)
    }
}

pub struct VirtioTxToken<'a> {
    dev: &'a mut VirtioSmoltcpDevice,
}

impl smoltcp::phy::TxToken for VirtioTxToken<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        // No diversion: this device has no loopback queue.
        self.dev.emit_frame(len, f, |_| false)
    }
}
