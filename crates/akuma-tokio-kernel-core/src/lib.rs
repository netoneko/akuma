//! The arch-neutral half of `akuma-tokio-kernel` (B21, `docs/archive/TAOXICHUAN_B21_AARCH64.md`).
//!
//! What a tokio kernel decides without naming its machine. The machine halves
//! (`akuma-tokio-kernel-amd64`, and the aarch64 one to come) build on it; nothing here
//! reads a register, a page table or a device. It moves in from the amd64 half one piece
//! at a time, starting with the pieces that never touched the machine at all.

#![no_std]
#![forbid(unsafe_code)]

#[cfg(feature = "lazy")]
extern crate alloc;

pub mod cmdline;
#[cfg(feature = "lazy")]
pub mod cow;
pub mod init;
#[cfg(feature = "lazy")]
pub mod lazy;
#[cfg(feature = "lazy")]
pub mod loader;
pub mod bkl;
#[cfg(feature = "net")]
pub mod net;
#[cfg(feature = "process")]
pub mod proc;
#[cfg(feature = "process")]
pub mod signal;
pub mod lazy_record;
#[cfg(feature = "lazy")]
pub mod mm;
pub mod mmu;
pub mod nosys;
pub mod ring;
pub mod task;
#[cfg(feature = "shmpages")]
pub mod shmpages;
#[cfg(feature = "lazy")]
pub mod tree;

/// Who the kernel is: its name on the console, its node name and release in `uname`, its
/// banner. Set once at the top of the boot from the `Kernel` the root built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    pub name: &'static str,
    pub nodename: &'static str,
    pub version: &'static str,
    pub art: &'static str,
}

impl Identity {
    /// The identity before the root has set one.
    pub const DEFAULT: Self = Self { name: "akuma", nodename: "akuma", version: "0", art: "" };
}

static IDENTITY: akuma_primitives::OnceCopy<Identity> = akuma_primitives::OnceCopy::new();

/// Set the kernel's identity (once, at the top of the boot).
pub fn set_identity(id: Identity) {
    IDENTITY.set(id);
}

/// The kernel's identity, or [`Identity::DEFAULT`] before [`set_identity`].
#[must_use]
pub fn identity() -> Identity {
    IDENTITY.get().unwrap_or(Identity::DEFAULT)
}
