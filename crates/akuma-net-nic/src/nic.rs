//! The virtio-net device, and the one place its `unsafe` API is called.
//!
//! # The obligation
//!
//! `akuma_virtio::net::NetRaw` (in-tree since 2026-11-01; it was `virtio-drivers`'
//! `VirtIONetRaw`, whose contract it kept) splits receive and transmit into
//! begin/complete pairs so the caller can keep several buffers in flight. Those
//! four calls are `unsafe fn` for one reason, and it is the same reason each
//! time:
//!
//! > The buffer handed to `receive_begin` / `transmit_begin` is **owned by the
//! > device** — written by DMA, or read by it — from that call until the
//! > matching `receive_complete` / `transmit_complete` for the same token. It
//! > must stay allocated, at a fixed address, and untouched by the driver for
//! > that whole window, and the completing call must be given the *same* buffer.
//!
//! No wrapper can discharge that on the caller's behalf while the caller
//! supplies the buffer: a safe signature taking `&mut [u8]` would accept a stack
//! temporary and let it die under the device.
//!
//! # How it is discharged here
//!
//! By not letting the caller supply the buffer. The safe entry points below
//! take a [`FrameArena`] slot instead, and the two halves of the obligation
//! become type-level facts:
//!
//! | obligation | discharged by |
//! |---|---|
//! | stays allocated, fixed address | the arena is a `static` in BSS |
//! | untouched by the driver until completion | the [`FrameLease`] the NIC holds for the device's whole ownership window |
//! | same buffer at completion | the lease *is* the buffer; completion consumes it |
//!
//! So `post_rx`/`complete_rx` have safe signatures
//! that are not lies, and the raw `unsafe` calls appear exactly once each, in
//! this file. Everything else in the crate calls them safely.
//!
//! There is deliberately no entry point taking a caller's `&mut [u8]`: one
//! (`post_rx_raw`/`complete_rx_raw`) existed with no caller, and a method of
//! that shape is the seam drawn in the wrong place. A new user of the device
//! gets an arena slot, or extends the arena.
//!
//! # Attribution
//!
//! The shape is `rump_tap.rs`'s, which has wrapped the same calls in
//! `akuma_rump::RawNic` since the rump port so the orchestration and its host
//! tests need no virtio knowledge. This applies it to the smoltcp path.

use crate::frames::{FrameArena, FrameLease};
use akuma_virtio::net::NetRaw;

/// The virtio-net device this crate binds. `16` is the virtqueue depth.
pub type NetDev = NetRaw;

/// A virtio-net device with the buffer-lifetime obligation folded in.
pub struct Nic {
    inner: NetDev,
}

impl Nic {
    #[must_use]
    pub const fn new(inner: NetDev) -> Self {
        Self { inner }
    }

    #[must_use]
    pub fn mac_address(&self) -> [u8; 6] {
        self.inner.mac_address()
    }

    /// The token of a receive the device has finished, if any. Does not consume
    /// it — `complete_rx` does.
    pub fn poll_receive(&mut self) -> Option<u16> {
        self.inner.poll_receive()
    }

    /// Blocking transmit: `add_notify_wait_pop`, which spins until the host has
    /// consumed the descriptor.
    ///
    /// Safe upstream because the buffer is released before it returns — that is
    /// exactly what makes it blocking. The asynchronous transmit ring
    /// (`net-noalloc`) measured worse end to end and was removed 2026-11-11.
    pub fn send_blocking(&mut self, frame: &[u8]) -> bool {
        self.inner.send(frame).is_ok()
    }

    /// Post arena slot `slot` to the device to receive into.
    ///
    /// Returns the device token and the lease that must be kept until
    /// [`Self::complete_rx`] — dropping it early releases the slot for reuse
    /// while the device is still writing to it, which is why it is
    /// `#[must_use]`.
    ///
    /// `None` if the slot is out of range, already borrowed, or the virtqueue
    /// is full.
    #[must_use]
    pub fn post_rx<const S: usize, const L: usize>(
        &mut self,
        arena: &'static FrameArena<S, L>,
        slot: usize,
    ) -> Option<(u16, FrameLease<'static, S, L>)> {
        let mut lease = arena.lease(slot)?;
        // SAFETY: the buffer is a slot of a `static` arena, so it is allocated
        // for the program's life at a fixed address; `lease` is returned to the
        // caller and holds the slot's exclusive borrow for the device's whole
        // ownership window; and `complete_rx` takes that same lease back, so
        // the completing call cannot be given a different buffer.
        match unsafe { self.inner.receive_begin(&mut lease) } {
            Ok(token) => Some((token, lease)),
            Err(_) => None,
        }
    }

    /// Complete a receive started with `post_rx`.
    ///
    /// Consumes the lease — the device's ownership window ends here — and
    /// returns `(header_len, packet_len)`; the frame is
    /// `slot[header_len .. header_len + packet_len]`.
    ///
    /// The length pair is validated against the slot before it is returned. A
    /// malformed device response claiming a frame longer than the buffer would
    /// otherwise have smoltcp parse memory past the slot; see the EL1 `EC=0x25`
    /// entry in `docs/runbooks/debug-network.md`.
    pub fn complete_rx<const S: usize, const L: usize>(
        &mut self,
        token: u16,
        mut lease: FrameLease<'static, S, L>,
    ) -> Option<(usize, usize)> {
        // SAFETY: same buffer as the matching `post_rx` — the lease is the
        // proof — and this ends the window the lease was held for.
        let (hdr_len, pkt_len) = unsafe { self.inner.receive_complete(token, &mut lease) }.ok()?;
        if hdr_len.saturating_add(pkt_len) > lease.len() {
            return None;
        }
        Some((hdr_len, pkt_len))
    }
}
