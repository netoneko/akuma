//! Tar extraction, as a library.
//!
//! `box pull` used to extract layers by spawning `/bin/tar`. That binary is a
//! path, and a path can be replaced: `/bin/tar` was in fact a busybox applet
//! symlink for the whole life of the feature, so every image layer was extracted
//! by busybox, whose hardlinks go through `link()` — which akuma implements as a
//! full file copy. One 1.9 MB layer with 410 hardlinks to a single binary
//! extracted to 467 MB, and the copies lost their mode bits, so the container
//! could not execute its own commands. Linking this in makes the extractor a
//! dependency instead of a lookup.
//!
//! [`format`] is pure and host-testable; everything else needs libakuma and is
//! compiled only with the `akuma` feature (on by default).

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod format;

#[cfg(feature = "akuma")]
mod extract;

#[cfg(feature = "akuma")]
pub use extract::{ExtractOptions, Stats, TarError, extract_file};
