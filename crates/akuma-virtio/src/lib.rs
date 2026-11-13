//! Shared virtio scaffolding: MMIO device probing, the virtio-mmio transport,
//! and the device drivers.
//!
//! This crate exists to hold the pieces every virtio consumer needs, which were
//! previously duplicated between the kernel bin crate and `akuma-net` —
//! see [`hal`] for the history of the two `Hal` impls that became one.
//!
//! Layering: this sits above `akuma-exec` (for the `mmu` address seam) and below
//! both the kernel and `akuma-net`.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

/// A `Display` impl for a fieldless error enum, from a variant → message table.
///
/// The three driver error types below (`BlockError`, `RngError`, `AudioError`)
/// each hand-wrote the same `fmt` → `match self` → `write!(f, "…")` body over a
/// list of unit variants; the only thing that differed was the list. Defined
/// here rather than in `akuma-primitives` because all three consumers are in
/// this crate — it stops being intra-crate the moment a fourth appears
/// elsewhere, and that is when to move it.
///
/// `f.write_str` rather than `write!`: every message is a literal with no
/// arguments, so there is nothing to format.
///
/// Declared before the `mod` items on purpose — a crate-root `macro_rules!` is
/// only in scope for modules declared after it in source order.
macro_rules! impl_display {
    ($ty:ty { $($variant:ident => $msg:literal),+ $(,)? }) => {
        impl core::fmt::Display for $ty {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.write_str(match self {
                    $(Self::$variant => $msg,)+
                })
            }
        }
    };
}

pub mod mmio;
pub mod probe;
mod ring;

// `virtio-drivers`, its `Hal` and the stepped-status wrapper around its
// transport: kept only for virtio-sound. Without `sound`, neither it nor
// `zerocopy`/`safe-mmio`/`bitflags` 2/`thiserror` is compiled.
#[cfg(feature = "sound")]
pub mod hal;
#[cfg(feature = "sound")]
pub mod transport;

// The virtio-mmio device drivers. These lived in the kernel bin crate until they
// followed the probe loop they all shared into this one. `block` and `net` are
// driven in-tree over `akuma-virtqueue` (2026-11-01).
pub mod block;
pub mod net;
pub mod rng;
pub mod audio;

#[cfg(feature = "sound")]
pub use hal::VirtioHal;
pub use probe::{device_id, num_slots, slot_addr, slot_addrs};

/// The virtio-mmio transport every in-tree driver is built on.
///
/// Its status writes step one milestone at a time, because Firecracker validates
/// the virtio 1.0 §3.1.1 sequence strictly while QEMU does not.
pub type VirtioTransport = mmio::Transport;
#[cfg(feature = "sound")]
pub use transport::SteppedMmioTransport;

/// The tree's one heap-free print macro, re-exported so this crate's
/// `crate::safe_print!(…)` call sites resolve.
///
/// This crate used to carry its own copy as `print::vprint!` — because, as that
/// module's header put it, "a library crate cannot reach that macro". There is
/// now a leaf crate that can hold it, so the copy is gone.
pub use akuma_primitives::safe_print;
