//! The virtio-net device, and the one place its `unsafe` API is called.
//!
//! # The obligation
//!
//! `virtio-drivers`' `VirtIONetRaw` splits receive and transmit into
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
//! So `post_rx`/`complete_rx`/`submit_tx`/`complete_tx` have safe signatures
//! that are not lies, and the raw `unsafe` calls appear exactly once each, in
//! this file. Everything else in the crate calls them safely.
//!
//! The escape hatch (`post_rx_raw` and friends) still exists for a caller with
//! a buffer of its own, and is `unsafe fn` carrying the contract above. Nothing
//! in-tree uses it; it is here so that a future caller reaches for something
//! documented rather than reintroducing a bare `unsafe { dev.… }`.
//!
//! # Attribution
//!
//! The shape is `rump_tap.rs`'s, which has wrapped the same calls in
//! `akuma_rump::RawNic` since the rump port so the orchestration and its host
//! tests need no virtio knowledge. This applies it to the smoltcp path.

use crate::frames::{FrameArena, FrameLease};
use akuma_virtio::{VirtioHal, VirtioTransport};
use virtio_drivers::device::net::VirtIONetRaw;

/// The virtio-net device this crate binds. `16` is the virtqueue depth.
pub type NetDev = VirtIONetRaw<VirtioHal, VirtioTransport, 16>;

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

    /// Write the virtio net header into `buf`, returning its length.
    ///
    /// Needed by the ring transmit path: `transmit_begin` sends the buffer
    /// verbatim, unlike `send`, which prepends its own header.
    pub fn fill_buffer_header(&self, buf: &mut [u8]) -> usize {
        self.inner.fill_buffer_header(buf).unwrap_or(0)
    }

    /// The token of a receive the device has finished, if any. Does not consume
    /// it — `complete_rx` does.
    pub fn poll_receive(&mut self) -> Option<u16> {
        self.inner.poll_receive()
    }

    /// The token of a transmit the device has finished, if any.
    pub fn poll_transmit(&mut self) -> Option<u16> {
        self.inner.poll_transmit()
    }

    /// Blocking transmit: `add_notify_wait_pop`, which spins until the host has
    /// consumed the descriptor.
    ///
    /// Safe upstream because the buffer is released before it returns — that is
    /// exactly what makes it blocking, and why the ring path exists. Must never
    /// be interleaved with `submit_tx` on the same device: `send` pops the used
    /// ring by head token and fails with `WrongToken` when anything else is in
    /// flight, leaking the descriptor chain (`virtio_rings::TxRing::claim`).
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

    /// Submit `len` bytes (virtio header included) from a leased arena slot.
    ///
    /// On success the lease is handed back alongside the token and must be kept
    /// until [`Self::complete_tx`]. On refusal the lease comes back unconsumed
    /// so the caller can reuse the slot immediately.
    #[allow(clippy::type_complexity)]
    pub fn submit_tx<const S: usize, const L: usize>(
        &mut self,
        lease: FrameLease<'static, S, L>,
        len: usize,
    ) -> Result<(u16, FrameLease<'static, S, L>), FrameLease<'static, S, L>> {
        let len = len.min(lease.len());
        // SAFETY: as `post_rx` — a `static` arena slot whose exclusive borrow
        // the returned lease holds for the device's ownership window, handed
        // back verbatim to `complete_tx`.
        match unsafe { self.inner.transmit_begin(&lease[..len]) } {
            Ok(token) => Ok((token, lease)),
            Err(_) => Err(lease),
        }
    }

    /// Complete a transmit started with `submit_tx`.
    ///
    /// `len` must be the length that was submitted: `pop_used` walks the
    /// descriptor chain to unshare it, so a different length unshares the wrong
    /// range.
    ///
    /// On failure the lease comes **back** rather than being dropped, and that
    /// is deliberate. A refused completion means the token/descriptor map has
    /// desynchronised, so the device may still own the buffer; releasing the
    /// slot would hand it to the next transmit while DMA is potentially live.
    /// Holding the lease leaks one slot instead, which is what the ring did
    /// before this wrapper existed and is the safe direction.
    pub fn complete_tx<const S: usize, const L: usize>(
        &mut self,
        token: u16,
        lease: FrameLease<'static, S, L>,
        len: usize,
    ) -> Result<(), FrameLease<'static, S, L>> {
        let len = len.min(lease.len());
        // SAFETY: same buffer and same length as the matching `submit_tx`.
        if unsafe { self.inner.transmit_complete(token, &lease[..len]) }.is_ok() {
            Ok(())
        } else {
            Err(lease)
        }
    }

    /// Post a caller-supplied buffer to receive into.
    ///
    /// # Safety
    /// The module-level obligation, in full: `buf` must remain allocated at a
    /// fixed address and untouched until the matching [`Self::complete_rx_raw`]
    /// for the returned token, which must be given the same buffer. Prefer
    /// [`Self::post_rx`], which discharges all of that with an arena lease.
    pub unsafe fn post_rx_raw(&mut self, buf: &mut [u8]) -> Option<u16> {
        unsafe { self.inner.receive_begin(buf) }.ok()
    }

    /// Complete a receive started with [`Self::post_rx_raw`].
    ///
    /// # Safety
    /// `buf` must be the buffer that was posted under `token`.
    pub unsafe fn complete_rx_raw(&mut self, token: u16, buf: &mut [u8]) -> Option<(usize, usize)> {
        let (hdr_len, pkt_len) = unsafe { self.inner.receive_complete(token, buf) }.ok()?;
        if hdr_len.saturating_add(pkt_len) > buf.len() {
            return None;
        }
        Some((hdr_len, pkt_len))
    }
}
