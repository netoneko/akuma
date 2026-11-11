//! The AArch64 root kernel's boot self-test suite.
//!
//! Moved out of `src/` (2026-11-11) so the kernel's bin is its entry and nothing else. The
//! suite runs inside `akuma_kernel_glue::kernel_main`, which cannot depend on this crate, so
//! the entry points are registered as hooks: the root bin is
//! `akuma_kernel::entry!(akuma_kernel_tests_aarch64::register)`, and `register` runs before
//! `kernel_main` (`akuma_kernel_glue::BootTestHooks`).
//!
//! The suite still spells the kernel's subsystems as `crate::x`, as it did in the bin. The
//! re-exports below are the same ones `src/main.rs` carried for it, so the move changed no
//! test. A build with `no-tests` (or the `extreme-size` profile) has no `kernel_tests` cfg
//! and this crate is only an empty `register`.
//!
//! Kernel-only: on any other target the crate is empty.

#![no_std]
#![cfg(all(target_arch = "aarch64", target_os = "none"))]
// Kernel-specific: MMIO and error-code paths require these casts intentionally.
#![allow(clippy::cast_ptr_alignment)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::wrong_self_convention)]
#![allow(clippy::inline_always)]
#![allow(clippy::needless_pass_by_value)]
// The rump-only devbox compiles smoltcp out; see the root `src/main.rs`.
#![cfg_attr(not(feature = "smoltcp"), allow(dead_code))]
#![cfg_attr(not(kernel_tests), allow(unused_imports))]

extern crate alloc;

pub use akuma_primitives::{safe_print, tprint};

pub use akuma_alloc as allocator;
pub use akuma_kernel_core::akuma;
#[cfg(kernel_bkl_profile)]
pub use akuma_kernel_core::bkl_profile;
pub use akuma_kernel_core::config;
pub use akuma_kernel_core::file_page_cache;
pub use akuma_kernel_core::fs;
pub use akuma_kernel_core::irq;
pub use akuma_kernel_core::klog;
#[cfg(feature = "net-profile")]
pub use akuma_kernel_core::nic_profile;
pub use akuma_kernel_core::ntp_boot;
pub use akuma_kernel_core::pmm;
pub use akuma_kernel_core::timer;
pub use akuma_kernel_core::timer::GLOBAL_POLL_STEP;
pub use akuma_kernel_glue::boot;
pub use akuma_kernel_glue::console;
pub use akuma_kernel_glue::halt;
#[cfg(kernel_tests)]
pub use akuma_kernel_glue::linker_syms;
pub use akuma_kernel_glue::platform;
#[cfg(feature = "rump")]
pub use akuma_kernel_glue::rump_proxy;
#[cfg(kernel_smp_shared)]
pub use akuma_kernel_glue::smp_shared;
pub use akuma_kernel_glue::syscall;
pub use akuma_kernel_glue::vfs;
pub use akuma_kernel_glue::{
    compute_heap_size, compute_memory_layout, compute_thread_limit, reserve_calc_ram,
    MemoryLayout,
};
#[cfg(kernel_tests)]
pub(crate) use akuma_virtio::audio;
#[cfg(kernel_tests)]
pub(crate) use akuma_virtio::rng;
#[cfg(kernel_tests)]
use akuma_exec::mmu;

#[cfg(kernel_tests)]
mod async_tests;
#[cfg(kernel_tests)]
mod daif_tests;
#[cfg(kernel_tests)]
mod fs_tests;
#[cfg(all(kernel_tests, feature = "smoltcp"))]
mod network_tests;
#[cfg(kernel_tests)]
mod process_tests;
#[cfg(kernel_tests)]
mod pthread_tests;
#[cfg(all(
    not(kernel_profile_extreme),
    feature = "rump",
    any(not(feature = "no-tests"), feature = "rump-tests"),
))]
mod rump_tests;
#[cfg(kernel_tests)]
mod sync_tests;
#[cfg(kernel_tests)]
mod tests;

/// Hand the suite's entry points to `kernel_main`. The root bin passes this to
/// `akuma_kernel::entry!` as its `before_main`; it does nothing in a build without the suite.
pub fn register() {
    #[cfg(kernel_tests)]
    akuma_kernel_glue::set_boot_test_hooks(akuma_kernel_glue::BootTestHooks {
        daif_tests: daif_tests::run_all_tests,
        memory_tests: tests::run_memory_tests,
        async_tests: async_tests::run_all,
        fs_tests: fs_tests::run_all_tests,
        threading_tests: tests::run_threading_tests,
        sync_tests: sync_tests::run_all_tests,
        pthread_tests: pthread_tests::run_all_tests,
        process_tests: process_tests::run_all_tests,
        benchmarks: tests::run_benchmarks,
        #[cfg(feature = "smoltcp")]
        network_tests: network_tests::run_tests,
        #[cfg(feature = "smoltcp")]
        process_network_tests: process_tests::run_network_tests,
    });
    #[cfg(all(
        not(kernel_profile_extreme),
        feature = "rump",
        any(not(feature = "no-tests"), feature = "rump-tests"),
    ))]
    akuma_kernel_glue::set_rump_tests_hook(rump_tests::run_all_tests);
}
