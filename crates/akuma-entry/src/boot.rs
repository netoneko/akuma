//! Boot code for AArch64: `_boot`, in `akuma-boot-aarch64` since 2026-11-04 so
//! the tokio kernel boots through the same instructions. Re-exported here so
//! `boot::x0_at_entry` keeps its path.

pub use akuma_boot_aarch64::{BOOT_X0_AT_ENTRY, x0_at_entry};
