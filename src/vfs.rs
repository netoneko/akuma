//! Shim over [`akuma_vfs_glue`].
//!
//! `src/vfs/` became `crates/akuma-vfs-glue` on 2026-09-01, once
//! `akuma-syscalls-log` and `akuma-syscalls-ipc` had broken the
//! `src/syscall/` ↔ `src/vfs/` cycle that kept it here
//! (`docs/archive/SRC_SYSCALL_EXTRACTION.md`).
//!
//! One thing keeps this file alive: the four `/proc` facts only the binary can
//! answer are registered here. The rest is a re-export, so no call site in
//! `src/syscall/` changed. (The config half went to `akuma-config`.)

pub use akuma_vfs_glue::*;

/// Install the four `/proc` facts only the binary can answer. Called once from
/// `kernel_main`, before the root filesystem is mounted.
///
/// The config half of this went away when `akuma-config` landed — the crate reads
/// the tunables as `const`s now. What is left is genuinely un-const-able: an
/// inline `mod audio` in `main.rs`, `fs::exists`, the SMP topology probe, and the
/// wall clock.
pub fn register() {
    akuma_vfs_glue::set_hooks(akuma_vfs_glue::VfsGlueHooks {
        audio_is_available: crate::audio::is_available,
        fs_exists: crate::fs::exists,
        probed_core_count,
        utc_time_us: crate::timer::utc_time_us,
        fpcache_init: crate::file_page_cache::init,
    });
}

#[cfg(kernel_smp_shared)]
fn probed_core_count() -> usize {
    crate::smp_shared::probed_core_count()
}

/// Without real SMP there is one core; the crate's caller is gated off anyway.
#[cfg(not(kernel_smp_shared))]
fn probed_core_count() -> usize {
    1
}
