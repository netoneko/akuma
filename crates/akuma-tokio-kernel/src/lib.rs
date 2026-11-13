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
//! (`akuma-tokio-kernel-amd64` on `x86_64-unknown-none`). Each default device is a feature and a
//! builder method; the README is the full brief.

#![no_std]
#![forbid(unsafe_code)]

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub use akuma_tokio_kernel_amd64::{Kernel, boot_multiboot2};

/// The kernel's entry points, calling `main` with a [`Kernel`] named after the calling package
/// (`CARGO_PKG_NAME` is `uname -n`, `CARGO_PKG_VERSION` the release).
///
/// Expands to the two symbols the boot trampoline calls: `kmain` (the PVH entry) and `kmain_mb2`
/// (multiboot2, which says it is not supported and halts).
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
