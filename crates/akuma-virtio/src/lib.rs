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
