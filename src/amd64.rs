//! The amd64 kernel's bin half: register the drivers and the boot suite, then the entry.
//!
//! The heap, the panic handler and the OOM policy are the machine half's
//! (`akuma-kernel-amd64`), and so is the linker script (`build.rs` passes it). `entry!` defines
//! `kmain`/`kmain_mb2`, which the PVH and multiboot2 boot stubs call.

use akuma_kernel_amd64 as _;
#[cfg(feature = "no-tests")]
use akuma_kernel_tests_amd64 as _;

fn before_main() {
    akuma_drivers_amd64::register();
    #[cfg(not(feature = "no-tests"))]
    akuma_kernel_tests_amd64::register();
}

akuma_kernel::entry!(before_main);
