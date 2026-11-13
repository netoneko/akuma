//! Denver's platform front crate: the whole kernel platform of
//! `akuma-tokio-kernel` with a feature map and zero capabilities of its own.
//!
//! ```ignore
//! #![no_std]
//! #![no_main]
//!
//! akuma_jvm_kernel::entry!(main);
//!
//! fn main(kernel: akuma_jvm_kernel::Kernel) -> ! {
//!     kernel.console().disk().net().smp().run_init()
//! }
//! ```
//!
//! Every feature is a 1:1 pass-through to `akuma-tokio-kernel`, which maps its
//! own onto the machine half (`akuma-tokio-kernel-amd64`) the target selects —
//! the same relay one level down: front crate, feature map, `Kernel` re-export.
//! Capabilities added in later passes land behind features *here*, mapped onto
//! existing infra — never a fork (R2), never a default the repo's build did not
//! already have (R6).

#![no_std]
#![forbid(unsafe_code)]

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub use akuma_tokio_kernel::{Kernel, boot_multiboot2};

/// The entry macro, re-exported so a kernel writes `akuma_jvm_kernel::entry!(main)`
/// and names itself through its own package (`CARGO_PKG_NAME` is `uname -n`).
pub use akuma_tokio_kernel::entry;
