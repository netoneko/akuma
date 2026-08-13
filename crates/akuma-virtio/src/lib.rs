//! Shared virtio scaffolding: the DMA HAL, MMIO device probing, and the
//! virtio-mmio device drivers.
//!
//! This crate exists to hold the pieces every virtio consumer needs, which were
//! previously duplicated between the kernel bin crate and `akuma-net` —
//! see [`hal`] for the history of the two `Hal` impls that became one.
//!
//! Layering: this sits above `akuma-exec` (for the `mmu` address seam) and below
//! both the kernel and `akuma-net`.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod hal;
pub mod probe;

// The virtio-mmio device drivers. These lived in the kernel bin crate until they
// followed the `Hal` and the probe loop they all shared into this one.
pub mod block;
pub mod rng;
pub mod audio;

pub use hal::VirtioHal;
pub use probe::{VIRTIO_MMIO_ADDRS, device_id};

/// The tree's one heap-free print macro, re-exported so this crate's
/// `crate::safe_print!(…)` call sites resolve.
///
/// This crate used to carry its own copy as `print::vprint!` — because, as that
/// module's header put it, "a library crate cannot reach that macro". There is
/// now a leaf crate that can hold it, so the copy is gone.
pub use akuma_primitives::safe_print;
