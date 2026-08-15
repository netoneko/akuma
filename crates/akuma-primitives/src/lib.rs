//! Kernel primitives with no dependencies — the leaf of the crate graph.
//!
//! # Why this crate exists
//!
//! Several primitives in this tree existed in two to five copies, and every copy
//! had the same cause: **the canonical version lived in a crate the duplicator
//! could not depend on.** The bin crate owns the console, so `akuma-exec` grew
//! its own `StackWriter`/`safe_print!` rather than depend on the bin crate (which
//! would be a cycle); `akuma-virtio` then grew a third copy as `vprint!`, with a
//! header comment explaining that "a library crate cannot reach that macro".
//! `OnceCopy` and `PreemptGuard` live in `akuma-exec`, so `akuma-ext2` and
//! `akuma-net` compile the 23.8k-line execution crate to reach ~40 lines of
//! guard. None of that was carelessness; it was a missing crate.
//!
//! See `docs/archive/TRIM_FAT_EMBARASSING_DUPLICATIONS.md` §5.55.
//!
//! # The rule for adding to this crate
//!
//! **No dependencies, ever.** This is the leaf everything else may depend on, so
//! anything added here joins the whole tree's dependency closure. A primitive
//! that needs another crate does not belong here.
//!
//! Where a primitive needs something only the kernel can provide — a console, a
//! clock — it takes it as a boot-registered [`OnceCopy`] hook and **degrades**
//! when unregistered rather than panicking. That keeps host unit tests and
//! early-boot callers working, which is the property the copies in `akuma-exec`
//! and `akuma-virtio` were each hand-rolling.

#![cfg_attr(not(test), no_std)]
// `#[inline(always)]` is load-bearing here, not a hint. These wrappers replace
// open-coded `asm!` at their call sites — several on the BKL acquire and
// per-packet DMA paths — and the merge is only behaviour-preserving if it emits
// the same instructions with no call overhead. Both the bin crate
// (`src/main.rs:8`) and `akuma-exec` (`lib.rs:50`) carry the same allow.
#![allow(clippy::inline_always)]
// `const INIT: AtomicUsize = …; [INIT; MAX_THREADS]` is the only way to build a
// const array of atomics, and it is the pattern every per-slot static in
// `akuma-exec`'s threading module uses. Allowed there (`lib.rs:22`) for the same
// reason.
#![allow(clippy::declare_interior_mutable_const)]

pub mod addr;
pub mod clock;
pub mod console;
pub mod errno;
pub mod inode_pin;
pub mod irq;
pub mod mmio;
pub mod once;
pub mod preempt;
pub mod toggled_guard;

pub use addr::{phys_to_virt, virt_to_phys};
pub use toggled_guard::{GuardToggle, ToggledGuard};
pub use console::{FmtBuf, StackWriter};
pub use preempt::{MAX_THREADS, PreemptGuard};
pub use irq::{
    DAIF_I_MASKED, IrqGuard, irq_restore, irq_save_mask, mask_irqs_sync, read_daif, unmask_irqs,
    unmask_irqs_sync, with_irqs_disabled,
};
pub use once::{OnceCopy, Registered};
pub use inode_pin::InodePin;
