//! The driver side of virtio over MMIO, as decisions with no hardware attached.
//!
//! Akuma drives two virtio devices on every VMM target: the disk (virtio-blk) and
//! the NIC (virtio-net). This crate is everything about that which can be stated
//! without touching memory a device shares:
//!
//! | module | |
//! |---|---|
//! | [`mmio`] | the virtio-mmio v2 register offsets and the magic/version a slot must show |
//! | [`status`] | the device-status bits, in the order virtio 1.x §3.1.1 sets them |
//! | [`features`] | feature negotiation: what the driver asks for, and the one bit it cannot do without |
//! | [`queue`] | the split virtqueue: layout, descriptor free list, available ring, used ring |
//! | [`blk`] | the virtio-blk request header, status byte and config offsets |
//! | [`net`] | the virtio-net header length and config offsets |
//!
//! `akuma-virtio` performs it: the register window, the DMA statics the rings live
//! in, the busy-wait for completion. It replaced the external `virtio-drivers`
//! crate for both devices (2026-11-01); that crate is still used for virtio-sound.
//!
//! What is deliberately absent: legacy (version 1) MMIO, the packed ring,
//! indirect descriptors, `EVENT_IDX`, and every optional device feature. The
//! kernel needs none of them, and each is a branch a device can steer.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub mod blk;
pub mod features;
pub mod mmio;
pub mod net;
pub mod queue;
pub mod status;

pub use queue::{Buf, Layout, QueueMem, SplitQueue};
