//! A whole Akuma kernel from one short `main.rs`.
//!
//! ```ignore
//! #![no_std]
//! #![no_main]
//!
//! akuma_tokio_kernel::entry!(main);
//!
//! fn main(kernel: akuma_tokio_kernel::Kernel) -> ! {
//!     kernel.console().disk().net().smp().run_init()
//! }
//! ```
//!
//! The kernel runs unmodified Linux musl binaries — busybox, sshd, git, tokio programs — through
//! `akuma-runtime-tokio`'s syscall surface. This crate is the arch-neutral front: the [`entry!`]
//! macro and the [`Kernel`] builder, re-exported from the machine half the target selects
//! (`akuma-tokio-kernel-amd64` on `x86_64-unknown-none`, `akuma-tokio-kernel-aarch64` on
//! `aarch64-unknown-none`). Each default device is a feature and a
//! builder method; the README is the full brief.

#![no_std]
#![forbid(unsafe_code)]

/// Syscall middleware (B22): one optional table of hooks around the kernel's syscall dispatch, registered with
/// `Kernel::middleware`. Only with the `middleware` feature, which is off by default.
#[cfg(feature = "middleware")]
pub use akuma_middleware as middleware;

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub use akuma_tokio_kernel_amd64::{Kernel, boot_multiboot2};
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub use akuma_tokio_kernel_aarch64::{Kernel, secondary_start};

/// The kernel's entry points, calling `main` with a [`Kernel`] named after the calling package
/// (`CARGO_PKG_NAME` is `uname -n`, `CARGO_PKG_VERSION` the release).
///
/// On amd64 it expands to the two symbols the boot trampoline calls: `kmain` (the PVH entry) and
/// `kmain_mb2` (multiboot2, which says it is not supported and halts). On aarch64 it expands to
/// `rust_start`, which `_boot` (`akuma-boot-aarch64`) calls with the device tree's address.
#[cfg(target_arch = "x86_64")]
#[macro_export]
macro_rules! entry {
    ($main:path) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn kmain(start_info: u64) -> ! {
            $main($crate::Kernel::new(start_info, env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")))
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn kmain_mb2(_info: u64) -> ! {
            $crate::boot_multiboot2()
        }
    };
}

/// See the amd64 spelling: `rust_start(dtb)` is the aarch64 entry `_boot` calls, and
/// `secondary_shared_start` the one a secondary core's trampoline calls.
#[cfg(target_arch = "aarch64")]
#[macro_export]
macro_rules! entry {
    ($main:path) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn rust_start(dtb: u64) -> ! {
            $main($crate::Kernel::new(dtb, env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")))
        }

        /// Where `akuma-boot-aarch64`'s secondary trampoline lands a core PSCI
        /// started (only with the `smp` feature; unreferenced otherwise).
        #[unsafe(no_mangle)]
        pub extern "C" fn secondary_shared_start(_context_id: u64, core: u64) -> ! {
            $crate::secondary_start(core)
        }
    };
}
