//! virtio-net on the wire (virtio 1.x §5.1).
//!
//! With [`crate::features::VERSION_1`] and no offloads negotiated, every frame in
//! either direction is preceded by a [`HEADER_LEN`]-byte `virtio_net_hdr` that is
//! all zero on transmit, and each receive buffer holds a whole frame (no
//! `MRG_RXBUF`).

/// `virtio_net_hdr` with `num_buffers`, the size under `VERSION_1`.
pub const HEADER_LEN: usize = 12;
/// Feature: the device has a MAC address in config space.
pub const F_MAC: u64 = 1 << 5;
/// Config offset of the 6-byte MAC.
pub const CONFIG_MAC: usize = 0;

pub const RX_QUEUE: u16 = 0;
pub const TX_QUEUE: u16 = 1;
