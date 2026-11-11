//! The amd64 Akuma kernel. The kernel is `akuma-kernel-amd64` (`amd64/src`); this is its
//! entry symbols, `kmain` and `kmain_mb2`, which `akuma-boot-amd64`'s `boot.s` calls, and the
//! boot suite (`akuma-kernel-tests-amd64`), registered before either runs.

#![no_std]
#![no_main]

// The machine half is linked for its features and its `#[global_allocator]`/`#[panic_handler]`;
// the entry symbols reach it through the front.
use akuma_kernel_amd64 as _;

#[cfg(not(feature = "no-tests"))]
akuma_kernel::entry!(akuma_kernel_tests_amd64::register);
// Named so cargo does not call the dependency unused; under `no-tests` the crate is empty.
#[cfg(feature = "no-tests")]
use akuma_kernel_tests_amd64 as _;
#[cfg(feature = "no-tests")]
akuma_kernel::entry!();
