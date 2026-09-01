//! Shim over [`akuma_vfs_glue`].
//!
//! `src/vfs/` became `crates/akuma-vfs-glue` on 2026-09-01, once
//! `akuma-syscalls-log` and `akuma-syscalls-ipc` had broken the
//! `src/syscall/` ↔ `src/vfs/` cycle that kept it here
//! (`docs/archive/SRC_SYSCALL_EXTRACTION.md`).
//!
//! Two things keep this file alive, and both are the binary's job:
//! **`src/config.rs` stays the single source of truth** for the tunables, and
//! the four `/proc` facts only the binary can answer are registered here. The
//! rest is a re-export, so no call site in `src/syscall/` changed.

pub use akuma_vfs_glue::*;

/// Hand the binary's config and callbacks to the crate. Called once from
/// `kernel_main`, before the root filesystem is mounted.
pub fn register() {
    akuma_vfs_glue::set_config(akuma_vfs_glue::VfsGlueConfig {
        proc_syscall_log_enabled: crate::config::PROC_SYSCALL_LOG_ENABLED,
        proc_sysvipc_enabled: crate::config::PROC_SYSVIPC_ENABLED,
        shared_file_pages_enabled: crate::config::SHARED_FILE_PAGES_ENABLED,
        syscall_debug_info_enabled: crate::config::SYSCALL_DEBUG_INFO_ENABLED,
        max_threads: crate::config::MAX_THREADS,
        proc_stdout_max_size: crate::config::PROC_STDOUT_MAX_SIZE,
    });
    akuma_vfs_glue::set_hooks(akuma_vfs_glue::VfsGlueHooks {
        audio_is_available: crate::audio::is_available,
        fs_exists: crate::fs::exists,
        probed_core_count,
        utc_time_us: crate::timer::utc_time_us,
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
