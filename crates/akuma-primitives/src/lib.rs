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
//! See `docs/archive/TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md` §5.55.
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

pub mod console;
pub mod once;

pub use console::{FmtBuf, StackWriter};
pub use once::OnceCopy;
