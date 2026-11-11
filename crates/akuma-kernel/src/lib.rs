//! The full Akuma kernel's arch-neutral front.
//!
//! ```ignore
//! #![no_std]
//! #![no_main]
//!
//! akuma_kernel::entry!();
//! ```
//!
//! That is a whole kernel bin on either architecture. [`entry!`] expands to the symbols the
//! target's boot assembly calls, and each symbol calls into the machine half the target
//! selects: `akuma-kernel-amd64` (`amd64/`) on `x86_64-unknown-none`, `akuma-kernel-glue` on
//! AArch64. It is the same shape as `akuma-tokio-kernel`, one level up
//! (`docs/archive/AKUMA_ARCH_CONVERGENCE_RESEARCH.md` §4, option C).
//!
//! Build features belong to the machine half, not to this crate: a bin depends on its half
//! directly and names the features there.

#![no_std]
#![forbid(unsafe_code)]

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub use akuma_kernel_amd64::{kmain, kmain_mb2};
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub use akuma_kernel_glue::start;

/// The default `before_main` for [`entry!`]: does nothing.
#[doc(hidden)]
pub fn nothing_before_main() {}

/// The kernel's entry symbols.
///
/// On amd64: `kmain` (PVH: QEMU, Firecracker, the in-tree KVM guest) and `kmain_mb2`
/// (GRUB/multiboot2, the bare-metal path), which `akuma-boot-amd64`'s `boot.s` calls once it
/// reaches long mode. The optional `before_main` runs first thing in both; the bin registers
/// its boot self-tests there (`akuma-kernel-tests-amd64`).
#[cfg(target_arch = "x86_64")]
#[macro_export]
macro_rules! entry {
    () => {
        $crate::entry!($crate::nothing_before_main);
    };
    ($before_main:path) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn kmain(hvm_start_info: u64) -> ! {
            $crate::kmain(hvm_start_info, $before_main)
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn kmain_mb2(info_phys: u64) -> ! {
            $crate::kmain_mb2($crate::PhysAddr::new(info_phys), $before_main)
        }
    };
}

/// The kernel's entry symbol.
///
/// On AArch64: `rust_start(dtb)`, which `_boot` (`akuma-boot-aarch64`) calls with the device
/// tree's address. `before_main` runs after the console hook is installed and before
/// `kernel_main`; the root kernel registers its boot self-tests there.
#[cfg(target_arch = "aarch64")]
#[macro_export]
macro_rules! entry {
    () => {
        $crate::entry!($crate::nothing_before_main);
    };
    ($before_main:path) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn rust_start(dtb_ptr: usize) -> ! {
            $crate::start(dtb_ptr, $before_main)
        }
    };
}

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
#[doc(hidden)]
pub use akuma_kernel_amd64::PhysAddr;
