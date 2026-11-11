//! The virtio-net device, raw: frames in and out of two virtqueues, no stack.
//!
//! Driven in-tree since 2026-11-01; it replaced `virtio-drivers`' `VirtIONetRaw`
//! and keeps its method names and contracts, so `akuma-net-nic`'s `Nic` and the
//! rump tap changed only their type. Every buffer carries the 12-byte
//! `virtio_net_hdr` at its front ([`HEADER_LEN`]): the caller reserves it on
//! transmit ([`NetRaw::fill_buffer_header`]) and skips it on receive.
//!
//! # The obligation
//!
//! `receive_begin` / `transmit_begin` hand the caller's buffer to the device,
//! which owns it (writes it, or reads it) until the matching `*_complete` for the
//! same token. That cannot be stated in a safe signature over `&mut [u8]`, so
//! those four are `unsafe fn` with the obligation written on each, exactly as
//! before; `akuma-net-nic` discharges it with its static frame arenas.

use core::sync::atomic::{AtomicBool, Ordering};

use akuma_dma::DmaArray;
use akuma_dma::align::A64;
use akuma_virtqueue::net::{CONFIG_MAC, F_MAC, RX_QUEUE, TX_QUEUE};
use akuma_virtqueue::queue::UsedError;
use akuma_virtqueue::{Buf, Layout};

use crate::mmio::Transport;
use crate::ring::{Queue, Ring, phys_of};

pub use akuma_virtqueue::net::HEADER_LEN;

/// Entries per queue: up to 16 receive buffers posted and 16 frames in flight.
pub const QUEUE_SIZE: usize = 16;
const RING_WORDS: usize = Layout::new(QUEUE_SIZE).words();
/// Devices this driver can run at once: the stack's NIC and the rump tap's.
pub const MAX_NICS: usize = 2;

static RINGS: [[Ring<RING_WORDS>; 2]; MAX_NICS] = [const { [const { Ring::new() }; 2] }; MAX_NICS];
/// The all-zero `virtio_net_hdr` [`NetRaw::send`] chains in front of a bare
/// frame. Never written, so one serves every device.
static ZERO_HEADER: DmaArray<u8, 16, A64> = DmaArray::new();
static CLAIMED: [AtomicBool; MAX_NICS] = [const { AtomicBool::new(false) }; MAX_NICS];

/// Why a net operation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetError {
    /// The transport did not come up (see [`crate::mmio::Error`]).
    Transport,
    /// Every one of [`MAX_NICS`] is taken.
    NoSlot,
    /// The queue has no free descriptor.
    Full,
    /// The request has not completed yet.
    NotReady,
    /// The device completed something else, or something it was never given.
    Device,
    /// A buffer too short to hold the header, or too long for a descriptor.
    Buffer,
}

impl_display!(NetError {
    Transport => "virtio-net transport init failed",
    NoSlot => "no free virtio-net slot",
    Full => "virtqueue full",
    NotReady => "not completed",
    Device => "device completed an unexpected request",
    Buffer => "bad buffer length",
});

type NetQueue = Queue<QUEUE_SIZE, RING_WORDS>;

/// A virtio-net device with its receive and transmit queues.
pub struct NetRaw {
    transport: Transport,
    rx: NetQueue,
    tx: NetQueue,
    mac: [u8; 6],
}

impl NetRaw {
    /// Bring the device in `transport` up.
    pub fn new(mut transport: Transport) -> Result<Self, NetError> {
        let n = CLAIMED.iter().position(|c| !c.swap(true, Ordering::AcqRel)).ok_or(NetError::NoSlot)?;
        let [rx_ring, tx_ring] = &RINGS[n];
        let features = transport.begin(F_MAC).map_err(|_| NetError::Transport)?;
        let rx = NetQueue::setup(&mut transport, RX_QUEUE, rx_ring.claim().ok_or(NetError::NoSlot)?)
            .map_err(|_| NetError::Transport)?;
        let tx = NetQueue::setup(&mut transport, TX_QUEUE, tx_ring.claim().ok_or(NetError::NoSlot)?)
            .map_err(|_| NetError::Transport)?;
        transport.driver_ok();
        // QEMU and Firecracker always offer MAC; without it the address is the
        // locally administered one QEMU would have picked.
        let mac = if features & F_MAC != 0 {
            core::array::from_fn(|i| transport.config_u8(CONFIG_MAC + i))
        } else {
            [0x52, 0x54, 0x00, 0x12, 0x34, 0x56]
        };
        Ok(Self { transport, rx, tx, mac })
    }

    #[must_use]
    pub const fn mac_address(&self) -> [u8; 6] {
        self.mac
    }

    /// The transport's slot register base, for the IRQ acknowledge path.
    #[must_use]
    pub const fn transport(&self) -> &Transport {
        &self.transport
    }

    /// The token of the next completed receive, if any, without consuming it.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)] // a junk id fails `receive_complete`'s token match
    pub fn poll_receive(&self) -> Option<u16> {
        self.rx.sq.peek_used(self.rx.mem).map(|id| id as u16)
    }

    /// Transmit one bare frame (no header: a zeroed one is chained in front of
    /// it, as `VirtIONetRaw::send` did) and wait for the device to take it.
    ///
    /// Refused while any other transmit is in flight: then the only element the
    /// device can complete is this one, so waiting for it cannot consume
    /// another request's completion.
    pub fn send(&mut self, frame: &[u8]) -> Result<(), NetError> {
        if self.tx.sq.num_free() != QUEUE_SIZE {
            return Err(NetError::Full);
        }
        let len = u32::try_from(frame.len()).map_err(|_| NetError::Buffer)?;
        if len == 0 {
            return Err(NetError::Buffer);
        }
        #[allow(clippy::cast_possible_truncation)]
        let bufs = [
            Buf { addr: phys_of(ZERO_HEADER.addr() as *const u8), len: HEADER_LEN as u32, device_writes: false },
            Buf { addr: phys_of(frame.as_ptr()), len, device_writes: false },
        ];
        // `frame` is borrowed for this whole call and the loop below does not
        // return until the device has completed this token, so the device's
        // ownership window ends before the borrow does.
        let token = self.tx.submit(&self.transport, &bufs).map_err(|_| NetError::Full)?;
        loop {
            match self.tx.sq.pop_used(self.tx.mem) {
                Some(Ok((h, _))) if h == token => return Ok(()),
                Some(_) => {}
                None => core::hint::spin_loop(),
            }
        }
    }

    /// Post `buf` for the device to receive one frame into. Returns its token.
    ///
    /// # Safety
    /// `buf` is owned by the device from this call until [`Self::receive_complete`]
    /// returns `Ok` for the returned token: it must stay allocated, at the same
    /// address, and untouched until then, and that call must be given the same
    /// buffer.
    pub unsafe fn receive_begin(&mut self, buf: &mut [u8]) -> Result<u16, NetError> {
        let len = buf_len(buf.len())?;
        let b = Buf { addr: phys_of(buf.as_ptr()), len, device_writes: true };
        self.rx.submit(&self.transport, &[b]).map_err(|_| NetError::Full)
    }

    /// Finish receive `token`: `(header length, frame length)` within `buf`.
    ///
    /// # Safety
    /// `buf` is the buffer [`Self::receive_begin`] returned `token` for.
    pub unsafe fn receive_complete(&mut self, token: u16, buf: &mut [u8]) -> Result<(usize, usize), NetError> {
        let len = complete(&mut self.rx, token)? as usize;
        // The length is the device's word; it must fit what was offered.
        if len < HEADER_LEN || len > buf.len() {
            return Err(NetError::Device);
        }
        Ok((HEADER_LEN, len - HEADER_LEN))
    }

}

/// A buffer length a descriptor can carry, and long enough for the header.
fn buf_len(len: usize) -> Result<u32, NetError> {
    if len < HEADER_LEN {
        return Err(NetError::Buffer);
    }
    u32::try_from(len).map_err(|_| NetError::Buffer)
}

/// Consume `token`'s completion: the bytes the device wrote.
fn complete(q: &mut NetQueue, token: u16) -> Result<u32, NetError> {
    match q.sq.pop_used_expect(q.mem, token) {
        None => Err(NetError::NotReady),
        Some(Ok(len)) => Ok(len),
        Some(Err(UsedError::WrongToken { .. } | UsedError::BadId(_) | UsedError::NotInFlight(_))) => Err(NetError::Device),
    }
}
