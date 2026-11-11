//! The amd64 Akuma kernel. The kernel is `akuma-kernel-amd64` (`amd64/src`); this is its
//! entry symbols, `kmain` and `kmain_mb2`, which `akuma-boot-amd64`'s `boot.s` calls, and the
//! two crates the machine half reaches only through registration: the device drivers
//! (`akuma-drivers-amd64`) and the boot suite (`akuma-kernel-tests-amd64`).

#![no_std]
#![no_main]

// The machine half is linked for its features and its `#[global_allocator]`/`#[panic_handler]`;
// the entry symbols reach it through the front.
use akuma_kernel_amd64 as _;
// Named so cargo does not call the dependency unused; under `no-tests` the crate is empty.
#[cfg(feature = "no-tests")]
use akuma_kernel_tests_amd64 as _;

/// Runs first thing in both entry points, before anything probes or tests.
fn before_main() {
    akuma_drivers_amd64::register();
    #[cfg(not(feature = "no-tests"))]
    akuma_kernel_tests_amd64::register();
}

akuma_kernel::entry!(before_main);
