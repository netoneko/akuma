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
pub mod print;
pub mod probe;

// The virtio-mmio device drivers. These lived in the kernel bin crate until they
// followed the `Hal` and the probe loop they all shared into this one.
pub mod block;
pub mod rng;
pub mod audio;

pub use hal::VirtioHal;
pub use probe::{VIRTIO_MMIO_ADDRS, device_id};

// Re-exported so the `vprint!` macro expands to a path that works in every
// consumer without each one having to depend on `akuma-exec` directly.
#[doc(hidden)]
pub use akuma_exec::process::FmtBuf;
