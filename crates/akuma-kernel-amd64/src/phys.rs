//! Translating physical addresses into ones the kernel can dereference.
//!
//! The logic lives in `akuma-boot-amd64::phys` since the Taoxichuan stage-1
//! extraction (2026-10-24): a composed kernel pulls the physmap as a crate,
//! and this module re-exports it so every `crate::phys::…` call site in this
//! kernel is unchanged. The window layout, the `PHYSMAP_LIMIT` derivation and
//! `FreshFrame`'s ownership discipline are documented at the crate.

pub use akuma_boot_amd64::phys::*;
